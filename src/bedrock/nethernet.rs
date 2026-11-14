//! `NetherNet` HTTPS/HTTP status queries.
//!
//! [`query`] always speaks the `NetherNet` join endpoint, even when the server
//! also answers `RakNet`; for automatic transport selection use
//! [`crate::bedrock::query`]. The JSON codec lives in
//! [`crate::protocol::bedrock::nethernet`].
//!
//! Bedrock Edition `1.26.51+` servers advertise their status through
//! `GET https://{host}:{game-port}/v1/join`; any `2xx` status means the server
//! supports `NetherNet`. See the
//! [NetherNet onboarding guide](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/).

use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::address::ServerAddress;
use crate::client::{Parts, QueryOptions, ResolvedOptions};
use crate::error::{Attempt, Error, Result};
use crate::protocol::bedrock::nethernet as codec;
use crate::status::{BedrockDetails, Details, Status, Transport};

const JOIN_PATH: &str = "/v1/join";
const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Default response body cap for `NetherNet` join responses (64 KiB).
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Process-wide shared HTTP client used by the default [`crate::Client`].
static SHARED_HTTP: OnceLock<std::result::Result<reqwest::Client, String>> = OnceLock::new();

/// Build the shared HTTP client used for `NetherNet` queries.
///
/// The client uses rustls with the webpki root store, a 5 second connect
/// timeout, no redirects, and a `mc-motd/<version>` user agent. Reusing one
/// client keeps its connection pool alive across queries instead of rebuilding
/// it per query.
pub(crate) fn build_http_client() -> std::result::Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .user_agent(concat!("mc-motd/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CLIENT_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| Error::Http(error.to_string()))
}

/// Return the HTTP client used by the default [`crate::Client`].
pub(crate) fn shared_http() -> std::result::Result<&'static reqwest::Client, Error> {
    SHARED_HTTP
        .get_or_init(|| build_http_client().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|message| Error::Http(message.clone()))
}

/// Query a Bedrock Edition server address over `NetherNet` only.
///
/// The address may be `host`, `host:port`, or `[ipv6]:port`; the Bedrock
/// default port applies when no explicit port is present. The endpoint URL is
/// `https://{host}:{port}/v1/join`; a plain `http://` endpoint on the same
/// host and port is only tried after a transport-level failure of the HTTPS
/// attempt and when [`QueryOptions::allow_insecure_http`] is set.
///
/// # Errors
///
/// Returns [`Error`] if the address is invalid, the HTTP request fails or
/// exceeds the deadline, the response exceeds the body size limit, the status
/// is not `2xx`, or the JSON document cannot be parsed or validated.
pub async fn query(address: impl AsRef<str>) -> Result<Status> {
    crate::client::default_client()?.nethernet(address).await
}

/// Query a Bedrock Edition server address over `NetherNet` only with explicit
/// options.
///
/// # Errors
///
/// Returns [`Error`] under the same conditions as [`query`].
pub async fn query_with(address: impl AsRef<str>, options: QueryOptions) -> Result<Status> {
    crate::client::default_client()?
        .nethernet_with(address, options)
        .await
}

/// Query a `NetherNet` join endpoint URL directly.
///
/// Accepts `https://` and `http://` URLs. An `https://` URL falls back to the
/// same host and port over plain `http://` when the TLS attempt fails at the
/// transport level and the insecure-HTTP policy allows it. Receiving any HTTP
/// status, including non-`2xx`, never triggers the fallback.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the URL is not a valid `http` or
/// `https` URL, [`Error::HttpStatus`] for non-`2xx` responses, and [`Error`]
/// for transport failures, oversized bodies, and invalid JSON.
pub async fn query_url(url: &str, options: QueryOptions) -> Result<Status> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| Error::InvalidInput("NetherNet URL is invalid"))?;
    let resolved = options.resolve_over(&crate::client::QueryOptions::default());
    let started = Instant::now();
    let deadline = started
        .checked_add(resolved.timeout)
        .ok_or(Error::InvalidInput("query timeout is too large"))?;
    let http = shared_http()?;
    let used_https;
    let mut details = match parsed.scheme() {
        "https" => {
            let (details, scheme) = probe_join(http, &parsed, deadline, &resolved, true).await?;
            used_https = scheme;
            details
        }
        "http" => {
            used_https = false;
            let body = fetch_join(http, &parsed, deadline, resolved.max_response_bytes).await?;
            let mut details = codec::parse_join_slice(&body)?;
            details.insecure_http = true;
            details
        }
        _ => {
            return Err(Error::InvalidInput(
                "NetherNet URL scheme must be http or https",
            ));
        }
    };
    let elapsed = started.elapsed();
    let port = parsed.port().unwrap_or(crate::DEFAULT_BEDROCK_PORT);
    // Plain HTTP answers are marked on the details so the derived transport
    // reports the insecure scheme.
    details.insecure_http = !used_https;
    let mut status = Status::from_details(
        ServerAddress {
            host: parsed.host_str().unwrap_or_default().to_string(),
            // The join endpoint is Bedrock-only; both port fields carry the
            // endpoint port so the struct keeps its non-zero invariant.
            java_port: port,
            bedrock_port: port,
            explicit_port: parsed.port(),
        },
        Details::Bedrock(BedrockDetails::NetherNet(details)),
    );
    status.elapsed = elapsed;
    Ok(status)
}

/// Probe the `NetherNet` transport for the engine, caching the transport on
/// success.
pub(crate) async fn probe(
    address: &ServerAddress,
    deadline: Instant,
    parts: &Parts<'_>,
    resolved: &ResolvedOptions,
    prefer_https: bool,
) -> Result<Status, Attempt> {
    let port = address.bedrock_port;
    let target = format!("{}:{}", address.host, port);
    let attempt_transport = if prefer_https {
        Transport::NetherNetHttps
    } else {
        Transport::NetherNetHttp
    };

    let url = match build_join_url(
        if prefer_https { "https" } else { "http" },
        &address.host,
        port,
    ) {
        Ok(url) => url,
        Err(error) => {
            return Err(Attempt {
                transport: attempt_transport,
                target,
                error: Box::new(error),
            });
        }
    };

    let used_https;
    let details = match probe_join(parts.http, &url, deadline, resolved, prefer_https).await {
        Ok((details, scheme)) => {
            used_https = scheme;
            details
        }
        Err(error) => {
            return Err(Attempt {
                transport: attempt_transport,
                target,
                error: Box::new(error),
            });
        }
    };
    parts.cache.store(
        &address.host,
        port,
        if used_https {
            Transport::NetherNetHttps
        } else {
            Transport::NetherNetHttp
        },
    );

    let mut details = details;
    details.insecure_http = !used_https;
    Ok(Status::from_details(
        address.clone(),
        Details::Bedrock(BedrockDetails::NetherNet(details)),
    ))
}

/// Probe the HTTPS join endpoint with the documented plain-HTTP fallback.
async fn probe_join(
    http: &reqwest::Client,
    url: &reqwest::Url,
    deadline: Instant,
    resolved: &ResolvedOptions,
    prefer_https: bool,
) -> Result<(codec::NetherNetDetails, bool), Error> {
    let first_result = fetch_join(http, url, deadline, resolved.max_response_bytes).await;
    let first_is_https = prefer_https;
    match first_result {
        Ok(body) => Ok((codec::parse_join_slice(&body)?, first_is_https)),
        Err(first_error) => {
            // Only switch schemes when the first attempt failed before the
            // server produced any response: transport errors and timeouts. A
            // server that answers (any status, or a rejected body) is treated
            // as a definitive answer about its `NetherNet` support.
            let received_response =
                matches!(first_error, Error::HttpStatus(_) | Error::InvalidPacket(_));
            if received_response {
                return Err(first_error);
            }
            let insecure_allowed = if first_is_https {
                resolved.allow_insecure_http
            } else {
                // Plain HTTP failed; switching to HTTPS is always allowed
                // because it is the stronger scheme.
                true
            };
            if !insecure_allowed {
                return Err(first_error);
            }
            let other_url = switch_scheme(url)?;
            match fetch_join(http, &other_url, deadline, resolved.max_response_bytes).await {
                Ok(body) => {
                    let other_is_https = !first_is_https;
                    Ok((codec::parse_join_slice(&body)?, other_is_https))
                }
                Err(_) => Err(first_error),
            }
        }
    }
}

/// Swap the URL scheme between `https` and `http`.
fn switch_scheme(url: &reqwest::Url) -> Result<reqwest::Url, Error> {
    let mut switched = url.clone();
    let target_scheme = if url.scheme() == "https" {
        "http"
    } else {
        "https"
    };
    switched
        .set_scheme(target_scheme)
        .map_err(|()| Error::InvalidInput("NetherNet URL scheme cannot be switched"))?;
    Ok(switched)
}

/// Fetch the join response body with a deadline and a hard body size cap.
async fn fetch_join(
    http: &reqwest::Client,
    url: &reqwest::Url,
    deadline: Instant,
    max_response_bytes: usize,
) -> Result<Vec<u8>, Error> {
    let mut response = http
        .get(url.clone())
        .timeout(crate::client::engine::remaining(deadline)?)
        .send()
        .await
        .map_err(|error| map_transport_error(&error))?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::HttpStatus(status.as_u16()));
    }

    if response
        .content_length()
        .is_some_and(|length| length > max_response_bytes as u64)
    {
        return Err(Error::InvalidPacket(
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
            return Err(Error::InvalidPacket(
                "NetherNet response body exceeds the size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

fn map_transport_error(error: &reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Http(error.to_string())
    }
}

/// Build the join endpoint URL, bracketing IPv6 literals.
fn build_join_url(scheme: &str, host: &str, port: u16) -> Result<reqwest::Url, Error> {
    let host_text = match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]"),
        _ => host.to_string(),
    };
    reqwest::Url::parse(&format!("{scheme}://{host_text}:{port}{JOIN_PATH}"))
        .map_err(|_| Error::InvalidInput("NetherNet URL is invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{Ipv4Addr, SocketAddr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const ENCHANTED_JOIN_JSON: &str = r#"{"dataVersion":7,"name":"FROSTBITE EVENT ★","protocol":2225,"version":"1.26.60","level":"Enchanted","players":1261,"maxPlayers":2261,"gameType":0,"editor":false,"hardcore":false,"onlineAuth":true,"selfSignedAuth":false,"nonce":"58357ae4f92347f2","transportLayer":2,"connection":4}"#;
    const HIVE_JOIN_JSON: &str = r#"{"name":"HIVE-O-WEEN | BEDWARS S5","protocol":944,"version":"1.26.14.1","level":"Hive Games","players":10012,"maxPlayers":100000,"gameType":0}"#;

    #[tokio::test]
    async fn http_url_parses_join_response() {
        let address = spawn_mock_join_server("HTTP/1.1 200 OK", ENCHANTED_JOIN_JSON).await;
        let url = format!("http://{address}/v1/join");

        let status = query_url(&url, QueryOptions::default())
            .await
            .expect("mock join should parse");

        assert_eq!(status.motd, "FROSTBITE EVENT ★");
        assert_eq!(status.version.protocol, Some(2225));
        assert_eq!(status.transport(), Transport::NetherNetHttp);
    }

    #[tokio::test]
    async fn https_transport_failure_falls_back_to_http_when_allowed() {
        let address = spawn_mock_join_server("HTTP/1.1 200 OK", HIVE_JOIN_JSON).await;
        let url = format!("https://{address}/v1/join");

        // The fallback requires the explicit insecure-HTTP policy.
        let denied = query_url(&url, QueryOptions::default())
            .await
            .expect_err("the insecure fallback is disabled by default");
        assert!(matches!(
            denied,
            Error::Http(_) | Error::Timeout | Error::InvalidInput(_)
        ));

        let options = QueryOptions::default().allow_insecure_http(true);
        let status = query_url(&url, options)
            .await
            .expect("http fallback should parse when the policy allows it");

        assert_eq!(status.motd, "HIVE-O-WEEN | BEDWARS S5");
        assert_eq!(status.transport(), Transport::NetherNetHttp);
    }

    #[tokio::test]
    async fn non_2xx_status_is_reported_without_fallback() {
        let address = spawn_mock_join_server("HTTP/1.1 404 Not Found", "nope").await;
        let url = format!("http://{address}/v1/join");

        let error = query_url(&url, QueryOptions::default())
            .await
            .expect_err("404 should fail");

        assert!(matches!(error, Error::HttpStatus(404)));
    }

    #[tokio::test]
    async fn insecure_http_can_be_disabled() {
        let address = spawn_mock_join_server("HTTP/1.1 200 OK", HIVE_JOIN_JSON).await;
        let url = format!("https://{address}/v1/join");
        let options = QueryOptions::default().allow_insecure_http(false);

        let error = query_url(&url, options)
            .await
            .expect_err("fallback disabled should fail");

        assert!(matches!(error, Error::Http(_) | Error::Timeout));
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let address = spawn_mock_join_server("HTTP/1.1 200 OK", ENCHANTED_JOIN_JSON).await;
        let url = format!("http://{address}/v1/join");
        let options = QueryOptions::default().max_response_bytes(16);

        let error = query_url(&url, options)
            .await
            .expect_err("oversized body should fail");

        assert!(matches!(error, Error::InvalidPacket(_)));
    }

    #[tokio::test]
    async fn closed_port_maps_to_transport_error() {
        // Port 1 on the loopback interface is virtually always closed; the
        // failure surfaces as a connect error or a timeout depending on the
        // platform's firewall behavior.
        let error = query_url("http://127.0.0.1:1/v1/join", QueryOptions::default())
            .await
            .expect_err("closed port should fail");

        assert!(matches!(error, Error::Http(_) | Error::Timeout));
    }

    #[tokio::test]
    #[ignore = "queries public Minecraft servers; run with --ignored"]
    async fn live_nethernet_servers_respond() {
        for host in [
            "play.enchanted.gg",
            "nethernet.playhive.com",
            "mco.lbsg.net",
        ] {
            let status = query(format!("{host}:19132"))
                .await
                .unwrap_or_else(|error| panic!("{host} should answer NetherNet: {error}"));

            assert!(!status.motd.is_empty(), "{host} should report a name");
            assert!(
                status.version.protocol.is_some(),
                "{host} should report a protocol"
            );
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
