use std::borrow::Cow;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::{UdpSocket, lookup_host};
use tokio::time;

use crate::MotdError;

const RAKNET_MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];
const UNCONNECTED_PING: u8 = 0x01;
const UNCONNECTED_PONG: u8 = 0x1c;
const PONG_HEADER_LEN: usize = 1 + 8 + 8 + RAKNET_MAGIC.len() + 2;

/// Owned Bedrock Edition MOTD response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BedrockMotd {
    /// Edition identifier, usually `MCPE`.
    pub edition: String,
    /// First MOTD line.
    pub motd_line_1: String,
    /// Bedrock protocol version.
    pub protocol: Option<u32>,
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

/// Borrowed Bedrock Edition MOTD response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BedrockMotdRef<'a> {
    /// Edition identifier, usually `MCPE`.
    pub edition: Cow<'a, str>,
    /// First MOTD line.
    pub motd_line_1: Cow<'a, str>,
    /// Bedrock protocol version.
    pub protocol: Option<u32>,
    /// Human-readable server version.
    pub version: Cow<'a, str>,
    /// Current online player count.
    pub players_online: Option<u32>,
    /// Maximum player count.
    pub players_max: Option<u32>,
    /// Server id reported by the pong payload.
    pub server_id: Cow<'a, str>,
    /// Second MOTD line.
    pub motd_line_2: Cow<'a, str>,
    /// Game mode text.
    pub game_mode: Cow<'a, str>,
    /// Numeric game mode id.
    pub game_mode_numeric: Option<u32>,
    /// IPv4 port reported by the server.
    pub port_ipv4: Option<u16>,
    /// IPv6 port reported by the server.
    pub port_ipv6: Option<u16>,
}

impl BedrockMotdRef<'_> {
    #[must_use]
    pub fn to_owned(&self) -> BedrockMotd {
        BedrockMotd {
            edition: self.edition.to_string(),
            motd_line_1: self.motd_line_1.to_string(),
            protocol: self.protocol,
            version: self.version.to_string(),
            players_online: self.players_online,
            players_max: self.players_max,
            server_id: self.server_id.to_string(),
            motd_line_2: self.motd_line_2.to_string(),
            game_mode: self.game_mode.to_string(),
            game_mode_numeric: self.game_mode_numeric,
            port_ipv4: self.port_ipv4,
            port_ipv6: self.port_ipv6,
        }
    }
}

/// Query a Bedrock Edition server by host and port.
///
/// # Errors
///
/// Returns [`MotdError`] if DNS lookup fails or times out, no socket address is
/// resolved, UDP I/O fails or times out, or the Bedrock pong packet is invalid.
pub async fn query_bedrock(
    host: impl AsRef<str>,
    port: u16,
    timeout: Duration,
) -> Result<BedrockMotd, MotdError> {
    let mut resolved = time::timeout(timeout, lookup_host((host.as_ref(), port)))
        .await
        .map_err(|_| MotdError::Timeout)??;
    let address = resolved.next().ok_or(MotdError::ResolveFailed)?;
    query_bedrock_addr(address, timeout).await
}

/// Query a Bedrock Edition server at an already resolved socket address.
///
/// # Errors
///
/// Returns [`MotdError`] if the UDP socket cannot be created or connected, the
/// ping cannot be sent, receiving times out or fails, or the pong packet is
/// invalid.
pub async fn query_bedrock_addr(
    address: SocketAddr,
    timeout: Duration,
) -> Result<BedrockMotd, MotdError> {
    let bind_address = if address.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = UdpSocket::bind(bind_address).await?;
    socket.connect(address).await?;

    let mut packet = [0_u8; 33];
    write_ping_packet(&mut packet);
    socket.send(&packet).await?;

    let mut buffer = [0_u8; 2048];
    let len = time::timeout(timeout, socket.recv(&mut buffer))
        .await
        .map_err(|_| MotdError::Timeout)??;

    parse_bedrock_pong(&buffer[..len]).map(|motd| motd.to_owned())
}

/// Build a Bedrock `RakNet` unconnected ping packet.
#[must_use]
pub fn build_unconnected_ping_packet() -> [u8; 33] {
    let mut packet = [0_u8; 33];
    write_ping_packet(&mut packet);
    packet
}

/// Parse a Bedrock `RakNet` unconnected pong packet.
///
/// # Errors
///
/// Returns [`MotdError`] if the packet is too short, has the wrong packet id,
/// is missing the `RakNet` magic bytes, advertises a MOTD length beyond the
/// packet, contains invalid UTF-8, or contains too few MOTD fields.
pub fn parse_bedrock_pong(packet: &[u8]) -> Result<BedrockMotdRef<'_>, MotdError> {
    if packet.len() < PONG_HEADER_LEN {
        return Err(MotdError::InvalidPacket("Bedrock packet is too short"));
    }
    if packet[0] != UNCONNECTED_PONG {
        return Err(MotdError::InvalidPacket(
            "Bedrock packet id is not unconnected pong",
        ));
    }
    if packet[17..33] != RAKNET_MAGIC {
        return Err(MotdError::InvalidPacket("Bedrock RakNet magic is missing"));
    }

    let motd_len = u16::from_be_bytes([packet[33], packet[34]]) as usize;
    let motd_start = PONG_HEADER_LEN;
    let motd_end = motd_start.saturating_add(motd_len);
    if motd_end > packet.len() {
        return Err(MotdError::InvalidPacket(
            "Bedrock MOTD length exceeds packet size",
        ));
    }

    parse_bedrock_motd_text(std::str::from_utf8(&packet[motd_start..motd_end])?)
}

/// Parse the semicolon-separated Bedrock MOTD text payload.
///
/// The parser tolerates short payloads and semicolons in the first MOTD line.
///
/// # Errors
///
/// Returns [`MotdError::InvalidPacket`] when the payload does not include the
/// required edition and first MOTD line.
pub fn parse_bedrock_motd_text(text: &str) -> Result<BedrockMotdRef<'_>, MotdError> {
    let trimmed = text.trim_end_matches('\0');
    let mut fields: Vec<&str> = trimmed.split(';').collect();
    while fields.last().is_some_and(|field| field.is_empty()) {
        fields.pop();
    }
    if fields.len() < 2 {
        return Err(MotdError::InvalidPacket("Bedrock MOTD line is missing"));
    }

    if fields.len() < 12 {
        return Ok(BedrockMotdRef {
            edition: Cow::Borrowed(fields[0]),
            motd_line_1: Cow::Borrowed(fields[1]),
            protocol: parse_optional_u32(fields.get(2).copied()),
            version: Cow::Borrowed(fields.get(3).copied().unwrap_or_default()),
            players_online: parse_optional_u32(fields.get(4).copied()),
            players_max: parse_optional_u32(fields.get(5).copied()),
            server_id: Cow::Borrowed(fields.get(6).copied().unwrap_or_default()),
            motd_line_2: Cow::Borrowed(fields.get(7).copied().unwrap_or_default()),
            game_mode: Cow::Borrowed(fields.get(8).copied().unwrap_or_default()),
            game_mode_numeric: parse_optional_u32(fields.get(9).copied()),
            port_ipv4: parse_optional_u16(fields.get(10).copied()),
            port_ipv6: parse_optional_u16(fields.get(11).copied()),
        });
    }

    let tail_start = fields.len() - 10;
    if tail_start <= 1 {
        return Err(MotdError::InvalidPacket("Bedrock MOTD line is missing"));
    }

    let motd_line_1 = if tail_start == 2 {
        Cow::Borrowed(fields[1])
    } else {
        Cow::Owned(fields[1..tail_start].join(";"))
    };

    Ok(BedrockMotdRef {
        edition: Cow::Borrowed(fields[0]),
        motd_line_1,
        protocol: fields[tail_start].parse().ok(),
        version: Cow::Borrowed(fields[tail_start + 1]),
        players_online: fields[tail_start + 2].parse().ok(),
        players_max: fields[tail_start + 3].parse().ok(),
        server_id: Cow::Borrowed(fields[tail_start + 4]),
        motd_line_2: Cow::Borrowed(fields[tail_start + 5]),
        game_mode: Cow::Borrowed(fields[tail_start + 6]),
        game_mode_numeric: fields[tail_start + 7].parse().ok(),
        port_ipv4: fields[tail_start + 8].parse().ok(),
        port_ipv6: fields[tail_start + 9].parse().ok(),
    })
}

fn parse_optional_u32(value: Option<&str>) -> Option<u32> {
    value?.parse().ok()
}

fn parse_optional_u16(value: Option<&str>) -> Option<u16> {
    value?.parse().ok()
}

fn write_ping_packet(packet: &mut [u8; 33]) {
    packet[0] = UNCONNECTED_PING;
    packet[1..9].copy_from_slice(&current_millis().to_be_bytes());
    packet[9..25].copy_from_slice(&RAKNET_MAGIC);
    packet[25..33].copy_from_slice(&0_i64.to_be_bytes());
}

fn current_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::net::UdpSocket;

    #[test]
    fn ping_packet_contains_raknet_magic() {
        let packet = build_unconnected_ping_packet();

        assert_eq!(packet[0], UNCONNECTED_PING);
        assert_eq!(packet[9..25], RAKNET_MAGIC);
    }

    #[test]
    fn parses_bedrock_motd_text() {
        let motd = parse_bedrock_motd_text(
            "MCPE;§aServer;589;1.21.100;3;20;12345;§bLine2;Survival;1;19132;19133;",
        )
        .expect("motd should parse");

        assert_eq!(motd.edition, "MCPE");
        assert_eq!(motd.motd_line_1, "§aServer");
        assert_eq!(motd.protocol, Some(589));
        assert_eq!(motd.players_online, Some(3));
        assert_eq!(motd.port_ipv4, Some(19132));
    }

    #[test]
    fn tolerates_semicolon_in_first_motd_line() {
        let motd = parse_bedrock_motd_text(
            "MCPE;Hello;World;589;1.21.100;0;10;12345;Line2;Survival;1;19132;19133;",
        )
        .expect("motd should parse");

        assert_eq!(motd.motd_line_1, "Hello;World");
        assert_eq!(motd.version, "1.21.100");
    }

    #[test]
    fn tolerates_short_bedrock_motd_text() {
        let motd = parse_bedrock_motd_text("MCPE;Mini Server;589;1.21.100;3;20;")
            .expect("motd should parse");

        assert_eq!(motd.edition, "MCPE");
        assert_eq!(motd.motd_line_1, "Mini Server");
        assert_eq!(motd.protocol, Some(589));
        assert_eq!(motd.version, "1.21.100");
        assert_eq!(motd.players_online, Some(3));
        assert_eq!(motd.players_max, Some(20));
        assert_eq!(motd.port_ipv4, None);
    }

    #[test]
    fn tolerates_minimal_bedrock_motd_text() {
        let motd = parse_bedrock_motd_text("MCPE;Minimal Server").expect("motd should parse");

        assert_eq!(motd.edition, "MCPE");
        assert_eq!(motd.motd_line_1, "Minimal Server");
        assert_eq!(motd.protocol, None);
        assert_eq!(motd.version, "");
        assert_eq!(motd.players_online, None);
        assert_eq!(motd.players_max, None);
    }

    #[test]
    fn parses_unconnected_pong_packet() {
        let text = "MCPE;Server;589;1.21.100;1;8;12345;Line2;Survival;1;19132;19133;";
        let mut packet = Vec::with_capacity(PONG_HEADER_LEN + text.len());
        packet.push(UNCONNECTED_PONG);
        packet.extend_from_slice(&1_i64.to_be_bytes());
        packet.extend_from_slice(&2_i64.to_be_bytes());
        packet.extend_from_slice(&RAKNET_MAGIC);
        packet.extend_from_slice(
            &u16::try_from(text.len())
                .expect("fixture MOTD length fits in u16")
                .to_be_bytes(),
        );
        packet.extend_from_slice(text.as_bytes());

        let motd = parse_bedrock_pong(&packet).expect("pong should parse");

        assert_eq!(motd.motd_line_1, "Server");
        assert_eq!(motd.players_max, Some(8));
    }

    #[tokio::test]
    async fn query_bedrock_addr_reads_mock_pong() {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock socket should bind");
        let address = socket.local_addr().expect("mock address should exist");
        let server = tokio::spawn(async move {
            respond_with_pong(socket).await;
        });

        let motd = query_bedrock_addr(address, Duration::from_millis(500))
            .await
            .expect("mock pong should parse");

        assert_eq!(motd.edition, "MCPE");
        assert_eq!(motd.motd_line_1, "Server");
        server.await.expect("server task should finish");
    }

    #[tokio::test]
    async fn query_bedrock_addr_uses_ipv6_socket_for_ipv6_targets() {
        let Ok(socket) = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).await else {
            return;
        };
        let address = socket.local_addr().expect("mock address should exist");
        let server = tokio::spawn(async move {
            respond_with_pong(socket).await;
        });

        let motd = query_bedrock_addr(address, Duration::from_millis(500))
            .await
            .expect("mock IPv6 pong should parse");

        assert_eq!(motd.port_ipv6, Some(19_133));
        server.await.expect("server task should finish");
    }

    async fn respond_with_pong(socket: UdpSocket) {
        let mut buffer = [0_u8; 64];
        let (len, peer) = socket
            .recv_from(&mut buffer)
            .await
            .expect("ping should be readable");
        assert_eq!(len, 33);
        assert_eq!(buffer[0], UNCONNECTED_PING);

        let text = "MCPE;Server;589;1.21.100;1;8;12345;Line2;Survival;1;19132;19133;";
        let mut packet = Vec::with_capacity(PONG_HEADER_LEN + text.len());
        packet.push(UNCONNECTED_PONG);
        packet.extend_from_slice(&1_i64.to_be_bytes());
        packet.extend_from_slice(&2_i64.to_be_bytes());
        packet.extend_from_slice(&RAKNET_MAGIC);
        packet.extend_from_slice(
            &u16::try_from(text.len())
                .expect("fixture MOTD length fits in u16")
                .to_be_bytes(),
        );
        packet.extend_from_slice(text.as_bytes());

        socket
            .send_to(&packet, peer)
            .await
            .expect("pong should be writable");
    }
}
