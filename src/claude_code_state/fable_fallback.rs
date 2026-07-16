use colored::Colorize;
use serde_json::{Value, json};
use tracing::{Instrument, info, warn};

use crate::{
    api::usage::extract_usage_fields,
    config::{CLEWDR_CONFIG, Reason},
    error::ClewdrError,
    types::claude::CreateMessageParams,
};

use super::ClaudeCodeState;

pub const FABLE_5: &str = "claude-fable-5";
pub const OPUS_48: &str = "claude-opus-4-8";
pub const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-06-01";
pub const FALLBACK_NOTICE: &str = "⚠️ Clewdr：所有可用 Cookie 的 Fable 7 日配额均已耗尽；在额度状态变化前，本次及后续 Fable 请求将由 Opus 4.8 接管。\n\n";

pub fn is_fable_model(model: &str) -> bool {
    let model = model.strip_suffix("-1M").unwrap_or(model);
    model == FABLE_5
        || model
            .rsplit_once('/')
            .is_some_and(|(_, leaf)| leaf == FABLE_5)
}

/// Adapted from gproxy's opt-in Claude Fable fallback request shaping.
/// Existing caller-supplied fallback chains are deliberately preserved.
pub fn apply_fable_to_opus48(body: &mut Value) -> bool {
    let Some(root) = body.as_object_mut() else {
        return false;
    };
    let Some(model) = root.get("model").and_then(Value::as_str) else {
        return false;
    };
    let Some(fallback_model) = fallback_model_for(model) else {
        return false;
    };
    if !root.contains_key("fallbacks") {
        root.insert("fallbacks".into(), json!([{ "model": fallback_model }]));
    }
    true
}

fn fallback_model_for(model: &str) -> Option<String> {
    if model == FABLE_5 {
        return Some(OPUS_48.to_string());
    }
    let (namespace, leaf) = model.rsplit_once('/')?;
    (leaf == FABLE_5).then(|| format!("{namespace}/{OPUS_48}"))
}

fn confirmed_fable_reset(usage: &Value, fallback_reset: i64) -> FableQuotaVerdict {
    const EXHAUSTED_PERCENT: f64 = 99.999;
    let Some(fields) = extract_usage_fields(usage) else {
        return FableQuotaVerdict::Unverified;
    };
    if fields.five_hour.utilization >= EXHAUSTED_PERCENT
        || fields.seven_day.utilization >= EXHAUSTED_PERCENT
    {
        return FableQuotaVerdict::GlobalExhausted;
    }
    let Some(fable) = fields.seven_day_fable else {
        return FableQuotaVerdict::Unverified;
    };
    if fable.utilization < EXHAUSTED_PERCENT {
        return FableQuotaVerdict::Unverified;
    }
    FableQuotaVerdict::FableExhausted(
        fable
            .resets_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.timestamp())
            .unwrap_or(fallback_reset),
    )
}

#[derive(Debug, PartialEq)]
enum FableQuotaVerdict {
    /// Fable's model-scoped weekly window is exhausted; global windows are fine.
    FableExhausted(i64),
    /// A global (5h/7d) window is exhausted; the upstream reset is trustworthy.
    GlobalExhausted,
    /// Usage data missing or contradictory; do not trust a long upstream reset.
    Unverified,
}

impl ClaudeCodeState {
    pub(super) async fn try_fable_chat(
        &mut self,
        p: CreateMessageParams,
    ) -> Result<axum::response::Response, ClewdrError> {
        let max_attempts = self
            .cookie_actor_handle
            .get_status()
            .await?
            .valid
            .len()
            .saturating_add(CLEWDR_CONFIG.load().max_retries)
            .max(1);

        for attempt in 0..max_attempts {
            let mut state = self.to_owned();
            let cookie = match state.request_fable_cookie().await {
                Ok(cookie) => cookie,
                Err(ClewdrError::FableQuotaExhausted) => {
                    return self.handle_fable_pool_exhausted(p).await;
                }
                Err(err) => return Err(err),
            };
            if attempt > 0 {
                info!("[FABLE][RETRY] attempt: {}", attempt.to_string().green());
            }
            let retry =
                state
                    .send_with_current_cookie(p.clone(), false)
                    .instrument(tracing::info_span!(
                        "claude_code_fable",
                        "cookie" = cookie.cookie.mask()
                    ));
            match retry.await {
                Ok(response) => return Ok(response),
                Err(ClewdrError::InvalidCookie { reason }) => {
                    let reason = match reason {
                        Reason::TooManyRequest(reset) => {
                            state.classify_fable_rate_limit(reset).await
                        }
                        other => other,
                    };
                    state.return_cookie(Some(reason)).await;
                }
                Err(err) => return Err(err),
            }
        }

        match self.request_fable_cookie().await {
            Err(ClewdrError::FableQuotaExhausted) => self.handle_fable_pool_exhausted(p).await,
            Err(err) => Err(err),
            Ok(_) => Err(ClewdrError::TooManyRetries),
        }
    }

    async fn handle_fable_pool_exhausted(
        &mut self,
        p: CreateMessageParams,
    ) -> Result<axum::response::Response, ClewdrError> {
        if !CLEWDR_CONFIG.load().enable_fable_fallback {
            return Err(ClewdrError::FableQuotaExhausted);
        }
        info!("[FABLE] all cookies exhausted; enabling Opus 4.8 server-side fallback");
        for attempt in 0..CLEWDR_CONFIG.load().max_retries + 1 {
            let mut state = self.to_owned();
            let cookie = state.request_cookie().await?;
            if attempt > 0 {
                info!(
                    "[FABLE][OPUS-4.8][RETRY] attempt: {}",
                    attempt.to_string().green()
                );
            }
            let retry =
                state
                    .send_with_current_cookie(p.clone(), true)
                    .instrument(tracing::info_span!(
                        "claude_code_fable_fallback",
                        "cookie" = cookie.cookie.mask()
                    ));
            match retry.await {
                Ok(response) => return Ok(response),
                Err(ClewdrError::InvalidCookie { reason }) => {
                    state.return_cookie(Some(reason)).await;
                }
                Err(err) => return Err(err),
            }
        }
        Err(ClewdrError::TooManyRetries)
    }

    async fn classify_fable_rate_limit(&mut self, fallback_reset: i64) -> Reason {
        // A Fable-model 429 often carries Fable's 7-day reset. If we cannot
        // verify the limit is Fable-scoped, blindly trusting that timestamp
        // would bench the cookie globally for days. Cap unverified cooldowns.
        const UNVERIFIED_COOLDOWN_SECS: i64 = 3600;
        let capped = || {
            let cap = chrono::Utc::now().timestamp() + UNVERIFIED_COOLDOWN_SECS;
            Reason::TooManyRequest(fallback_reset.min(cap))
        };
        let Ok(usage) = self.fetch_usage_metrics().await else {
            warn!("Could not verify Fable-scoped quota; capping global cooldown at 1h");
            return capped();
        };
        match confirmed_fable_reset(&usage, fallback_reset) {
            FableQuotaVerdict::FableExhausted(reset) => {
                info!("Confirmed Fable-scoped quota exhaustion until {reset}");
                Reason::FableRateLimited(reset)
            }
            FableQuotaVerdict::GlobalExhausted => Reason::TooManyRequest(fallback_reset),
            FableQuotaVerdict::Unverified => {
                warn!("Usage data inconclusive for Fable 429; capping global cooldown at 1h");
                capped()
            }
        }
    }
}

pub fn prepend_notice_to_response(body: &[u8]) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(body).ok()?;
    let content = value.get_mut("content")?.as_array_mut()?;
    content.insert(0, json!({"type": "text", "text": FALLBACK_NOTICE}));
    serde_json::to_vec(&value).ok()
}

pub fn shift_content_block_index(data: &str) -> Option<String> {
    let mut value = serde_json::from_str::<Value>(data).ok()?;
    let event_type = value.get("type")?.as_str()?;
    if !matches!(
        event_type,
        "content_block_start" | "content_block_delta" | "content_block_stop"
    ) {
        return None;
    }
    let index = value.get("index")?.as_u64()?;
    value["index"] = json!(index + 1);
    serde_json::to_string(&value).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn injects_fable_fallback_and_preserves_existing_chain() {
        let mut body = json!({"model": FABLE_5});
        assert!(apply_fable_to_opus48(&mut body));
        assert_eq!(body["fallbacks"], json!([{"model": OPUS_48}]));

        let existing = json!([{"model": "custom-model"}]);
        body["fallbacks"] = existing.clone();
        assert!(apply_fable_to_opus48(&mut body));
        assert_eq!(body["fallbacks"], existing);
    }

    #[test]
    fn ignores_non_fable_and_shifts_stream_indexes() {
        let mut body = json!({"model": "claude-sonnet-4-5"});
        assert!(!apply_fable_to_opus48(&mut body));
        assert!(body.get("fallbacks").is_none());

        let shifted = shift_content_block_index(
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"x"}}"#,
        )
        .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&shifted).unwrap()["index"], 3);
    }

    #[test]
    fn confirms_only_model_scoped_fable_exhaustion() {
        let mut usage = json!({
            "five_hour": {"utilization": 20},
            "seven_day": {"utilization": 40},
            "limits": [{
                "kind": "weekly_scoped",
                "percent": 100,
                "resets_at": "2026-07-16T06:59:59Z",
                "scope": {"model": {"display_name": "Fable"}}
            }]
        });
        assert!(matches!(
            confirmed_fable_reset(&usage, 1),
            FableQuotaVerdict::FableExhausted(_)
        ));

        usage["five_hour"]["utilization"] = json!(100);
        assert_eq!(
            confirmed_fable_reset(&usage, 1),
            FableQuotaVerdict::GlobalExhausted
        );

        // Missing Fable window with healthy global windows must not be trusted.
        usage["five_hour"]["utilization"] = json!(20);
        usage["limits"] = json!([]);
        assert_eq!(
            confirmed_fable_reset(&usage, 1),
            FableQuotaVerdict::Unverified
        );
    }
}
