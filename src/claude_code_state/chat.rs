use axum::{Json, response::IntoResponse};
use colored::Colorize;
use http::header::{ACCEPT, USER_AGENT};
use snafu::ResultExt;
use tracing::{Instrument, error, info, warn};
use wreq::Method;

use crate::{
    claude_code_state::{ClaudeCodeState, TokenStatus},
    config::{CLAUDE_CODE_USER_AGENT, CLEWDR_CONFIG, ModelFamily},
    error::{CheckClaudeErr, ClewdrError, WreqSnafu},
    types::claude::{CountMessageTokensResponse, CreateMessageParams},
};

use super::fable_fallback::{SERVER_SIDE_FALLBACK_BETA, apply_fable_to_opus48, is_fable_model};

pub(super) const CLAUDE_BETA_BASE: &str = "oauth-2025-04-20";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub(super) const CLAUDE_API_VERSION: &str = "2023-06-01";

impl ClaudeCodeState {
    /// Attempts to send a chat message to Claude API with retry mechanism
    ///
    /// This method handles the complete chat flow including:
    /// - Request preparation and logging
    /// - Cookie management for authentication
    /// - Executing the chat request with automatic retries on failure
    /// - Response transformation according to the specified API format
    /// - Error handling and cleanup
    ///
    /// The method implements a sophisticated retry mechanism to handle transient failures,
    /// and manages conversation cleanup to prevent resource leaks. It also includes
    /// performance tracking to measure response times.
    ///
    /// # Arguments
    /// * `p` - The client request body containing messages and configuration
    ///
    /// # Returns
    /// * `Result<axum::response::Response, ClewdrError>` - Formatted response or error
    pub async fn try_chat(
        &mut self,
        p: CreateMessageParams,
    ) -> Result<axum::response::Response, ClewdrError> {
        if is_fable_model(&p.model) {
            return self.try_fable_chat(p).await;
        }
        self.try_standard_chat(p).await
    }

    async fn try_standard_chat(
        &mut self,
        p: CreateMessageParams,
    ) -> Result<axum::response::Response, ClewdrError> {
        for i in 0..CLEWDR_CONFIG.load().max_retries + 1 {
            if i > 0 {
                info!("[RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.to_owned();
            let p = p.to_owned();

            let cookie = state.request_cookie().await?;
            let retry = state
                .send_with_current_cookie(p, false)
                .instrument(tracing::info_span!(
                    "claude_code",
                    "cookie" = cookie.cookie.mask()
                ));
            match retry.await {
                Ok(res) => {
                    return Ok(res);
                }
                Err(e) => {
                    error!(
                        "[{}] {}",
                        state.cookie.as_ref().unwrap().cookie.mask().green(),
                        e
                    );
                    // 429 error
                    if let ClewdrError::InvalidCookie { reason } = e {
                        state.return_cookie(Some(reason.to_owned())).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(ClewdrError::TooManyRetries)
    }

    pub(super) async fn send_with_current_cookie(
        &mut self,
        p: CreateMessageParams,
        enable_fable_fallback: bool,
    ) -> Result<axum::response::Response, ClewdrError> {
        match self.check_token() {
            TokenStatus::None => {
                info!("No token found, requesting new token");
                let org = self.get_organization().await?;
                let code_res = self.exchange_code(&org).await?;
                self.exchange_token(code_res).await?;
                self.return_cookie(None).await;
            }
            TokenStatus::Expired => {
                info!("Token expired, refreshing token");
                self.refresh_token().await?;
                self.return_cookie(None).await;
            }
            TokenStatus::Valid => info!("Token is valid, proceeding with request"),
        }
        let access_token = self
            .cookie
            .as_ref()
            .and_then(|cookie| cookie.token.as_ref())
            .ok_or(ClewdrError::UnexpectedNone {
                msg: "No access token found in cookie",
            })?
            .access_token
            .clone();
        self.send_chat(access_token, p, enable_fable_fallback).await
    }

    pub async fn send_chat(
        &mut self,
        access_token: String,
        mut p: CreateMessageParams,
        enable_fable_fallback: bool,
    ) -> Result<axum::response::Response, ClewdrError> {
        if let Some(stripped) = p.model.strip_suffix("-1M") {
            p.model = stripped.to_string();
        }
        let model_family = if enable_fable_fallback {
            ModelFamily::Opus
        } else {
            Self::classify_model(&p.model)
        };
        let response = self
            .execute_claude_request(&access_token, &p, enable_fable_fallback)
            .await?;
        self.handle_success_response(
            response,
            model_family,
            enable_fable_fallback,
            p.model.clone(),
        )
        .await
    }

    async fn execute_claude_request(
        &mut self,
        access_token: &str,
        body: &CreateMessageParams,
        enable_fable_fallback: bool,
    ) -> Result<wreq::Response, ClewdrError> {
        let mut body = serde_json::to_value(body)?;
        let fallback_applied = enable_fable_fallback && apply_fable_to_opus48(&mut body);
        let beta_header =
            Self::build_beta_header(self.anthropic_beta_header.as_deref(), fallback_applied);
        self.client
            .post(
                self.endpoint
                    .join("v1/messages")
                    .map_err(|e| ClewdrError::Whatever {
                        message: format!("Parse URL error: {e}"),
                        source: Some(Box::new(e)),
                    })?
                    .to_string(),
            )
            .bearer_auth(access_token)
            .header(USER_AGENT, CLAUDE_CODE_USER_AGENT)
            .header("anthropic-beta", beta_header)
            .header("anthropic-version", CLAUDE_API_VERSION)
            .json(&body)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to send chat message",
            })?
            .check_claude()
            .await
    }

    async fn persist_count_tokens_allowed(&mut self, value: bool) {
        if let Some(cookie) = self.cookie.as_mut() {
            if cookie.count_tokens_allowed == Some(value) {
                return;
            }
            cookie.set_count_tokens_allowed(Some(value));
            let cloned = cookie.clone();
            if let Err(err) = self.cookie_actor_handle.return_cookie(cloned, None).await {
                warn!("Failed to persist count_tokens permission: {}", err);
            }
        }
    }

    pub async fn fetch_usage_metrics(&mut self) -> Result<serde_json::Value, ClewdrError> {
        match self.check_token() {
            TokenStatus::None => {
                let org = self.get_organization().await?;
                let code = self.exchange_code(&org).await?;
                self.exchange_token(code).await?;
            }
            TokenStatus::Expired => {
                self.refresh_token().await?;
            }
            TokenStatus::Valid => {}
        }

        let access_token = self
            .cookie
            .as_ref()
            .and_then(|c| c.token.as_ref())
            .ok_or(ClewdrError::UnexpectedNone {
                msg: "No access token available",
            })?
            .access_token
            .to_owned();

        self.client
            .request(Method::GET, CLAUDE_USAGE_URL)
            .bearer_auth(access_token)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(USER_AGENT, CLAUDE_CODE_USER_AGENT)
            .header("anthropic-beta", CLAUDE_BETA_BASE)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to fetch usage metrics",
            })?
            .check_claude()
            .await?
            .json::<serde_json::Value>()
            .await
            .context(WreqSnafu {
                msg: "Failed to parse usage metrics response",
            })
    }

    /// Fetch account email and plan tier from the OAuth profile endpoint
    /// (same approach as gproxy's `enrich_from_profile`).
    /// Requires a valid access token; call after `fetch_usage_metrics` or
    /// any flow that guarantees a fresh token.
    ///
    /// # Returns
    /// * `Result<bool, ClewdrError>` - true if account info changed
    pub async fn fetch_oauth_profile(&mut self) -> Result<bool, ClewdrError> {
        let access_token = self
            .cookie
            .as_ref()
            .and_then(|c| c.token.as_ref())
            .ok_or(ClewdrError::UnexpectedNone {
                msg: "No access token available",
            })?
            .access_token
            .to_owned();

        let profile = self
            .client
            .request(Method::GET, CLAUDE_PROFILE_URL)
            .bearer_auth(access_token)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(USER_AGENT, CLAUDE_CODE_USER_AGENT)
            .header("anthropic-beta", CLAUDE_BETA_BASE)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to fetch oauth profile",
            })?
            .check_claude()
            .await?
            .json::<serde_json::Value>()
            .await
            .context(WreqSnafu {
                msg: "Failed to parse oauth profile response",
            })?;

        let email = profile
            .get("account")
            .and_then(|a| a.get("email"))
            .and_then(|e| e.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let tier = profile
            .get("organization")
            .and_then(|o| o.get("rate_limit_tier"))
            .and_then(|t| t.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        Ok(self
            .cookie
            .as_mut()
            .is_some_and(|c| c.set_account_info(email, tier)))
    }

    pub async fn try_count_tokens(
        &mut self,
        p: CreateMessageParams,
        for_web: bool,
    ) -> Result<axum::response::Response, ClewdrError> {
        for i in 0..CLEWDR_CONFIG.load().max_retries + 1 {
            if i > 0 {
                info!("[TOKENS][RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.to_owned();
            let p = p.to_owned();

            let cookie = state.request_cookie().await?;
            let web_attempt_allowed = CLEWDR_CONFIG.load().enable_web_count_tokens;
            let cookie_disallows = matches!(cookie.count_tokens_allowed, Some(false));
            if cookie_disallows || (for_web && !web_attempt_allowed) {
                if cookie_disallows {
                    state.persist_count_tokens_allowed(false).await;
                }
                return Ok(Self::local_count_tokens_response(&p));
            }
            let retry = async {
                match state.check_token() {
                    TokenStatus::None => {
                        info!("No token found, requesting new token");
                        let org = state.get_organization().await?;
                        let code_res = state.exchange_code(&org).await?;
                        state.exchange_token(code_res).await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Expired => {
                        info!("Token expired, refreshing token");
                        state.refresh_token().await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Valid => {
                        info!("Token is valid, proceeding with count_tokens");
                    }
                }
                let Some(access_token) = state.cookie.as_ref().and_then(|c| c.token.to_owned())
                else {
                    return Err(ClewdrError::UnexpectedNone {
                        msg: "No access token found in cookie",
                    });
                };
                state
                    .perform_count_tokens(access_token.access_token.to_owned(), p, for_web)
                    .await
            }
            .instrument(tracing::info_span!(
                "claude_code_tokens",
                "cookie" = cookie.cookie.mask()
            ));
            match retry.await {
                Ok(res) => {
                    return Ok(res);
                }
                Err(e) => {
                    error!(
                        "[{}][TOKENS] {}",
                        state.cookie.as_ref().unwrap().cookie.mask().green(),
                        e
                    );
                    if let ClewdrError::InvalidCookie { reason } = e {
                        state.return_cookie(Some(reason.to_owned())).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(ClewdrError::TooManyRetries)
    }

    async fn perform_count_tokens(
        &mut self,
        access_token: String,
        mut p: CreateMessageParams,
        allow_fallback: bool,
    ) -> Result<axum::response::Response, ClewdrError> {
        p.stream = Some(false);
        if let Some(stripped) = p.model.strip_suffix("-1M") {
            p.model = stripped.to_string();
        }

        match self
            .execute_claude_count_tokens_request(&access_token, &p)
            .await
        {
            Ok(response) => {
                self.persist_count_tokens_allowed(true).await;
                let (resp, _) = Self::materialize_non_stream_response(response, false).await?;
                Ok(resp)
            }
            Err(err) => {
                if Self::is_count_tokens_unauthorized(&err) {
                    self.persist_count_tokens_allowed(false).await;
                    if allow_fallback {
                        return Ok(Self::local_count_tokens_response(&p));
                    }
                }
                Err(err)
            }
        }
    }

    async fn execute_claude_count_tokens_request(
        &mut self,
        access_token: &str,
        body: &CreateMessageParams,
    ) -> Result<wreq::Response, ClewdrError> {
        let beta_header = Self::build_beta_header(self.anthropic_beta_header.as_deref(), false);
        self.client
            .post(
                self.endpoint
                    .join("v1/messages/count_tokens")
                    .map_err(|e| ClewdrError::Whatever {
                        message: format!("Parse URL error: {e}"),
                        source: Some(Box::new(e)),
                    })?
                    .to_string(),
            )
            .bearer_auth(access_token)
            .header(USER_AGENT, CLAUDE_CODE_USER_AGENT)
            .header("anthropic-beta", beta_header)
            .header("anthropic-version", CLAUDE_API_VERSION)
            .json(body)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to call Claude count_tokens",
            })?
            .check_claude()
            .await
    }

    fn build_beta_header(extra: Option<&str>, enable_fable_fallback: bool) -> String {
        let mut parts = vec![CLAUDE_BETA_BASE.to_string()];
        if let Some(extra) = extra {
            for token in extra.split(',') {
                let t = token.trim();
                if !t.is_empty() && !parts.iter().any(|part| part == t) {
                    parts.push(t.to_string());
                }
            }
        }
        if enable_fable_fallback && !parts.iter().any(|part| part == SERVER_SIDE_FALLBACK_BETA) {
            parts.push(SERVER_SIDE_FALLBACK_BETA.to_string());
        }
        parts.join(",")
    }

    fn classify_model(model: &str) -> ModelFamily {
        ModelFamily::classify(model)
    }

    fn local_count_tokens_response(body: &CreateMessageParams) -> axum::response::Response {
        let estimate = CountMessageTokensResponse {
            input_tokens: body.count_tokens(),
        };
        Json(estimate).into_response()
    }

    fn is_count_tokens_unauthorized(error: &ClewdrError) -> bool {
        if let ClewdrError::ClaudeHttpError { code, .. } = error {
            return matches!(code.as_u16(), 401 | 403 | 404);
        }
        false
    }
}
