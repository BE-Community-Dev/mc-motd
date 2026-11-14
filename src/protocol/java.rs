//! Java Edition status protocol codec.
//!
//! Pure encoding and decoding of the Minecraft Java status handshake: no
//! sockets, no timeouts. The network flow lives in [`crate::java`].

use crate::error::Error;

const STATUS_STATE: i32 = 1;
const HANDSHAKE_PACKET_ID: i32 = 0;
const STATUS_REQUEST_PACKET_ID: i32 = 0;
const MAX_JAVA_STATUS_PACKET_LEN: usize = 2 * 1024 * 1024;

/// Build the two-frame Java Edition status request.
///
/// The returned bytes contain the handshake frame followed by the status
/// request frame.
///
/// # Panics
///
/// Panics if `host` or the generated handshake frame is too large to encode as
/// a Minecraft `VarInt` length. This requires input far beyond a valid server
/// host.
#[must_use]
pub fn encode_status_request(host: &str, port: u16, protocol_version: i32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(host.len() + 16);
    write_varint(&mut packet, HANDSHAKE_PACKET_ID);
    write_varint(&mut packet, protocol_version);
    write_string(&mut packet, host);
    packet.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut packet, STATUS_STATE);

    let mut frame = Vec::with_capacity(packet.len() + 4);
    write_varint(
        &mut frame,
        i32::try_from(packet.len()).expect("Java handshake frame length fits in i32"),
    );
    frame.extend_from_slice(&packet);
    frame.extend_from_slice(&[0x01, 0x00]);
    frame
}

/// Parse a Java Edition status response packet payload into the raw JSON.
///
/// The input must be the decoded packet payload after the outer frame length
/// has already been removed. The returned string borrows from `packet`.
///
/// # Errors
///
/// Returns [`Error`] if the packet id is not a status response, any `VarInt`
/// is truncated or too long, the JSON length is invalid, or the JSON bytes are
/// not valid UTF-8.
pub fn parse_status_response(packet: &[u8]) -> Result<&str, Error> {
    let mut cursor = 0;
    let packet_id = read_varint_from_slice(packet, &mut cursor)?;
    if packet_id != STATUS_REQUEST_PACKET_ID {
        return Err(Error::InvalidPacket(
            "Java packet id is not status response",
        ));
    }

    let json_len = read_varint_from_slice(packet, &mut cursor)?;
    let json_len = usize::try_from(json_len)
        .map_err(|_| Error::InvalidPacket("Java status JSON length is negative"))?;
    let Some(json_end) = cursor.checked_add(json_len) else {
        return Err(Error::InvalidPacket("Java status JSON length overflow"));
    };
    if json_end > packet.len() {
        return Err(Error::InvalidPacket(
            "Java status JSON length exceeds packet size",
        ));
    }

    std::str::from_utf8(&packet[cursor..json_end]).map_err(Error::from)
}

/// Maximum accepted Java status packet length.
pub(crate) const MAX_STATUS_PACKET_LEN: usize = MAX_JAVA_STATUS_PACKET_LEN;

/// Read one `VarInt` from the stream while it stays readable.
pub(crate) async fn read_varint(stream: &mut tokio::net::TcpStream) -> Result<i32, Error> {
    use tokio::io::AsyncReadExt;

    let mut value = 0_u32;
    for position in 0..5 {
        let byte = stream.read_u8().await?;
        value |= u32::from(byte & 0x7f) << (7 * position);
        if byte & 0x80 == 0 {
            return Ok(i32::from_ne_bytes(value.to_ne_bytes()));
        }
    }
    Err(Error::InvalidPacket("Java VarInt is too long"))
}

fn read_varint_from_slice(packet: &[u8], cursor: &mut usize) -> Result<i32, Error> {
    let mut value = 0_u32;
    for position in 0..5 {
        let Some(byte) = packet.get(*cursor).copied() else {
            return Err(Error::InvalidPacket("Java VarInt is truncated"));
        };
        *cursor += 1;
        value |= u32::from(byte & 0x7f) << (7 * position);
        if byte & 0x80 == 0 {
            return Ok(i32::from_ne_bytes(value.to_ne_bytes()));
        }
    }
    Err(Error::InvalidPacket("Java VarInt is too long"))
}

fn write_string(buffer: &mut Vec<u8>, value: &str) {
    write_varint(
        buffer,
        i32::try_from(value.len()).expect("Java string length fits in i32"),
    );
    buffer.extend_from_slice(value.as_bytes());
}

fn write_varint(buffer: &mut Vec<u8>, value: i32) {
    let mut value = u32::from_ne_bytes(value.to_ne_bytes());
    loop {
        let mut byte = u8::try_from(value & 0x7f).expect("masked VarInt byte fits in u8");
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buffer.push(byte);
        if value == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_request_contains_two_frames() {
        let packet = encode_status_request("localhost", 25_565, 763);

        assert!(packet.len() > "localhost".len());
        assert_eq!(&packet[packet.len() - 2..], &[0x01, 0x00]);
    }

    #[test]
    fn parses_status_response() {
        let json = r#"{"version":{"name":"1.21.4"},"players":{"online":1,"max":20}}"#;
        let mut packet = Vec::new();
        write_varint(&mut packet, STATUS_REQUEST_PACKET_ID);
        write_string(&mut packet, json);

        let raw_json = parse_status_response(&packet).expect("status should parse");

        assert_eq!(raw_json, json);
    }

    #[test]
    fn rejects_truncated_status_response() {
        let packet = [0x00, 0x10, b'{'];

        let error = parse_status_response(&packet).expect_err("packet should fail");

        assert!(matches!(error, Error::InvalidPacket(_)));
    }

    #[test]
    fn rejects_negative_status_json_length() {
        let packet = [0x00, 0xff, 0xff, 0xff, 0xff, 0x0f];

        let error = parse_status_response(&packet).expect_err("packet should fail");

        assert!(matches!(error, Error::InvalidPacket(_)));
    }
}
