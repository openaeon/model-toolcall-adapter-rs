pub mod deepseek_web;
pub mod openai_compat;

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::AdapterError;
use crate::types::UnifiedRequest;
use crate::upstream::{UpstreamRequestOptions, UpstreamResponse};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiCompat,
    DeepSeekWeb,
}

pub fn resolve_provider(
    request: &UnifiedRequest,
    options: &UpstreamRequestOptions,
) -> ProviderKind {
    if request.model.starts_with("deepseek-web/")
        || options
            .provider
            .as_deref()
            .is_some_and(|provider| provider.eq_ignore_ascii_case("deepseek-web"))
    {
        ProviderKind::DeepSeekWeb
    } else {
        ProviderKind::OpenAiCompat
    }
}

pub async fn complete(
    request: &UnifiedRequest,
    prompt: &str,
    options: &UpstreamRequestOptions,
    openai: &openai_compat::OpenAiCompatClient,
) -> Result<UpstreamResponse, AdapterError> {
    match resolve_provider(request, options) {
        ProviderKind::OpenAiCompat => openai.complete(request, prompt, options).await,
        ProviderKind::DeepSeekWeb => deepseek_web::complete(request, prompt, options).await,
    }
}

pub async fn list_models(
    fallback_model: &str,
    model_aliases: &HashMap<String, String>,
    options: &UpstreamRequestOptions,
    openai: &openai_compat::OpenAiCompatClient,
) -> Result<Value, AdapterError> {
    let provider_kind = if fallback_model.starts_with("deepseek-web/")
        || model_aliases
            .values()
            .any(|model| model.starts_with("deepseek-web/"))
        || options
            .provider
            .as_deref()
            .is_some_and(|provider| provider.eq_ignore_ascii_case("deepseek-web"))
    {
        ProviderKind::DeepSeekWeb
    } else {
        ProviderKind::OpenAiCompat
    };

    match provider_kind {
        ProviderKind::OpenAiCompat => {
            openai
                .list_models(fallback_model, model_aliases, options)
                .await
        }
        ProviderKind::DeepSeekWeb => {
            let session_status = deepseek_web::session_status_from_options(options);
            if !session_status
                .get("configured")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(AdapterError::Upstream(
                    "DeepSeek Web session missing. Login first, then paste Session JSON/Cookie and retry fetching models.".to_string(),
                ));
            }
            Ok(deepseek_web::model_list_with_status(
                model_aliases,
                session_status,
            ))
        }
    }
}

pub fn alias_model_items(model_aliases: &HashMap<String, String>) -> Vec<Value> {
    model_aliases
        .keys()
        .map(|alias| {
            json!({
                "id": alias,
                "object": "model",
                "owned_by": "adapter-alias"
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{deepseek_web, ProviderKind};
    use crate::upstream::UpstreamRequestOptions;

    #[test]
    fn deepseek_model_list_reports_session_source() {
        let options = UpstreamRequestOptions {
            provider: Some("deepseek-web".to_string()),
            deepseek_session: Some("sessionid=abc".to_string()),
            ..Default::default()
        };

        let status = deepseek_web::session_status_from_options(&options);
        let body = deepseek_web::model_list_with_status(&HashMap::new(), status);

        assert_eq!(body["provider"], "deepseek-web");
        assert_eq!(body["session"]["configured"], true);
        assert_eq!(body["session"]["source"], "x-deepseek-session");
        assert_eq!(body["data"][0]["id"], "deepseek-web/reasoner");
    }

    #[test]
    fn resolves_deepseek_provider_from_header() {
        let request = crate::types::UnifiedRequest {
            model: "local-model".to_string(),
            max_tokens: 16,
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: Default::default(),
            parallel_tool_calls: false,
            stream: false,
            background: false,
            previous_response_id: None,
        };
        let options = UpstreamRequestOptions {
            provider: Some("deepseek-web".to_string()),
            ..Default::default()
        };

        assert_eq!(
            super::resolve_provider(&request, &options),
            ProviderKind::DeepSeekWeb
        );
    }
}
