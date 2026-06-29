use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

use crate::config::AppConfig;
use crate::error::AdapterError;
use crate::providers::{
    self, deepseek_web::DeepSeekStreamEvent, openai_compat::OpenAiCompatClient,
};
use crate::types::UnifiedRequest;

#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    pub text: String,
    pub reasoning: Option<String>,
    pub provider_session_id: Option<String>,
}

#[derive(Clone)]
pub struct OpenAiChatUpstream {
    openai_compat: OpenAiCompatClient,
}

#[derive(Debug, Clone, Default)]
pub struct UpstreamRequestOptions {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub deepseek_session: Option<String>,
    pub provider_session_id: Option<String>,
}

impl UpstreamRequestOptions {
    pub fn redacted(&self) -> RedactedUpstreamRequestOptions<'_> {
        RedactedUpstreamRequestOptions {
            provider: self.provider.as_deref(),
            base_url: self.base_url.as_deref(),
            api_key_present: self
                .api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            deepseek_session_present: self
                .deepseek_session
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            provider_session_id: self.provider_session_id.as_deref(),
        }
    }
}

pub struct RedactedUpstreamRequestOptions<'a> {
    pub provider: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub api_key_present: bool,
    pub deepseek_session_present: bool,
    pub provider_session_id: Option<&'a str>,
}

impl fmt::Debug for RedactedUpstreamRequestOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedactedUpstreamRequestOptions")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("api_key_present", &self.api_key_present)
            .field("deepseek_session_present", &self.deepseek_session_present)
            .field("provider_session_id", &self.provider_session_id)
            .finish()
    }
}

impl OpenAiChatUpstream {
    pub fn new(config: &AppConfig) -> Result<Self, AdapterError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|error| AdapterError::Upstream(error.to_string()))?;
        let openai_compat = OpenAiCompatClient::new(
            client.clone(),
            config.upstream_base_url.clone(),
            config.upstream_api_key.clone(),
        );
        Ok(Self { openai_compat })
    }

    pub async fn complete(
        &self,
        request: &UnifiedRequest,
        prompt: &str,
        options: &UpstreamRequestOptions,
    ) -> Result<UpstreamResponse, AdapterError> {
        providers::complete(request, prompt, options, &self.openai_compat).await
    }

    pub async fn complete_stream(
        &self,
        request: &UnifiedRequest,
        prompt: &str,
        options: &UpstreamRequestOptions,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<DeepSeekStreamEvent, AdapterError>>, AdapterError>
    {
        match providers::resolve_provider(request, options) {
            providers::ProviderKind::DeepSeekWeb => {
                providers::deepseek_web::complete_stream(request, prompt, options).await
            }
            providers::ProviderKind::OpenAiCompat => Err(AdapterError::StreamUnsupported),
        }
    }

    pub async fn list_models(
        &self,
        fallback_model: &str,
        model_aliases: &HashMap<String, String>,
        options: &UpstreamRequestOptions,
    ) -> Result<Value, AdapterError> {
        providers::list_models(fallback_model, model_aliases, options, &self.openai_compat).await
    }
}
