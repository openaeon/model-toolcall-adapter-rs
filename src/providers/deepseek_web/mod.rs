mod client;

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Value};

pub use client::DeepSeekWebClient;

use crate::error::AdapterError;
use crate::providers::alias_model_items;
use crate::types::UnifiedRequest;
use crate::upstream::{UpstreamRequestOptions, UpstreamResponse};

pub async fn complete(
    request: &UnifiedRequest,
    prompt: &str,
    options: &UpstreamRequestOptions,
) -> Result<UpstreamResponse, AdapterError> {
    let session = raw_session_from_options(options)?;
    let client = DeepSeekWebClient::new(session);
    let response = client
        .complete(&request.model, prompt)
        .await
        .map_err(|error| {
            let text = error.to_string();
            if is_auth_error(&text) {
                session_expired_error()
            } else {
                AdapterError::Upstream(format!("deepseek web provider: {text}"))
            }
        })?;

    if response.text.trim().is_empty()
        && response
            .reasoning
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        return Err(AdapterError::Upstream(
            "deepseek web provider returned empty text and reasoning".to_string(),
        ));
    }

    Ok(UpstreamResponse {
        text: clean_output(&response.text),
        reasoning: response.reasoning,
    })
}

pub fn model_list_with_status(
    model_aliases: &HashMap<String, String>,
    session_status: Value,
) -> Value {
    let mut data = vec![
        json!({ "id": "deepseek-web/reasoner", "object": "model", "owned_by": "deepseek-web", "tool_calling": false }),
        json!({ "id": "deepseek-web/chat", "object": "model", "owned_by": "deepseek-web", "tool_calling": false }),
        json!({ "id": "deepseek-web/search", "object": "model", "owned_by": "deepseek-web", "tool_calling": false }),
    ];
    data.extend(alias_model_items(model_aliases));
    json!({
        "object": "list",
        "data": data,
        "provider": "deepseek-web",
        "session": session_status
    })
}

pub fn session_status_from_options(options: &UpstreamRequestOptions) -> Value {
    match session_from_options_with_source(options) {
        Some((session, source)) => session_status_from_raw_with_source(&session, source),
        None => json!({
            "configured": false,
            "source": null,
            "message": "DeepSeek Web session missing. Paste Session JSON/Cookie or save one to ~/.model-toolcall-adapter/deepseek_session.json."
        }),
    }
}

fn raw_session_from_options(options: &UpstreamRequestOptions) -> Result<String, AdapterError> {
    session_from_options_with_source(options)
        .map(|(session, _)| session)
        .ok_or_else(|| {
            AdapterError::Upstream(
                "DeepSeek Web session missing. Paste session JSON/Cookie or save one to ~/.model-toolcall-adapter/deepseek_session.json.".to_string(),
            )
        })
}

fn session_from_options_with_source(
    options: &UpstreamRequestOptions,
) -> Option<(String, &'static str)> {
    if let Some(session) = options
        .deepseek_session
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some((session.to_string(), "x-deepseek-session"));
    }
    if let Some(session) = options
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some((session.to_string(), "x-upstream-api-key"));
    }
    read_default_session().map(|session| (session, "session_file"))
}

fn read_default_session() -> Option<String> {
    let path = default_session_path().ok()?;
    std::fs::read_to_string(path).ok()
}

pub fn default_session_path() -> Result<PathBuf, AdapterError> {
    if let Some(path) = std::env::var_os("ADAPTER_DEEPSEEK_SESSION_FILE") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AdapterError::Upstream("HOME is not set".to_string()))?;
    Ok(PathBuf::from(home)
        .join(".model-toolcall-adapter")
        .join("deepseek_session.json"))
}

fn clean_output(text: &str) -> String {
    text.trim_end_matches("FINISHED").trim_end().to_string()
}

fn session_status_from_raw_with_source(session: &str, source: &str) -> Value {
    json!({
        "configured": true,
        "source": source,
        "format": session_format(session),
        "bytes": session.len(),
    })
}

fn session_format(session: &str) -> &'static str {
    let trimmed = session.trim();
    if trimmed.starts_with('{') {
        "json"
    } else if trimmed.contains('=') || trimmed.contains(';') {
        "cookie"
    } else {
        "unknown"
    }
}

fn is_auth_error(text: &str) -> bool {
    text.contains("Authorization Failed")
        || text.contains("invalid token")
        || text.contains("\"code\":40003")
}

fn session_expired_error() -> AdapterError {
    AdapterError::Upstream(
        "DeepSeek Web session invalid or expired. Re-login DeepSeek Web and refresh ~/.model-toolcall-adapter/deepseek_session.json, or paste a fresh session JSON/Cookie in the UI.".to_string(),
    )
}
