use std::borrow::Cow;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, lookup_host};
use tokio::time;

use crate::MotdError;

const STATUS_STATE: i32 = 1;
const HANDSHAKE_PACKET_ID: i32 = 0;
const STATUS_REQUEST_PACKET_ID: i32 = 0;
const MAX_JAVA_STATUS_PACKET_LEN: usize = 2 * 1024 * 1024;

/// Owned Java Edition status response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaStatus {
    /// Raw JSON returned by the server status endpoint.
    pub raw_json: String,
}

/// Borrowed Java Edition status response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaStatusRef<'a> {
    /// Raw JSON returned by the server status endpoint.
    pub raw_json: Cow<'a, str>,
}

impl JavaStatusRef<'_> {
    #[must_use]
    pub fn to_owned(&self) -> JavaStatus {
        JavaStatus {
            raw_json: self.raw_json.to_string(),
        }
    }
}

/// Query a Java Edition server by host and port.
///
/// # Errors
///
/// Returns [`MotdError`] if DNS lookup fails or times out, no socket address is
/// resolved, TCP I/O fails or times out, or the status packet is invalid.
pub async fn query_java(
    host: impl AsRef<str>,
    port: u16,
    timeout: Duration,
) -> Result<JavaStatus, MotdError> {
    let host = host.as_ref();
    let mut resolved = time::timeout(timeout, lookup_host((host, port)))
        .await
        .map_err(|_| MotdError::Timeout)??;
    let address = resolved.next().ok_or(MotdError::ResolveFailed)?;
    query_java_addr(address, host, port, -1, timeout).await
}

/// Query a Java Edition server at an already resolved socket address.
///
/// The `host` and `port` values are still written into the Minecraft handshake.
///
/// # Errors
///
/// Returns [`MotdError`] if the TCP connection, write, read, UTF-8 validation,
/// or Java status packet parsing fails or times out.
pub async fn query_java_addr(
    address: SocketAddr,
    host: &str,
    port: u16,
    protocol_version: i32,
    timeout: Duration,
) -> Result<JavaStatus, MotdError> {
    let mut stream = time::timeout(timeout, TcpStream::connect(address))
        .await
        .map_err(|_| MotdError::Timeout)??;

    let handshake = build_java_status_request(host, port, protocol_version);
    time::timeout(timeout, stream.write_all(&handshake))
        .await
        .map_err(|_| MotdError::Timeout)??;

    let packet_len = time::timeout(timeout, read_varint_from_stream(&mut stream))
        .await
        .map_err(|_| MotdError::Timeout)??;
    let packet_len = usize::try_from(packet_len)
        .map_err(|_| MotdError::InvalidPacket("Java status packet length is negative"))?;
    if packet_len > MAX_JAVA_STATUS_PACKET_LEN {
        return Err(MotdError::InvalidPacket("Java status packet is too large"));
    }

    let mut packet = vec![0_u8; packet_len];
    time::timeout(timeout, stream.read_exact(&mut packet))
        .await
        .map_err(|_| MotdError::Timeout)??;

    parse_java_status_response(&packet).map(|status| status.to_owned())
}

/// Build the two-frame Java Edition status request.
///
/// The returned bytes contain the handshake frame followed by the status
/// request frame.
///
/// # Panics
///
/// Panics if `host` or the generated handshake frame is too large to encode as a
/// Minecraft `VarInt` length. This requires input far beyond a valid server host.
#[must_use]
pub fn build_java_status_request(host: &str, port: u16, protocol_version: i32) -> Vec<u8> {
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

/// Parse a Java Edition status response packet payload.
///
/// The input must be the decoded packet payload after the outer frame length has
/// already been removed.
///
/// # Errors
///
/// Returns [`MotdError`] if the packet id is not a status response, any `VarInt`
/// is truncated or too long, the JSON length is invalid, or the JSON bytes are
/// not valid UTF-8.
pub fn parse_java_status_response(packet: &[u8]) -> Result<JavaStatusRef<'_>, MotdError> {
    let mut cursor = 0;
    let packet_id = read_varint_from_slice(packet, &mut cursor)?;
    if packet_id != STATUS_REQUEST_PACKET_ID {
        return Err(MotdError::InvalidPacket(
            "Java packet id is not status response",
        ));
    }

    let json_len = read_varint_from_slice(packet, &mut cursor)?;
    let json_len = usize::try_from(json_len)
        .map_err(|_| MotdError::InvalidPacket("Java status JSON length is negative"))?;
    let Some(json_end) = cursor.checked_add(json_len) else {
        return Err(MotdError::InvalidPacket("Java status JSON length overflow"));
    };
    if json_end > packet.len() {
        return Err(MotdError::InvalidPacket(
            "Java status JSON length exceeds packet size",
        ));
    }

    Ok(JavaStatusRef {
        raw_json: Cow::Borrowed(std::str::from_utf8(&packet[cursor..json_end])?),
    })
}

async fn read_varint_from_stream(stream: &mut TcpStream) -> Result<i32, MotdError> {
    let mut value = 0_u32;
    for position in 0..5 {
        let byte = stream.read_u8().await?;
        value |= u32::from(byte & 0x7f) << (7 * position);
        if byte & 0x80 == 0 {
            return Ok(i32::from_ne_bytes(value.to_ne_bytes()));
        }
    }
    Err(MotdError::InvalidPacket("Java VarInt is too long"))
}

fn read_varint_from_slice(packet: &[u8], cursor: &mut usize) -> Result<i32, MotdError> {
    let mut value = 0_u32;
    for position in 0..5 {
        let Some(byte) = packet.get(*cursor).copied() else {
            return Err(MotdError::InvalidPacket("Java VarInt is truncated"));
        };
        *cursor += 1;
        value |= u32::from(byte & 0x7f) << (7 * position);
        if byte & 0x80 == 0 {
            return Ok(i32::from_ne_bytes(value.to_ne_bytes()));
        }
    }
    Err(MotdError::InvalidPacket("Java VarInt is too long"))
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

    use tokio::net::TcpListener;

    #[test]
    fn java_status_request_contains_two_frames() {
        let packet = build_java_status_request("localhost", 25565, 763);

        assert!(packet.len() > "localhost".len());
        assert_eq!(&packet[packet.len() - 2..], &[0x01, 0x00]);
    }

    #[test]
    fn parses_java_status_response() {
        let json = r#"{"version":{"name":"1.21.4"},"players":{"online":1,"max":20}}"#;
        let mut packet = Vec::new();
        write_varint(&mut packet, STATUS_REQUEST_PACKET_ID);
        write_string(&mut packet, json);

        let status = parse_java_status_response(&packet).expect("status should parse");

        assert_eq!(status.raw_json, json);
    }

    #[test]
    fn rejects_truncated_java_status_response() {
        let packet = [0x00, 0x10, b'{'];

        let error = parse_java_status_response(&packet).expect_err("packet should fail");

        assert!(matches!(error, MotdError::InvalidPacket(_)));
    }

    #[test]
    fn rejects_negative_java_status_json_length() {
        let packet = [0x00, 0xff, 0xff, 0xff, 0xff, 0x0f];

        let error = parse_java_status_response(&packet).expect_err("packet should fail");

        assert!(matches!(error, MotdError::InvalidPacket(_)));
    }

    #[tokio::test]
    async fn query_java_addr_reads_mock_status() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client should connect");
            let mut request = [0_u8; 256];
            let bytes_read = stream
                .read(&mut request)
                .await
                .expect("request should be readable");
            assert!(bytes_read > 0);

            let json = r#"{"version":{"name":"1.21.4"},"players":{"online":1,"max":20}}"#;
            let mut packet = Vec::new();
            write_varint(&mut packet, STATUS_REQUEST_PACKET_ID);
            write_string(&mut packet, json);

            let mut frame = Vec::new();
            write_varint(
                &mut frame,
                i32::try_from(packet.len()).expect("fixture packet length fits in i32"),
            );
            frame.extend_from_slice(&packet);
            stream
                .write_all(&frame)
                .await
                .expect("response should be writable");
        });

        let status = query_java_addr(address, "localhost", 25_565, -1, Duration::from_millis(500))
            .await
            .expect("mock status should parse");

        assert!(status.raw_json.contains(r#""online":1"#));
        server.await.expect("server task should finish");
    }
}
