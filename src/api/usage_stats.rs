//! Usage & cost statistics API endpoints.

use std::collections::{BTreeMap, HashMap};

use axum::{Json, extract::Query};
use clewdr_types::{
    CookieUsageSummary, ModelPricing, ModelUsageRow, SeriesBucketApi, TokenUsage, UsageSeriesApi,
    UsageSummaryApi,
};
use serde::Deserialize;

use crate::services::{pricing, usage_tracker::usage_tracker};

/// Resolve pricing with a small per-request memo.
struct PricingMemo(HashMap<String, (ModelPricing, &'static str)>);

impl PricingMemo {
    fn new() -> Self {
        Self(HashMap::new())
    }
    fn get(&mut self, model: &str) -> (ModelPricing, &'static str) {
        *self
            .0
            .entry(model.to_string())
            .or_insert_with(|| pricing::resolve(model))
    }
}

/// GET /api/usage/summary
/// Lifetime per-cookie per-model usage with USD cost.
pub async fn api_usage_summary() -> Json<UsageSummaryApi> {
    let store = usage_tracker().snapshot().await;
    let mut memo = PricingMemo::new();

    // cookie -> model -> usage, merged across hourly + daily buckets
    let mut merged: BTreeMap<String, BTreeMap<String, TokenUsage>> = BTreeMap::new();
    for buckets in [&store.daily, &store.hourly] {
        for cookies in buckets.values() {
            for (cookie, models) in cookies {
                let entry = merged.entry(cookie.clone()).or_default();
                for (model, usage) in models {
                    entry.entry(model.clone()).or_default().add(usage);
                }
            }
        }
    }

    let mut out = UsageSummaryApi::default();
    for (cookie, models) in merged {
        let mut summary = CookieUsageSummary {
            cookie,
            ..Default::default()
        };
        for (model, usage) in models {
            let (price, source) = memo.get(&model);
            let cost = price.cost(&usage);
            summary.total.add(&usage);
            summary.total_cost += cost;
            summary.models.push(ModelUsageRow {
                model,
                usage,
                cost,
                pricing_source: source.to_string(),
            });
        }
        // biggest spenders first
        summary.models.sort_by(|a, b| {
            b.cost
                .partial_cmp(&a.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.total_cost += summary.total_cost;
        out.cookies.push(summary);
    }
    out.cookies.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Json(out)
}

#[derive(Deserialize)]
pub struct SeriesQuery {
    /// "hour" or "day" (default day)
    #[serde(default)]
    bucket: Option<String>,
    /// window size in days (default 30, max 3650)
    #[serde(default)]
    days: Option<i64>,
    /// optional cookie filter (full cookie string)
    #[serde(default)]
    cookie: Option<String>,
}

/// GET /api/usage/series?bucket=day&days=30&cookie=...
pub async fn api_usage_series(Query(q): Query<SeriesQuery>) -> Json<UsageSeriesApi> {
    let store = usage_tracker().snapshot().await;
    let bucket_kind = match q.bucket.as_deref() {
        Some("hour") => "hour",
        _ => "day",
    };
    let days = q.days.unwrap_or(30).clamp(1, 3650);
    let now = chrono::Utc::now().timestamp();
    let since = now - days * 86400;
    let bucket_size: i64 = if bucket_kind == "hour" { 3600 } else { 86400 };
    let cookie_filter = q.cookie.as_deref().filter(|c| !c.is_empty());

    let mut memo = PricingMemo::new();
    // bucket start -> model -> usage
    let mut series: BTreeMap<i64, BTreeMap<String, TokenUsage>> = BTreeMap::new();
    let mut model_totals: HashMap<String, u64> = HashMap::new();
    let mut cookie_set: std::collections::BTreeSet<String> = Default::default();

    for (source, native_size) in [(&store.daily, 86400i64), (&store.hourly, 3600i64)] {
        for (&start, cookies) in source.iter() {
            for (cookie, models) in cookies {
                cookie_set.insert(cookie.clone());
                if let Some(filter) = cookie_filter
                    && cookie != filter
                {
                    continue;
                }
                if start + native_size <= since {
                    continue;
                }
                let bucket_start = start - start.rem_euclid(bucket_size);
                let entry = series.entry(bucket_start).or_default();
                for (model, usage) in models {
                    entry.entry(model.clone()).or_default().add(usage);
                    *model_totals.entry(model.clone()).or_default() += usage.total_tokens();
                }
            }
        }
    }

    let mut buckets = Vec::with_capacity(series.len());
    for (start, models) in series {
        let mut costs = BTreeMap::new();
        let mut total = 0.0;
        for (model, usage) in &models {
            let (price, _) = memo.get(model);
            let cost = price.cost(usage);
            costs.insert(model.clone(), cost);
            total += cost;
        }
        buckets.push(SeriesBucketApi {
            start,
            models,
            costs,
            cost: total,
        });
    }

    // legend order: biggest models first
    let mut models: Vec<String> = model_totals.keys().cloned().collect();
    models.sort_by_key(|m| std::cmp::Reverse(model_totals.get(m).copied().unwrap_or(0)));

    Json(UsageSeriesApi {
        bucket: bucket_kind.to_string(),
        buckets,
        models,
        cookies: cookie_set.into_iter().collect(),
    })
}
