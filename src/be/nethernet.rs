//! NetherNet status queries for Bedrock Edition servers.
//!
//! Bedrock Edition `1.26.51+` servers can advertise their status through a
//! NetherNet join endpoint instead of (or in addition to) the legacy RakNet
//! unconnected ping. The endpoint is `GET {serverUrl}/v1/join` and returns a
//! small JSON document; any `2xx` status means the server supports NetherNet.
//! See the
//! [NetherNet onboarding guide](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/).
//!
//! Queries reuse a process-wide
//! [`shared_client`](crate::be::nethernet::shared_client) so connection
//! pooling is not rebuilt per query, cap the response body size, and validate
//! the reported player counts and protocol field. The HTTPS attempt on the
//! game port is the primary path; a plain `http://` endpoint on the same host
//! and port is only tried after a transport-level failure (connection, TLS, or
//! timeout), never after the server answered with an HTTP status.
//!
//! [`BedrockNetherNetMotd::latency`](crate::be::nethernet::BedrockNetherNetMotd::latency)
//! reports the HTTP signaling round trip. It is *not* the game connection RTT:
//! the game itself runs over WebRTC/UDP after the join handshake, so the two
//! numbers must not be equated.

use std::borrow::Cow;
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::MotdError;
use crate::utils::{normalize_server_host, validate_port};

const JOIN_PATH: &str = "/v1/join";
const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default response body cap for `NetherNet` join responses (64 KiB).
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Process-wide shared HTTP client used by every `NetherNet` query.
static SHARED_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Returns the process-wide shared HTTP client used for `NetherNet` queries.
///
/// The client uses rustls with the webpki root store, a 5 second connect
/// timeout, no redirects, and a `mc-motd/<version>` user agent. Reusing one
/// client keeps its connection pool alive across queries instead of rebuilding
/// it per query.
#[must_use]
pub fn shared_client() -> &'static reqwest::Client {
    SHARED_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("mc-motd/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CLIENT_CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Owned Bedrock Edition `NetherNet` join response.
///
/// Fields the server did not report stay `None`; unknown fields in the JSON
/// document are ignored so servers can add extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BedrockNetherNetMotd {
    /// Server name shown as the first MOTD line (`name`).
    pub name: String,
    /// Bedrock protocol version (`protocol`).
    pub protocol: Option<u32>,
    /// Human-readable game version (`version`).
    pub version: String,
    /// World or level name (`level`).
    pub level: String,
    /// Current online player count (`players`).
    pub players_online: Option<u32>,
    /// Maximum player count (`maxPlayers`).
    pub players_max: Option<u32>,
    /// Numeric game mode id (`gameType`): 0 Survival, 1 Creative, 2 Adventure.
    pub game_type: Option<u32>,
    /// Server data schema version (`dataVersion`), when reported.
    pub data_version: Option<u32>,
    /// Whether the world allows the editor (`editor`), when reported.
    pub editor: Option<bool>,
    /// Whether the world is hardcore (`hardcore`), when reported.
    pub hardcore: Option<bool>,
    /// Whether online authentication is required (`onlineAuth`), when reported.
    pub online_auth: Option<bool>,
    /// Whether self-signed authentication is accepted (`selfSignedAuth`).
    pub self_signed_auth: Option<bool>,
    /// Join nonce (`nonce`), when reported.
    pub nonce: Option<String>,
    /// `NetherNet` transport layer id (`transportLayer`), when reported.
    pub transport_layer: Option<u32>,
    /// Connection id (`connection`), when reported.
    pub connection: Option<u32>,
    /// HTTP round-trip time of the successful query.
    ///
    /// This is HTTPS/HTTP signaling latency measured around the join request.
    /// It is not the game connection RTT, which requires a WebRTC handshake.
    pub latency: Duration,
}

impl BedrockNetherNetMotd {
    /// Human-readable name for the numeric game mode id.
    #[must_use]
    pub fn game_type_name(&self) -> Option<&'static str> {
        game_type_name(self.game_type)
    }
}

/// Borrowed Bedrock Edition `NetherNet` join response.
///
/// String fields borrow from the input text, so parsing this variant performs
/// no per-field allocation. Use [`BedrockNetherNetMotdRef::to_owned`] when an
/// owned value is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BedrockNetherNetMotdRef<'a> {
    /// Server name shown as the first MOTD line (`name`).
    pub name: Cow<'a, str>,
    /// Bedrock protocol version (`protocol`).
    pub protocol: Option<u32>,
    /// Human-readable game version (`version`).
    pub version: Cow<'a, str>,
    /// World or level name (`level`).
    pub level: Cow<'a, str>,
    /// Current online player count (`players`).
    pub players_online: Option<u32>,
    /// Maximum player count (`maxPlayers`).
    pub players_max: Option<u32>,
    /// Numeric game mode id (`gameType`): 0 Survival, 1 Creative, 2 Adventure.
    pub game_type: Option<u32>,
    /// Server data schema version (`dataVersion`), when reported.
    pub data_version: Option<u32>,
    /// Whether the world allows the editor (`editor`), when reported.
    pub editor: Option<bool>,
    /// Whether the world is hardcore (`hardcore`), when reported.
    pub hardcore: Option<bool>,
    /// Whether online authentication is required (`onlineAuth`), when reported.
    pub online_auth: Option<bool>,
    /// Whether self-signed authentication is accepted (`selfSignedAuth`).
    pub self_signed_auth: Option<bool>,
    /// Join nonce (`nonce`), when reported.
    pub nonce: Option<Cow<'a, str>>,
    /// `NetherNet` transport layer id (`transportLayer`), when reported.
    pub transport_layer: Option<u32>,
    /// Connection id (`connection`), when reported.
    pub connection: Option<u32>,
    /// HTTP round-trip time of the successful query; zero for pure parsing.
    ///
    /// This is HTTPS/HTTP signaling latency, not the game connection RTT.
    pub latency: Duration,
}

impl BedrockNetherNetMotdRef<'_> {
    /// Convert the borrowed response into an owned one.
    #[must_use]
    pub fn to_owned(&self) -> BedrockNetherNetMotd {
        BedrockNetherNetMotd {
            name: self.name.to_string(),
            protocol: self.protocol,
            version: self.version.to_string(),
            level: self.level.to_string(),
            players_online: self.players_online,
            players_max: self.players_max,
            game_type: self.game_type,
            data_version: self.data_version,
            editor: self.editor,
            hardcore: self.hardcore,
            online_auth: self.online_auth,
            self_signed_auth: self.self_signed_auth,
            nonce: self.nonce.as_ref().map(Cow::to_string),
            transport_layer: self.transport_layer,
            connection: self.connection,
            latency: self.latency,
        }
    }

    /// Human-readable name for the numeric game mode id.
    #[must_use]
    pub fn game_type_name(&self) -> Option<&'static str> {
        game_type_name(self.game_type)
    }
}

/// Human-readable name for the numeric `NetherNet` game mode id.
fn game_type_name(game_type: Option<u32>) -> Option<&'static str> {
    match game_type? {
        0 => Some("Survival"),
        1 => Some("Creative"),
        2 => Some("Adventure"),
        _ => None,
    }
}

/// Options for `NetherNet` join endpoint queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetherNetQueryOptions {
    /// Timeout applied to each HTTP attempt, including the body read.
    pub timeout: Duration,
    /// Maximum accepted response body size in bytes.
    pub max_response_bytes: usize,
    /// Whether a plain `http://` endpoint on the same host and port is tried
    /// after the primary `https://` attempt fails at the transport level.
    pub allow_http_fallback: bool,
}

impl Default for NetherNetQueryOptions {
    fn default() -> Self {
        Self {
            timeout: crate::DEFAULT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            allow_http_fallback: true,
        }
    }
}

/// Query a Bedrock Edition `NetherNet` join endpoint by host and port.
///
/// The endpoint URL is `https://{host}:{port}/v1/join`; the same game port
/// serves the `NetherNet` signaling endpoint. See [`query_nethernet_url`] for
/// URL-level control.
///
/// # Errors
///
/// Returns [`MotdError`] if the host or port is invalid, the HTTP request
/// fails or times out, the response exceeds the body size limit, the status is
/// not `2xx`, or the JSON document cannot be parsed or validated.
pub async fn query_nethernet(
    host: impl AsRef<str>,
    port: u16,
    timeout: Duration,
) -> Result<BedrockNetherNetMotd, MotdError> {
    query_nethernet_with_options(
        host,
        port,
        NetherNetQueryOptions {
            timeout,
            ..NetherNetQueryOptions::default()
        },
    )
    .await
}

/// Query a Bedrock Edition `NetherNet` join endpoint with explicit options.
///
/// # Errors
///
/// Returns [`MotdError`] if the host or port is invalid, the HTTP request
/// fails or times out, the response exceeds the body size limit, the status is
/// not `2xx`, or the JSON document cannot be parsed or validated.
pub async fn query_nethernet_with_options(
    host: impl AsRef<str>,
    port: u16,
    options: NetherNetQueryOptions,
) -> Result<BedrockNetherNetMotd, MotdError> {
    validate_port(port)?;
    let host = normalize_server_host(host.as_ref())?;
    let url = build_join_url("https", host.as_ref(), port);
    query_nethernet_url(&url, options).await
}

/// Query a `NetherNet` join endpoint URL directly.
///
/// Accepts `https://` and `http://` URLs. An `https://` URL falls back to the
/// same host and port over plain `http://` when the TLS attempt fails at the
/// transport level and [`NetherNetQueryOptions::allow_http_fallback`] is set.
/// Receiving any HTTP status, including non-`2xx`, never triggers the
/// fallback.
///
/// # Errors
///
/// Returns [`MotdError::InvalidInput`] if the URL is not a valid `http` or
/// `https` URL, [`MotdError::HttpStatus`] for non-`2xx` responses, and
/// [`MotdError`] for transport failures, oversized bodies, and invalid JSON.
pub async fn query_nethernet_url(
    url: &str,
    options: NetherNetQueryOptions,
) -> Result<BedrockNetherNetMotd, MotdError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| MotdError::InvalidInput("NetherNet URL is invalid"))?;
    match parsed.scheme() {
        "https" => probe_https(parsed, options).await,
        "http" => {
            let started = Instant::now();
            let body = fetch_join(&parsed, options.timeout, options.max_response_bytes).await?;
            finish_join_slice(&body, started.elapsed())
        }
        _ => Err(MotdError::InvalidInput(
            "NetherNet URL scheme must be http or https",
        )),
    }
}

/// Parse the JSON body of a `NetherNet` `GET /v1/join` response.
///
/// The parsed [`BedrockNetherNetMotd::latency`] is zero because parsing alone
/// performs no network I/O.
///
/// # Errors
///
/// Returns [`MotdError::InvalidPacket`] when the body is not valid JSON, the
/// required `name` field is missing or empty, a present numeric field is not a
/// valid non-negative `u32`, or `players` exceeds `maxPlayers`.
pub fn parse_nethernet_join_json(text: &str) -> Result<BedrockNetherNetMotd, MotdError> {
    Ok(parse_nethernet_join_json_ref(text)?.to_owned())
}

/// Parse the JSON body of a `NetherNet` `GET /v1/join` response without
/// allocating.
///
/// String fields borrow from `text` via [`BedrockNetherNetMotdRef`], so
/// repeated parsing of polling responses allocates nothing per call.
///
/// # Errors
///
/// Returns [`MotdError::InvalidPacket`] when the body is not valid JSON, the
/// required `name` field is missing or empty, a present numeric field is not a
/// valid non-negative `u32`, or `players` exceeds `maxPlayers`.
pub fn parse_nethernet_join_json_ref(text: &str) -> Result<BedrockNetherNetMotdRef<'_>, MotdError> {
    let response: JoinResponseRef<'_> = serde_json::from_str(text)
        .map_err(|_| MotdError::InvalidPacket("NetherNet join response is not valid JSON"))?;
    validate_join_response(response)
}

/// Validate a parsed `NetherNet` join response.
fn validate_join_response(
    response: JoinResponseRef<'_>,
) -> Result<BedrockNetherNetMotdRef<'_>, MotdError> {
    if response.name.is_empty() {
        return Err(MotdError::InvalidPacket(
            "NetherNet join response is missing the name field",
        ));
    }
    if let (Some(players), Some(max_players)) = (response.players, response.max_players) {
        if players > max_players {
            return Err(MotdError::InvalidPacket(
                "NetherNet player count exceeds the reported maximum",
            ));
        }
    }

    Ok(BedrockNetherNetMotdRef {
        name: response.name,
        protocol: response.protocol,
        version: response.version,
        level: response.level,
        players_online: response.players,
        players_max: response.max_players,
        game_type: response.game_type,
        data_version: response.data_version,
        editor: response.editor,
        hardcore: response.hardcore,
        online_auth: response.online_auth,
        self_signed_auth: response.self_signed_auth,
        nonce: response.nonce,
        transport_layer: response.transport_layer,
        connection: response.connection,
        latency: Duration::ZERO,
    })
}

/// Raw `NetherNet` join response as reported by the server. String fields
/// borrow from the input so parsing stays allocation-free.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct JoinResponseRef<'a> {
    #[serde(borrow)]
    name: Cow<'a, str>,
    protocol: Option<u32>,
    #[serde(borrow)]
    version: Cow<'a, str>,
    #[serde(borrow)]
    level: Cow<'a, str>,
    players: Option<u32>,
    max_players: Option<u32>,
    game_type: Option<u32>,
    data_version: Option<u32>,
    editor: Option<bool>,
    hardcore: Option<bool>,
    online_auth: Option<bool>,
    self_signed_auth: Option<bool>,
    #[serde(borrow)]
    nonce: Option<Cow<'a, str>>,
    transport_layer: Option<u32>,
    connection: Option<u32>,
}

async fn probe_https(
    url: reqwest::Url,
    options: NetherNetQueryOptions,
) -> Result<BedrockNetherNetMotd, MotdError> {
    let deadline = Instant::now() + options.timeout;
    let attempt_started = Instant::now();
    match fetch_join(&url, options.timeout, options.max_response_bytes).await {
        Ok(body) => finish_join_slice(&body, attempt_started.elapsed()),
        Err(https_error) => {
            // Only fall back when the HTTPS attempt failed before the server
            // produced any response: transport errors and timeouts. A server
            // that answers (any status, or a rejected body) is treated as a
            // definitive answer about its NetherNet support.
            let received_response = matches!(
                https_error,
                MotdError::HttpStatus(_) | MotdError::InvalidPacket(_)
            );
            if received_response || !options.allow_http_fallback {
                return Err(https_error);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(https_error);
            }
            let mut http_url = url.clone();
            if http_url.set_scheme("http").is_err() {
                return Err(https_error);
            }
            let fallback_started = Instant::now();
            match fetch_join(&http_url, remaining, options.max_response_bytes).await {
                Ok(body) => finish_join_slice(&body, fallback_started.elapsed()),
                Err(_) => Err(https_error),
            }
        }
    }
}

async fn fetch_join(
    url: &reqwest::Url,
    timeout: Duration,
    max_response_bytes: usize,
) -> Result<Vec<u8>, MotdError> {
    let mut response = shared_client()
        .get(url.clone())
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| map_transport_error(&error))?;

    let status = response.status();
    if !status.is_success() {
        return Err(MotdError::HttpStatus(status.as_u16()));
    }

    if response
        .content_length()
        .is_some_and(|length| length > max_response_bytes as u64)
    {
        return Err(MotdError::InvalidPacket(
            "NetherNet response body exceeds the size limit",
        ));
    }

    let mut body = Vec::with_capacity(1024);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| map_transport_error(&error))?
    {
        if body.len() + chunk.len() > max_response_bytes {
            return Err(MotdError::InvalidPacket(
                "NetherNet response body exceeds the size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// Parse a fetched join response body into an owned MOTD with latency set.
fn finish_join_slice(bytes: &[u8], latency: Duration) -> Result<BedrockNetherNetMotd, MotdError> {
    let response: JoinResponseRef<'_> = serde_json::from_slice(bytes)
        .map_err(|_| MotdError::InvalidPacket("NetherNet join response is not valid JSON"))?;
    validate_join_response(response).map(|motd| {
        let mut motd = motd.to_owned();
        motd.latency = latency;
        motd
    })
}

fn map_transport_error(error: &reqwest::Error) -> MotdError {
    if error.is_timeout() {
        MotdError::Timeout
    } else {
        MotdError::Http(error.to_string())
    }
}

fn build_join_url(scheme: &str, host: &str, port: u16) -> String {
    let host_text = match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]"),
        _ => host.to_string(),
    };
    format!("{scheme}://{host_text}:{port}{JOIN_PATH}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{Ipv4Addr, SocketAddr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const ENCHANTED_JOIN_JSON: &str = r#"{"dataVersion":7,"name":"FROSTBITE EVENT ★","protocol":2225,"version":"1.26.60","level":"Enchanted","players":1261,"maxPlayers":2261,"gameType":0,"editor":false,"hardcore":false,"onlineAuth":true,"selfSignedAuth":false,"nonce":"58357ae4f92347f2","transportLayer":2,"connection":4}"#;
    const HIVE_JOIN_JSON: &str = r#"{"name":"HIVE-O-WEEN | BEDWARS S5","protocol":944,"version":"1.26.14.1","level":"Hive Games","players":10012,"maxPlayers":100000,"gameType":0}"#;

    #[test]
    fn parses_full_join_response() {
        let motd = parse_nethernet_join_json(ENCHANTED_JOIN_JSON).expect("join should parse");

        assert_eq!(motd.name, "FROSTBITE EVENT ★");
        assert_eq!(motd.protocol, Some(2225));
        assert_eq!(motd.version, "1.26.60");
        assert_eq!(motd.level, "Enchanted");
        assert_eq!(motd.players_online, Some(1261));
        assert_eq!(motd.players_max, Some(2261));
        assert_eq!(motd.game_type, Some(0));
        assert_eq!(motd.data_version, Some(7));
        assert_eq!(motd.online_auth, Some(true));
        assert_eq!(motd.nonce.as_deref(), Some("58357ae4f92347f2"));
        assert_eq!(motd.transport_layer, Some(2));
        assert_eq!(motd.connection, Some(4));
        assert_eq!(motd.latency, Duration::ZERO);
    }

    #[test]
    fn parses_minimal_join_response() {
        let motd = parse_nethernet_join_json(HIVE_JOIN_JSON).expect("join should parse");

        assert_eq!(motd.name, "HIVE-O-WEEN | BEDWARS S5");
        assert_eq!(motd.protocol, Some(944));
        assert_eq!(motd.players_online, Some(10_012));
        assert_eq!(motd.players_max, Some(100_000));
        assert_eq!(motd.data_version, None);
        assert_eq!(motd.nonce, None);
        assert_eq!(motd.transport_layer, None);
        assert_eq!(motd.game_type_name(), Some("Survival"));
    }

    #[test]
    fn parses_join_response_borrowed_without_allocation() {
        let motd = parse_nethernet_join_json_ref(ENCHANTED_JOIN_JSON).expect("join should parse");

        assert!(matches!(motd.name, Cow::Borrowed(_)), "name should borrow");
        assert_eq!(motd.name, "FROSTBITE EVENT ★");
        assert_eq!(motd.protocol, Some(2225));
        assert_eq!(
            motd.nonce.as_ref().map(Cow::as_ref),
            Some("58357ae4f92347f2")
        );
        assert_eq!(motd.game_type_name(), Some("Survival"));

        let owned = motd.to_owned();
        assert_eq!(owned.name, "FROSTBITE EVENT ★");
        assert_eq!(owned.latency, Duration::ZERO);
    }

    #[test]
    fn rejects_missing_name() {
        let error = parse_nethernet_join_json(r#"{"protocol":2225}"#)
            .expect_err("missing name should fail");

        assert!(matches!(error, MotdError::InvalidPacket(_)));
    }

    #[test]
    fn rejects_players_above_max() {
        let error = parse_nethernet_join_json(r#"{"name":"Server","players":20,"maxPlayers":10}"#)
            .expect_err("players above max should fail");

        assert!(matches!(error, MotdError::InvalidPacket(_)));
    }

    #[test]
    fn rejects_invalid_numeric_field() {
        let error = parse_nethernet_join_json(r#"{"name":"Server","players":12.5}"#)
            .expect_err("float player count should fail");

        assert!(matches!(error, MotdError::InvalidPacket(_)));
    }

    #[test]
    fn rejects_invalid_json() {
        let error = parse_nethernet_join_json("not json").expect_err("bad json should fail");

        assert!(matches!(error, MotdError::InvalidPacket(_)));
    }

    #[test]
    fn game_type_names_cover_documented_modes() {
        let mut motd =
            parse_nethernet_join_json(r#"{"name":"Server"}"#).expect("join should parse");

        motd.game_type = Some(1);
        assert_eq!(motd.game_type_name(), Some("Creative"));
        motd.game_type = Some(2);
        assert_eq!(motd.game_type_name(), Some("Adventure"));
        motd.game_type = Some(7);
        assert_eq!(motd.game_type_name(), None);
        motd.game_type = None;
        assert_eq!(motd.game_type_name(), None);
    }

    #[test]
    fn join_url_brackets_ipv6_literals() {
        assert_eq!(
            build_join_url("https", "play.example.com", 19_132),
            "https://play.example.com:19132/v1/join"
        );
        assert_eq!(
            build_join_url("https", "::1", 19_132),
            "https://[::1]:19132/v1/join"
        );
        assert_eq!(
            build_join_url("https", "127.0.0.1", 19_132),
            "https://127.0.0.1:19132/v1/join"
        );
    }

    #[tokio::test]
    async fn http_url_parses_join_response() {
        let address = spawn_mock_join_server("HTTP/1.1 200 OK", ENCHANTED_JOIN_JSON).await;
        let url = format!("http://{address}/v1/join");

        let motd = query_nethernet_url(&url, NetherNetQueryOptions::default())
            .await
            .expect("mock join should parse");

        assert_eq!(motd.name, "FROSTBITE EVENT ★");
        assert_eq!(motd.protocol, Some(2225));
    }

    #[tokio::test]
    async fn https_transport_failure_falls_back_to_http() {
        let address = spawn_mock_join_server("HTTP/1.1 200 OK", HIVE_JOIN_JSON).await;
        let url = format!("https://{address}/v1/join");

        let motd = query_nethernet_url(&url, NetherNetQueryOptions::default())
            .await
            .expect("http fallback should parse");

        assert_eq!(motd.name, "HIVE-O-WEEN | BEDWARS S5");
    }

    #[tokio::test]
    async fn non_2xx_status_is_reported_without_fallback() {
        let address = spawn_mock_join_server("HTTP/1.1 404 Not Found", "nope").await;
        let url = format!("http://{address}/v1/join");

        let error = query_nethernet_url(&url, NetherNetQueryOptions::default())
            .await
            .expect_err("404 should fail");

        assert!(matches!(error, MotdError::HttpStatus(404)));
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let address = spawn_mock_join_server("HTTP/1.1 200 OK", ENCHANTED_JOIN_JSON).await;
        let url = format!("http://{address}/v1/join");
        let options = NetherNetQueryOptions {
            max_response_bytes: 16,
            ..NetherNetQueryOptions::default()
        };

        let error = query_nethernet_url(&url, options)
            .await
            .expect_err("oversized body should fail");

        assert!(matches!(error, MotdError::InvalidPacket(_)));
    }

    #[tokio::test]
    async fn closed_port_maps_to_transport_error() {
        // Port 1 on the loopback interface is virtually always closed; the
        // failure surfaces as a connect error or a timeout depending on the
        // platform's firewall behavior.
        let error = query_nethernet_url(
            "http://127.0.0.1:1/v1/join",
            NetherNetQueryOptions::default(),
        )
        .await
        .expect_err("closed port should fail");

        assert!(matches!(error, MotdError::Http(_) | MotdError::Timeout));
    }

    #[tokio::test]
    #[ignore = "queries public Minecraft servers; run with --ignored"]
    async fn live_nethernet_servers_respond() {
        for host in [
            "play.enchanted.gg",
            "nethernet.playhive.com",
            "mco.lbsg.net",
        ] {
            let motd = query_nethernet(host, 19_132, Duration::from_secs(5))
                .await
                .unwrap_or_else(|error| panic!("{host} should answer NetherNet: {error}"));

            assert!(!motd.name.is_empty(), "{host} should report a name");
            assert!(motd.protocol.is_some(), "{host} should report a protocol");
        }
    }

    /// Spawn a plain HTTP/1.1 mock server that answers `GET /v1/join` with the
    /// given status line and body. TLS handshake bytes are dropped immediately
    /// so HTTPS attempts fail fast and exercise the HTTP fallback path.
    async fn spawn_mock_join_server(status_line: &'static str, body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = [0_u8; 512];
                let first_len = match stream.read(&mut buffer).await {
                    Ok(len) if len > 0 => len,
                    _ => continue,
                };
                if !buffer[..first_len].starts_with(b"GET") {
                    // TLS handshake or other non-HTTP traffic: close now so
                    // the client fails fast instead of waiting for a reply.
                    continue;
                }
                let mut request = Vec::from(&buffer[..first_len]);
                let header_end: &[u8] = b"\r\n\r\n";
                loop {
                    if request
                        .windows(header_end.len())
                        .any(|window| window == header_end)
                    {
                        break;
                    }
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(len) => request.extend_from_slice(&buffer[..len]),
                    }
                }
                let response = format!(
                    "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        address
    }
}
