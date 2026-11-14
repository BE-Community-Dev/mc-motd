//! Legacy `RakNet` UDP status queries.
//!
//! [`query`] always speaks the RakNet unconnected ping, even when the server
//! also supports `NetherNet`; for automatic transport selection use
//! [`crate::bedrock::query`]. The codec lives in
//! [`crate::protocol::bedrock::raknet`].

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use tokio::net::{UdpSocket, lookup_host};
use tokio::time;

use crate::address::ServerAddress;
use crate::client::{Parts, QueryOptions};
use crate::error::{Attempt, Error, Result};
use crate::protocol::bedrock::raknet::{self as codec, RakNetDetailsRef};
use crate::status::RakNetDetails;
use crate::status::{BedrockDetails, Details, Status, Transport};

/// Recommended pong buffer size covering every known MOTD.
pub const PONG_BUFFER_SIZE: usize = 2048;

/// Query a Bedrock Edition server address over `RakNet` only.
///
/// The address may be `host`, `host:port`, or `[ipv6]:port`; the Bedrock
/// default port applies when no explicit port is present.
///
/// # Errors
///
/// Returns [`Error`] if the address is invalid, DNS lookup fails, the UDP
/// exchange fails or times out, or the pong packet is invalid.
pub async fn query(address: impl AsRef<str>) -> Result<Status> {
    crate::client::default_client().raknet(address).await
}

/// Query a Bedrock Edition server address over `RakNet` only with explicit
/// options.
///
/// # Errors
///
/// Returns [`Error`] under the same conditions as [`query`].
pub async fn query_with(address: impl AsRef<str>, options: QueryOptions) -> Result<Status> {
    crate::client::default_client()
        .raknet_with(address, options)
        .await
}

/// Query an already resolved socket address, reading the pong packet into a
/// caller-provided buffer.
///
/// This is the zero-allocation path: the call itself allocates nothing on the
/// heap, and the returned [`RakNetDetailsRef`] borrows the parsed fields from
/// `buffer`. Reuse one buffer across repeated polls to keep steady-state
/// allocations at zero. `buffer` must hold at least 35 bytes; [`PONG_BUFFER_SIZE`]
/// covers every known MOTD, and a truncated pong fails parsing with
/// [`Error::InvalidPacket`].
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if `buffer` is too small for a pong header,
/// [`Error`] for socket creation, send, and receive failures or timeouts, and
/// [`Error::InvalidPacket`] for invalid or truncated pongs.
pub async fn query_addr_into(
    address: SocketAddr,
    buffer: &mut [u8],
    timeout: Duration,
) -> Result<RakNetDetailsRef<'_>, Error> {
    if buffer.len() < codec::PONG_HEADER_LEN {
        return Err(Error::InvalidInput(
            "pong buffer must hold at least 35 bytes",
        ));
    }
    let deadline = Instant::now() + timeout;
    let bind_address = if address.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = UdpSocket::bind(bind_address).await?;
    time::timeout(
        crate::client::engine::remaining(deadline)?,
        socket.connect(address),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    let ping = codec::encode_ping();
    time::timeout(
        crate::client::engine::remaining(deadline)?,
        socket.send(&ping),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    let len = time::timeout(
        crate::client::engine::remaining(deadline)?,
        socket.recv(&mut buffer[..]),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    codec::decode_pong(&buffer[..len])
}

/// Query a host over `RakNet`, resolving it first.
/// Query a host over `RakNet`, resolving it first and trying every resolved
/// address in order (IPv4 first) within the shared deadline.
async fn query_host(host: &str, port: u16, deadline: Instant) -> Result<RakNetDetails, Error> {
    let remaining = crate::client::engine::remaining(deadline)?;
    let mut resolved = time::timeout(remaining, lookup_host((host, port)))
        .await
        .map_err(|_| Error::Timeout)??;
    // Prefer IPv4 addresses; Bedrock servers are most reliably reached there.
    let mut addresses: Vec<SocketAddr> = resolved.by_ref().collect();
    addresses.sort_by_key(|addr| i32::from(!addr.is_ipv4()));

    let mut buffer = vec![0_u8; PONG_BUFFER_SIZE];
    let mut last = None;
    for address in addresses {
        let remaining = crate::client::engine::remaining(deadline)?;
        match query_addr_into(address, &mut buffer, remaining).await {
            Ok(pong) => return Ok(pong.to_owned()),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or(Error::ResolveFailed))
}

/// Probe the `RakNet` transport for the engine, caching the transport on
/// success.
pub(crate) async fn probe(
    address: &ServerAddress,
    deadline: Instant,
    ips: &[IpAddr],
    parts: &Parts<'_>,
) -> Result<Status, Attempt> {
    let port = address.bedrock_port;
    let target = format!("{}:{}", address.host, port);

    let result: Result<RakNetDetails, Error> = async {
        let mut buffer = vec![0_u8; PONG_BUFFER_SIZE];
        if ips.is_empty() {
            return query_host(&address.host, port, deadline).await;
        }
        // UDP has no fast failure signal, so every resolved address is tried
        // in order until one answers or the shared deadline runs out.
        let mut last = None;
        for ip in ips {
            let remaining = crate::client::engine::remaining(deadline)?;
            match query_addr_into(SocketAddr::new(*ip, port), &mut buffer, remaining).await {
                Ok(pong) => return Ok(pong.to_owned()),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or(Error::ResolveFailed))
    }
    .await;

    match result {
        Ok(pong) => {
            parts.cache.store(&address.host, port, Transport::RakNetUdp);
            Ok(Status::from_details(
                address.clone(),
                Details::Bedrock(BedrockDetails::RakNet(pong)),
            ))
        }
        Err(error) => Err(Attempt {
            transport: Transport::RakNetUdp,
            target,
            error: Box::new(error),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::Ipv4Addr;

    use tokio::net::UdpSocket;

    const MOCK_MOTD_TEXT: &str =
        "MCPE;RakNet Mock;589;1.21.100;3;20;12345;Line2;Survival;1;19132;19133;";

    /// Build a `RakNet` unconnected pong packet carrying the MOTD text.
    fn build_pong(text: &str) -> Vec<u8> {
        let mut packet = Vec::with_capacity(35 + text.len());
        packet.push(0x1c);
        packet.extend_from_slice(&1_i64.to_be_bytes());
        packet.extend_from_slice(&2_i64.to_be_bytes());
        packet.extend_from_slice(&[
            0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34,
            0x56, 0x78,
        ]);
        packet.extend_from_slice(
            &u16::try_from(text.len())
                .expect("fixture MOTD length fits in u16")
                .to_be_bytes(),
        );
        packet.extend_from_slice(text.as_bytes());
        packet
    }

    async fn spawn_pong_mock() -> SocketAddr {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock socket should bind");
        let address = socket.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            let mut buffer = [0_u8; 64];
            loop {
                let Ok((len, peer)) = socket.recv_from(&mut buffer).await else {
                    return;
                };
                if len != 33 {
                    return;
                }
                let pong = build_pong(MOCK_MOTD_TEXT);
                let _ = socket.send_to(&pong, peer).await;
            }
        });
        address
    }

    #[tokio::test]
    async fn query_addr_into_parses_borrowed_pong() {
        let mock = spawn_pong_mock().await;

        let mut buffer = [0_u8; PONG_BUFFER_SIZE];
        let motd = query_addr_into(mock, &mut buffer, Duration::from_millis(500))
            .await
            .expect("mock pong should parse");

        assert_eq!(motd.edition, "MCPE");
        assert_eq!(motd.motd_line_1, "RakNet Mock");
        assert_eq!(motd.players_max, Some(20));
    }

    #[tokio::test]
    async fn query_addr_into_rejects_small_buffer() {
        let mut buffer = [0_u8; 16];
        let address: SocketAddr = "127.0.0.1:19132".parse().expect("address should parse");

        let error = query_addr_into(address, &mut buffer, Duration::from_millis(100))
            .await
            .expect_err("small buffer should fail");

        assert!(matches!(error, Error::InvalidInput(_)));
    }

    #[tokio::test]
    async fn query_answers_with_unified_status() {
        let mock = spawn_pong_mock().await;
        let host = mock.ip().to_string();
        let port = mock.port();

        let status = query(format!("{host}:{port}"))
            .await
            .expect("raknet mock should answer");

        assert_eq!(status.address.host, host);
        assert_eq!(status.address.bedrock_port, port);
        assert_eq!(status.motd, "RakNet Mock");
        assert_eq!(status.version.name.as_deref(), Some("1.21.100"));
        assert_eq!(status.version.protocol, Some(589));
        assert_eq!(status.players.online, Some(3));
        assert_eq!(status.players.max, Some(20));
        assert_eq!(status.transport(), Transport::RakNetUdp);
        #[allow(irrefutable_let_patterns)]
        if let Details::Bedrock(BedrockDetails::RakNet(details)) = &status.details {
            assert_eq!(details.server_id, "12345");
        }
        #[cfg(feature = "nethernet")]
        if let Details::Bedrock(BedrockDetails::NetherNet(_)) = &status.details {
            panic!("expected raknet details");
        }
    }
}
