use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use axum::response::{IntoResponse, Sse, sse::Event as SseEvent};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use snafu::{GenerateImplicitData, ResultExt};
use tracing::warn;

use crate::{
    config::{CookieStatus, ModelFamily, UsageBreakdown},
    error::{ClewdrError, WreqSnafu},
    services::cookie_actor::CookieActorHandle,
    types::claude::{CreateMessageResponse, StreamEvent},
};

use super::{
    ClaudeCodeState,
    fable_fallback::{FALLBACK_NOTICE, prepend_notice_to_response, shift_content_block_index},
};

impl ClaudeCodeState {
    pub(super) async fn handle_success_response(
        &mut self,
        response: wreq::Response,
        model_family: ModelFamily,
        show_fallback_notice: bool,
    ) -> Result<axum::response::Response, ClewdrError> {
        if self.stream {
            return self
                .forward_stream_with_usage(response, model_family, show_fallback_notice)
                .await;
        }
        let (response, usage) =
            Self::materialize_non_stream_response(response, show_fallback_notice).await?;
        let (input, output) = usage.unwrap_or((self.usage.input_tokens as u64, 0));
        self.persist_usage_totals(input, output, model_family).await;
        Ok(response)
    }

    async fn persist_usage_totals(&mut self, input: u64, output: u64, family: ModelFamily) {
        if input == 0 && output == 0 {
            return;
        }
        if let Some(cookie) = self.cookie.as_mut() {
            Self::update_cookie_boundaries_if_due(cookie, &self.cookie_actor_handle).await;
            cookie.add_and_bucket_usage(input, output, family);
            if let Err(err) = self
                .cookie_actor_handle
                .return_cookie(cookie.clone(), None)
                .await
            {
                warn!("Failed to persist usage statistics: {err}");
            }
        }
    }

    async fn forward_stream_with_usage(
        &mut self,
        response: wreq::Response,
        family: ModelFamily,
        show_fallback_notice: bool,
    ) -> Result<axum::response::Response, ClewdrError> {
        let input_tokens = self.usage.input_tokens as u64;
        let output_sum = Arc::new(AtomicU64::new(0));
        let handle = self.cookie_actor_handle.clone();
        let cookie = self.cookie.clone();
        let osum = output_sum.clone();
        let mut upstream = response.bytes_stream().eventsource();
        let stream = async_stream::stream! {
            let mut notice_injected = false;
            while let Some(result) = upstream.next().await {
                let event = match result {
                    Ok(event) => event,
                    Err(error) => {
                        yield Err::<SseEvent, std::io::Error>(std::io::Error::other(error.to_string()));
                        break;
                    }
                };
                if let Ok(parsed) = serde_json::from_str::<StreamEvent>(&event.data) {
                    match parsed {
                        StreamEvent::MessageDelta { usage: Some(usage), .. } => {
                            osum.fetch_add(usage.output_tokens as u64, Ordering::Relaxed);
                        }
                        StreamEvent::MessageStop => {
                            if let (Some(cookie), handle) = (cookie.clone(), handle.clone()) {
                                let total_out = osum.load(Ordering::Relaxed);
                                tokio::spawn(async move {
                                    let mut cookie = cookie;
                                    Self::update_cookie_boundaries_if_due(&mut cookie, &handle).await;
                                    cookie.add_and_bucket_usage(input_tokens, total_out, family);
                                    let _ = handle.return_cookie(cookie, None).await;
                                });
                            }
                        }
                        _ => {}
                    }
                }

                let is_message_start = event.event == "message_start";
                let data = if show_fallback_notice && notice_injected {
                    shift_content_block_index(&event.data).unwrap_or(event.data)
                } else {
                    event.data
                };
                let mirrored = SseEvent::default().event(event.event).id(event.id);
                let mirrored = if let Some(retry) = event.retry {
                    mirrored.retry(retry)
                } else {
                    mirrored
                };
                yield Ok::<SseEvent, std::io::Error>(mirrored.data(data));

                if show_fallback_notice && !notice_injected && is_message_start {
                    yield Ok::<SseEvent, std::io::Error>(SseEvent::default().event("content_block_start").data(
                        serde_json::json!({
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": {"type": "text", "text": ""}
                        }).to_string()
                    ));
                    yield Ok::<SseEvent, std::io::Error>(SseEvent::default().event("content_block_delta").data(
                        serde_json::json!({
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {"type": "text_delta", "text": FALLBACK_NOTICE}
                        }).to_string()
                    ));
                    yield Ok::<SseEvent, std::io::Error>(SseEvent::default().event("content_block_stop").data(
                        serde_json::json!({"type": "content_block_stop", "index": 0}).to_string()
                    ));
                    notice_injected = true;
                }
            }
        };
        Ok(Sse::new(stream)
            .keep_alive(Default::default())
            .into_response())
    }

    pub(super) async fn materialize_non_stream_response(
        response: wreq::Response,
        show_fallback_notice: bool,
    ) -> Result<(axum::response::Response, Option<(u64, u64)>), ClewdrError> {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.context(WreqSnafu {
            msg: "Failed to read Claude response body",
        })?;
        let usage = Self::extract_usage_from_bytes(&bytes);
        let body = if show_fallback_notice {
            prepend_notice_to_response(&bytes).unwrap_or_else(|| bytes.to_vec())
        } else {
            bytes.to_vec()
        };
        let mut builder = http::Response::builder().status(status);
        for (key, value) in headers.iter() {
            builder = builder.header(key, value);
        }
        let response = builder
            .body(axum::body::Body::from(body))
            .map_err(|source| ClewdrError::HttpError {
                loc: snafu::Location::generate(),
                source,
            })?;
        Ok((response, usage))
    }

    fn extract_usage_from_bytes(bytes: &[u8]) -> Option<(u64, u64)> {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes)
            && let Some(usage) = value.get("usage")
        {
            let token = |key| {
                usage.get(key).and_then(|value| {
                    value
                        .as_u64()
                        .or_else(|| value.as_i64().map(|n| n.max(0) as u64))
                })
            };
            if let (Some(input), Some(output)) = (token("input_tokens"), token("output_tokens")) {
                return Some((input, output));
            }
        }
        serde_json::from_slice::<CreateMessageResponse>(bytes)
            .ok()
            .map(|response| (0, response.count_tokens() as u64))
    }

    async fn update_cookie_boundaries_if_due(
        cookie: &mut CookieStatus,
        handle: &CookieActorHandle,
    ) {
        let now = chrono::Utc::now().timestamp();
        const SESSION_WINDOW_SECS: i64 = 5 * 60 * 60;
        const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;
        let tracked = |flag: Option<bool>| flag == Some(true);
        let unknown = |flag: Option<bool>| flag.is_none();
        let due = |ts: Option<i64>| ts.is_some_and(|value| now >= value);
        let session_tracked = tracked(cookie.session_has_reset);
        let weekly_tracked = tracked(cookie.weekly_has_reset);
        let sonnet_tracked = tracked(cookie.weekly_sonnet_has_reset);
        let session_due = session_tracked && due(cookie.session_resets_at);
        let weekly_due = weekly_tracked && due(cookie.weekly_resets_at);
        let sonnet_due = sonnet_tracked && due(cookie.weekly_sonnet_resets_at);
        let need_probe = unknown(cookie.session_has_reset)
            || unknown(cookie.weekly_has_reset)
            || unknown(cookie.weekly_sonnet_has_reset)
            || session_due
            || weekly_due
            || sonnet_due;
        if !need_probe {
            return;
        }

        cookie.resets_last_checked_at = Some(now);
        let Some((session, weekly, sonnet)) = Self::fetch_usage_resets(cookie, handle).await else {
            if session_due && session_tracked {
                cookie.session_usage = UsageBreakdown::default();
                cookie.session_resets_at = Some(now + SESSION_WINDOW_SECS);
            }
            if weekly_due && weekly_tracked {
                cookie.weekly_usage = UsageBreakdown::default();
                cookie.weekly_resets_at = Some(now + WEEKLY_WINDOW_SECS);
            }
            if sonnet_due && sonnet_tracked {
                cookie.weekly_sonnet_usage = UsageBreakdown::default();
                cookie.weekly_sonnet_resets_at = Some(now + WEEKLY_WINDOW_SECS);
            }
            return;
        };

        Self::update_window(
            &mut cookie.session_has_reset,
            &mut cookie.session_resets_at,
            &mut cookie.session_usage,
            session,
            session_due,
        );
        Self::update_window(
            &mut cookie.weekly_has_reset,
            &mut cookie.weekly_resets_at,
            &mut cookie.weekly_usage,
            weekly,
            weekly_due,
        );
        Self::update_window(
            &mut cookie.weekly_sonnet_has_reset,
            &mut cookie.weekly_sonnet_resets_at,
            &mut cookie.weekly_sonnet_usage,
            sonnet,
            sonnet_due,
        );
    }

    fn update_window(
        tracked: &mut Option<bool>,
        resets_at: &mut Option<i64>,
        usage: &mut UsageBreakdown,
        server_reset: Option<i64>,
        was_due: bool,
    ) {
        if tracked.is_none() {
            *tracked = Some(server_reset.is_some());
        }
        if was_due {
            *usage = UsageBreakdown::default();
        }
        if *tracked == Some(true) {
            *resets_at = server_reset;
            if server_reset.is_none() {
                *tracked = Some(false);
            }
        }
    }

    async fn fetch_usage_resets(
        cookie: &mut CookieStatus,
        handle: &CookieActorHandle,
    ) -> Option<(Option<i64>, Option<i64>, Option<i64>)> {
        let mut state = Self::from_cookie(handle.clone(), cookie.clone()).ok()?;
        let usage = state.fetch_usage_metrics().await.ok()?;
        state.return_cookie(None).await;
        if let Some(updated) = state.cookie {
            *cookie = updated;
        }
        let parse = |key: &str| {
            usage
                .get(key)
                .and_then(|window| window.get("resets_at"))
                .and_then(serde_json::Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp())
        };
        Some((
            parse("five_hour"),
            parse("seven_day"),
            parse("seven_day_sonnet"),
        ))
    }
}
