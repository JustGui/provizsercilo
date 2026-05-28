use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};

struct SearchEvent {
    at: Instant,
    provider: String,
    is_error: bool,
    duration_ms: u64,
}

#[derive(Clone, Default)]
pub struct StatsTracker {
    events: Arc<Mutex<VecDeque<SearchEvent>>>,
}

impl StatsTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_search(&self, provider: &str, is_error: bool, duration_ms: u64) {
        let mut events = self.events.lock().unwrap();
        events.push_back(SearchEvent {
            at: Instant::now(),
            provider: provider.to_string(),
            is_error,
            duration_ms,
        });
        // Prune entries older than 24h to prevent unbounded growth.
        let cutoff = Instant::now() - std::time::Duration::from_secs(86400);
        while events.front().is_some_and(|e| e.at < cutoff) {
            events.pop_front();
        }
    }

    pub fn window_stats(&self, window_secs: u64) -> WindowStats {
        let events = self.events.lock().unwrap();
        let cutoff = Instant::now() - std::time::Duration::from_secs(window_secs);
        let mut searches = 0i64;
        let mut errors = 0i64;
        for e in events.iter().filter(|e| e.at >= cutoff) {
            searches += 1;
            if e.is_error {
                errors += 1;
            }
        }
        WindowStats { searches, errors }
    }

    pub fn by_provider_stats(&self, window_secs: u64) -> Vec<ProviderStats> {
        let events = self.events.lock().unwrap();
        let cutoff = Instant::now() - std::time::Duration::from_secs(window_secs);

        let mut map: std::collections::HashMap<String, (i64, i64, i64, i64)> =
            std::collections::HashMap::new();

        for e in events.iter().filter(|e| e.at >= cutoff) {
            let entry = map.entry(e.provider.clone()).or_default();
            entry.0 += 1; // searches
            if e.is_error {
                entry.1 += 1; // errors
            }
            entry.2 += e.duration_ms as i64; // total latency
            entry.3 += 1; // count for avg
        }

        map.into_iter()
            .map(|(slug, (searches, errors, total_lat, cnt))| ProviderStats {
                slug,
                searches,
                errors,
                avg_latency_ms: if cnt > 0 { Some(total_lat / cnt) } else { None },
            })
            .collect()
    }
}

#[derive(Debug, serde::Serialize)]
pub struct WindowStats {
    pub searches: i64,
    pub errors: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct ProviderStats {
    pub slug: String,
    pub searches: i64,
    pub errors: i64,
    pub avg_latency_ms: Option<i64>,
}
