use std::collections::HashMap;
use std::fmt;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::error::AdapterError;

const DEFAULT_BASE_URL: &str = "https://chat.deepseek.com";
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const CURRENT_APP_VERSION: &str = "2.0.0";

static SESSION_MAP: OnceLock<Mutex<HashMap<String, DeepSeekSessionState>>> = OnceLock::new();

#[derive(Debug, Clone, Default)]
struct DeepSeekSessionState {
    session_id: String,
    parent_message_id: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct DeepSeekWebResponse {
    pub text: String,
    pub reasoning: Option<String>,
    pub session_id: String,
    pub parent_message_id: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepSeekStreamEvent {
    ReasoningDelta(String),
    TextDelta(String),
    SessionId(String),
    Done {
        text: String,
        reasoning: Option<String>,
        session_id: String,
        parent_message_id: Option<Value>,
    },
}

#[derive(Clone)]
pub struct DeepSeekWebClient {
    http: reqwest::Client,
    cookie: String,
    bearer: Option<String>,
    user_agent: String,
    session_key_override: Option<String>,
    parent_message_id_override: Option<Value>,
    runtime_session_key: String,
}

impl fmt::Debug for DeepSeekWebClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeepSeekWebClient")
            .field("cookie", &redact_secret(&self.cookie))
            .field("bearer", &self.bearer.as_deref().map(redact_secret))
            .field("user_agent", &self.user_agent)
            .field("session_key_override", &self.session_key_override)
            .field(
                "parent_message_id_override",
                &self.parent_message_id_override,
            )
            .field("runtime_session_key", &self.runtime_session_key)
            .finish_non_exhaustive()
    }
}

impl DeepSeekWebClient {
    pub fn new(cookie_or_json: impl Into<String>) -> Self {
        let raw = cookie_or_json.into();
        let mut options = DeepSeekWebOptions::from_raw(&raw);
        if let Some(bearer) = options.bearer.as_deref() {
            if bearer.starts_with('{') {
                if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(bearer) {
                    if let Some(Value::String(value)) = map.get("value") {
                        options.bearer = Some(value.clone());
                    }
                }
            }
        }

        let http = reqwest::Client::builder().build().unwrap_or_default();

        Self {
            http,
            cookie: options.cookie,
            bearer: options.bearer,
            user_agent: options
                .user_agent
                .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string()),
            session_key_override: options.last_session_id.filter(|id| !id.trim().is_empty()),
            parent_message_id_override: options.last_parent_message_id,
            runtime_session_key: format!("new-{}", Uuid::new_v4()),
        }
    }

    pub async fn complete(
        &self,
        model: &str,
        prompt: &str,
    ) -> Result<DeepSeekWebResponse, AdapterError> {
        let session_state = self.get_session_state().await?;
        let payload = completion_payload(model, prompt, &session_state);

        let response = self.post_raw("/api/v0/chat/completion", &payload).await?;
        let text = response
            .text()
            .await
            .map_err(|error| AdapterError::Upstream(format!("deepseek web response: {error}")))?;

        let mut output = String::new();
        let mut reasoning = String::new();
        let mut last_fragment_kind = None;
        for parsed in parse_deepseek_sse_values(&text) {
            if let Some(parent_id) = extract_parent_message_id(&parsed) {
                self.update_parent_message_id(parent_id);
            }
            for (kind, content) in extract_deepseek_deltas(&parsed, &mut last_fragment_kind) {
                match kind {
                    DeepSeekDeltaKind::Thinking => reasoning.push_str(&content),
                    DeepSeekDeltaKind::Text => output.push_str(&content),
                }
            }
        }

        if output.trim().is_empty() && reasoning.trim().is_empty() {
            output = text;
        }

        Ok(DeepSeekWebResponse {
            text: output,
            reasoning: (!reasoning.trim().is_empty()).then(|| reasoning.trim().to_string()),
            session_id: session_state.session_id,
            parent_message_id: self.current_parent_message_id(),
        })
    }

    pub async fn complete_stream(
        &self,
        model: &str,
        prompt: &str,
    ) -> Result<mpsc::Receiver<Result<DeepSeekStreamEvent, AdapterError>>, AdapterError> {
        let session_state = self.get_session_state().await?;
        let payload = completion_payload(model, prompt, &session_state);
        let response = self.post_raw("/api/v0/chat/completion", &payload).await?;
        let mut bytes = response.bytes_stream();
        let client = self.clone();
        let session_id = session_state.session_id.clone();
        let (tx, rx) = mpsc::channel(128);
        tokio::spawn(async move {
            let mut buffer = String::new();
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut last_fragment_kind = None;
            let mut parent_message_id = session_state.parent_message_id.clone();

            while let Some(chunk) = bytes.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        let _ = tx
                            .send(Err(AdapterError::Upstream(format!(
                                "deepseek web stream: {error}"
                            ))))
                            .await;
                        return;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                drain_deepseek_stream_lines(
                    &client,
                    &tx,
                    &mut buffer,
                    &mut text,
                    &mut reasoning,
                    &mut last_fragment_kind,
                    &mut parent_message_id,
                )
                .await;
            }

            if !buffer.trim().is_empty() {
                let mut line = String::new();
                std::mem::swap(&mut line, &mut buffer);
                process_deepseek_stream_line(
                    &client,
                    &tx,
                    &line,
                    &mut text,
                    &mut reasoning,
                    &mut last_fragment_kind,
                    &mut parent_message_id,
                )
                .await;
            }

            let parent_message_id = client.current_parent_message_id().or(parent_message_id);
            let _ = tx
                .send(Ok(DeepSeekStreamEvent::Done {
                    text,
                    reasoning: (!reasoning.trim().is_empty()).then(|| reasoning.trim().to_string()),
                    session_id,
                    parent_message_id,
                }))
                .await;
        });
        Ok(rx)
    }

    fn headers(&self) -> Result<reqwest::header::HeaderMap, AdapterError> {
        use reqwest::header::{
            HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE, COOKIE, ORIGIN, REFERER, USER_AGENT,
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.user_agent)
                .map_err(|error| AdapterError::Upstream(format!("invalid user-agent: {error}")))?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ORIGIN, HeaderValue::from_static(DEFAULT_BASE_URL));
        headers.insert(REFERER, HeaderValue::from_static(DEFAULT_BASE_URL));
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&self.cookie)
                .map_err(|error| AdapterError::Upstream(format!("invalid cookie: {error}")))?,
        );
        headers.insert(
            "x-app-version",
            HeaderValue::from_static(CURRENT_APP_VERSION),
        );
        headers.insert(
            "x-client-version",
            HeaderValue::from_static(CURRENT_APP_VERSION),
        );
        headers.insert(
            "accept-language",
            HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
        );
        headers.insert("x-client-locale", HeaderValue::from_static("zh_CN"));
        headers.insert("x-client-platform", HeaderValue::from_static("web"));
        headers.insert(
            "x-client-timezone-offset",
            HeaderValue::from_static("28800"),
        );

        if let Some(bearer) = &self.bearer {
            let value = HeaderValue::from_str(&format!("Bearer {bearer}")).map_err(|error| {
                AdapterError::Upstream(format!("invalid authorization header: {error}"))
            })?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        Ok(headers)
    }

    fn session_key(&self) -> String {
        self.session_key_override
            .clone()
            .unwrap_or_else(|| self.runtime_session_key.clone())
    }

    async fn get_session_state(&self) -> Result<DeepSeekSessionState, AdapterError> {
        let key = self.session_key();
        {
            let map = SESSION_MAP
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .expect("DeepSeek session map poisoned");
            if let Some(state) = map.get(&key) {
                return Ok(state.clone());
            }
        }

        if let Some(id) = &self.session_key_override {
            let state = DeepSeekSessionState {
                session_id: id.clone(),
                parent_message_id: self.parent_message_id_override.clone(),
            };
            SESSION_MAP
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .expect("DeepSeek session map poisoned")
                .insert(key, state.clone());
            return Ok(state);
        }

        let response = self
            .http
            .post(format!("{DEFAULT_BASE_URL}/api/v0/chat_session/create"))
            .headers(self.headers()?)
            .body(
                serde_json::to_vec(&json!({ "character_id": null })).map_err(|error| {
                    AdapterError::Upstream(format!("deepseek web session request json: {error}"))
                })?,
            )
            .send()
            .await
            .map_err(|error| AdapterError::Upstream(format!("deepseek web session: {error}")))?;
        let response = expect_success_response(response).await?;
        let text = response.text().await.map_err(|error| {
            AdapterError::Upstream(format!("deepseek web session body: {error}"))
        })?;
        let payload: Value = serde_json::from_str(&text).map_err(|error| {
            AdapterError::Upstream(format!("deepseek web session json: {error}: {text}"))
        })?;
        let id = find_deepseek_session_id(&payload).ok_or_else(|| {
            AdapterError::Upstream(format!(
                "DeepSeek create-session response did not include a session id: {}",
                truncate_for_error(&text, 600)
            ))
        })?;
        let state = DeepSeekSessionState {
            session_id: id,
            parent_message_id: None,
        };
        SESSION_MAP
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("DeepSeek session map poisoned")
            .insert(key, state.clone());
        Ok(state)
    }

    fn current_parent_message_id(&self) -> Option<Value> {
        let key = self.session_key();
        SESSION_MAP
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("DeepSeek session map poisoned")
            .get(&key)
            .and_then(|state| state.parent_message_id.clone())
    }

    fn update_parent_message_id(&self, parent_message_id: Value) {
        if parent_message_id.is_null() {
            return;
        }
        let key = self.session_key();
        let mut map = SESSION_MAP
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("DeepSeek session map poisoned");
        map.entry(key)
            .and_modify(|state| state.parent_message_id = Some(parent_message_id.clone()))
            .or_insert_with(|| DeepSeekSessionState {
                session_id: self.session_key_override.clone().unwrap_or_default(),
                parent_message_id: Some(parent_message_id),
            });
    }

    async fn create_pow_challenge(&self, target_path: &str) -> Result<Value, AdapterError> {
        let response = self
            .http
            .post(format!(
                "{DEFAULT_BASE_URL}/api/v0/chat/create_pow_challenge"
            ))
            .headers(self.headers()?)
            .body(
                serde_json::to_vec(&json!({ "target_path": target_path })).map_err(|error| {
                    AdapterError::Upstream(format!("deepseek web pow request json: {error}"))
                })?,
            )
            .send()
            .await
            .map_err(|error| AdapterError::Upstream(format!("deepseek web pow: {error}")))?;
        let response = expect_success_response(response).await?;
        let text = response
            .text()
            .await
            .map_err(|error| AdapterError::Upstream(format!("deepseek web pow body: {error}")))?;
        let mut payload: Value = serde_json::from_str(&text).map_err(|error| {
            AdapterError::Upstream(format!("deepseek web pow json: {error}: {text}"))
        })?;
        payload
            .pointer_mut("/data/biz_data/challenge")
            .map(Value::take)
            .or_else(|| payload.pointer_mut("/data/challenge").map(Value::take))
            .or_else(|| payload.pointer_mut("/challenge").map(Value::take))
            .ok_or_else(|| {
                AdapterError::Upstream(format!("missing DeepSeek Web PoW challenge: {text}"))
            })
    }

    fn solve_pow(challenge: &Value) -> Result<Value, AdapterError> {
        if challenge
            .get("algorithm")
            .and_then(Value::as_str)
            .is_some_and(|algorithm| algorithm.eq_ignore_ascii_case("sha256"))
        {
            return solve_sha256_pow(challenge);
        }

        let script_content = include_str!("pow_solver.js");
        let temp_script =
            std::env::temp_dir().join("model_toolcall_adapter_deepseek_pow_solver.js");
        std::fs::write(&temp_script, script_content)
            .map_err(|error| AdapterError::Upstream(format!("write PoW solver: {error}")))?;
        let challenge = serde_json::to_string(challenge)
            .map_err(|error| AdapterError::Upstream(format!("serialize PoW challenge: {error}")))?;
        let output = Command::new(node_command())
            .arg(&temp_script)
            .arg(&challenge)
            .output()
            .map_err(|error| {
                AdapterError::Upstream(format!(
                    "failed to execute Node.js PoW solver for unsupported DeepSeek PoW algorithm. \
Install Node.js or put it on PATH, or update the adapter: {error}"
                ))
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AdapterError::Upstream(format!(
                "PoW solver failed: {}; stderr: {}",
                stdout.trim(),
                stderr.trim()
            )));
        }

        #[derive(Deserialize)]
        struct SolverResult {
            status: String,
            answer: Value,
            message: Option<String>,
        }

        let result: SolverResult = serde_json::from_str(&stdout).map_err(|error| {
            AdapterError::Upstream(format!("parse PoW solver output: {error}: {stdout}"))
        })?;
        if result.status == "success" {
            Ok(result.answer)
        } else {
            Err(AdapterError::Upstream(format!(
                "PoW solver failed: {}",
                result
                    .message
                    .unwrap_or_else(|| "unknown error".to_string())
            )))
        }
    }

    async fn post_raw(
        &self,
        endpoint: &str,
        payload: &Value,
    ) -> Result<reqwest::Response, AdapterError> {
        let challenge = self.create_pow_challenge(endpoint).await?;
        let answer = Self::solve_pow(&challenge)?;
        let pow_response =
            serde_json::to_string(&deepseek_pow_response_payload(&challenge, answer, endpoint))
                .map_err(|error| {
                    AdapterError::Upstream(format!("serialize DeepSeek PoW response: {error}"))
                })?;

        let mut headers = self.headers()?;
        headers.insert(
            "x-ds-pow-response",
            reqwest::header::HeaderValue::from_str(
                &base64::engine::general_purpose::STANDARD.encode(pow_response),
            )
            .map_err(|error| AdapterError::Upstream(format!("invalid PoW header: {error}")))?,
        );

        let response = self
            .http
            .post(format!("{DEFAULT_BASE_URL}{endpoint}"))
            .headers(headers)
            .body(serde_json::to_vec(payload).map_err(|error| {
                AdapterError::Upstream(format!("deepseek web completion json: {error}"))
            })?)
            .send()
            .await
            .map_err(|error| AdapterError::Upstream(format!("deepseek web completion: {error}")))?;
        expect_success_response(response).await
    }
}

#[derive(Debug, Deserialize)]
struct DeepSeekWebOptions {
    cookie: String,
    bearer: Option<String>,
    user_agent: Option<String>,
    last_session_id: Option<String>,
    last_parent_message_id: Option<Value>,
}

impl DeepSeekWebOptions {
    fn from_raw(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_else(|_| Self {
            cookie: raw.to_string(),
            bearer: None,
            user_agent: None,
            last_session_id: None,
            last_parent_message_id: None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeepSeekDeltaKind {
    Thinking,
    Text,
}

fn completion_payload(model: &str, prompt: &str, session_state: &DeepSeekSessionState) -> Value {
    let thinking_enabled = model.contains("reasoner") || model.contains("thinking");
    let search_enabled = model.contains("search");
    json!({
        "chat_session_id": session_state.session_id,
        "parent_message_id": session_state.parent_message_id,
        "model_type": deepseek_model_type(model),
        "prompt": truncate_middle_to_bytes(prompt, 64 * 1024),
        "ref_file_ids": [],
        "thinking_enabled": thinking_enabled,
        "search_enabled": search_enabled,
        "preempt": false,
    })
}

async fn drain_deepseek_stream_lines(
    client: &DeepSeekWebClient,
    tx: &mpsc::Sender<Result<DeepSeekStreamEvent, AdapterError>>,
    buffer: &mut String,
    text: &mut String,
    reasoning: &mut String,
    last_fragment_kind: &mut Option<DeepSeekDeltaKind>,
    parent_message_id: &mut Option<Value>,
) {
    while let Some(index) = buffer.find('\n') {
        let line = buffer[..index].to_string();
        buffer.drain(..=index);
        process_deepseek_stream_line(
            client,
            tx,
            &line,
            text,
            reasoning,
            last_fragment_kind,
            parent_message_id,
        )
        .await;
    }
}

async fn process_deepseek_stream_line(
    client: &DeepSeekWebClient,
    tx: &mpsc::Sender<Result<DeepSeekStreamEvent, AdapterError>>,
    line: &str,
    text: &mut String,
    reasoning: &mut String,
    last_fragment_kind: &mut Option<DeepSeekDeltaKind>,
    parent_message_id: &mut Option<Value>,
) {
    let Some(parsed) = parse_deepseek_sse_line(line) else {
        return;
    };
    if let Some(parent_id) = extract_parent_message_id(&parsed) {
        *parent_message_id = Some(parent_id.clone());
        client.update_parent_message_id(parent_id);
    }
    if let Some(session_id) = find_deepseek_session_id(&parsed) {
        let _ = tx
            .send(Ok(DeepSeekStreamEvent::SessionId(session_id)))
            .await;
    }
    for (kind, content) in extract_deepseek_deltas(&parsed, last_fragment_kind) {
        match kind {
            DeepSeekDeltaKind::Thinking => {
                reasoning.push_str(&content);
                let _ = tx
                    .send(Ok(DeepSeekStreamEvent::ReasoningDelta(content)))
                    .await;
            }
            DeepSeekDeltaKind::Text => {
                text.push_str(&content);
                let _ = tx.send(Ok(DeepSeekStreamEvent::TextDelta(content))).await;
            }
        }
    }
}

async fn expect_success_response(
    response: reqwest::Response,
) -> Result<reqwest::Response, AdapterError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(AdapterError::Upstream(format!(
        "DeepSeek Web returned HTTP {status}: {body}"
    )))
}

fn node_command() -> &'static str {
    "node"
}

fn solve_sha256_pow(challenge: &Value) -> Result<Value, AdapterError> {
    let target = challenge
        .get("challenge")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AdapterError::Upstream("sha256 PoW challenge is missing challenge".into())
        })?;
    let salt = challenge
        .get("salt")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Upstream("sha256 PoW challenge is missing salt".into()))?;
    let difficulty = challenge
        .get("difficulty")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            AdapterError::Upstream("sha256 PoW challenge is missing difficulty".into())
        })?;
    let target_difficulty = if difficulty > 1000 {
        (u64::BITS - 1 - difficulty.leading_zeros()) as u32
    } else {
        difficulty as u32
    };

    for nonce in 0_u64..=10_000_000 {
        let mut hasher = Sha256::new();
        hasher.update(format!("{salt}{target}{nonce}").as_bytes());
        let digest = hasher.finalize();
        if leading_zero_bits(&digest) >= target_difficulty {
            return Ok(json!({
                "algorithm": "sha256",
                "challenge": target,
                "salt": salt,
                "answer": nonce,
                "signature": true,
                "target_path": challenge.get("target_path").cloned().unwrap_or(Value::Null)
            }));
        }
    }

    Err(AdapterError::Upstream(
        "sha256 PoW solver timeout after 10000000 attempts".into(),
    ))
}

fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut total = 0;
    for byte in bytes {
        if *byte == 0 {
            total += 8;
        } else {
            total += byte.leading_zeros();
            break;
        }
    }
    total
}

fn deepseek_model_type(model: &str) -> &'static str {
    let normalized = model.to_ascii_lowercase();
    if normalized.contains("vision") {
        "vision"
    } else if normalized.contains("expert") || normalized.contains("pro") {
        "expert"
    } else {
        "default"
    }
}

fn parse_deepseek_sse_values(text: &str) -> Vec<Value> {
    text.lines().filter_map(parse_deepseek_sse_line).collect()
}

fn parse_deepseek_sse_line(line: &str) -> Option<Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed == "data: [DONE]" {
        return None;
    }
    let json_str = trimmed.strip_prefix("data: ").unwrap_or(trimmed);
    serde_json::from_str::<Value>(json_str).ok()
}

fn extract_parent_message_id(payload: &Value) -> Option<Value> {
    [
        "/data/biz_data/response_message_id",
        "/data/biz_data/message_id",
        "/data/biz_data/msg_id",
        "/data/biz_data/chunk/response_message_id",
        "/data/biz_data/chunk/message_id",
        "/data/biz_data/chunk/msg_id",
        "/data/response_message_id",
        "/data/message_id",
        "/data/msg_id",
        "/response_message_id",
        "/message_id",
        "/msg_id",
    ]
    .iter()
    .find_map(|pointer| {
        let value = payload.pointer(pointer)?;
        match value {
            Value::String(raw) if !raw.trim().is_empty() => Some(Value::String(raw.clone())),
            Value::Number(_) => Some(value.clone()),
            _ => None,
        }
    })
}

fn extract_deepseek_deltas(
    payload: &Value,
    last_fragment_kind: &mut Option<DeepSeekDeltaKind>,
) -> Vec<(DeepSeekDeltaKind, String)> {
    let mut deltas = Vec::new();

    if let Some(chunk) = payload.pointer("/data/biz_data/chunk") {
        if let Some(content) = deepseek_content_text(chunk) {
            let kind = deepseek_kind_from_type(
                chunk
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            push_deepseek_delta(&mut deltas, kind, content);
            *last_fragment_kind = Some(kind);
        }
    }

    if let Some(chunk_type) = payload.get("type").and_then(Value::as_str) {
        if let Some(content) = payload.get("content").and_then(Value::as_str) {
            let kind = deepseek_kind_from_type(chunk_type);
            push_deepseek_delta(&mut deltas, kind, content);
            *last_fragment_kind = Some(kind);
        }
    }

    if let Some(value) = payload.get("v").and_then(Value::as_str) {
        let path = payload.get("p").and_then(Value::as_str).unwrap_or_default();
        let kind = if path.contains("reasoning")
            || path.contains("thinking")
            || path.contains("thought")
            || payload.get("type").and_then(Value::as_str) == Some("thinking")
        {
            DeepSeekDeltaKind::Thinking
        } else if path.is_empty() || path.contains("content") || path.contains("choices") {
            last_fragment_kind.unwrap_or(DeepSeekDeltaKind::Text)
        } else {
            DeepSeekDeltaKind::Text
        };
        push_deepseek_delta(&mut deltas, kind, value);
        *last_fragment_kind = Some(kind);
    }

    for fragment in deepseek_fragments(payload) {
        let kind = fragment
            .get("type")
            .and_then(Value::as_str)
            .map(deepseek_kind_from_type)
            .unwrap_or(DeepSeekDeltaKind::Text);
        *last_fragment_kind = Some(kind);
        if let Some(content) = fragment
            .get("content")
            .or_else(|| fragment.get("v"))
            .or_else(|| fragment.get("text"))
            .or_else(|| fragment.get("delta"))
            .and_then(Value::as_str)
        {
            push_deepseek_delta(&mut deltas, kind, content);
        }
    }

    for pointer in [
        "/data/biz_data/content",
        "/data/biz_data/text",
        "/data/biz_data/message/content",
        "/data/biz_data/message",
        "/data/biz_data/answer",
        "/data/content",
        "/data/text",
        "/data/message/content",
        "/data/message",
        "/data/answer",
    ] {
        if let Some(content) = payload.pointer(pointer).and_then(Value::as_str) {
            push_deepseek_delta(&mut deltas, DeepSeekDeltaKind::Text, content);
            *last_fragment_kind = Some(DeepSeekDeltaKind::Text);
        }
    }

    for pointer in [
        "/data/biz_data/reasoning_content",
        "/data/biz_data/thinking_content",
        "/data/biz_data/thinking",
        "/data/reasoning_content",
        "/data/thinking_content",
        "/data/thinking",
        "/reasoning_content",
        "/thinking_content",
    ] {
        if let Some(content) = payload.pointer(pointer).and_then(Value::as_str) {
            push_deepseek_delta(&mut deltas, DeepSeekDeltaKind::Thinking, content);
            *last_fragment_kind = Some(DeepSeekDeltaKind::Thinking);
        }
    }

    if deltas.is_empty() {
        if let Some(error) = extract_deepseek_error_text(payload) {
            push_deepseek_delta(&mut deltas, DeepSeekDeltaKind::Text, &error);
        }
    }

    deltas
}

fn deepseek_content_text(value: &Value) -> Option<&str> {
    [
        "content",
        "text",
        "delta",
        "message",
        "answer",
        "reasoning_content",
        "thinking_content",
    ]
    .iter()
    .find_map(|key| value.get(key).and_then(Value::as_str))
}

fn push_deepseek_delta(
    deltas: &mut Vec<(DeepSeekDeltaKind, String)>,
    kind: DeepSeekDeltaKind,
    content: &str,
) {
    if !content.is_empty() {
        deltas.push((kind, content.to_string()));
    }
}

fn deepseek_kind_from_type(chunk_type: &str) -> DeepSeekDeltaKind {
    match chunk_type.to_ascii_uppercase().as_str() {
        "THINK" | "THINKING" | "REASONING" => DeepSeekDeltaKind::Thinking,
        _ => DeepSeekDeltaKind::Text,
    }
}

fn deepseek_fragments(payload: &Value) -> Vec<&Value> {
    if let Some(fragment) = payload
        .get("v")
        .and_then(Value::as_object)
        .filter(|object| object.contains_key("type"))
        .map(|_| payload.get("v").expect("v checked"))
    {
        return vec![fragment];
    }

    payload
        .pointer("/v/response/fragments")
        .and_then(Value::as_array)
        .or_else(|| payload.get("v").and_then(Value::as_array))
        .map(|fragments| fragments.iter().collect())
        .unwrap_or_default()
}

fn extract_deepseek_error_text(payload: &Value) -> Option<String> {
    let code = payload
        .get("code")
        .or_else(|| payload.pointer("/data/biz_code"))
        .or_else(|| payload.pointer("/data/code"))
        .and_then(Value::as_i64);
    let message = payload
        .get("msg")
        .or_else(|| payload.get("message"))
        .or_else(|| payload.pointer("/data/biz_msg"))
        .or_else(|| payload.pointer("/data/msg"))
        .or_else(|| payload.pointer("/error/message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());

    match (code, message) {
        (Some(0) | None, _) => None,
        (Some(code), Some(message)) => Some(format!("DeepSeek Web returned {code}: {message}")),
        (Some(code), None) => Some(format!("DeepSeek Web returned error code {code}")),
    }
}

fn find_deepseek_session_id(payload: &Value) -> Option<String> {
    [
        "/data/biz_data/id",
        "/data/biz_data/chat_session_id",
        "/data/biz_data/session_id",
        "/data/biz_data/chat_session/id",
        "/data/biz_data/chat_session/chat_session_id",
        "/data/id",
        "/data/chat_session_id",
        "/id",
    ]
    .iter()
    .find_map(|pointer| {
        payload
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn deepseek_pow_response_payload(challenge: &Value, answer: Value, target_path: &str) -> Value {
    let mut pow_payload = challenge.clone();
    if let Some(obj) = pow_payload.as_object_mut() {
        obj.insert("answer".to_string(), answer);
        obj.insert("target_path".to_string(), json!(target_path));
    }
    pow_payload
}

fn truncate_middle_to_bytes(content: &str, max_bytes: usize) -> String {
    let trimmed = content.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_string();
    }

    let marker = "\n\n[truncated middle for deepseek web request budget]\n\n";
    let budget = max_bytes.saturating_sub(marker.len());
    let head_budget = budget / 2;
    let tail_budget = budget.saturating_sub(head_budget);
    let mut head_end = 0;
    for (idx, ch) in trimmed.char_indices() {
        let next = idx + ch.len_utf8();
        if next > head_budget {
            break;
        }
        head_end = next;
    }

    let mut tail_start = trimmed.len();
    let mut used_tail = 0;
    for (idx, ch) in trimmed.char_indices().rev() {
        let next = used_tail + ch.len_utf8();
        if next > tail_budget {
            break;
        }
        used_tail = next;
        tail_start = idx;
    }

    let mut output = trimmed[..head_end].to_string();
    output.push_str(marker);
    output.push_str(&trimmed[tail_start..]);
    output
}

fn truncate_for_error(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out = trimmed.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

fn redact_secret(secret: &str) -> String {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let prefix = trimmed.chars().take(4).collect::<String>();
    format!("{prefix}...[redacted]")
}

#[allow(dead_code)]
fn current_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{leading_zero_bits, solve_sha256_pow, truncate_middle_to_bytes};
    use serde_json::json;
    use sha2::Digest;

    #[test]
    fn deepseek_prompt_truncation_preserves_head_and_latest_user_request() {
        let prompt = format!(
            "<tool_protocol>must call tools</tool_protocol>\n{}\nuser: 看看当前项目有什么问题",
            "x".repeat(10_000)
        );

        let truncated = truncate_middle_to_bytes(&prompt, 1024);

        assert!(truncated.contains("<tool_protocol>must call tools</tool_protocol>"));
        assert!(truncated.contains("[truncated middle for deepseek web request budget]"));
        assert!(truncated.contains("user: 看看当前项目有什么问题"));
    }

    #[test]
    fn solves_sha256_pow_without_node_runtime() {
        let answer = solve_sha256_pow(&json!({
            "algorithm": "sha256",
            "challenge": "unit",
            "salt": "test-",
            "difficulty": 4,
            "target_path": "/api/v0/chat/completion"
        }))
        .unwrap();

        let nonce = answer["answer"].as_u64().unwrap();
        let digest = sha2::Sha256::digest(format!("test-unit{nonce}").as_bytes());

        assert_eq!(answer["algorithm"], "sha256");
        assert!(leading_zero_bits(&digest) >= 4);
    }
}
