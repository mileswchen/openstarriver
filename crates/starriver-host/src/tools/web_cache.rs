//! Rendered-result cache for web_fetch: an in-memory LRU keyed by the SHA-256
//! of the normalized request URL, with a TTL. Successful (2xx) full renders
//! are cached BEFORE per-request char truncation, so the same document serves
//! every `max_chars` budget. No I/O; disk persistence (a `~/.starriver/cache/
//! web_fetch` directory with a longer TTL) is a follow-up described in the
//! design doc.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Default entry count before LRU eviction.
pub const DEFAULT_CACHE_CAPACITY: usize = 32;
/// Default freshness window for a cached render.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(600);

/// A successfully rendered response, ready for per-request truncation.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedRender {
    /// The model-facing prefix (`HTTP {status}\n\n{rendered body}`) before
    /// the output char cap is applied.
    pub prefix: String,
    /// Whether the 2 MiB byte cap cut the source body (forces the
    /// truncation footer regardless of the char budget).
    pub truncated_bytes: bool,
}

/// Canonical cache identity of a request URL: lowercase scheme/host, default
/// port dropped, fragment dropped; path and query preserved. `None` for URLs
/// that are not http(s) or carry no host.
pub fn normalize_url(url: &str) -> Option<String> {
    let mut parsed = reqwest::Url::parse(url).ok()?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    parsed.set_fragment(None);
    // `Url::set_host` resets an explicit port, so capture it first and restore
    // it afterwards (unless it is the scheme default).
    let explicit_port = parsed.port();
    let host = parsed.host_str()?.to_ascii_lowercase();
    let _ = parsed.set_host(Some(&host));
    if parsed.port().is_none() {
        if let Some(port) = explicit_port.filter(|p| Some(*p) != parsed.port_or_known_default()) {
            let _ = parsed.set_port(Some(port));
        }
    }
    Some(parsed.to_string())
}

/// Cache key for a normalized URL.
pub fn cache_key(normalized_url: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(normalized_url.as_bytes());
    hasher.finalize().into()
}

#[derive(Debug, Default)]
struct Inner {
    map: HashMap<[u8; 32], (Instant, CachedRender)>,
    /// LRU order: front = most recently used.
    order: VecDeque<[u8; 32]>,
}

/// Thread-safe LRU + TTL cache of rendered fetch results.
#[derive(Debug)]
pub struct RenderCache {
    inner: std::sync::Mutex<Inner>,
    capacity: usize,
    ttl: Duration,
}

impl RenderCache {
    /// A cache holding at most `capacity` fresh-for-`ttl` renders.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        assert!(capacity > 0, "cache capacity must be positive");
        Self {
            inner: std::sync::Mutex::new(Inner::default()),
            capacity,
            ttl,
        }
    }

    /// The default cache (capacity [`DEFAULT_CACHE_CAPACITY`], TTL
    /// [`DEFAULT_CACHE_TTL`]).
    pub fn default_cache() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY, DEFAULT_CACHE_TTL)
    }

    fn drop_key(inner: &mut Inner, key: &[u8; 32]) {
        inner.map.remove(key);
        inner.order.retain(|existing| existing != key);
    }

    /// Fetch a still-fresh render, refreshing its LRU position on hit.
    pub fn get(&self, key: &[u8; 32]) -> Option<CachedRender> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.map.get(key) {
            Some((stored_at, cached)) if stored_at.elapsed() < self.ttl => {
                let cached = cached.clone();
                Self::drop_key(&mut inner, key);
                inner.order.push_front(*key);
                inner.map.insert(*key, (Instant::now(), cached.clone()));
                Some(cached)
            }
            Some(_) => {
                Self::drop_key(&mut inner, key);
                None
            }
            None => None,
        }
    }

    /// Insert (or refresh) a render, evicting expired and least-recently-used
    /// entries beyond the capacity.
    pub fn put(&self, key: &[u8; 32], render: CachedRender) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Opportunistic sweep of expired entries keeps long-running caches
        // from pinning dead keys.
        let expired: Vec<[u8; 32]> = inner
            .map
            .iter()
            .filter(|(_, (stored_at, _))| stored_at.elapsed() >= self.ttl)
            .map(|(key, _)| *key)
            .collect();
        for key in expired {
            Self::drop_key(&mut inner, &key);
        }
        Self::drop_key(&mut inner, key);
        inner.order.push_front(*key);
        inner.map.insert(*key, (Instant::now(), render));
        while inner.map.len() > self.capacity {
            match inner.order.pop_back() {
                Some(evicted) => {
                    inner.map.remove(&evicted);
                }
                None => break,
            }
        }
    }

    /// Number of entries currently stored (fresh and expired alike).
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }

    /// Whether the cache stores no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(prefix: &str) -> CachedRender {
        CachedRender {
            prefix: prefix.into(),
            truncated_bytes: false,
        }
    }

    #[test]
    fn normalizes_scheme_host_port_and_fragment() {
        assert_eq!(
            normalize_url("HTTP://Example.COM:80/a?b=1#frag").as_deref(),
            Some("http://example.com/a?b=1")
        );
        assert_eq!(
            normalize_url("https://example.com:443/").as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(
            normalize_url("https://example.com:8443/x").as_deref(),
            Some("https://example.com:8443/x")
        );
        assert_eq!(normalize_url("ftp://example.com/x"), None);
        assert_eq!(normalize_url("not a url"), None);
    }

    #[test]
    fn normalization_folds_case_and_default_ports_into_shared_key() {
        let a = cache_key(&normalize_url("http://EXAMPLE.com:80/doc").unwrap());
        let b = cache_key(&normalize_url("http://example.com/doc#anchor").unwrap());
        assert_eq!(a, b);
        let c = cache_key(&normalize_url("http://example.com:8080/doc").unwrap());
        assert_ne!(a, c);
    }

    #[test]
    fn hit_miss_expiry_and_lru_eviction() {
        let cache = RenderCache::new(2, Duration::from_millis(50));
        let k1 = cache_key("http://one.test/a");
        let k2 = cache_key("http://two.test/a");
        let k3 = cache_key("http://three.test/a");
        assert!(cache.get(&k1).is_none());
        cache.put(&k1, render("one"));
        assert_eq!(cache.get(&k1).unwrap().prefix, "one");
        // Capacity 2: inserting two more keys evicts the LRU (k1).
        cache.put(&k2, render("two"));
        cache.get(&k2); // refresh k2 as most recent
        cache.put(&k3, render("three"));
        assert!(cache.get(&k1).is_none(), "LRU entry must be evicted");
        assert!(cache.get(&k2).is_some());
        assert!(cache.get(&k3).is_some());
        assert_eq!(cache.len(), 2);
        // TTL: entries expire after 50ms (querying each drops it).
        std::thread::sleep(Duration::from_millis(60));
        assert!(cache.get(&k2).is_none());
        assert!(cache.get(&k3).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn put_refreshes_an_existing_key_in_place() {
        let cache = RenderCache::new(2, Duration::from_secs(60));
        let k = cache_key("http://same.test/a");
        cache.put(&k, render("old"));
        cache.put(&k, render("new"));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&k).unwrap().prefix, "new");
    }
}
