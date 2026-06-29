mod client;

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Value};

pub use client::{DeepSeekStreamEvent, DeepSeekWebClient};

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
    let session = with_provider_session_id(session, options.provider_session_id.as_deref());
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
        provider_session_id: Some(provider_state_json(
            &response.session_id,
            response.parent_message_id.as_ref(),
        )),
    })
}

pub async fn complete_stream(
    request: &UnifiedRequest,
    prompt: &str,
    options: &UpstreamRequestOptions,
) -> Result<tokio::sync::mpsc::Receiver<Result<DeepSeekStreamEvent, AdapterError>>, AdapterError> {
    let session = raw_session_from_options(options)?;
    let session = with_provider_session_id(session, options.provider_session_id.as_deref());
    let client = DeepSeekWebClient::new(session);
    client
        .complete_stream(&request.model, prompt)
        .await
        .map_err(|error| {
            let text = error.to_string();
            if is_auth_error(&text) {
                session_expired_error()
            } else {
                AdapterError::Upstream(format!("deepseek web provider: {text}"))
            }
        })
}

fn with_provider_session_id(session: String, provider_session_id: Option<&str>) -> String {
    let Some(provider_state) = provider_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return session;
    };
    let (session_id, parent_message_id) = decode_provider_state(provider_state);
    let Ok(mut value) = serde_json::from_str::<Value>(&session) else {
        let mut object = json!({
            "cookie": session,
            "last_session_id": session_id,
        });
        if let Some(parent_message_id) = parent_message_id {
            object["last_parent_message_id"] = parent_message_id;
        }
        return object.to_string();
    };
    if let Value::Object(object) = &mut value {
        object.insert("last_session_id".to_string(), Value::String(session_id));
        if let Some(parent_message_id) = parent_message_id {
            object.insert("last_parent_message_id".to_string(), parent_message_id);
        }
        return value.to_string();
    }
    session
}

fn provider_state_json(session_id: &str, parent_message_id: Option<&Value>) -> String {
    let mut state = json!({
        "chat_session_id": session_id,
    });
    if let Some(parent_message_id) = parent_message_id {
        state["parent_message_id"] = parent_message_id.clone();
    }
    state.to_string()
}

fn decode_provider_state(raw: &str) -> (String, Option<Value>) {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return (raw.to_string(), None);
    };
    let session_id = value
        .get("chat_session_id")
        .or_else(|| value.get("session_id"))
        .or_else(|| value.get("last_session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| raw.to_string());
    let parent_message_id = value
        .get("parent_message_id")
        .or_else(|| value.get("last_parent_message_id"))
        .filter(|value| !value.is_null())
        .cloned();
    (session_id, parent_message_id)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{decode_provider_state, provider_state_json, with_provider_session_id};

    #[test]
    fn provider_state_preserves_session_and_parent_message() {
        let state = provider_state_json("chat_a", Some(&json!("msg_b")));

        let (session_id, parent_message_id) = decode_provider_state(&state);

        assert_eq!(session_id, "chat_a");
        assert_eq!(parent_message_id, Some(json!("msg_b")));
    }

    #[test]
    fn provider_state_accepts_legacy_plain_session_id() {
        let (session_id, parent_message_id) = decode_provider_state("chat_legacy");

        assert_eq!(session_id, "chat_legacy");
        assert!(parent_message_id.is_none());
    }

    #[test]
    fn injects_provider_state_into_session_json() {
        let session = with_provider_session_id(
            json!({"cookie":"a=b"}).to_string(),
            Some(r#"{"chat_session_id":"chat_a","parent_message_id":"msg_b"}"#),
        );
        let value: serde_json::Value = serde_json::from_str(&session).unwrap();

        assert_eq!(value["last_session_id"], "chat_a");
        assert_eq!(value["last_parent_message_id"], "msg_b");
    }
}
