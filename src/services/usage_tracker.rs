//! Usage tracker service: aggregates per-request token usage into
//! (cookie x model x time-bucket) counters and persists them as JSON next to
//! the config file. Inspired by gproxy's usage_rollups, kept file-based and
//! dependency-free for personal use.
//!
//! Layout:
//! - hourly buckets, kept for `HOURLY_RETENTION_DAYS`
//! - older data is rolled up into daily buckets (kept forever)
//!
//! Recording is fire-and-forget over an unbounded channel; the store is
//! flushed to disk with a debounce.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::OnceLock,
};

use clewdr_types::TokenUsage;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

use crate::config::{CLEWDR_CONFIG, CONFIG_PATH};

const HOURLY_RETENTION_DAYS: i64 = 90;
const FLUSH_INTERVAL_SECS: u64 = 20;
const STATS_FILE: &str = "usage_stats.json";

/// One usage event, reported from a chat response path.
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub cookie: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// True when token counts were locally estimated (claude.ai web path)
    pub estimated: bool,
}

/// bucket start -> cookie -> model -> usage
type Buckets = BTreeMap<i64, HashMap<String, HashMap<String, TokenUsage>>>;

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct UsageStore {
    #[serde(default)]
    pub hourly: Buckets,
    #[serde(default)]
    pub daily: Buckets,
}

impl UsageStore {
    fn record(&mut self, r: &UsageRecord, now: i64) {
        let hour = now - now.rem_euclid(3600);
        let usage = self
            .hourly
            .entry(hour)
            .or_default()
            .entry(r.cookie.clone())
            .or_default()
            .entry(r.model.clone())
            .or_default();
        usage.add(&TokenUsage {
            requests: 1,
            input: r.input,
            output: r.output,
            cache_read: r.cache_read,
            cache_write: r.cache_write,
            estimated_requests: r.estimated as u64,
        });
    }

    /// Roll hourly buckets older than the retention window into daily buckets.
    fn rollup(&mut self, now: i64) {
        let cutoff = now - HOURLY_RETENTION_DAYS * 86400;
        let expired: Vec<i64> = self.hourly.range(..cutoff).map(|(k, _)| *k).collect();
        for hour in expired {
            if let Some(cookies) = self.hourly.remove(&hour) {
                let day = hour - hour.rem_euclid(86400);
                let day_entry = self.daily.entry(day).or_default();
                for (cookie, models) in cookies {
                    let cookie_entry = day_entry.entry(cookie).or_default();
                    for (model, usage) in models {
                        cookie_entry.entry(model).or_default().add(&usage);
                    }
                }
            }
        }
    }
}

enum Msg {
    Record(UsageRecord),
    Snapshot(oneshot::Sender<UsageStore>),
}

#[derive(Clone)]
pub struct UsageTrackerHandle {
    tx: mpsc::UnboundedSender<Msg>,
}

impl UsageTrackerHandle {
    /// Fire-and-forget usage recording.
    pub fn record(&self, record: UsageRecord) {
        if record.input == 0
            && record.output == 0
            && record.cache_read == 0
            && record.cache_write == 0
        {
            return;
        }
        let _ = self.tx.send(Msg::Record(record));
    }

    /// Get a full snapshot of the store (for API queries).
    pub async fn snapshot(&self) -> UsageStore {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Snapshot(tx)).is_err() {
            return UsageStore::default();
        }
        rx.await.unwrap_or_default()
    }
}

fn stats_path() -> PathBuf {
    CONFIG_PATH
        .parent()
        .map(|p| p.join(STATS_FILE))
        .unwrap_or_else(|| PathBuf::from(STATS_FILE))
}

async fn load_store() -> UsageStore {
    let path = stats_path();
    match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice::<UsageStore>(&bytes) {
            Ok(store) => {
                info!("Usage stats loaded from {}", path.display());
                store
            }
            Err(e) => {
                warn!("Failed to parse {}: {}, starting fresh", path.display(), e);
                UsageStore::default()
            }
        },
        Err(_) => UsageStore::default(),
    }
}

async fn save_store(store: &UsageStore) {
    if CLEWDR_CONFIG.load().no_fs {
        return;
    }
    let path = stats_path();
    match serde_json::to_vec(store) {
        Ok(bytes) => {
            if let Err(e) = tokio::fs::write(&path, bytes).await {
                error!("Failed to write {}: {}", path.display(), e);
            }
        }
        Err(e) => error!("Failed to serialize usage stats: {}", e),
    }
}

async fn run(mut rx: mpsc::UnboundedReceiver<Msg>) {
    let mut store = load_store().await;
    let mut dirty = false;
    let mut flush = tokio::time::interval(std::time::Duration::from_secs(FLUSH_INTERVAL_SECS));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    Msg::Record(record) => {
                        let now = chrono::Utc::now().timestamp();
                        store.record(&record, now);
                        // Trigger a background pricing fetch if this model is unknown
                        crate::services::pricing::ensure_model_known(&record.model);
                        dirty = true;
                    }
                    Msg::Snapshot(tx) => {
                        let _ = tx.send(store.clone());
                    }
                }
            }
            _ = flush.tick() => {
                if dirty {
                    store.rollup(chrono::Utc::now().timestamp());
                    save_store(&store).await;
                    dirty = false;
                }
            }
        }
    }
    if dirty {
        save_store(&store).await;
    }
}

static TRACKER: OnceLock<UsageTrackerHandle> = OnceLock::new();

/// Global usage tracker handle. Spawns the tracker task on first use
/// (must be called from within a tokio runtime).
pub fn usage_tracker() -> &'static UsageTrackerHandle {
    TRACKER.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(rx));
        UsageTrackerHandle { tx }
    })
}
