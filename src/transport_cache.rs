//! Process-wide cache of successful Bedrock transport types.
//!
//! Automatic probing remembers which transport (`NetherNet` HTTP or legacy
//! `RakNet` UDP) last answered for a `host:port` pair so later queries skip the
//! failing probe instead of paying for it on every call. Only successful
//! transport answers are cached; entries expire after ten minutes so servers
//! that switch transports are picked up again automatically.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::Transport;

/// How long a cached transport answer stays valid.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Maximum number of cached `host:port` entries.
const CACHE_CAPACITY: usize = 1024;

struct Entry {
    transport: Transport,
    expires_at: Instant,
}

static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Build the cache key for a Bedrock server endpoint.
#[must_use]
pub(crate) fn cache_key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.to_ascii_lowercase())
}

/// Return the cached transport for a host and port, if present and fresh.
///
/// This is a read-only view of the probe cache; writes happen only inside
/// automatic probing after a successful response.
#[must_use]
pub fn cached_transport(host: &str, port: u16) -> Option<Transport> {
    let key = cache_key(host, port);
    let mut map = cache().lock().ok()?;
    let entry = map.get(&key)?;
    if Instant::now() >= entry.expires_at {
        map.remove(&key);
        return None;
    }
    Some(entry.transport)
}

/// Store the transport that last answered successfully for a host and port.
///
/// Storing refreshes the entry expiry. The map is pruned when it grows beyond
/// [`CACHE_CAPACITY`]: expired entries first, then the entries closest to
/// expiry.
pub(crate) fn store_transport(host: &str, port: u16, transport: Transport) {
    let key = cache_key(host, port);
    let Ok(mut map) = cache().lock() else {
        return;
    };
    insert_with_capacity(
        &mut map,
        key,
        transport,
        Instant::now(),
        CACHE_TTL,
        CACHE_CAPACITY,
    );
}

/// Clear every cached transport answer.
pub fn clear_transport_cache() {
    if let Ok(mut map) = cache().lock() {
        map.clear();
    }
}

fn insert_with_capacity(
    map: &mut HashMap<String, Entry>,
    key: String,
    transport: Transport,
    now: Instant,
    ttl: Duration,
    capacity: usize,
) {
    if map.len() >= capacity && !map.contains_key(&key) {
        map.retain(|_, entry| entry.expires_at > now);
        while map.len() >= capacity {
            let Some(oldest) = map
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            map.remove(&oldest);
        }
    }
    map.insert(
        key,
        Entry {
            transport,
            expires_at: now + ttl,
        },
    );
}

/// Serializes tests that depend on the shared process-wide cache. A tokio
/// mutex is used so guards can be held across awaits in async tests without
/// poisoning.
#[cfg(test)]
pub(crate) static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_are_case_insensitive() {
        assert_eq!(
            cache_key("Play.Example.COM", 19_132),
            cache_key("play.example.com", 19_132)
        );
        assert_eq!(
            cache_key("Play.Example.COM", 19_132),
            "play.example.com:19132"
        );
    }

    #[tokio::test]
    async fn stored_transport_is_readable() {
        let _guard = TEST_LOCK.lock().await;
        store_transport("cache-read.test", 19_132, Transport::NetherNetHttp);

        assert_eq!(
            cached_transport("cache-read.test", 19_132),
            Some(Transport::NetherNetHttp)
        );
        assert_eq!(
            cached_transport("CACHE-READ.test", 19_132),
            Some(Transport::NetherNetHttp)
        );
    }

    #[test]
    fn unknown_hosts_are_not_cached() {
        assert_eq!(cached_transport("cache-missing.test", 19_132), None);
    }

    #[tokio::test]
    async fn expired_entries_are_dropped_on_lookup() {
        let _guard = TEST_LOCK.lock().await;
        // Insert an already-expired entry directly so the TTL does not have to
        // be waited out; the key is unique to this test.
        let key = cache_key("cache-expired.test", 19_132);
        let entry = Entry {
            transport: Transport::RakNetUdp,
            expires_at: Instant::now(),
        };
        {
            let mut map = cache().lock().expect("cache should not be poisoned");
            map.insert(key, entry);
        }

        assert_eq!(cached_transport("cache-expired.test", 19_132), None);
        assert!(
            !cache()
                .lock()
                .expect("cache should not be poisoned")
                .contains_key(&cache_key("cache-expired.test", 19_132))
        );
    }

    #[test]
    fn capacity_is_enforced_by_evicting_oldest() {
        let mut map = HashMap::new();
        let now = Instant::now();
        for index in 0..4_u64 {
            insert_with_capacity(
                &mut map,
                format!("evict.test:{index}"),
                Transport::RakNetUdp,
                now,
                Duration::from_secs(index),
                4,
            );
        }
        assert_eq!(map.len(), 4);

        // A fifth entry evicts the one closest to expiry.
        insert_with_capacity(
            &mut map,
            "evict.test:4".to_string(),
            Transport::NetherNetHttp,
            now,
            Duration::from_secs(10),
            4,
        );

        assert_eq!(map.len(), 4);
        assert!(map.contains_key("evict.test:4"));
        assert!(!map.contains_key("evict.test:0"));
    }

    #[tokio::test]
    async fn clear_transport_cache_empties_entries() {
        let _guard = TEST_LOCK.lock().await;
        store_transport("cache-clear.test", 19_132, Transport::RakNetUdp);
        assert!(cached_transport("cache-clear.test", 19_132).is_some());

        clear_transport_cache();

        assert_eq!(cached_transport("cache-clear.test", 19_132), None);
    }
}
