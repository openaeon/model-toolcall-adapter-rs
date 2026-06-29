use std::collections::HashMap;

use reqwest::Client;
use serde_json::{json, Value};

use crate::error::AdapterError;
use crate::providers::alias_model_items;
use crate::types::UnifiedRequest;
use crate::upstream::{UpstreamRequestOptions, UpstreamResponse};

#[derive(Clone)]
pub struct OpenAiCompatClient {
    client: Client,
    base_url: String,
    api_key: String,
}

impl OpenAiCompatClient {
    pub fn new(client: Client, base_url: String, api_key: String) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    pub async fn complete(
        &self,
        request: &UnifiedRequest,
        prompt: &str,
        options: &UpstreamRequestOptions,
    ) -> Result<UpstreamResponse, AdapterError> {
        let base_url = self.effective_base_url(options);
        let api_key = self.effective_api_key(options);
        let endpoint = chat_completions_endpoint(&base_url);
        let body = json!({
            "model": request.model,
            "stream": false,
            "max_tokens": request.max_tokens,
            "messages": [{ "role": "user", "content": prompt }]
        });
        let mut builder = self.client.post(endpoint).json(&body);
        if !api_key.trim().is_empty() {
            builder = builder.bearer_auth(api_key);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| AdapterError::Upstream(error.to_string()))?;
        let status = response.status();
        let payload = response
            .json::<Value>()
            .await
            .map_err(|error| AdapterError::Upstream(error.to_string()))?;
        if !status.is_success() {
            return Err(AdapterError::Upstream(payload.to_string()));
        }
        let text = payload
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                payload
                    .get("response")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .ok_or_else(|| AdapterError::Upstream(format!("missing assistant text: {payload}")))?;

        let reasoning = payload
            .pointer("/choices/0/message/reasoning_content")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                payload
                    .pointer("/choices/0/message/reasoning")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            });

        Ok(UpstreamResponse { text, reasoning })
    }

    pub async fn list_models(
        &self,
        fallback_model: &str,
        model_aliases: &HashMap<String, String>,
        options: &UpstreamRequestOptions,
    ) -> Result<Value, AdapterError> {
        let base_url = self.effective_base_url(options);
        let api_key = self.effective_api_key(options);
        let endpoint = models_endpoint(&base_url);
        let mut builder = self.client.get(endpoint);
        if !api_key.trim().is_empty() {
            builder = builder.bearer_auth(api_key);
        }
        let response = match builder.send().await {
            Ok(response) => response,
            Err(error) => {
                return Ok(fallback_models(
                    fallback_model,
                    model_aliases,
                    error.to_string(),
                ))
            }
        };
        let status = response.status();
        let payload = match response.json::<Value>().await {
            Ok(payload) => payload,
            Err(error) => {
                return Ok(fallback_models(
                    fallback_model,
                    model_aliases,
                    error.to_string(),
                ))
            }
        };
        if status.is_success() {
            return Ok(with_alias_models(payload, model_aliases));
        }
        Ok(fallback_models(
            fallback_model,
            model_aliases,
            payload.to_string(),
        ))
    }

    fn effective_base_url(&self, options: &UpstreamRequestOptions) -> String {
        options
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.base_url)
            .trim_end_matches('/')
            .to_string()
    }

    fn effective_api_key<'a>(&'a self, options: &'a UpstreamRequestOptions) -> &'a str {
        options
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.api_key)
    }
}

fn chat_completions_endpoint(base_url: &str) -> String {
    let lower = base_url.to_ascii_lowercase();
    if lower.ends_with("/chat/completions") {
        base_url.to_string()
    } else if lower.ends_with("/v1") {
        format!("{base_url}/chat/completions")
    } else {
        format!("{base_url}/v1/chat/completions")
    }
}

fn models_endpoint(base_url: &str) -> String {
    let lower = base_url.to_ascii_lowercase();
    if lower.ends_with("/models") {
        base_url.to_string()
    } else if lower.ends_with("/v1") {
        format!("{base_url}/models")
    } else {
        format!("{base_url}/v1/models")
    }
}

fn with_alias_models(mut payload: Value, model_aliases: &HashMap<String, String>) -> Value {
    if let Some(data) = payload.get_mut("data").and_then(Value::as_array_mut) {
        data.extend(alias_model_items(model_aliases));
    }
    payload
}

fn fallback_models(
    fallback_model: &str,
    model_aliases: &HashMap<String, String>,
    warning: impl Into<String>,
) -> Value {
    let mut data = vec![json!({
        "id": fallback_model,
        "object": "model",
        "owned_by": "adapter-fallback"
    })];
    data.extend(alias_model_items(model_aliases));
    json!({
        "object": "list",
        "data": data,
        "warning": warning.into()
    })
}
