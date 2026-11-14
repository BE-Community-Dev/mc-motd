//! Java Edition status queries.
//!
//! [`query`] resolves `_minecraft._tcp` SRV records when the address has no
//! explicit port, then probes the Java status endpoint over TCP. For custom
//! timeouts and options use [`query_with`] or [`crate::Client`]. The pure
//! protocol codec lives in [`crate::protocol::java`].

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time;

use crate::address::{self, ServerAddress};
use crate::client::{QueryOptions, ResolvedOptions};
use crate::error::{Attempt, Error, Result};
use crate::protocol::java as codec;
use crate::status::{Details, JavaDetails, Players, Status, Transport, Version};

/// Java Edition SRV lookup target.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SrvTarget {
    /// Target hostname with the trailing DNS root dot removed.
    pub host: String,
    /// Target port from the SRV record.
    pub port: u16,
    /// SRV priority. Lower values are preferred.
    pub priority: u16,
    /// SRV weight. Higher values are preferred within the same priority.
    pub weight: u16,
}

/// Query a Java Edition server address.
///
/// The address may be `host`, `host:port`, or `[ipv6]:port`. Without an
/// explicit port, `_minecraft._tcp` SRV records are resolved first and the
/// direct Java default port is used as fallback.
///
/// # Errors
///
/// Returns [`Error`] if the address is invalid, DNS lookup fails, the network
/// exchange fails or exceeds the deadline, or no Java server answers.
pub async fn query(address: impl AsRef<str>) -> Result<Status> {
    crate::client::default_client()?.java(address).await
}

/// Query a Java Edition server address with explicit options.
///
/// The `edition` field of `options` is ignored here; this function always
/// probes Java.
///
/// # Errors
///
/// Returns [`Error`] under the same conditions as [`query`].
pub async fn query_with(address: impl AsRef<str>, options: QueryOptions) -> Result<Status> {
    crate::client::default_client()?
        .java_with(address, options)
        .await
}

/// Resolve `_minecraft._tcp` SRV records for a host.
///
/// Records are ordered by priority; within one priority the standard SRV
/// weighted-random selection is applied. IP addresses and IPv6 literals
/// return an empty list because SRV records only apply to DNS names.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the host is invalid, [`Error::Timeout`]
/// if the lookup exceeds `timeout`, or [`Error::Dns`] if resolver setup or
/// lookup fails.
pub async fn resolve_srv(host: impl AsRef<str>, timeout: Duration) -> Result<Vec<SrvTarget>> {
    let resolver = crate::client::default_client()?.srv_resolver().clone();
    resolve_srv_with(&resolver, host, timeout).await
}

/// Resolve SRV records with a caller-supplied resolver so client queries
/// reuse one resolver per client instead of rebuilding it per query.
pub(crate) async fn resolve_srv_with(
    resolver: &hickory_resolver::TokioResolver,
    host: impl AsRef<str>,
    timeout: Duration,
) -> Result<Vec<SrvTarget>> {
    let host = address::normalize_server_host(host.as_ref())?;
    if !address::can_query_srv(host.as_ref()) {
        return Ok(Vec::new());
    }

    let name = address::minecraft_srv_name(host.as_ref());
    let lookup = time::timeout(timeout, resolver.srv_lookup(name))
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|error| Error::Dns(error.to_string()))?;

    let records = lookup
        .answers()
        .iter()
        .filter_map(|record| match &record.data {
            hickory_resolver::proto::rr::RData::SRV(srv) => Some(SrvTarget {
                host: srv.target.to_utf8().trim_end_matches('.').to_string(),
                port: srv.port,
                priority: srv.priority,
                weight: srv.weight,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if records.is_empty() {
        return Ok(Vec::new());
    }
    // RFC 2782: a "." target or port 0 means the service is decidedly
    // unavailable — that is an authoritative answer, not a fallback signal.
    if records
        .iter()
        .all(|target| target.host.is_empty() || target.port == 0)
    {
        return Err(Error::Dns(
            "SRV records mark the service as unavailable".into(),
        ));
    }
    let usable = records
        .into_iter()
        .filter(|target| !target.host.is_empty() && target.port != 0)
        .collect::<Vec<_>>();
    Ok(weighted_srv_order(usable))
}

/// Order SRV records by priority with RFC 2782 weighted-random selection
/// inside each priority class. Zero-weight records keep the lowest selection
/// probability per the RFC and are only ordered once nothing else remains.
fn weighted_srv_order(mut records: Vec<SrvTarget>) -> Vec<SrvTarget> {
    records.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.host.cmp(&right.host))
            .then_with(|| left.port.cmp(&right.port))
    });

    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1_u64, |duration| u64::from(duration.subsec_nanos()) | 1);
    let mut random = move || {
        // xorshift64: tiny dependency-free PRNG, good enough for ordering.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    let mut ordered = Vec::with_capacity(records.len());
    while !records.is_empty() {
        let priority = records[0].priority;
        let group_end = records
            .iter()
            .take_while(|target| target.priority == priority)
            .count();
        let mut group: Vec<SrvTarget> = records.drain(0..group_end).collect();
        while !group.is_empty() {
            // RFC 2782: sum the weights of the records yet to be ordered and
            // pick the record whose range covers a uniform random point in
            // [0, Sum). Zero-weight records own an empty range and are only
            // selected once nothing else remains.
            let total: u64 = group.iter().map(|target| u64::from(target.weight)).sum();
            let picked = if total == 0 {
                0
            } else {
                let mut point = random() % total;
                let mut index = group.len() - 1;
                for (position, target) in group.iter().enumerate() {
                    if u64::from(target.weight) > point {
                        index = position;
                        break;
                    }
                    point -= u64::from(target.weight);
                }
                index
            };
            ordered.push(group.remove(picked));
        }
    }
    ordered
}

/// Whether the SRV lookup should run for this address.
///
/// SRV applies only when the caller let the library choose the port (no
/// explicit port), the option is enabled, and the host is a DNS name.
fn srv_applicable(address: &ServerAddress, resolved: &ResolvedOptions) -> bool {
    resolved.try_srv && address.explicit_port.is_none() && address::can_query_srv(&address.host)
}

/// Full Java path used by the engine: SRV records first, then direct.
pub(crate) async fn run(
    address: &ServerAddress,
    resolved: &ResolvedOptions,
    deadline: Instant,
    srv_resolver: &hickory_resolver::TokioResolver,
) -> Result<Status> {
    let mut prefix_attempts: Vec<Attempt> = Vec::new();

    if srv_applicable(address, resolved) {
        // Cap the SRV budget so a slow SRV path cannot starve direct probes.
        let srv_budget =
            crate::client::engine::remaining(deadline)?.min(crate::client::SRV_LOOKUP_BUDGET);
        match resolve_srv_with(srv_resolver, &address.host, srv_budget).await {
            Ok(targets) if !targets.is_empty() => {
                return probe_srv_sequence(address, &targets, resolved, deadline)
                    .await
                    .map_err(|attempts| Error::NoCompatibleJavaServer {
                        host: address.host.clone(),
                        java_port: address.java_port,
                        attempts,
                    });
            }
            Ok(_) => prefix_attempts.push(Attempt {
                transport: Transport::JavaTcp,
                target: address.host.clone(),
                error: Box::new(Error::Dns("no SRV records found".to_string())),
            }),
            // SRV records pointing at "." are an authoritative "service
            // unavailable": do not silently fall back to direct probing.
            Err(error @ Error::Dns(_)) if error.to_string().contains("unavailable") => {
                prefix_attempts.push(Attempt {
                    transport: Transport::JavaTcp,
                    target: address.host.clone(),
                    error: Box::new(error),
                });
                return Err(Error::NoCompatibleJavaServer {
                    host: address.host.clone(),
                    java_port: address.java_port,
                    attempts: prefix_attempts,
                });
            }
            Err(error) => prefix_attempts.push(Attempt {
                transport: Transport::JavaTcp,
                target: address.host.clone(),
                error: Box::new(error),
            }),
        }
    }

    let ips = crate::client::engine::resolve_probe_ips(&address.host, deadline)
        .await
        .map_err(|error| Error::NoCompatibleJavaServer {
            host: address.host.clone(),
            java_port: address.java_port,
            attempts: vec![Attempt {
                transport: Transport::JavaTcp,
                target: address.host.clone(),
                error: Box::new(error),
            }],
        })?;

    probe_direct(address, &ips, resolved, deadline)
        .await
        .map_err(|mut attempts| {
            let mut all = std::mem::take(&mut prefix_attempts);
            all.append(&mut attempts);
            Error::NoCompatibleJavaServer {
                host: address.host.clone(),
                java_port: address.java_port,
                attempts: all,
            }
        })
}

/// Probe SRV targets in order, then fall back to the direct address.
pub(crate) async fn probe_srv_sequence(
    address: &ServerAddress,
    targets: &[SrvTarget],
    resolved: &ResolvedOptions,
    deadline: Instant,
) -> Result<Status, Vec<Attempt>> {
    let mut attempts = Vec::new();
    // Fair-share the remaining deadline across SRV targets so one silent
    // target cannot consume the budget meant for the rest.
    let mut targets_left = targets.len();
    for target in targets {
        let remaining = crate::client::engine::remaining(deadline).unwrap_or_default();
        let target_budget = remaining / u32::try_from(targets_left).unwrap_or(u32::MAX);
        let target_deadline = Instant::now()
            .checked_add(target_budget)
            .unwrap_or(deadline);
        targets_left = targets_left.saturating_sub(1);

        match probe_target(address, target, resolved, target_deadline).await {
            Ok(status) => return Ok(status),
            Err(error) => attempts.push(Attempt {
                transport: Transport::JavaTcp,
                target: format!("{}:{}", target.host, target.port),
                error: Box::new(error),
            }),
        }
    }
    // Direct fallback: an empty address list makes probe_direct resolve
    // the input host itself.
    match probe_direct(address, &[], resolved, deadline).await {
        Ok(status) => Ok(status),
        Err(mut direct_attempts) => {
            attempts.append(&mut direct_attempts);
            Err(attempts)
        }
    }
}

/// Probe one SRV target, trying every resolved address in order. The
/// handshake speaks to the target while `Status.address` keeps the requested
/// address and the SRV record lands in [`JavaDetails::srv`].
async fn probe_target(
    requested: &ServerAddress,
    target: &SrvTarget,
    resolved: &ResolvedOptions,
    deadline: Instant,
) -> Result<Status, Error> {
    let ips = crate::client::engine::resolve_probe_ips(&target.host, deadline).await?;
    let mut last = None;
    // The target deadline is already a fair slice of the whole query; split
    // it across the target's own addresses.
    let mut attempts_left = ips.len();
    for ip in ips {
        let remaining = crate::client::engine::remaining(deadline)?;
        let attempt_deadline = Instant::now()
            .checked_add(remaining / u32::try_from(attempts_left).unwrap_or(u32::MAX))
            .unwrap_or(deadline);
        attempts_left = attempts_left.saturating_sub(1);

        let socket = SocketAddr::new(ip, target.port);
        match query_socket(
            socket,
            &target.host,
            target.port,
            requested,
            resolved,
            attempt_deadline,
        )
        .await
        {
            Ok(status) => return Ok(mark_srv(status, target)),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or(Error::ResolveFailed))
}

/// Stamp the SRV record onto a Java status payload.
fn mark_srv(mut status: Status, target: &SrvTarget) -> Status {
    #[allow(irrefutable_let_patterns)]
    if let Status {
        details: Details::Java(details),
        ..
    } = &mut status
    {
        details.srv = Some(target.clone());
    }
    status
}

/// Probe the direct Java port, trying every resolved address in order until
/// one answers. With an empty `ips` slice the input host is resolved first.
/// The whole sequence is deadline-bounded end to end.
pub(crate) async fn probe_direct(
    address: &ServerAddress,
    ips: &[IpAddr],
    resolved: &ResolvedOptions,
    deadline: Instant,
) -> Result<Status, Vec<Attempt>> {
    let mut attempts = Vec::new();
    let resolved_ips;
    let candidate_ips: &[IpAddr] = if ips.is_empty() {
        resolved_ips = crate::client::engine::resolve_probe_ips(&address.host, deadline)
            .await
            .map_err(|error| {
                vec![Attempt {
                    transport: Transport::JavaTcp,
                    target: address.host.clone(),
                    error: Box::new(error),
                }]
            })?;
        &resolved_ips
    } else {
        ips
    };

    // Fair-share the shared deadline across the addresses so a silently
    // dropping first address gets its slice and the remaining candidates
    // split whatever is left.
    let mut attempts_left = candidate_ips.len();
    for ip in candidate_ips {
        let remaining = crate::client::engine::remaining(deadline).unwrap_or_default();
        let attempt_budget = remaining / u32::try_from(attempts_left).unwrap_or(u32::MAX);
        let attempt_deadline = Instant::now()
            .checked_add(attempt_budget)
            .unwrap_or(deadline);
        attempts_left = attempts_left.saturating_sub(1);

        let socket = SocketAddr::new(*ip, address.java_port);
        match query_socket(
            socket,
            &address.host,
            address.java_port,
            address,
            resolved,
            attempt_deadline,
        )
        .await
        {
            Ok(status) => return Ok(status),
            Err(error) => attempts.push(Attempt {
                transport: Transport::JavaTcp,
                target: format!("{}:{}", address.host, address.java_port),
                error: Box::new(error),
            }),
        }
    }
    Err(attempts)
}

/// Connect, exchange the status handshake, and build the Java [`Status`].
async fn query_socket(
    socket: SocketAddr,
    host: &str,
    port: u16,
    requested: &ServerAddress,
    resolved: &ResolvedOptions,
    deadline: Instant,
) -> Result<Status, Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = time::timeout(
        crate::client::engine::remaining(deadline)?,
        TcpStream::connect(socket),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    let handshake = codec::encode_status_request(host, port, -1);
    time::timeout(
        crate::client::engine::remaining(deadline)?,
        stream.write_all(&handshake),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    let packet_len = time::timeout(
        crate::client::engine::remaining(deadline)?,
        codec::read_varint(&mut stream),
    )
    .await
    .map_err(|_| Error::Timeout)??;
    let packet_len = usize::try_from(packet_len)
        .map_err(|_| Error::InvalidPacket("Java status packet length is negative"))?;
    if packet_len > resolved.max_java_status_bytes {
        return Err(Error::InvalidPacket("Java status packet is too large"));
    }

    let mut packet = vec![0_u8; packet_len];
    time::timeout(
        crate::client::engine::remaining(deadline)?,
        stream.read_exact(&mut packet),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    let raw_json = codec::parse_status_response(&packet)?.to_owned();
    // A well-formed status packet carrying garbage is a protocol failure, not
    // a blank server: without this check automatic probing would stop at a
    // broken Java response. One typed parse validates the structure and
    // yields the normalized fields; missing optional fields stay lenient.
    // A status object must be a JSON object: serde would otherwise accept
    // sequences like `[]` as field-order tuples for this struct.
    if !raw_json.trim_start().starts_with('{') {
        return Err(Error::InvalidPacket(
            "Java status JSON is not a valid status object",
        ));
    }
    let parsed = serde_json::from_str::<JavaJson>(&raw_json)
        .map_err(|_| Error::InvalidPacket("Java status JSON is not a valid status object"))?;
    Ok(Status {
        address: requested.clone(),
        motd: describe(&parsed.description),
        players: Players {
            online: parsed.players.online,
            max: parsed.players.max,
        },
        version: Version {
            name: parsed.version.name,
            protocol: parsed
                .version
                .protocol
                .and_then(|protocol| i32::try_from(protocol).ok()),
        },
        elapsed: Duration::ZERO,
        details: Details::Java(JavaDetails {
            raw_json,
            srv: None,
        }),
    })
}

/// Typed Java status JSON used for the single structural parse.
#[derive(Deserialize)]
pub(crate) struct JavaJson {
    #[serde(default)]
    pub(crate) description: serde_json::Value,
    #[serde(default)]
    pub(crate) players: JavaPlayers,
    #[serde(default)]
    pub(crate) version: JavaVersion,
}

#[derive(Deserialize, Default)]
pub(crate) struct JavaPlayers {
    pub(crate) online: Option<u32>,
    pub(crate) max: Option<u32>,
}

#[derive(Deserialize, Default)]
pub(crate) struct JavaVersion {
    pub(crate) name: Option<String>,
    pub(crate) protocol: Option<i64>,
}

/// Flatten a Java description component into plain text.
///
/// Handles plain strings, text components (`text` plus nested `extra` arrays
/// and `translate` fallbacks), and arrays of components. Anything else is
/// rendered as its JSON form; there is no legacy color-code stripping.
fn describe(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(components) => components.iter().map(describe).collect::<String>(),
        serde_json::Value::Object(object) => {
            let mut text = object
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map_or_else(String::new, ToString::to_string);
            if text.is_empty() {
                text.push_str(
                    object
                        .get("translate")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                );
            }
            if let Some(extra) = object.get("extra").and_then(serde_json::Value::as_array) {
                for component in extra {
                    text.push_str(&describe(component));
                }
            }
            text
        }
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::Ipv4Addr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn srv_rules() {
        let resolved = ResolvedOptions {
            edition: None,
            java_port: 25_565,
            bedrock_port: 19_132,
            timeout: crate::client::DEFAULT_TIMEOUT,
            try_srv: true,
            allow_insecure_http: false,
            max_response_bytes: 64 * 1024,
            max_java_status_bytes: crate::client::DEFAULT_MAX_JAVA_STATUS_BYTES,
        };
        let mut address = crate::address::parse_server_address("play.example.com", 25_565, 19_132)
            .expect("address should parse");
        assert!(srv_applicable(&address, &resolved));

        // An explicit port pins the endpoint: no SRV.
        address = crate::address::parse_server_address("play.example.com:25566", 25_565, 19_132)
            .expect("address should parse");
        assert!(!srv_applicable(&address, &resolved));

        // IP literals never have SRV records.
        address = crate::address::parse_server_address("127.0.0.1", 25_565, 19_132)
            .expect("address should parse");
        assert!(!srv_applicable(&address, &resolved));

        // The option can disable SRV entirely.
        address = crate::address::parse_server_address("play.example.com", 25_565, 19_132)
            .expect("address should parse");
        let disabled = ResolvedOptions {
            try_srv: false,
            ..resolved.clone()
        };
        assert!(!srv_applicable(&address, &disabled));
    }

    #[test]
    fn weighted_srv_order_is_strict_rfc_2782() {
        // A record with a large weight is always picked before zero-weight
        // records, whose empty range only yields once nothing else remains.
        let records = vec![
            SrvTarget {
                host: "heavy.example.com".into(),
                port: 25_565,
                priority: 0,
                weight: 100,
            },
            SrvTarget {
                host: "light.example.com".into(),
                port: 25_565,
                priority: 0,
                weight: 0,
            },
        ];
        let ordered = weighted_srv_order(records);
        assert_eq!(ordered[0].host, "heavy.example.com");

        // All-zero weights keep a deterministic order.
        let records = vec![
            SrvTarget {
                host: "a.example.com".into(),
                port: 25_565,
                priority: 0,
                weight: 0,
            },
            SrvTarget {
                host: "b.example.com".into(),
                port: 25_565,
                priority: 0,
                weight: 0,
            },
        ];
        let ordered = weighted_srv_order(records);
        assert_eq!(ordered[0].host, "a.example.com");
        assert_eq!(ordered[1].host, "b.example.com");
    }

    #[test]
    fn describe_flattens_nested_components() {
        let json = r#"{"description":{"text":"Hello ","extra":[{"text":"World"},{"translate":"key.menu"}]},"players":{"online":1,"max":20},"version":{"name":"1.21.4","protocol":763}}"#;
        let parsed = serde_json::from_str::<JavaJson>(json).expect("json should parse");
        let motd = describe(&parsed.description);
        let players = Players {
            online: parsed.players.online,
            max: parsed.players.max,
        };
        let version = Version {
            name: parsed.version.name,
            protocol: parsed
                .version
                .protocol
                .and_then(|protocol| i32::try_from(protocol).ok()),
        };

        assert_eq!(motd, "Hello Worldkey.menu");
        assert_eq!(players.online, Some(1));
        assert_eq!(players.max, Some(20));
        assert_eq!(version.name.as_deref(), Some("1.21.4"));
        assert_eq!(version.protocol, Some(763));
    }

    #[tokio::test]
    async fn invalid_json_fails_the_query() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client should connect");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;

            // Well-formed status frame carrying garbage JSON.
            let body = b"this is not json";
            let mut packet = vec![0x00];
            packet.push(u8::try_from(body.len()).expect("length fits in u8"));
            packet.extend_from_slice(body);
            let mut frame = vec![u8::try_from(packet.len()).expect("length fits in u8")];
            frame.extend_from_slice(&packet);
            let _ = stream.write_all(&frame).await;
        });

        let error = query(format!("127.0.0.1:{}", address.port()))
            .await
            .expect_err("invalid JSON should fail the query");

        // Automatic probing wraps the probe failure; the inner error must be
        // the invalid-packet failure.
        let Error::NoCompatibleJavaServer { attempts, .. } = &error else {
            panic!("expected a Java aggregate error, got {error:?}");
        };
        assert!(
            matches!(
                attempts[0].error.as_ref(),
                Error::InvalidPacket("Java status JSON is not a valid status object")
            ),
            "inner failure should be the invalid JSON, got {:?}",
            attempts[0].error
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[tokio::test]
    async fn structurally_invalid_json_fails_the_query() {
        // `[]` is valid JSON but not a status object: it must fail rather
        // than produce a blank status.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client should connect");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;

            let body = b"[]";
            let mut packet = vec![0x00];
            packet.push(u8::try_from(body.len()).expect("length fits in u8"));
            packet.extend_from_slice(body);
            let mut frame = vec![u8::try_from(packet.len()).expect("length fits in u8")];
            frame.extend_from_slice(&packet);
            let _ = stream.write_all(&frame).await;
        });

        let error = query(format!("127.0.0.1:{}", address.port()))
            .await
            .expect_err("structural mismatch should fail the query");

        let Error::NoCompatibleJavaServer { attempts, .. } = &error else {
            panic!("expected a Java aggregate error, got {error:?}");
        };
        assert!(
            matches!(
                attempts[0].error.as_ref(),
                Error::InvalidPacket("Java status JSON is not a valid status object")
            ),
            "inner failure should be the structural mismatch, got {:?}",
            attempts[0].error
        );
    }

    #[tokio::test]
    async fn explicit_port_status_keeps_the_requested_address() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client should connect");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;

            let json = r#"{"description":"Mock","players":{"online":1,"max":20},"version":{"name":"1.21.4","protocol":763}}"#;
            let body = json.as_bytes();
            let mut packet = vec![0x00];
            packet.push(u8::try_from(body.len()).expect("length fits in u8"));
            packet.extend_from_slice(body);
            let mut frame = vec![u8::try_from(packet.len()).expect("length fits in u8")];
            frame.extend_from_slice(&packet);
            let _ = stream.write_all(&frame).await;
        });

        let status = query(format!("127.0.0.1:{}", address.port()))
            .await
            .expect("mock java server should answer");

        // The requested address (including the explicit port) is preserved.
        assert_eq!(status.address.host, "127.0.0.1");
        assert_eq!(status.address.java_port, address.port());
        assert_eq!(status.address.explicit_port, Some(address.port()));
        assert_eq!(status.motd, "Mock");
        assert_eq!(status.players.online, Some(1));
        assert_eq!(status.version.protocol, Some(763));
    }
}
