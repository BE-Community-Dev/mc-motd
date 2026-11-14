//! Per-client cache of successful Bedrock transport types.
//!
//! Automatic probing remembers which transport (`NetherNet` HTTP or legacy
//! `RakNet` UDP) last answered for a `host:port` pair so later queries skip
//! the failing probe instead of paying for it on every call. Only successful
//! transport answers are cached; entries expire after ten minutes so servers
//! that switch transports are picked up again automatically. Every
//! [`crate::Client`] owns its own cache, so different clients can use
//! different strategies without interfering.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::status::Transport;

/// How long a cached transport answer stays valid.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
struct Entry {
    transport: Transport,
    expires_at: Instant,
}

/// Bounded cache of the transport that last answered for a `host:port` pair.
#[derive(Debug)]
pub struct TransportCache {
    map: Mutex<HashMap<String, Entry>>,
    capacity: usize,
}

impl TransportCache {
    /// Create a cache holding up to `capacity` entries; zero disables caching.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// Build the cache key for a Bedrock server endpoint.
    fn cache_key(host: &str, port: u16) -> String {
        format!("{}:{port}", host.to_ascii_lowercase())
    }

    /// Return the cached transport for a host and port, if present and fresh.
    #[must_use]
    pub fn cached(&self, host: &str, port: u16) -> Option<Transport> {
        if self.capacity == 0 {
            return None;
        }
        let key = Self::cache_key(host, port);
        let mut map = self.map.lock().ok()?;
        let entry = map.get(&key)?;
        if Instant::now() >= entry.expires_at {
            map.remove(&key);
            return None;
        }
        Some(entry.transport)
    }

    /// Store the transport that last answered successfully for a host and
    /// port, refreshing the entry expiry.
    pub(crate) fn store(&self, host: &str, port: u16, transport: Transport) {
        if self.capacity == 0 {
            return;
        }
        let key = Self::cache_key(host, port);
        let Ok(mut map) = self.map.lock() else {
            return;
        };
        let now = Instant::now();
        if map.len() >= self.capacity && !map.contains_key(&key) {
            map.retain(|_, entry| entry.expires_at > now);
            while map.len() >= self.capacity {
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
                expires_at: now + CACHE_TTL,
            },
        );
    }

    /// Clear every cached transport answer.
    pub fn clear(&self) {
        if let Ok(mut map) = self.map.lock() {
            map.clear();
        }
    }
}

#[cfg(all(test, feature = "bedrock"))]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_are_case_insensitive() {
        assert_eq!(
            TransportCache::cache_key("Play.Example.COM", 19_132),
            TransportCache::cache_key("play.example.com", 19_132)
        );
        assert_eq!(
            TransportCache::cache_key("Play.Example.COM", 19_132),
            "play.example.com:19132"
        );
    }

    #[test]
    fn stores_and_reads_transports() {
        let cache = TransportCache::new(16);
        cache.store("cache-read.test", 19_132, Transport::RakNetUdp);

        assert_eq!(
            cache.cached("cache-read.test", 19_132),
            Some(Transport::RakNetUdp)
        );
        assert_eq!(
            cache.cached("CACHE-READ.test", 19_132),
            Some(Transport::RakNetUdp)
        );
        assert_eq!(cache.cached("cache-missing.test", 19_132), None);
    }

    #[test]
    fn zero_capacity_disables_caching() {
        let cache = TransportCache::new(0);
        cache.store("cache-off.test", 19_132, Transport::RakNetUdp);

        assert_eq!(cache.cached("cache-off.test", 19_132), None);
    }

    #[test]
    fn clear_empties_entries() {
        let cache = TransportCache::new(16);
        cache.store("cache-clear.test", 19_132, Transport::RakNetUdp);
        assert!(cache.cached("cache-clear.test", 19_132).is_some());

        cache.clear();

        assert_eq!(cache.cached("cache-clear.test", 19_132), None);
    }

    #[test]
    fn capacity_is_enforced_by_evicting_oldest() {
        let cache = TransportCache::new(4);
        for index in 0..4_u64 {
            let key = format!("evict-{index}.test:19132");
            cache
                .map
                .lock()
                .expect("cache should not be poisoned")
                .insert(
                    key,
                    Entry {
                        transport: Transport::RakNetUdp,
                        expires_at: Instant::now() + Duration::from_secs(index),
                    },
                );
        }
        assert_eq!(
            cache
                .map
                .lock()
                .expect("cache should not be poisoned")
                .len(),
            4
        );

        // A fifth entry evicts the one closest to expiry.
        cache.store("evict-4.test", 19_132, Transport::RakNetUdp);

        let map = cache.map.lock().expect("cache should not be poisoned");
        assert_eq!(map.len(), 4);
        assert!(map.contains_key("evict-4.test:19132"));
        assert!(!map.contains_key("evict-0.test:19132"));
    }
}
