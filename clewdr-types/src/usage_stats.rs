//! Shared types for usage & cost statistics (backend API <-> frontend).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Aggregated token usage counters for one dimension bucket.
/// `input` counts only NON-cached input tokens (cache read/write separate),
/// following gproxy's NormalizedUsage convention.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// Number of requests whose token counts were estimated locally
    /// (claude.ai web path does not report usage)
    #[serde(default)]
    pub estimated_requests: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.requests = self.requests.saturating_add(other.requests);
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.estimated_requests = self
            .estimated_requests
            .saturating_add(other.estimated_requests);
    }

    pub fn total_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    pub fn is_empty(&self) -> bool {
        self.requests == 0 && self.total_tokens() == 0
    }
}

/// USD prices per 1,000,000 tokens.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelPricing {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

impl ModelPricing {
    /// Cost in USD of `u` at these rates.
    pub fn cost(&self, u: &TokenUsage) -> f64 {
        (u.input as f64 * self.input
            + u.output as f64 * self.output
            + u.cache_read as f64 * self.cache_read
            + u.cache_write as f64 * self.cache_write)
            / 1_000_000.0
    }
}

/// Per-model usage row inside a cookie summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsageRow {
    pub model: String,
    pub usage: TokenUsage,
    /// USD equivalent API cost
    pub cost: f64,
    /// Pricing provenance: "override" | "litellm" | "fallback"
    #[serde(default)]
    pub pricing_source: String,
}

/// Lifetime usage summary for one cookie.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CookieUsageSummary {
    pub cookie: String,
    pub models: Vec<ModelUsageRow>,
    pub total: TokenUsage,
    pub total_cost: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSummaryApi {
    pub cookies: Vec<CookieUsageSummary>,
    pub total_cost: f64,
}

/// One time bucket in a usage series, broken down by model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SeriesBucketApi {
    /// Bucket start, epoch seconds UTC
    pub start: i64,
    /// model -> usage in this bucket
    pub models: BTreeMap<String, TokenUsage>,
    /// model -> USD cost in this bucket
    pub costs: BTreeMap<String, f64>,
    /// total USD cost of the bucket
    pub cost: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSeriesApi {
    /// "hour" or "day"
    pub bucket: String,
    pub buckets: Vec<SeriesBucketApi>,
    /// All model names appearing in the series (stable legend order)
    pub models: Vec<String>,
    /// All cookies available for filtering
    pub cookies: Vec<String>,
}
