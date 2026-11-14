//! LAN discovery for Bedrock Edition servers via UDP broadcast.
//!
//! LAN discovery is independent from the public status query helpers: it
//! broadcasts a `RakNet` unconnected ping on the local network and collects
//! the unconnected pong replies. It never contacts public `NetherNet` HTTP
//! endpoints, and the public automatic query helpers never broadcast on the
//! LAN.
//!
//! The default broadcast port is [`DEFAULT_PORT`] (7551). Vanilla Bedrock LAN
//! hosts advertise on UDP 19132 ([`VANILLA_PORT`]); pass that port via
//! [`DiscoveryOptions::broadcast_port`] to discover vanilla worlds.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time;

use crate::error::{Error, Result};
use crate::protocol::bedrock::raknet::{self as codec, RakNetDetails};
use crate::status::Transport;

/// Default LAN discovery broadcast port used by this library.
pub const DEFAULT_PORT: u16 = 7_551;
/// Broadcast port used by vanilla Bedrock Edition LAN hosts.
pub const VANILLA_PORT: u16 = 19_132;

/// Default LAN discovery listen window.
const DEFAULT_LISTEN_DURATION: Duration = Duration::from_millis(1_500);
/// Default cap on discovered servers.
const DEFAULT_MAX_RESULTS: usize = 32;

/// Options for LAN discovery broadcasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiscoveryOptions {
    /// UDP port used for the broadcast ping and the listening socket.
    pub broadcast_port: u16,
    /// IPv4 broadcast address the ping is sent to.
    pub broadcast_addr: IpAddr,
    /// How long pong replies are collected before returning.
    pub listen_duration: Duration,
    /// Upper bound on the number of servers returned.
    pub max_results: usize,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            broadcast_port: DEFAULT_PORT,
            broadcast_addr: IpAddr::V4(Ipv4Addr::BROADCAST),
            listen_duration: DEFAULT_LISTEN_DURATION,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }
}

impl DiscoveryOptions {
    /// Set the UDP broadcast port.
    #[must_use]
    pub fn broadcast_port(mut self, port: u16) -> Self {
        self.broadcast_port = port;
        self
    }

    /// Set the IPv4 broadcast address.
    #[must_use]
    pub fn broadcast_addr(mut self, addr: IpAddr) -> Self {
        self.broadcast_addr = addr;
        self
    }

    /// Set the reply collection window.
    #[must_use]
    pub fn listen_duration(mut self, duration: Duration) -> Self {
        self.listen_duration = duration;
        self
    }

    /// Set the upper bound on the number of servers returned.
    #[must_use]
    pub fn max_results(mut self, max_results: usize) -> Self {
        self.max_results = max_results;
        self
    }
}

/// A Bedrock Edition server discovered on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LanServer {
    /// Socket address the pong reply was received from.
    pub source: SocketAddr,
    /// Raw `RakNet` pong details carried by the reply.
    pub details: RakNetDetails,
}

impl LanServer {
    /// Transport that produced this entry; always [`Transport::RakNetUdp`].
    #[must_use]
    pub fn transport(&self) -> Transport {
        Transport::RakNetUdp
    }
}

/// Discover Bedrock Edition servers on the local network.
///
/// A `RakNet` unconnected ping is broadcast to
/// `{broadcast_addr}:{broadcast_port}` and unconnected pong replies are
/// collected for `listen_duration`. One entry is kept per replying source
/// address; replies that fail to decode are ignored.
///
/// This helper only talks to hosts on the local network; use the regular
/// query helpers for public servers.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] for a zero broadcast port or a non-IPv4
/// broadcast address, and [`Error::Io`] if the listening socket cannot be
/// created or the broadcast cannot be sent.
pub async fn discover(options: DiscoveryOptions) -> Result<Vec<LanServer>> {
    if options.broadcast_port == 0 {
        return Err(Error::InvalidInput("server port must be greater than 0"));
    }
    let IpAddr::V4(broadcast) = options.broadcast_addr else {
        return Err(Error::InvalidInput(
            "LAN discovery requires an IPv4 broadcast address",
        ));
    };

    let socket = bind_lan_socket(options.broadcast_port)?;
    let target = SocketAddr::from((broadcast, options.broadcast_port));
    let ping = codec::encode_ping();
    socket.send_to(&ping, target).await?;

    collect_pongs(socket, &ping, options.listen_duration, options.max_results).await
}

/// Bind a broadcast-capable IPv4 UDP socket on the given port.
///
/// `SO_REUSEADDR` is set on a best-effort basis so discovery can coexist with
/// a running game client that already listens on the same port.
fn bind_lan_socket(port: u16) -> Result<UdpSocket, Error> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    let _ = socket.set_reuse_address(true);
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into()).map_err(Error::from)
}

async fn collect_pongs(
    socket: UdpSocket,
    ping: &[u8],
    listen_duration: Duration,
    max_results: usize,
) -> Result<Vec<LanServer>, Error> {
    let deadline = Instant::now()
        .checked_add(listen_duration)
        .ok_or(Error::InvalidInput("listen duration is too large"))?;
    // One entry per replying socket address, so multiple servers on one host
    // are all reported.
    let mut servers: HashMap<SocketAddr, LanServer> = HashMap::new();
    let mut buffer = [0_u8; 2048];

    loop {
        if servers.len() >= max_results {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match time::timeout(remaining, socket.recv_from(&mut buffer)).await {
            Err(_) => break,
            Ok(Err(error)) => return Err(error.into()),
            Ok(Ok((len, source))) => {
                if len == 0 {
                    continue;
                }
                // Correlate the pong with our ping timestamp to ignore stale
                // or foreign replies.
                if buffer.len() < 9 || buffer[1..9] != ping[1..9] {
                    continue;
                }
                let Ok(details) = codec::decode_pong(&buffer[..len]).map(|motd| motd.to_owned())
                else {
                    continue;
                };
                servers.insert(source, LanServer { source, details });
            }
        }
    }

    let mut servers: Vec<LanServer> = servers.into_values().collect();
    servers.sort_by_key(|server| server.source);
    Ok(servers)
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::net::UdpSocket;

    #[test]
    fn default_options_use_lan_defaults() {
        let options = DiscoveryOptions::default();

        assert_eq!(options.broadcast_port, DEFAULT_PORT);
        assert_eq!(options.broadcast_port, 7_551);
        assert_eq!(options.broadcast_addr, IpAddr::V4(Ipv4Addr::BROADCAST));
    }

    #[tokio::test]
    async fn rejects_zero_broadcast_port() {
        let options = DiscoveryOptions::default().broadcast_port(0);

        let error = discover(options).await.expect_err("zero port should fail");

        assert!(matches!(error, Error::InvalidInput(_)));
    }

    #[tokio::test]
    async fn rejects_ipv6_broadcast_address() {
        let options =
            DiscoveryOptions::default().broadcast_addr(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));

        let error = discover(options)
            .await
            .expect_err("IPv6 broadcast should fail");

        assert!(matches!(error, Error::InvalidInput(_)));
    }

    /// Build a `RakNet` unconnected pong packet carrying the MOTD text and
    /// echoing the ping timestamp so the correlation check accepts it.
    fn build_pong(ping_time: [u8; 8], text: &str) -> Vec<u8> {
        let mut packet = Vec::with_capacity(35 + text.len());
        packet.push(0x1c);
        packet.extend_from_slice(&ping_time);
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

    #[tokio::test]
    async fn collect_pongs_parses_and_dedupes_mock_replies() {
        let listener = bind_lan_socket(0).expect("listener should bind");
        // The listener binds 0.0.0.0; loopback is the reachable route for the
        // mock replies in this test.
        let listen_address = SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            listener
                .local_addr()
                .expect("listener address should exist")
                .port(),
        ));

        let ping = codec::encode_ping();
        let echo = ping[1..9].to_vec();
        let responder = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("responder should bind");
        tokio::spawn(async move {
            let _ = responder
                .send_to(
                    &build_pong(
                        echo.clone().try_into().expect("8 bytes"),
                        "MCPE;LAN Server;589;1.21.100;3;20;1;Line2;Survival;1;19132;19133;",
                    ),
                    listen_address,
                )
                .await;
            let _ = responder.send_to(b"not a pong", listen_address).await;
            let _ = responder
                .send_to(
                    &build_pong(
                        echo.try_into().expect("8 bytes"),
                        "MCPE;LAN Server v2;589;1.21.100;4;20;1;Line2;Survival;1;19132;19133;",
                    ),
                    listen_address,
                )
                .await;
        });
        let servers = collect_pongs(listener, &ping, Duration::from_millis(300), 32)
            .await
            .expect("collection should succeed");

        assert_eq!(servers.len(), 1, "replies from one host should dedupe");
        let server = &servers[0];
        assert_eq!(server.source.ip(), Ipv4Addr::LOCALHOST);
        assert_eq!(
            server.details.motd_line_1, "LAN Server v2",
            "latest reply wins"
        );
        assert_eq!(server.details.players_online, Some(4));
        assert_eq!(server.transport(), Transport::RakNetUdp);
    }

    #[tokio::test]
    async fn collect_pongs_keeps_multiple_hosts() {
        let listener = bind_lan_socket(0).expect("listener should bind");
        let listen_address = SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            listener
                .local_addr()
                .expect("listener address should exist")
                .port(),
        ));

        let first = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("first responder should bind");
        let second = UdpSocket::bind(("127.0.0.2", 0)).await;
        let Ok(second) = second else {
            // Platform does not expose multi-address loopback.
            return;
        };
        let ping = codec::encode_ping();
        let first_echo = ping[1..9].to_vec();
        let second_echo = first_echo.clone();
        let first = first;
        let second = second;
        tokio::spawn(async move {
            let _ = first
                .send_to(
                    &build_pong(
                        first_echo.try_into().expect("8 bytes"),
                        "MCPE;First;589;1.21.100;1;20;1;Line2;Survival;1;19132;19133;",
                    ),
                    listen_address,
                )
                .await;
        });
        tokio::spawn(async move {
            let _ = second
                .send_to(
                    &build_pong(
                        second_echo.try_into().expect("8 bytes"),
                        "MCPE;Second;589;1.21.100;2;20;2;Line2;Survival;1;19133;19134;",
                    ),
                    listen_address,
                )
                .await;
        });

        let servers = collect_pongs(listener, &ping, Duration::from_millis(300), 32)
            .await
            .expect("collection should succeed");

        let names: Vec<&str> = servers
            .iter()
            .map(|server| server.details.motd_line_1.as_str())
            .collect();
        assert_eq!(names, vec!["First", "Second"]);
    }

    #[tokio::test]
    async fn collect_pongs_returns_empty_without_replies() {
        let listener = bind_lan_socket(0).expect("listener should bind");

        let ping = codec::encode_ping();
        let servers = collect_pongs(listener, &ping, Duration::from_millis(50), 32)
            .await
            .expect("collection should succeed");

        assert_eq!(servers, [] as [LanServer; 0]);
    }

    #[tokio::test]
    #[ignore = "requires a LAN with discoverable Bedrock servers"]
    async fn live_lan_discovery_runs() {
        let servers = discover(DiscoveryOptions::default())
            .await
            .expect("discovery should run");

        // LAN contents vary by environment; this only checks the broadcast
        // and collection path completes without an error.
        for server in &servers {
            assert_ne!(server.details.edition, "");
        }
    }
}
