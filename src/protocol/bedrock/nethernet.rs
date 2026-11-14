//! `NetherNet` join endpoint codec.
//!
//! Pure parsing of the `GET /v1/join` JSON document: no HTTP client, no
//! caching. The HTTPS/HTTP query flow lives in [`crate::bedrock::nethernet`].
//! See the
//! [NetherNet onboarding guide](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/).

use std::borrow::Cow;

use serde::Deserialize;

use crate::error::Error;
pub use crate::status::NetherNetDetails;

/// Borrowed fields of a `NetherNet` join response.
///
/// String fields borrow from the input text via zero-copy deserialization, so
/// parsing allocates nothing per field.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NetherNetDetailsRef<'a> {
    /// Server name shown as the first MOTD line (`name`).
    pub name: Cow<'a, str>,
    /// Bedrock protocol version (`protocol`).
    pub protocol: Option<i32>,
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
    /// Whether the response was fetched over plain HTTP.
    ///
    /// Parsing alone cannot know the scheme and defaults to `false`
    /// (HTTPS); the network layer overrides it when plain HTTP answered.
    pub insecure_http: bool,
}

impl NetherNetDetailsRef<'_> {
    /// Convert the borrowed response into an owned one.
    #[must_use]
    pub fn to_owned(&self) -> NetherNetDetails {
        NetherNetDetails {
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
            insecure_http: self.insecure_http,
        }
    }

    /// Human-readable name for the numeric game mode id.
    #[must_use]
    pub fn game_type_name(&self) -> Option<&'static str> {
        game_type_name(self.game_type)
    }
}

/// Human-readable name for the numeric `NetherNet` game mode id.
#[must_use]
pub fn game_type_name(game_type: Option<u32>) -> Option<&'static str> {
    match game_type? {
        0 => Some("Survival"),
        1 => Some("Creative"),
        2 => Some("Adventure"),
        _ => None,
    }
}

/// Parse the JSON body of a `NetherNet` `GET /v1/join` response into an owned
/// value.
///
/// # Errors
///
/// Returns [`Error::InvalidPacket`] when the body is not valid JSON, the
/// required `name` field is missing or empty, a present numeric field is not a
/// valid non-negative integer, or `players` exceeds `maxPlayers`.
pub fn parse_join(text: &str) -> Result<NetherNetDetails, Error> {
    Ok(parse_join_ref(text)?.to_owned())
}

/// Parse the JSON body of a `NetherNet` `GET /v1/join` response without
/// allocating.
///
/// String fields borrow from `text` via [`NetherNetDetailsRef`], so repeated
/// parsing of polling responses allocates nothing per call.
///
/// # Errors
///
/// Returns [`Error::InvalidPacket`] when the body is not valid JSON, the
/// required `name` field is missing or empty, a present numeric field is not a
/// valid non-negative integer, or `players` exceeds `maxPlayers`.
pub fn parse_join_ref(text: &str) -> Result<NetherNetDetailsRef<'_>, Error> {
    let response: JoinResponseRef<'_> = serde_json::from_str(text)
        .map_err(|_| Error::InvalidPacket("NetherNet join response is not valid JSON"))?;
    validate_join_response(response)
}

/// Parse a join response body that was received as raw bytes.
pub(crate) fn parse_join_slice(bytes: &[u8]) -> Result<NetherNetDetails, Error> {
    let response: JoinResponseRef<'_> = serde_json::from_slice(bytes)
        .map_err(|_| Error::InvalidPacket("NetherNet join response is not valid JSON"))?;
    Ok(validate_join_response(response)?.to_owned())
}

/// Validate a parsed `NetherNet` join response.
fn validate_join_response(response: JoinResponseRef<'_>) -> Result<NetherNetDetailsRef<'_>, Error> {
    if response.name.is_empty() {
        return Err(Error::InvalidPacket(
            "NetherNet join response is missing the name field",
        ));
    }
    if let (Some(players), Some(max_players)) = (response.players, response.max_players) {
        if players > max_players {
            return Err(Error::InvalidPacket(
                "NetherNet player count exceeds the reported maximum",
            ));
        }
    }

    Ok(NetherNetDetailsRef {
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
        insecure_http: false,
    })
}

/// Raw `NetherNet` join response as reported by the server. String fields
/// borrow from the input so parsing stays allocation-free.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct JoinResponseRef<'a> {
    #[serde(borrow)]
    name: Cow<'a, str>,
    protocol: Option<i32>,
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

#[cfg(test)]
mod tests {
    use super::*;

    const ENCHANTED_JOIN_JSON: &str = r#"{"dataVersion":7,"name":"FROSTBITE EVENT ★","protocol":2225,"version":"1.26.60","level":"Enchanted","players":1261,"maxPlayers":2261,"gameType":0,"editor":false,"hardcore":false,"onlineAuth":true,"selfSignedAuth":false,"nonce":"58357ae4f92347f2","transportLayer":2,"connection":4}"#;
    const HIVE_JOIN_JSON: &str = r#"{"name":"HIVE-O-WEEN | BEDWARS S5","protocol":944,"version":"1.26.14.1","level":"Hive Games","players":10012,"maxPlayers":100000,"gameType":0}"#;

    #[test]
    fn parses_full_join_response() {
        let motd = parse_join(ENCHANTED_JOIN_JSON).expect("join should parse");

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
    }

    #[test]
    fn parses_minimal_join_response() {
        let motd = parse_join(HIVE_JOIN_JSON).expect("join should parse");

        assert_eq!(motd.name, "HIVE-O-WEEN | BEDWARS S5");
        assert_eq!(motd.protocol, Some(944));
        assert_eq!(motd.players_online, Some(10_012));
        assert_eq!(motd.players_max, Some(100_000));
        assert_eq!(motd.data_version, None);
        assert_eq!(motd.nonce, None);
        assert_eq!(motd.transport_layer, None);
        assert_eq!(game_type_name(motd.game_type), Some("Survival"));
    }

    #[test]
    fn parses_join_response_borrowed_without_allocation() {
        let motd = parse_join_ref(ENCHANTED_JOIN_JSON).expect("join should parse");

        assert!(
            matches!(motd.name, Cow::Borrowed(_)),
            "name should borrow from the input"
        );
        assert_eq!(motd.name, "FROSTBITE EVENT ★");
        assert_eq!(motd.protocol, Some(2225));
        assert_eq!(
            motd.nonce.as_ref().map(Cow::as_ref),
            Some("58357ae4f92347f2")
        );
        assert_eq!(game_type_name(motd.game_type), Some("Survival"));

        let owned = motd.to_owned();
        assert_eq!(owned.name, "FROSTBITE EVENT ★");
    }

    #[test]
    fn rejects_missing_name() {
        let error = parse_join(r#"{"protocol":2225}"#).expect_err("missing name should fail");

        assert!(matches!(error, Error::InvalidPacket(_)));
    }

    #[test]
    fn rejects_players_above_max() {
        let error = parse_join(r#"{"name":"Server","players":20,"maxPlayers":10}"#)
            .expect_err("players above max should fail");

        assert!(matches!(error, Error::InvalidPacket(_)));
    }

    #[test]
    fn rejects_invalid_numeric_field() {
        let error = parse_join(r#"{"name":"Server","players":12.5}"#)
            .expect_err("float player count should fail");

        assert!(matches!(error, Error::InvalidPacket(_)));
    }

    #[test]
    fn rejects_invalid_json() {
        let error = parse_join("not json").expect_err("bad json should fail");

        assert!(matches!(error, Error::InvalidPacket(_)));
    }

    #[test]
    fn game_type_names_cover_documented_modes() {
        assert_eq!(game_type_name(Some(0)), Some("Survival"));
        assert_eq!(game_type_name(Some(1)), Some("Creative"));
        assert_eq!(game_type_name(Some(2)), Some("Adventure"));
        assert_eq!(game_type_name(Some(7)), None);
        assert_eq!(game_type_name(None), None);
    }

    #[test]
    fn malformed_inputs_never_panic() {
        let base = r#"{"name":"Server","protocol":589,"players":1,"maxPlayers":10}"#;
        for cut in 0..base.len() {
            let _ = parse_join(&base[..cut]);
        }
        for text in [
            "",
            "{",
            "[]",
            "null",
            "true",
            "\"just a string\"",
            "{\"name\":123}",
            "{\"name\":null}",
            "{\"name\":\"Server\",\"players\":-1}",
            "{\"name\":\"Server\",\"maxPlayers\":1e300}",
            "{\"name\":\"Server\",\"protocol\":\"589\"}",
        ] {
            let _ = parse_join(text);
        }
    }
}
