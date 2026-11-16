//! Small in-memory, expiry-aware stream cache keyed by (track, quality).
use crate::models::{StreamInfo, StreamQuality, TrackId};
use std::{
    collections::HashMap,
    sync::RwLock,
    time::{Duration, Instant},
};

struct Entry {
    value: StreamInfo,
    expires_at: Instant,
}
pub struct StreamCache {
    entries: RwLock<HashMap<(TrackId, StreamQuality), Entry>>,
    ttl: Duration,
    capacity: usize,
}
impl StreamCache {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
            capacity: capacity.max(1),
        }
    }
    pub fn get(&self, id: &TrackId, quality: StreamQuality) -> Option<StreamInfo> {
        let key = (id.clone(), quality);
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        match entries.get(&key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.value.clone()),
            Some(_) => {
                entries.remove(&key);
                None
            }
            None => None,
        }
    }
    pub fn insert(&self, id: TrackId, quality: StreamQuality, value: StreamInfo) {
        let now = Instant::now();
        let expiry = match value.expires_at_unix {
            Some(unix) => {
                let absolute =
                    std::time::SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(unix));
                match absolute
                    .and_then(|time| time.duration_since(std::time::SystemTime::now()).ok())
                {
                    Some(remaining) => now + remaining.min(self.ttl),
                    None => now,
                }
            }
            None => now + self.ttl,
        };
        let key = (id.clone(), quality);
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| entry.expires_at > now);
        if entries.len() >= self.capacity
            && !entries.contains_key(&key)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            Entry {
                value,
                expires_at: expiry,
            },
        );
    }
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    pub fn len(&self) -> usize {
        self.entries.read().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AudioFormat, StreamQuality};
    fn sample_stream(expires_in_secs: Option<u64>) -> StreamInfo {
        let expires_at_unix = expires_in_secs.map(|secs| {
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .expect("clock before unix epoch")
                .as_secs()
                + secs
        });
        StreamInfo {
            url: "https://example.invalid/audio".into(),
            quality: StreamQuality::High,
            format: AudioFormat::Mp3,
            duration: None,
            expires_at_unix,
            content_length: None,
            checksum: None,
            headers: vec![],
            fallback_urls: vec![],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
        }
    }
    #[test]
    fn expired_stream_urls_are_not_cached() {
        let cache = StreamCache::new(Duration::from_secs(300), 8);
        let stream = StreamInfo {
            url: "https://example.invalid/audio".into(),
            quality: StreamQuality::High,
            format: AudioFormat::Mp3,
            duration: None,
            expires_at_unix: Some(0),
            content_length: None,
            checksum: None,
            headers: vec![],
            fallback_urls: vec![],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
        };
        let id = TrackId::new("test", "1");
        cache.insert(id.clone(), StreamQuality::High, stream);
        assert!(cache.get(&id, StreamQuality::High).is_none());
    }
    #[test]
    fn different_qualities_do_not_share_entries() {
        let cache = StreamCache::new(Duration::from_secs(60), 8);
        let id = TrackId::new("test", "q");
        cache.insert(id.clone(), StreamQuality::High, sample_stream(Some(60)));
        assert!(cache.get(&id, StreamQuality::High).is_some());
        assert!(cache.get(&id, StreamQuality::Lossless).is_none());
        cache.insert(id.clone(), StreamQuality::Lossless, sample_stream(Some(60)));
        assert_eq!(cache.len(), 2);
    }
    #[test]
    fn capacity_evicts_the_earliest_expiring_entry() {
        let cache = StreamCache::new(Duration::from_secs(300), 2);
        let a = TrackId::new("test", "a");
        let b = TrackId::new("test", "b");
        let c = TrackId::new("test", "c");
        let q = StreamQuality::High;
        cache.insert(a.clone(), q, sample_stream(Some(10)));
        cache.insert(b.clone(), q, sample_stream(Some(20)));
        cache.insert(c.clone(), q, sample_stream(Some(30)));
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&a, q).is_none());
        assert!(cache.get(&b, q).is_some());
        assert!(cache.get(&c, q).is_some());
    }
    #[test]
    fn reinserting_the_same_id_does_not_evict_itself() {
        let cache = StreamCache::new(Duration::from_secs(300), 1);
        let id = TrackId::new("test", "a");
        let q = StreamQuality::High;
        cache.insert(id.clone(), q, sample_stream(Some(10)));
        cache.insert(id.clone(), q, sample_stream(Some(20)));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&id, q).is_some());
    }
    #[test]
    fn entries_without_url_expiry_live_for_the_full_ttl() {
        let cache = StreamCache::new(Duration::from_millis(50), 4);
        let id = TrackId::new("test", "ttl");
        cache.insert(id.clone(), StreamQuality::High, sample_stream(None));
        assert!(cache.get(&id, StreamQuality::High).is_some());
        std::thread::sleep(Duration::from_millis(120));
        assert!(cache.get(&id, StreamQuality::High).is_none());
        assert!(cache.is_empty());
    }
    #[test]
    fn url_expiry_far_in_the_future_is_capped_by_ttl() {
        let cache = StreamCache::new(Duration::from_millis(60), 4);
        let id = TrackId::new("test", "cap");
        cache.insert(id.clone(), StreamQuality::High, sample_stream(Some(3_600)));
        assert!(cache.get(&id, StreamQuality::High).is_some());
        std::thread::sleep(Duration::from_millis(130));
        assert!(cache.get(&id, StreamQuality::High).is_none());
    }
    #[test]
    fn clear_removes_all_entries() {
        let cache = StreamCache::new(Duration::from_secs(60), 4);
        let id = TrackId::new("test", "x");
        cache.insert(id.clone(), StreamQuality::High, sample_stream(Some(60)));
        cache.clear();
        assert!(cache.is_empty());
        assert!(cache.get(&id, StreamQuality::High).is_none());
    }
}
