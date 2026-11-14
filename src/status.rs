//! Unified server status model shared by every edition and transport.
//!
//! [`Status`] is the single result type callers interact with. Common fields
//! (`motd`, players, version) are normalized across protocols, while
//! [`Status::details`] keeps the protocol-specific raw information for
//! advanced users. Edition and transport are derived from `details`, so there
//! is never a second stored copy to drift out of sync.

use std::time::Duration;

use crate::address::ServerAddress;

/// Minecraft server edition reported by a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Edition {
    /// Java Edition.
    #[cfg(feature = "java")]
    Java,
    /// Bedrock Edition.
    #[cfg(feature = "bedrock")]
    Bedrock,
}

/// Transport that produced a query response.
///
/// The transport identifies how the status was fetched. For Bedrock servers
/// this distinguishes the legacy `RakNet` ping from the `NetherNet` join
/// endpoint; the two latencies have different meanings and must not be equated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Transport {
    /// Java Edition status query over TCP.
    #[cfg(feature = "java")]
    JavaTcp,
    /// Bedrock Edition `RakNet` unconnected ping over UDP.
    #[cfg(feature = "bedrock")]
    RakNetUdp,
    /// Bedrock Edition `NetherNet` join endpoint over HTTPS/HTTP.
    #[cfg(feature = "nethernet")]
    NetherNetHttp,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "java")]
            Self::JavaTcp => formatter.write_str("Java TCP"),
            #[cfg(feature = "bedrock")]
            Self::RakNetUdp => formatter.write_str("RakNet UDP"),
            #[cfg(feature = "nethernet")]
            Self::NetherNetHttp => formatter.write_str("NetherNet HTTP"),
        }
    }
}

/// Online and maximum player counts reported by a server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Players {
    /// Current online player count.
    pub online: Option<u32>,
    /// Maximum player count.
    pub max: Option<u32>,
}

/// Game version reported by a server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Version {
    /// Human-readable version string.
    pub name: Option<String>,
    /// Protocol version number.
    pub protocol: Option<i32>,
}

/// Edition-specific raw details kept alongside the normalized [`Status`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Details {
    /// Java Edition raw status JSON.
    #[cfg(feature = "java")]
    Java(JavaDetails),
    /// Bedrock Edition details, split by transport.
    #[cfg(feature = "bedrock")]
    Bedrock(BedrockDetails),
}

impl Details {
    /// Edition that produced these details.
    #[must_use]
    pub fn edition(&self) -> Option<Edition> {
        match self {
            #[cfg(feature = "java")]
            Self::Java(_) => Some(Edition::Java),
            #[cfg(feature = "bedrock")]
            Self::Bedrock(_) => Some(Edition::Bedrock),
        }
    }

    /// Transport that produced these details.
    #[must_use]
    pub fn transport(&self) -> Option<Transport> {
        match self {
            #[cfg(feature = "java")]
            Self::Java(_) => Some(Transport::JavaTcp),
            #[cfg(feature = "bedrock")]
            Self::Bedrock(details) => details.transport(),
        }
    }
}

/// Java Edition raw status payload.
#[cfg(feature = "java")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JavaDetails {
    /// Raw JSON returned by the server status endpoint.
    pub raw_json: String,
    /// SRV record the query was resolved through, if any.
    pub srv: Option<crate::java::SrvTarget>,
}

/// Bedrock Edition raw details, split by transport.
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BedrockDetails {
    /// Legacy `RakNet` unconnected pong.
    RakNet(RakNetDetails),
    /// `NetherNet` join response.
    #[cfg(feature = "nethernet")]
    NetherNet(NetherNetDetails),
}

#[cfg(feature = "bedrock")]
impl BedrockDetails {
    /// Transport that produced these details.
    #[must_use]
    pub fn transport(&self) -> Option<Transport> {
        match self {
            Self::RakNet(_) => Some(Transport::RakNetUdp),
            #[cfg(feature = "nethernet")]
            Self::NetherNet(_) => Some(Transport::NetherNetHttp),
        }
    }
}

/// Raw fields of a `RakNet` unconnected pong.
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RakNetDetails {
    /// Edition identifier, usually `MCPE`.
    pub edition: String,
    /// First MOTD line.
    pub motd_line_1: String,
    /// Bedrock protocol version.
    pub protocol: Option<i32>,
    /// Human-readable server version.
    pub version: String,
    /// Current online player count.
    pub players_online: Option<u32>,
    /// Maximum player count.
    pub players_max: Option<u32>,
    /// Server id reported by the pong payload.
    pub server_id: String,
    /// Second MOTD line.
    pub motd_line_2: String,
    /// Game mode text.
    pub game_mode: String,
    /// Numeric game mode id.
    pub game_mode_numeric: Option<u32>,
    /// IPv4 port reported by the server.
    pub port_ipv4: Option<u16>,
    /// IPv6 port reported by the server.
    pub port_ipv6: Option<u16>,
}

/// Raw fields of a `NetherNet` join response.
#[cfg(feature = "nethernet")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NetherNetDetails {
    /// Server name shown as the first MOTD line (`name`).
    pub name: String,
    /// Bedrock protocol version (`protocol`).
    pub protocol: Option<i32>,
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
}

/// Unified result of a successful query.
///
/// Common fields are normalized across Java, `RakNet`, and `NetherNet`
/// responses; [`Status::details`] exposes the protocol-specific payload for
/// advanced callers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Status {
    /// Address the query was sent to.
    pub address: ServerAddress,
    /// Normalized MOTD line: Java description text, `RakNet` first MOTD line,
    /// or the `NetherNet` server name.
    pub motd: String,
    /// Player counts.
    pub players: Players,
    /// Game version.
    pub version: Version,
    /// End-to-end wall time of the query, including DNS and every probe.
    pub elapsed: Duration,
    /// Protocol-specific raw details.
    pub details: Details,
}

impl Status {
    /// Edition that answered the query.
    #[must_use]
    pub fn edition(&self) -> Option<Edition> {
        self.details.edition()
    }

    /// Transport that answered the query.
    #[must_use]
    pub fn transport(&self) -> Option<Transport> {
        self.details.transport()
    }

    /// Build a [`Status`] from raw protocol details, extracting the normalized
    /// `motd`, players, and version fields.
    pub(crate) fn from_details(address: ServerAddress, details: Details) -> Self {
        let (motd, players, version) = match &details {
            #[cfg(feature = "java")]
            Details::Java(details) => crate::java::extract_java_fields(&details.raw_json),
            #[cfg(feature = "bedrock")]
            Details::Bedrock(BedrockDetails::RakNet(details)) => (
                details.motd_line_1.clone(),
                Players {
                    online: details.players_online,
                    max: details.players_max,
                },
                Version {
                    name: Some(details.version.clone()),
                    protocol: details.protocol,
                },
            ),
            #[cfg(feature = "nethernet")]
            Details::Bedrock(BedrockDetails::NetherNet(details)) => (
                details.name.clone(),
                Players {
                    online: details.players_online,
                    max: details.players_max,
                },
                Version {
                    name: Some(details.version.clone()),
                    protocol: details.protocol,
                },
            ),
        };
        Self {
            address,
            motd,
            players,
            version,
            elapsed: Duration::ZERO,
            details,
        }
    }
}
