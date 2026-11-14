//! Legacy `RakNet` offline-ping codec.
//!
//! Pure encoding and decoding of the Bedrock unconnected ping/pong exchange:
//! no sockets, no timeouts. The UDP query flow lives in
//! [`crate::bedrock::raknet`].

use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Error;
pub use crate::status::RakNetDetails;

const RAKNET_MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];
const UNCONNECTED_PING: u8 = 0x01;
const UNCONNECTED_PONG: u8 = 0x1c;
pub const PONG_HEADER_LEN: usize = 1 + 8 + 8 + RAKNET_MAGIC.len() + 2;
const PING_PACKET_LEN: usize = 33;

/// Borrowed fields of a `RakNet` unconnected pong.
///
/// String fields borrow from the packet bytes; decoding allocates only when
/// the first MOTD line contains semicolons and must be re-joined.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RakNetDetailsRef<'a> {
    /// Edition identifier, usually `MCPE`.
    pub edition: Cow<'a, str>,
    /// First MOTD line.
    pub motd_line_1: Cow<'a, str>,
    /// Bedrock protocol version.
    pub protocol: Option<i32>,
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

impl RakNetDetailsRef<'_> {
    /// Convert the borrowed pong fields into an owned value.
    #[must_use]
    pub fn to_owned(&self) -> RakNetDetails {
        RakNetDetails {
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

/// Build a Bedrock `RakNet` unconnected ping packet.
#[must_use]
pub fn encode_ping() -> [u8; PING_PACKET_LEN] {
    let mut packet = [0_u8; PING_PACKET_LEN];
    packet[0] = UNCONNECTED_PING;
    packet[1..9].copy_from_slice(&current_millis().to_be_bytes());
    packet[9..25].copy_from_slice(&RAKNET_MAGIC);
    packet[25..33].copy_from_slice(&0_i64.to_be_bytes());
    packet
}

/// Decode a Bedrock `RakNet` unconnected pong packet.
///
/// # Errors
///
/// Returns [`Error`] if the packet is too short, has the wrong packet id, is
/// missing the `RakNet` magic bytes, advertises a MOTD length beyond the
/// packet, contains invalid UTF-8, or contains too few MOTD fields.
pub fn decode_pong(packet: &[u8]) -> Result<RakNetDetailsRef<'_>, Error> {
    if packet.len() < PONG_HEADER_LEN {
        return Err(Error::InvalidPacket("Bedrock packet is too short"));
    }
    if packet[0] != UNCONNECTED_PONG {
        return Err(Error::InvalidPacket(
            "Bedrock packet id is not unconnected pong",
        ));
    }
    if packet[17..33] != RAKNET_MAGIC {
        return Err(Error::InvalidPacket("Bedrock RakNet magic is missing"));
    }

    let motd_len = u16::from_be_bytes([packet[33], packet[34]]) as usize;
    let motd_start = PONG_HEADER_LEN;
    let motd_end = motd_start.saturating_add(motd_len);
    if motd_end > packet.len() {
        return Err(Error::InvalidPacket(
            "Bedrock MOTD length exceeds packet size",
        ));
    }

    decode_motd_text(std::str::from_utf8(&packet[motd_start..motd_end])?)
}

/// Decode the semicolon-separated Bedrock MOTD text payload.
///
/// Decoding performs no heap allocation: every field slice borrows from
/// `text`, including a first MOTD line that contains semicolons (its parts
/// are one contiguous byte span and are borrowed as a whole).
///
/// The parser tolerates short payloads and semicolons in the first MOTD line.
///
/// # Errors
///
/// Returns [`Error::InvalidPacket`] when the payload does not include the
/// required edition and first MOTD line.
pub fn decode_motd_text(text: &str) -> Result<RakNetDetailsRef<'_>, Error> {
    let trimmed = text.trim_end_matches('\0');
    // Drop trailing empty fields without allocating: the original parser
    // collected all fields and popped trailing empties afterwards.
    let mut body = trimmed;
    while let Some((head, last)) = body.rsplit_once(';') {
        if !last.is_empty() {
            break;
        }
        body = head;
    }

    let field_count = body.matches(';').count() + 1;
    if field_count < 2 {
        return Err(Error::InvalidPacket("Bedrock MOTD line is missing"));
    }

    if field_count < 12 {
        // Short form: fields map by position and missing tail fields stay
        // unset. Sequential iteration keeps this allocation-free.
        let mut fields = body.split(';');
        let edition = fields.next().unwrap_or_default();
        let motd_line_1 = fields.next().unwrap_or_default();
        let protocol = fields.next().and_then(|value| value.parse::<i32>().ok());
        let version = fields.next().unwrap_or_default();
        let players_online = fields.next().and_then(|value| value.parse().ok());
        let players_max = fields.next().and_then(|value| value.parse().ok());
        let server_id = fields.next().unwrap_or_default();
        let motd_line_2 = fields.next().unwrap_or_default();
        let game_mode = fields.next().unwrap_or_default();
        let game_mode_numeric = fields.next().and_then(|value| value.parse().ok());
        let port_ipv4 = fields.next().and_then(|value| value.parse().ok());
        let port_ipv6 = fields.next().and_then(|value| value.parse().ok());

        return Ok(RakNetDetailsRef {
            edition: Cow::Borrowed(edition),
            motd_line_1: Cow::Borrowed(motd_line_1),
            protocol,
            version: Cow::Borrowed(version),
            players_online,
            players_max,
            server_id: Cow::Borrowed(server_id),
            motd_line_2: Cow::Borrowed(motd_line_2),
            game_mode: Cow::Borrowed(game_mode),
            game_mode_numeric,
            port_ipv4,
            port_ipv6,
        });
    }

    // Long form: fields 1..tail_start form the first MOTD line, which may
    // contain semicolons; the last 10 fields are the fixed tail. Those middle
    // fields are one contiguous byte span of `body`, so the first MOTD line
    // borrows directly instead of being split and re-joined.
    let tail_start = field_count - 10;
    let Some(first_semi) = body.find(';') else {
        return Err(Error::InvalidPacket("Bedrock MOTD line is missing"));
    };
    let line_1_end = body
        .match_indices(';')
        .nth(tail_start - 1)
        .map_or(body.len(), |(offset, _)| offset);
    let tail_text = &body[line_1_end + 1..];

    let edition = Cow::Borrowed(&body[..first_semi]);
    let motd_line_1 = Cow::Borrowed(&body[first_semi + 1..line_1_end]);
    let mut tail_fields = tail_text.split(';');
    let protocol = tail_fields
        .next()
        .and_then(|value| value.parse::<i32>().ok());
    let version = tail_fields.next().unwrap_or_default();
    let players_online = tail_fields.next().and_then(|value| value.parse().ok());
    let players_max = tail_fields.next().and_then(|value| value.parse().ok());
    let server_id = tail_fields.next().unwrap_or_default();
    let motd_line_2 = tail_fields.next().unwrap_or_default();
    let game_mode = tail_fields.next().unwrap_or_default();
    let game_mode_numeric = tail_fields.next().and_then(|value| value.parse().ok());
    let port_ipv4 = tail_fields.next().and_then(|value| value.parse().ok());
    let port_ipv6 = tail_fields.next().and_then(|value| value.parse().ok());

    Ok(RakNetDetailsRef {
        edition,
        motd_line_1,
        protocol,
        version: Cow::Borrowed(version),
        players_online,
        players_max,
        server_id: Cow::Borrowed(server_id),
        motd_line_2: Cow::Borrowed(motd_line_2),
        game_mode: Cow::Borrowed(game_mode),
        game_mode_numeric,
        port_ipv4,
        port_ipv6,
    })
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

    #[test]
    fn ping_packet_contains_raknet_magic() {
        let packet = encode_ping();

        assert_eq!(packet[0], UNCONNECTED_PING);
        assert_eq!(packet[9..25], RAKNET_MAGIC);
    }

    #[test]
    fn parses_bedrock_motd_text() {
        let motd = decode_motd_text(
            "MCPE;§aServer;589;1.21.100;3;20;12345;§bLine2;Survival;1;19132;19133;",
        )
        .expect("motd should parse");

        assert_eq!(motd.edition, "MCPE");
        assert_eq!(motd.motd_line_1, "§aServer");
        assert_eq!(motd.protocol, Some(589));
        assert_eq!(motd.players_online, Some(3));
        assert_eq!(motd.port_ipv4, Some(19_132));
    }

    #[test]
    fn tolerates_semicolon_in_first_motd_line() {
        let motd = decode_motd_text(
            "MCPE;Hello;World;589;1.21.100;0;10;12345;Line2;Survival;1;19132;19133;",
        )
        .expect("motd should parse");

        assert_eq!(motd.motd_line_1, "Hello;World");
        assert!(
            matches!(motd.motd_line_1, Cow::Borrowed(_)),
            "the joined line is one contiguous span of the input"
        );
        assert_eq!(motd.version, "1.21.100");
    }

    #[test]
    fn tolerates_short_bedrock_motd_text() {
        let motd =
            decode_motd_text("MCPE;Mini Server;589;1.21.100;3;20;").expect("motd should parse");

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
        let motd = decode_motd_text("MCPE;Minimal Server").expect("motd should parse");

        assert_eq!(motd.edition, "MCPE");
        assert_eq!(motd.motd_line_1, "Minimal Server");
        assert_eq!(motd.protocol, None);
        assert_eq!(motd.version, "");
        assert_eq!(motd.players_online, None);
        assert_eq!(motd.players_max, None);
    }

    #[test]
    fn rejects_motd_without_required_fields() {
        let error = decode_motd_text("MCPE;").expect_err("single field should fail");

        assert!(matches!(error, Error::InvalidPacket(_)));
        let error = decode_motd_text("").expect_err("empty text should fail");

        assert!(matches!(error, Error::InvalidPacket(_)));
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

        let motd = decode_pong(&packet).expect("pong should parse");

        assert_eq!(motd.motd_line_1, "Server");
        assert_eq!(motd.players_max, Some(8));
    }
}
