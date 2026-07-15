//! Model pricing resolution with three layers (highest priority first):
//! 1. `[[price_overrides]]` in clewdr.toml (manual override / hotfix)
//! 2. LiteLLM price catalog, fetched on demand and cached to
//!    `pricing_cache.json` next to the config file
//! 3. Built-in per-family fallback table
//!
//! When an unknown model shows up, a background fetch of the LiteLLM catalog
//! is triggered (rate limited to once per hour). Once cached, no re-fetch
//! happens unless another unknown model appears.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        LazyLock, RwLock,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

use clewdr_types::ModelPricing;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::{CLEWDR_CONFIG, CONFIG_PATH};

const LITELLM_PRICES_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const CACHE_FILE: &str = "pricing_cache.json";
/// Minimum interval between catalog fetch attempts (seconds)
const FETCH_MIN_INTERVAL: i64 = 3600;

/// Manual pricing override, configured in clewdr.toml:
/// ```toml
/// [[price_overrides]]
/// model_match = "claude-opus-4-6"   # substring match
/// input = 5.0                        # USD per million tokens
/// output = 25.0
/// cache_read = 0.5
/// cache_write = 6.25
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PriceOverride {
    pub model_match: String,
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PricingCache {
    #[serde(default)]
    fetched_at: i64,
    /// model name -> USD per-million rates
    #[serde(default)]
    models: HashMap<String, ModelPricing>,
}

fn cache_path() -> PathBuf {
    CONFIG_PATH
        .parent()
        .map(|p| p.join(CACHE_FILE))
        .unwrap_or_else(|| PathBuf::from(CACHE_FILE))
}

static CACHE: LazyLock<RwLock<PricingCache>> = LazyLock::new(|| {
    let cache = std::fs::read(cache_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PricingCache>(&bytes).ok())
        .unwrap_or_default();
    if !cache.models.is_empty() {
        info!("Pricing cache loaded: {} models", cache.models.len());
    }
    RwLock::new(cache)
});

static LAST_FETCH_ATTEMPT: AtomicI64 = AtomicI64::new(0);
static FETCH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Normalize a model name for pricing lookup: strip clewdr-specific suffixes.
fn normalize(model: &str) -> String {
    let mut m = model.trim().to_ascii_lowercase();
    for suffix in ["-thinking", "-1m"] {
        if let Some(stripped) = m.strip_suffix(suffix) {
            m = stripped.to_string();
        }
    }
    m
}

/// Strip a trailing date like "-20250929" from a model name.
fn strip_date(model: &str) -> Option<String> {
    let (base, tail) = model.rsplit_once('-')?;
    (tail.len() == 8 && tail.starts_with("20") && tail.chars().all(|c| c.is_ascii_digit()))
        .then(|| base.to_string())
}

/// Built-in fallback rates (USD per million) by model family.
fn fallback_pricing(model: &str) -> ModelPricing {
    let m = normalize(model);
    let p = |input: f64, output: f64| ModelPricing {
        input,
        output,
        cache_read: input * 0.1,
        cache_write: input * 1.25,
    };
    if m.contains("fable") {
        p(10.0, 50.0)
    } else if m.contains("opus") {
        // Opus 4.5+ dropped to $5/$25; older Opus is $15/$75
        if ["4-5", "4-6", "4-7", "4-8", "4.5", "4.6", "4.7", "4.8"]
            .iter()
            .any(|v| m.contains(v))
        {
            p(5.0, 25.0)
        } else {
            p(15.0, 75.0)
        }
    } else if m.contains("haiku") {
        p(1.0, 5.0)
    } else {
        // sonnet and anything unknown
        p(3.0, 15.0)
    }
}

fn cache_lookup(model: &str) -> Option<ModelPricing> {
    let cache = CACHE.read().ok()?;
    let norm = normalize(model);
    if let Some(p) = cache.models.get(model).or_else(|| cache.models.get(&norm)) {
        return Some(*p);
    }
    // dated variant not listed: try without the date suffix
    strip_date(&norm).and_then(|base| cache.models.get(&base).copied())
}

fn override_lookup(model: &str) -> Option<ModelPricing> {
    let m = normalize(model);
    CLEWDR_CONFIG
        .load()
        .price_overrides
        .iter()
        .find(|o| !o.model_match.is_empty() && m.contains(&normalize(&o.model_match)))
        .map(|o| ModelPricing {
            input: o.input,
            output: o.output,
            cache_read: o.cache_read,
            cache_write: o.cache_write,
        })
}

/// Resolve pricing for a model. Returns the rates and their provenance:
/// "override" | "litellm" | "fallback".
pub fn resolve(model: &str) -> (ModelPricing, &'static str) {
    if let Some(p) = override_lookup(model) {
        return (p, "override");
    }
    if let Some(p) = cache_lookup(model) {
        return (p, "litellm");
    }
    (fallback_pricing(model), "fallback")
}

/// If `model` has no override/cached pricing, trigger a background catalog
/// fetch (at most once per hour). Cheap to call on every usage record.
pub fn ensure_model_known(model: &str) {
    if override_lookup(model).is_some() || cache_lookup(model).is_some() {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let last = LAST_FETCH_ATTEMPT.load(Ordering::Relaxed);
    if now - last < FETCH_MIN_INTERVAL {
        return;
    }
    if LAST_FETCH_ATTEMPT
        .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
        || FETCH_IN_FLIGHT.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let model = model.to_string();
    tokio::spawn(async move {
        match fetch_catalog().await {
            Ok(count) => info!(
                "Pricing catalog fetched ({} claude models) triggered by unknown model {}",
                count, model
            ),
            Err(e) => warn!("Pricing catalog fetch failed: {}", e),
        }
        FETCH_IN_FLIGHT.store(false, Ordering::SeqCst);
    });
}

/// LiteLLM catalog entry (per-token USD costs).
#[derive(Deserialize)]
struct LiteLlmEntry {
    #[serde(default)]
    input_cost_per_token: Option<f64>,
    #[serde(default)]
    output_cost_per_token: Option<f64>,
    #[serde(default)]
    cache_creation_input_token_cost: Option<f64>,
    #[serde(default)]
    cache_read_input_token_cost: Option<f64>,
}

/// Fetch the LiteLLM catalog through the configured proxy and cache all
/// claude entries (per-million rates) to disk.
async fn fetch_catalog() -> Result<usize, String> {
    let mut builder = wreq::Client::builder();
    if let Some(proxy) = CLEWDR_CONFIG.load().wreq_proxy.clone() {
        builder = builder.proxy(proxy);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let body = client
        .get(LITELLM_PRICES_URL)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json::<HashMap<String, serde_json::Value>>()
        .await
        .map_err(|e| e.to_string())?;

    let mut models = HashMap::new();
    for (key, value) in body {
        // accept "claude-*" and provider-prefixed "xxx/claude-*" keys
        let name = key.rsplit('/').next().unwrap_or(&key).to_ascii_lowercase();
        if !name.starts_with("claude") {
            continue;
        }
        let Ok(entry) = serde_json::from_value::<LiteLlmEntry>(value) else {
            continue;
        };
        let (Some(input), Some(output)) = (entry.input_cost_per_token, entry.output_cost_per_token)
        else {
            continue;
        };
        let pricing = ModelPricing {
            input: input * 1e6,
            output: output * 1e6,
            cache_read: entry.cache_read_input_token_cost.unwrap_or(input * 0.1) * 1e6,
            cache_write: entry.cache_creation_input_token_cost.unwrap_or(input * 1.25) * 1e6,
        };
        // prefer non-prefixed (exact anthropic) entries over provider variants
        models.entry(name).or_insert(pricing);
    }
    if models.is_empty() {
        return Err("no claude models found in catalog".into());
    }
    let count = models.len();
    let cache = PricingCache {
        fetched_at: chrono::Utc::now().timestamp(),
        models,
    };
    if !CLEWDR_CONFIG.load().no_fs
        && let Ok(bytes) = serde_json::to_vec(&cache)
        && let Err(e) = tokio::fs::write(cache_path(), bytes).await
    {
        warn!("Failed to persist pricing cache: {}", e);
    }
    if let Ok(mut guard) = CACHE.write() {
        *guard = cache;
    }
    Ok(count)
}
