use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path as FsPath, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::config::{update_local_config, LocalConfig};
use crate::error::AdapterError;
use crate::http::{AppState, DeepSeekBrowserProcess};
use crate::protocol::{parse_tool_calls, render_tool_protocol_prompt};
use crate::providers::deepseek_web::default_session_path as default_deepseek_session_path;
use crate::responses_store;
use crate::types::{ParsedToolCall, UnifiedRequest};
use crate::ui::INDEX_HTML;
use crate::upstream::UpstreamRequestOptions;
use crate::wire::{chat, messages, responses, WireMode};

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

pub async fn ui() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub async fn setup_state(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AdapterError> {
    let local = LocalConfig::load_or_default()
        .map_err(|error| AdapterError::Upstream(format!("read local config: {error}")))?;
    let browser = {
        let guard = state
            .setup
            .lock()
            .map_err(|_| AdapterError::Upstream("setup state lock poisoned".to_string()))?;
        guard.deepseek_browser.as_ref().map(|browser| {
            json!({
                "port": browser.port,
                "user_data_dir": browser.user_data_dir,
                "debug_url": format!("http://127.0.0.1:{}", browser.port),
                "running": browser.child.is_some(),
            })
        })
    };
    let session_status = crate::providers::deepseek_web::session_status_from_options(
        &UpstreamRequestOptions::default(),
    );
    Ok(Json(json!({
        "setup": local.setup_json(&state.config.bind),
        "deepseek_browser": browser,
        "deepseek_session": session_status,
    })))
}

pub async fn setup_provider(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AdapterError> {
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| matches!(*value, "openai-compatible" | "deepseek-web"))
        .ok_or_else(|| {
            AdapterError::InvalidRequest(
                "provider must be openai-compatible or deepseek-web".to_string(),
            )
        })?;
    let local = update_local_config(|config| {
        config.provider = Some(provider.to_string());
        if provider == "deepseek-web" {
            config.upstream_model = Some("deepseek-web/reasoner".to_string());
        }
    })
    .map_err(|error| AdapterError::Upstream(format!("write local config: {error}")))?;
    Ok(Json(json!({
        "status": "saved",
        "setup": local.setup_json(&state.config.bind),
    })))
}

pub async fn setup_deepseek_browser_start(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AdapterError> {
    let browser = start_deepseek_browser()?;
    let body = json!({
        "status": "opened",
        "login_url": "https://chat.deepseek.com/",
        "debug_url": format!("http://127.0.0.1:{}", browser.port),
        "port": browser.port,
        "user_data_dir": browser.user_data_dir,
    });
    let mut guard = state
        .setup
        .lock()
        .map_err(|_| AdapterError::Upstream("setup state lock poisoned".to_string()))?;
    guard.deepseek_browser = Some(browser);
    Ok(Json(body))
}

pub async fn setup_deepseek_browser_capture(
    State(state): State<Arc<AppState>>,
    payload: Option<Json<Value>>,
) -> Result<Json<Value>, AdapterError> {
    let payload = payload
        .map(|Json(payload)| payload)
        .unwrap_or_else(|| json!({}));
    let port = payload
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .or_else(|| {
            state
                .setup
                .lock()
                .ok()
                .and_then(|guard| guard.deepseek_browser.as_ref().map(|browser| browser.port))
        })
        .unwrap_or(DEFAULT_DEEPSEEK_DEBUG_PORT);
    let session = capture_deepseek_session(port).await?;
    let path = default_deepseek_session_path()?;
    save_deepseek_session(&path, &session)?;
    let status = crate::providers::deepseek_web::session_status_from_options(
        &UpstreamRequestOptions::default(),
    );
    Ok(Json(json!({
        "status": "captured",
        "session_file": path,
        "session": status,
    })))
}

pub async fn setup_codex_apply(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AdapterError> {
    let local = LocalConfig::load_or_default()
        .map_err(|error| AdapterError::Upstream(format!("read local config: {error}")))?;
    let key = local.adapter_api_key.trim().to_string();
    if key.is_empty() {
        return Err(AdapterError::InvalidRequest(
            "adapter api key is missing; reload setup state first".to_string(),
        ));
    }
    let provider = "ModelToolCallAdapter";
    let model = if local.provider.as_deref() == Some("deepseek-web") {
        "deepseek-web/reasoner"
    } else {
        local
            .upstream_model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("local-model")
    };
    let codex_dir = codex_home_dir()?;
    std::fs::create_dir_all(&codex_dir).map_err(|error| {
        AdapterError::Upstream(format!(
            "failed to create Codex config dir {}: {error}",
            codex_dir.display()
        ))
    })?;
    let config_path = codex_dir.join("config.toml");
    let auth_path = codex_dir.join("auth.json");
    let mut backups = Vec::new();
    if let Some(path) = backup_file(&config_path)? {
        backups.push(path);
    }
    if let Some(path) = backup_file(&auth_path)? {
        backups.push(path);
    }

    let base_url = format!("http://{}/v1", state.config.bind);
    write_codex_config(&config_path, provider, model, &base_url)?;
    write_codex_auth(&auth_path, &key)?;

    Ok(Json(json!({
        "status": "configured",
        "codex_dir": codex_dir,
        "config_file": config_path,
        "auth_file": auth_path,
        "backups": backups,
        "provider": provider,
        "model": model,
        "base_url": base_url,
        "auth": {
            "auth_json_key": "OPENAI_API_KEY",
            "requires_openai_auth": true
        },
        "message": "Codex config written. Restart Codex CLI/app before using the adapter provider."
    })))
}

pub async fn deepseek_web_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, AdapterError> {
    verify_auth(&state, &headers)?;
    let session_file = default_deepseek_session_path()?;
    let log_path = deepseek_login_log_path()?;
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            AdapterError::Upstream(format!(
                "failed to create DeepSeek login log dir {}: {error}",
                parent.display()
            ))
        })?;
    }

    let opened = open_deepseek_login_page(&log_path)?;

    Ok(Json(json!({
        "status": if opened { "opened" } else { "manual" },
        "message": "DeepSeek login page opened. After login, paste Session JSON/Cookie into the UI and click login/fetch models; the adapter will save it locally.",
        "login_url": "https://chat.deepseek.com/",
        "session_file": session_file,
        "log_file": log_path,
    })))
}

pub async fn deepseek_web_session_save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AdapterError> {
    verify_auth(&state, &headers)?;
    let session = payload
        .get("session")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AdapterError::InvalidRequest("session is required".to_string()))?;
    let path = default_deepseek_session_path()?;
    save_deepseek_session(&path, session)?;
    Ok(Json(json!({
        "status": "saved",
        "session_file": path,
    })))
}

fn save_deepseek_session(path: &FsPath, session: &str) -> Result<(), AdapterError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            AdapterError::Upstream(format!(
                "failed to create DeepSeek session dir {}: {error}",
                parent.display()
            ))
        })?;
    }
    std::fs::write(path, normalize_deepseek_session_for_storage(session)?).map_err(|error| {
        AdapterError::Upstream(format!(
            "failed to write DeepSeek session {}: {error}",
            path.display()
        ))
    })
}

fn deepseek_login_log_path() -> Result<std::path::PathBuf, AdapterError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AdapterError::Upstream("HOME is not set".to_string()))?;
    Ok(std::path::PathBuf::from(home)
        .join(".model-toolcall-adapter")
        .join("deepseek_login_adapter.log"))
}

fn open_deepseek_login_page(log_path: &std::path::Path) -> Result<bool, AdapterError> {
    let url = "https://chat.deepseek.com/";
    let mut command = if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };

    match command.spawn() {
        Ok(_) => {
            let _ = std::fs::write(log_path, format!("opened {url}\n"));
            Ok(true)
        }
        Err(error) => {
            let _ = std::fs::write(log_path, format!("failed to open {url}: {error}\n"));
            Ok(false)
        }
    }
}

fn normalize_deepseek_session_for_storage(session: &str) -> Result<String, AdapterError> {
    if let Ok(value) = serde_json::from_str::<Value>(session) {
        return serde_json::to_string_pretty(&value)
            .map_err(|error| AdapterError::Upstream(format!("serialize session json: {error}")));
    }
    Ok(session.to_string())
}

const DEFAULT_DEEPSEEK_DEBUG_PORT: u16 = 9223;

fn start_deepseek_browser() -> Result<DeepSeekBrowserProcess, AdapterError> {
    let browser = find_browser_executable().ok_or_else(|| {
        AdapterError::Upstream(
            "Could not find Chrome, Chromium, Edge, or Brave. Install one or paste Session JSON/Cookie manually.".to_string(),
        )
    })?;
    let port = DEFAULT_DEEPSEEK_DEBUG_PORT;
    let user_data_dir = adapter_home_dir()?.join("deepseek-browser-profile");
    std::fs::create_dir_all(&user_data_dir).map_err(|error| {
        AdapterError::Upstream(format!(
            "failed to create browser profile {}: {error}",
            user_data_dir.display()
        ))
    })?;

    let child = Command::new(&browser)
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!(
            "--user-data-dir={}",
            user_data_dir.to_string_lossy()
        ))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-domain-reliability")
        .arg("--disable-features=OptimizationHints,AutofillServerCommunication,MediaRouter")
        .arg("--disable-sync")
        .arg("--disable-extensions")
        .arg("--metrics-recording-only")
        .arg("--safebrowsing-disable-auto-update")
        .arg("https://chat.deepseek.com/")
        .spawn()
        .map_err(|error| {
            AdapterError::Upstream(format!(
                "failed to start controlled browser {}: {error}",
                browser.display()
            ))
        })?;

    Ok(DeepSeekBrowserProcess {
        port,
        user_data_dir,
        child: Some(child),
    })
}

fn find_browser_executable() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe",
        ]
    } else {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/microsoft-edge",
            "/usr/bin/brave-browser",
        ]
    };
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .or_else(|| find_browser_on_path())
}

fn find_browser_on_path() -> Option<PathBuf> {
    [
        "google-chrome",
        "chrome",
        "chromium",
        "chromium-browser",
        "msedge",
        "brave-browser",
    ]
    .iter()
    .find_map(|name| {
        Command::new("which")
            .arg(name)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                (!path.is_empty()).then(|| PathBuf::from(path))
            })
    })
}

fn adapter_home_dir() -> Result<PathBuf, AdapterError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AdapterError::Upstream("HOME is not set".to_string()))?;
    Ok(PathBuf::from(home).join(".model-toolcall-adapter"))
}

async fn capture_deepseek_session(port: u16) -> Result<String, AdapterError> {
    let http = reqwest::Client::new();
    let tabs_url = format!("http://127.0.0.1:{port}/json");
    let tabs = http
        .get(&tabs_url)
        .send()
        .await
        .map_err(|error| {
            AdapterError::Upstream(format!(
                "cannot reach controlled browser at {tabs_url}: {error}"
            ))
        })?
        .json::<Value>()
        .await
        .map_err(|error| AdapterError::Upstream(format!("read browser tabs: {error}")))?;
    let tab = tabs
        .as_array()
        .into_iter()
        .flatten()
        .find(|tab| {
            tab.get("type").and_then(Value::as_str) == Some("page")
                && tab
                    .get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.starts_with("https://chat.deepseek.com"))
        })
        .or_else(|| {
            tabs.as_array().into_iter().flatten().find(|tab| {
                tab.get("type").and_then(Value::as_str) == Some("page")
                    && tab
                        .get("url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| url.contains("chat.deepseek.com"))
            })
        })
        .or_else(|| {
            tabs.as_array().into_iter().flatten().find(|tab| {
                tab.get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.contains("chat.deepseek.com"))
            })
        })
        .or_else(|| tabs.as_array().and_then(|tabs| tabs.first()))
        .ok_or_else(|| {
            AdapterError::Upstream("controlled browser has no debuggable tabs".to_string())
        })?;
    let websocket_url = tab
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AdapterError::Upstream(
                "controlled browser tab does not expose webSocketDebuggerUrl".to_string(),
            )
        })?;

    let (mut ws, _) = tokio_tungstenite::connect_async(websocket_url)
        .await
        .map_err(|error| AdapterError::Upstream(format!("connect CDP websocket: {error}")))?;
    cdp_send(&mut ws, 1, "Network.enable", json!({})).await?;
    let cookies = cdp_send(
        &mut ws,
        2,
        "Network.getCookies",
        json!({ "urls": ["https://chat.deepseek.com/"] }),
    )
    .await?;
    let storage = cdp_send(
        &mut ws,
        3,
        "Runtime.evaluate",
        json!({
            "expression": "JSON.stringify(Object.fromEntries(Array.from({length: localStorage.length}, (_, i) => { const k = localStorage.key(i); return [k, localStorage.getItem(k)]; })))",
            "returnByValue": true
        }),
    )
    .await
    .unwrap_or_else(|_| json!({}));
    let user_agent = cdp_send(
        &mut ws,
        4,
        "Runtime.evaluate",
        json!({
            "expression": "navigator.userAgent",
            "returnByValue": true
        }),
    )
    .await
    .ok()
    .and_then(|value| {
        value
            .pointer("/result/value")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
    .filter(|value| !value.trim().is_empty())
    .unwrap_or_else(|| "Mozilla/5.0".to_string());

    let cookie_header = cookies
        .get("cookies")
        .and_then(Value::as_array)
        .map(|cookies| {
            cookies
                .iter()
                .filter(|cookie| {
                    cookie
                        .get("domain")
                        .and_then(Value::as_str)
                        .is_some_and(|domain| domain.contains("deepseek.com"))
                })
                .filter_map(|cookie| {
                    let name = cookie.get("name").and_then(Value::as_str)?;
                    let value = cookie.get("value").and_then(Value::as_str)?;
                    Some(format!("{name}={value}"))
                })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    if cookie_header.trim().is_empty() {
        return Err(AdapterError::Upstream(
            "DeepSeek cookies were not found. Finish login in the controlled browser, then capture again.".to_string(),
        ));
    }

    let storage_text = storage
        .pointer("/result/value")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let storage_value = serde_json::from_str::<Value>(storage_text).unwrap_or_else(|_| json!({}));
    let bearer = find_deepseek_bearer(&storage_value);
    Ok(json!({
        "cookie": cookie_header,
        "bearer": bearer,
        "user_agent": user_agent,
        "local_storage_keys": storage_value.as_object().map(|object| object.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
    })
    .to_string())
}

async fn cdp_send<S>(
    ws: &mut S,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, AdapterError>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    ws.send(Message::Text(
        json!({ "id": id, "method": method, "params": params }).to_string(),
    ))
    .await
    .map_err(|error| AdapterError::Upstream(format!("send CDP {method}: {error}")))?;
    while let Some(message) = ws.next().await {
        let message = message
            .map_err(|error| AdapterError::Upstream(format!("read CDP {method}: {error}")))?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| AdapterError::Upstream(format!("parse CDP {method}: {error}")))?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(AdapterError::Upstream(format!(
                "CDP {method} failed: {error}"
            )));
        }
        return Ok(value.get("result").cloned().unwrap_or_else(|| json!({})));
    }
    Err(AdapterError::Upstream(format!(
        "CDP {method} did not return a response"
    )))
}

fn find_deepseek_bearer(storage: &Value) -> Option<String> {
    let object = storage.as_object()?;
    for key in ["userToken", "settingsJwt"] {
        if let Some(token) = object
            .get(key)
            .and_then(Value::as_str)
            .and_then(normalize_deepseek_auth_value)
        {
            return Some(token);
        }
    }

    object.iter().find_map(|(key, value)| {
        let key = key.to_ascii_lowercase();
        if key.contains("apm")
            || key.contains("tea")
            || key.contains("aws")
            || key.contains("smid")
            || key.contains("cache")
            || key.contains("challenge")
        {
            return None;
        }
        let raw = value.as_str().unwrap_or_default();
        if raw.trim_start().starts_with("eyJ") || key.contains("token") || key.contains("auth") {
            normalize_deepseek_auth_value(raw)
        } else {
            None
        }
    })
}

fn normalize_deepseek_auth_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.starts_with('{') {
        return Some(trimmed.to_string());
    }
    let parsed = serde_json::from_str::<Value>(trimmed).ok()?;
    find_auth_value_in_json(&parsed)
}

fn find_auth_value_in_json(value: &Value) -> Option<String> {
    match value {
        Value::String(raw) => normalize_deepseek_auth_value(raw),
        Value::Object(object) => [
            "token",
            "access_token",
            "jwt",
            "value",
            "userToken",
            "settingsJwt",
        ]
        .iter()
        .find_map(|key| object.get(*key).and_then(find_auth_value_in_json)),
        _ => None,
    }
}

fn codex_home_dir() -> Result<PathBuf, AdapterError> {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AdapterError::Upstream("HOME is not set".to_string()))?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn backup_file(path: &FsPath) -> Result<Option<PathBuf>, AdapterError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut backup = path.with_extension(match path.extension().and_then(|value| value.to_str()) {
        Some(ext) => format!("{ext}.bak"),
        None => "bak".to_string(),
    });
    if backup.exists() {
        let mut index = 1;
        loop {
            let candidate = PathBuf::from(format!("{}.bak.{index}", path.to_string_lossy()));
            if !candidate.exists() {
                backup = candidate;
                break;
            }
            index += 1;
        }
    }
    std::fs::copy(path, &backup).map_err(|error| {
        AdapterError::Upstream(format!(
            "failed to backup {} to {}: {error}",
            path.display(),
            backup.display()
        ))
    })?;
    Ok(Some(backup))
}

fn write_codex_config(
    path: &FsPath,
    provider: &str,
    model: &str,
    base_url: &str,
) -> Result<(), AdapterError> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let next = replace_codex_managed_blocks(
        &existing,
        &codex_managed_root_block(provider, model),
        &codex_managed_provider_block(provider, base_url),
    );
    std::fs::write(path, next).map_err(|error| {
        AdapterError::Upstream(format!(
            "failed to write Codex config {}: {error}",
            path.display()
        ))
    })
}

fn codex_managed_root_block(provider: &str, model: &str) -> String {
    format!(
        r#"# BEGIN model-toolcall-adapter root
model_provider = "{provider}"
model = "{model}"
review_model = "{model}"
model_reasoning_effort = "xhigh"
disable_response_storage = true
network_access = "enabled"
model_context_window = 1000000
model_auto_compact_token_limit = 900000
# END model-toolcall-adapter root
"#
    )
}

fn codex_managed_provider_block(provider: &str, base_url: &str) -> String {
    format!(
        r#"# BEGIN model-toolcall-adapter provider
[model_providers.{provider}]
name = "{provider}"
base_url = "{base_url}"
wire_api = "responses"
requires_openai_auth = true
# END model-toolcall-adapter provider
"#
    )
}

fn replace_codex_managed_blocks(existing: &str, root_block: &str, provider_block: &str) -> String {
    let without_root = remove_managed_block(
        existing,
        "# BEGIN model-toolcall-adapter root",
        "# END model-toolcall-adapter root",
    );
    let without_provider = remove_managed_block(
        &without_root,
        "# BEGIN model-toolcall-adapter provider",
        "# END model-toolcall-adapter provider",
    );
    let without_stale_root_keys = remove_codex_root_keys(&without_provider);
    let body = without_stale_root_keys.trim();
    if body.is_empty() {
        format!(
            "{}\n\n{}\n",
            root_block.trim_end(),
            provider_block.trim_end()
        )
    } else {
        format!(
            "{}\n\n{}\n\n{}\n",
            root_block.trim_end(),
            body,
            provider_block.trim_end()
        )
    }
}

fn remove_managed_block(existing: &str, begin: &str, end_marker: &str) -> String {
    if let Some(start) = existing.find(begin) {
        if let Some(end_rel) = existing[start..].find(end_marker) {
            let end = start + end_rel + end_marker.len();
            let mut output = String::new();
            output.push_str(existing[..start].trim_end());
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            output.push_str(existing[end..].trim_start_matches(['\r', '\n']));
            return output;
        }
    }
    existing.to_string()
}

fn remove_codex_root_keys(existing: &str) -> String {
    const ROOT_KEYS: &[&str] = &[
        "model_provider",
        "model",
        "review_model",
        "model_reasoning_effort",
        "disable_response_storage",
        "network_access",
        "model_context_window",
        "model_auto_compact_token_limit",
    ];

    let mut output = Vec::new();
    let mut in_root = true;
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_root = false;
        }
        let is_managed_key = in_root
            && ROOT_KEYS.iter().any(|key| {
                trimmed
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            });
        if !is_managed_key {
            output.push(line);
        }
    }
    output.join("\n")
}

fn write_codex_auth(path: &FsPath, key: &str) -> Result<(), AdapterError> {
    let mut auth = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !auth.is_object() {
        auth = json!({});
    }
    auth["OPENAI_API_KEY"] = Value::String(key.to_string());
    let raw = serde_json::to_string_pretty(&auth)
        .map_err(|error| AdapterError::Upstream(format!("serialize Codex auth: {error}")))?;
    std::fs::write(path, raw).map_err(|error| {
        AdapterError::Upstream(format!(
            "failed to write Codex auth {}: {error}",
            path.display()
        ))
    })
}

pub async fn models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, AdapterError> {
    verify_auth(&state, &headers)?;
    let upstream_options = upstream_options_from_headers(&headers);
    Ok(Json(
        state
            .upstream
            .list_models(
                &state.config.upstream_model,
                &state.config.model_alias_map(),
                &upstream_options,
            )
            .await?,
    ))
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AdapterError> {
    tracing::info!(
        payload_keys = ?payload.as_object().map(|object| object.keys().cloned().collect::<Vec<_>>()),
        "chat completions request received"
    );
    verify_auth(&state, &headers)?;
    handle(
        state,
        payload,
        WireMode::ChatCompletions,
        upstream_options_from_headers(&headers),
    )
    .await
}

pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AdapterError> {
    verify_auth(&state, &headers)?;
    handle(
        state,
        payload,
        WireMode::Messages,
        upstream_options_from_headers(&headers),
    )
    .await
}

pub async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, AdapterError> {
    tracing::info!(
        payload_keys = ?payload.as_object().map(|object| object.keys().cloned().collect::<Vec<_>>()),
        "responses request received"
    );
    verify_auth(&state, &headers)?;
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stream {
        return Ok(responses_streaming_sse(
            state,
            payload,
            upstream_options_from_headers(&headers),
        )?);
    }
    let body = handle_value(
        state,
        payload,
        WireMode::Responses,
        &mut upstream_options_from_headers(&headers),
    )
    .await?;
    Ok(Json(body).into_response())
}

pub async fn responses_retrieve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<Value>, AdapterError> {
    verify_auth(&state, &headers)?;
    let body = state
        .responses
        .retrieve(&response_id)
        .ok_or_else(|| AdapterError::NotFound(format!("response {response_id} not found")))?;
    Ok(Json(body))
}

pub async fn responses_input_items(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
    Query(query): Query<ResponseItemsQuery>,
) -> Result<Json<Value>, AdapterError> {
    verify_auth(&state, &headers)?;
    let ascending = query
        .order
        .as_deref()
        .map(|order| order.eq_ignore_ascii_case("asc"))
        .unwrap_or(false);
    let body = state
        .responses
        .list_input_items(&response_id, query.after.as_deref(), query.limit, ascending)
        .ok_or_else(|| AdapterError::NotFound(format!("response {response_id} not found")))?;
    Ok(Json(body))
}

pub async fn responses_cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<Value>, AdapterError> {
    verify_auth(&state, &headers)?;
    let body = match state.responses.cancel(&response_id) {
        Ok(body) => body,
        Err(crate::responses_store::StoreError::NotFound) => {
            return Err(AdapterError::NotFound(format!(
                "response {response_id} not found"
            )))
        }
        Err(crate::responses_store::StoreError::NotBackground) => {
            return Err(AdapterError::InvalidRequest(
                "only background responses can be cancelled".to_string(),
            ))
        }
    };
    Ok(Json(body))
}

pub async fn responses_compact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AdapterError> {
    verify_auth(&state, &headers)?;
    let request = if payload.get("input").is_some() {
        UnifiedRequest::from_wire_payload(WireMode::Responses, payload.clone())?
    } else {
        UnifiedRequest {
            model: payload
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            max_tokens: payload
                .get("max_output_tokens")
                .or_else(|| payload.get("max_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(1024) as u32,
            system: payload
                .get("instructions")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            messages: Vec::new(),
            tools: chat::parse_tools(payload.get("tools")),
            tool_choice: chat::parse_tool_choice(payload.get("tool_choice")),
            parallel_tool_calls: payload
                .get("parallel_tool_calls")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            stream: payload
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            background: false,
            previous_response_id: payload
                .get("previous_response_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        }
    };
    let mut output = if let Some(previous_response_id) = request.previous_response_id.as_deref() {
        state
            .responses
            .context_items_for(previous_response_id)
            .ok_or_else(|| {
                AdapterError::NotFound(format!(
                    "previous response {previous_response_id} not found"
                ))
            })?
    } else {
        Vec::new()
    };
    output.extend(responses_store::input_items_from_request(&request));
    Ok(Json(json!({
        "id": format!("cmp_{}", uuid::Uuid::new_v4()),
        "created_at": chrono::Utc::now().timestamp(),
        "object": "response.compaction",
        "output": output,
    })))
}

fn verify_auth(state: &AppState, headers: &HeaderMap) -> Result<(), AdapterError> {
    let expected = state.config.adapter_api_key.trim();
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim);

    tracing::debug!(
        auth_configured = !expected.is_empty(),
        bearer_present = bearer.is_some(),
        x_api_key_present = api_key.is_some(),
        "verifying adapter auth"
    );

    if expected.is_empty() {
        return Ok(());
    }
    if bearer == Some(expected) || api_key == Some(expected) {
        Ok(())
    } else {
        tracing::warn!("adapter auth failed");
        Err(AdapterError::Unauthorized)
    }
}

async fn handle(
    state: Arc<AppState>,
    payload: Value,
    mode: WireMode,
    mut upstream_options: UpstreamRequestOptions,
) -> Result<Json<Value>, AdapterError> {
    Ok(Json(
        handle_value(state, payload, mode, &mut upstream_options).await?,
    ))
}

async fn handle_value(
    state: Arc<AppState>,
    payload: Value,
    mode: WireMode,
    upstream_options: &mut UpstreamRequestOptions,
) -> Result<Value, AdapterError> {
    let conversation_key = matches!(mode, WireMode::Responses)
        .then(|| adapter_conversation_key(&payload))
        .flatten();
    let mut request = UnifiedRequest::from_wire_payload(mode, payload)?;
    log_codex_request_summary(
        mode,
        &request,
        upstream_options,
        conversation_key.as_deref(),
    );
    if request.stream {
        return Err(AdapterError::StreamUnsupported);
    }
    let response_input_items = matches!(mode, WireMode::Responses)
        .then(|| responses_store::input_items_from_request(&request));
    if matches!(mode, WireMode::Responses) {
        prepend_previous_response_context(&state, &mut request)?;
        attach_previous_provider_session(
            &state,
            &request,
            conversation_key.as_deref(),
            upstream_options,
        );
    }
    if request.model.trim().is_empty() || request.model.trim() == "local-model" {
        request.model = state.config.upstream_model.clone();
    }
    let external_model = request.model.clone();
    if let Some(upstream_model) = state.config.model_alias_map().get(request.model.trim()) {
        request.model = upstream_model.clone();
    } else if should_route_to_default_deepseek_model(
        &request.model,
        &state.config.upstream_model,
        &upstream_options,
    ) {
        request.model = state.config.upstream_model.clone();
    }
    tracing::info!("Mapped request model to={:?}", request.model);
    request.tools.truncate(state.config.max_tool_definitions);

    if matches!(mode, WireMode::Responses) {
        if let Some(body) = immediate_tool_response_if_needed(&request) {
            let mut body = body;
            restore_external_model(&mut body, &external_model);
            log_response_summary("immediate_tool", &body);
            let context_items = responses_store::input_items_from_request(&request);
            state.responses.insert_with_provider_session(
                strip_adapter_private_fields(&body),
                response_input_items.unwrap_or_else(|| context_items.clone()),
                context_items,
                request.background,
                None,
            );
            return Ok(strip_adapter_private_fields(&body));
        }
    }

    let protocol = render_tool_protocol_prompt(&request.tools);
    let _run_guard = acquire_provider_run_guard(&state, conversation_key.as_deref())?;
    let body = match mode {
        WireMode::Responses => {
            let prompt = request.render_prompt_with_tool_protocol(&protocol);
            let upstream_response = state
                .upstream
                .complete(&request, &prompt, upstream_options)
                .await?;
            if let Some(reasoning) = upstream_response.reasoning.as_deref() {
                tracing::debug!(
                    reasoning_chars = reasoning.chars().count(),
                    "upstream reasoning"
                );
            }
            let model_text = clean_model_visible_text(&upstream_response.text);
            let tool_parse_text =
                combined_tool_parse_text(upstream_response.reasoning.as_deref(), &model_text);
            let tool_calls = fallback_tool_call_if_needed(
                &request,
                &model_text,
                apply_tool_choice_to_tool_calls(
                    &request,
                    &model_text,
                    parse_tool_calls(&tool_parse_text),
                )?,
            )?;
            log_tool_call_candidates(None, &tool_calls);
            let output_text = if tool_calls.is_empty() {
                model_text.as_str()
            } else {
                ""
            };
            let output = if tool_calls.is_empty() {
                if request.tool_choice.requires_tool() {
                    return Err(AdapterError::InvalidRequest(
                        "tool_choice requires a tool call, but the upstream model returned text"
                            .to_string(),
                    ));
                }
                vec![responses::message_output_item(&model_text)]
            } else {
                let tool_calls = constrain_parallel_tool_calls(&request, tool_calls);
                responses::function_call_items(&tool_calls)
            };
            let output = prepend_reasoning_output(upstream_response.reasoning.as_deref(), output);
            let mut body = responses::response_from_output(&request, output, output_text);
            if let Some(session_id) = upstream_response.provider_session_id.as_deref() {
                body["_adapter_provider_session_id"] = Value::String(session_id.to_string());
            }
            restore_external_model(&mut body, &external_model);
            log_response_summary("nonstream_responses", &body);
            body
        }
        WireMode::ChatCompletions | WireMode::Messages => {
            let prompt = request.render_prompt_with_tool_protocol(&protocol);
            let upstream_response = state
                .upstream
                .complete(&request, &prompt, upstream_options)
                .await?;
            if let Some(reasoning) = upstream_response.reasoning.as_deref() {
                tracing::debug!(
                    reasoning_chars = reasoning.chars().count(),
                    "upstream reasoning"
                );
            }
            let model_text = clean_model_visible_text(&upstream_response.text);
            let tool_parse_text =
                combined_tool_parse_text(upstream_response.reasoning.as_deref(), &model_text);
            let tool_calls = fallback_tool_call_if_needed(
                &request,
                &model_text,
                apply_tool_choice_to_tool_calls(
                    &request,
                    &model_text,
                    parse_tool_calls(&tool_parse_text),
                )?,
            )?;
            log_tool_call_candidates(None, &tool_calls);
            let mut body = match mode {
                WireMode::ChatCompletions => chat::response(&request, &model_text, &tool_calls),
                WireMode::Messages => messages::response(&request, &model_text, &tool_calls),
                WireMode::Responses => unreachable!(),
            };
            restore_external_model(&mut body, &external_model);
            log_response_summary("nonstream_legacy", &body);
            body
        }
    };
    if matches!(mode, WireMode::Responses) {
        let provider_state = body
            .get("_adapter_provider_session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let context_items = responses_store::input_items_from_request(&request);
        state.responses.insert_with_provider_session(
            strip_adapter_private_fields(&body),
            response_input_items.unwrap_or_else(|| context_items.clone()),
            context_items,
            request.background,
            provider_state.clone(),
        );
        remember_provider_conversation_state(&state, conversation_key.as_deref(), provider_state);
    }
    Ok(strip_adapter_private_fields(&body))
}

fn responses_streaming_sse(
    state: Arc<AppState>,
    mut payload: Value,
    mut upstream_options: UpstreamRequestOptions,
) -> Result<Response, AdapterError> {
    payload["stream"] = Value::Bool(false);
    let conversation_key = adapter_conversation_key(&payload);
    let mut request = UnifiedRequest::from_wire_payload(WireMode::Responses, payload)?;
    log_codex_request_summary(
        WireMode::Responses,
        &request,
        &upstream_options,
        conversation_key.as_deref(),
    );
    let response_input_items = responses_store::input_items_from_request(&request);
    prepend_previous_response_context(&state, &mut request)?;
    attach_previous_provider_session(
        &state,
        &request,
        conversation_key.as_deref(),
        &mut upstream_options,
    );
    if request.model.trim().is_empty() || request.model.trim() == "local-model" {
        request.model = state.config.upstream_model.clone();
    }
    let external_model = request.model.clone();
    if let Some(upstream_model) = state.config.model_alias_map().get(request.model.trim()) {
        request.model = upstream_model.clone();
    } else if should_route_to_default_deepseek_model(
        &request.model,
        &state.config.upstream_model,
        &upstream_options,
    ) {
        request.model = state.config.upstream_model.clone();
    }
    tracing::info!("Mapped streaming request model to={:?}", request.model);
    request.tools.truncate(state.config.max_tool_definitions);

    let mut shell_response = responses::response_from_output(&request, Vec::new(), "");
    shell_response["completed_at"] = Value::Null;
    shell_response["status"] = Value::String("in_progress".to_string());
    restore_external_model(&mut shell_response, &external_model);

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let stream_state = state.clone();
    tokio::spawn(async move {
        let mut sink = SseSink::new(tx);
        sink.send_event(
            "response.created",
            json!({ "type": "response.created", "response": shell_response }),
        )
        .await;
        sink.send_event(
            "response.in_progress",
            json!({ "type": "response.in_progress", "response": body_without_output(&shell_response) }),
        )
        .await;

        if let Some(mut completed) = immediate_tool_response_if_needed(&request) {
            completed["id"] = shell_response["id"].clone();
            completed["created_at"] = shell_response["created_at"].clone();
            restore_external_model(&mut completed, &external_model);
            let public_completed = strip_adapter_private_fields(&completed);
            log_response_summary("stream_immediate_tool", &public_completed);
            for (index, item) in public_completed
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                emit_output_item(&mut sink, index, item).await;
            }
            sink.send_event(
                "response.completed",
                json!({ "type": "response.completed", "response": public_completed }),
            )
            .await;
            sink.send_done().await;
            let context_items = responses_store::input_items_from_request(&request);
            stream_state.responses.insert_with_provider_session(
                public_completed,
                response_input_items,
                context_items,
                request.background,
                None,
            );
            return;
        }

        let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(2));
        let protocol = render_tool_protocol_prompt(&request.tools);
        let prompt = request.render_prompt_with_tool_protocol(&protocol);
        match try_stream_deepseek_response(
            &stream_state,
            &request,
            &prompt,
            &upstream_options,
            &external_model,
            &mut sink,
            shell_response.clone(),
            response_input_items.clone(),
            conversation_key.clone(),
        )
        .await
        {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                emit_response_error(&mut sink, shell_response, error.to_string()).await;
                return;
            }
        }

        let upstream = stream_state
            .upstream
            .complete(&request, &prompt, &upstream_options);
        tokio::pin!(upstream);

        let result = loop {
            tokio::select! {
                _ = keepalive.tick() => {
                    sink.send_event(
                        "response.in_progress",
                        json!({
                            "type": "response.in_progress",
                            "response": body_without_output(&shell_response)
                        }),
                    ).await;
                }
                result = &mut upstream => break result,
            }
        };

        match result {
            Ok(upstream_response) => {
                let reasoning_item = upstream_response
                    .reasoning
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(responses::reasoning_output_item);
                if let Some(item) = reasoning_item.as_ref() {
                    emit_reasoning_item(&mut sink, 0, item).await;
                }
                let model_text = clean_model_visible_text(&upstream_response.text);
                let tool_parse_text =
                    combined_tool_parse_text(upstream_response.reasoning.as_deref(), &model_text);
                let parsed_tool_calls = match apply_tool_choice_to_tool_calls(
                    &request,
                    &model_text,
                    parse_tool_calls(&tool_parse_text),
                )
                .and_then(|tool_calls| {
                    fallback_tool_call_if_needed(&request, &model_text, tool_calls)
                }) {
                    Ok(tool_calls) => constrain_parallel_tool_calls(&request, tool_calls),
                    Err(error) => {
                        emit_response_error(&mut sink, shell_response, error.to_string()).await;
                        return;
                    }
                };
                if parsed_tool_calls.is_empty() && request.tool_choice.requires_tool() {
                    emit_response_error(
                        &mut sink,
                        shell_response,
                        "tool_choice requires a tool call, but the upstream model returned text",
                    )
                    .await;
                    return;
                }

                let provider_session_id = upstream_response.provider_session_id.clone();
                let (output, output_text) = if parsed_tool_calls.is_empty() {
                    (
                        vec![responses::message_output_item(&model_text)],
                        model_text.as_str(),
                    )
                } else {
                    (responses::function_call_items(&parsed_tool_calls), "")
                };
                let output = prepend_reasoning_item(reasoning_item, output);
                let mut completed = responses::response_from_output(&request, output, output_text);
                completed["id"] = shell_response["id"].clone();
                completed["created_at"] = shell_response["created_at"].clone();
                restore_external_model(&mut completed, &external_model);
                let public_completed = strip_adapter_private_fields(&completed);
                log_response_summary("stream_fallback_complete", &public_completed);

                for (index, item) in public_completed
                    .get("output")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        continue;
                    }
                    emit_output_item(&mut sink, index, item).await;
                }
                sink.send_event(
                    "response.completed",
                    json!({ "type": "response.completed", "response": public_completed }),
                )
                .await;
                sink.send_done().await;
                let context_items = responses_store::input_items_from_request(&request);
                stream_state.responses.insert_with_provider_session(
                    public_completed,
                    response_input_items,
                    context_items,
                    request.background,
                    provider_session_id.clone(),
                );
                remember_provider_conversation_state(
                    &stream_state,
                    conversation_key.as_deref(),
                    provider_session_id,
                );
            }
            Err(error) => {
                emit_response_error(&mut sink, shell_response, error.to_string()).await;
            }
        }
    });

    Ok((
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(MpscByteStream { rx }),
    )
        .into_response())
}

fn body_without_output(body: &Value) -> Value {
    let mut value = body.clone();
    value["output"] = json!([]);
    value
}

async fn try_stream_deepseek_response(
    state: &AppState,
    request: &UnifiedRequest,
    prompt: &str,
    upstream_options: &UpstreamRequestOptions,
    external_model: &str,
    sink: &mut SseSink,
    shell_response: Value,
    response_input_items: Vec<Value>,
    conversation_key: Option<String>,
) -> Result<bool, AdapterError> {
    let _run_guard = acquire_provider_run_guard(state, conversation_key.as_deref())?;
    let mut upstream = match state
        .upstream
        .complete_stream(request, prompt, upstream_options)
        .await
    {
        Ok(upstream) => upstream,
        Err(AdapterError::StreamUnsupported) => return Ok(false),
        Err(error) => return Err(error),
    };

    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(2));
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut provider_session_id = None;
    let mut text_started = false;
    let mut reasoning_started = false;
    let buffer_text_until_tool_decision = should_buffer_text_until_tool_decision(request);
    let mut reasoning_delta_count = 0usize;
    let mut text_delta_count = 0usize;
    let response_id = shell_response
        .get("id")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    tracing::info!(
        response_id = response_id.as_deref(),
        model = %request.model,
        tools_available = !request.tools.is_empty(),
        buffer_text_until_tool_decision,
        previous_response_id = request.previous_response_id.as_deref(),
        "deepseek streaming started"
    );

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                sink.send_event(
                    "response.in_progress",
                    json!({
                        "type": "response.in_progress",
                        "response": body_without_output(&shell_response)
                    }),
                ).await;
            }
            event = upstream.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event? {
                    crate::providers::deepseek_web::DeepSeekStreamEvent::ReasoningDelta(delta) => {
                        if !reasoning_started {
                            emit_reasoning_started(sink, 0).await;
                            reasoning_started = true;
                        }
                        reasoning.push_str(&delta);
                        reasoning_delta_count += 1;
                        emit_reasoning_delta(sink, 0, &delta).await;
                    }
                    crate::providers::deepseek_web::DeepSeekStreamEvent::TextDelta(delta) => {
                        text.push_str(&delta);
                        text_delta_count += 1;
                        if !buffer_text_until_tool_decision {
                            if !text_started {
                                emit_message_started(sink, 1).await;
                                text_started = true;
                            }
                            emit_text_delta(sink, 1, 0, &delta).await;
                        }
                    }
                    crate::providers::deepseek_web::DeepSeekStreamEvent::SessionId(session_id) => {
                        provider_session_id = Some(session_id);
                    }
                    crate::providers::deepseek_web::DeepSeekStreamEvent::Done {
                        text: done_text,
                        reasoning: done_reasoning,
                        session_id,
                        parent_message_id,
                    } => {
                        if text.is_empty() {
                            text = done_text;
                        }
                        if reasoning.is_empty() {
                            reasoning = done_reasoning.unwrap_or_default();
                        }
                        provider_session_id =
                            Some(deepseek_provider_state_json(&session_id, parent_message_id));
                        break;
                    }
                }
            }
        }
    }

    let reasoning_item =
        (!reasoning.trim().is_empty()).then(|| responses::reasoning_output_item(reasoning.trim()));
    if reasoning_started {
        emit_reasoning_done(sink, 0, reasoning.trim()).await;
    } else if let Some(item) = reasoning_item.as_ref() {
        emit_reasoning_item(sink, 0, item).await;
    }

    let tool_parse_text = combined_tool_parse_text(
        (!reasoning.trim().is_empty()).then_some(reasoning.as_str()),
        &text,
    );
    let visible_text = clean_model_visible_text(&text);
    let parsed_tool_calls =
        apply_tool_choice_to_tool_calls(request, &visible_text, parse_tool_calls(&tool_parse_text))
            .and_then(|tool_calls| {
                fallback_tool_call_if_needed(request, &visible_text, tool_calls)
            })?;
    let parsed_tool_calls = constrain_parallel_tool_calls(request, parsed_tool_calls);
    let parsed_tool_names = parsed_tool_calls
        .iter()
        .map(|call| call.name.as_str())
        .collect::<Vec<_>>();
    log_tool_call_candidates(response_id.as_deref(), &parsed_tool_calls);
    tracing::info!(
        response_id = response_id.as_deref(),
        reasoning_delta_count,
        text_delta_count,
        reasoning_chars = reasoning.chars().count(),
        text_chars = visible_text.chars().count(),
        parsed_tool_count = parsed_tool_calls.len(),
        parsed_tool_names = ?parsed_tool_names,
        buffer_text_until_tool_decision,
        provider_state = provider_session_id.as_deref(),
        "deepseek streaming finished"
    );
    if parsed_tool_calls.is_empty() && request.tool_choice.requires_tool() {
        return Err(AdapterError::InvalidRequest(
            "tool_choice requires a tool call, but the upstream model returned text".to_string(),
        ));
    }

    let (mut output, output_text) = if parsed_tool_calls.is_empty() {
        if buffer_text_until_tool_decision && !visible_text.trim().is_empty() {
            emit_message_started(sink, 1).await;
            text_started = true;
            emit_text_delta(sink, 1, 0, &visible_text).await;
        }
        if text_started {
            emit_text_done(sink, 1, 0, &visible_text).await;
            emit_message_done(sink, 1, &visible_text).await;
        }
        (
            vec![responses::message_output_item(&visible_text)],
            visible_text.as_str(),
        )
    } else {
        (responses::function_call_items(&parsed_tool_calls), "")
    };
    output = prepend_reasoning_item(reasoning_item, output);

    let mut completed = responses::response_from_output(request, output, output_text);
    completed["id"] = shell_response["id"].clone();
    completed["created_at"] = shell_response["created_at"].clone();
    restore_external_model(&mut completed, external_model);
    let public_completed = strip_adapter_private_fields(&completed);
    log_response_summary("stream_deepseek_complete", &public_completed);

    if !parsed_tool_calls.is_empty() {
        for (index, item) in public_completed
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                continue;
            }
            emit_output_item(sink, index, item).await;
        }
    }
    sink.send_event(
        "response.completed",
        json!({ "type": "response.completed", "response": public_completed }),
    )
    .await;
    sink.send_done().await;
    let context_items = responses_store::input_items_from_request(request);
    state.responses.insert_with_provider_session(
        public_completed,
        response_input_items,
        context_items,
        request.background,
        provider_session_id.clone(),
    );
    remember_provider_conversation_state(state, conversation_key.as_deref(), provider_session_id);
    Ok(true)
}

fn push_message_text_events(stream: &mut String, output_index: usize, item: &Value) {
    if item.get("type").and_then(Value::as_str) != Some("message") {
        return;
    }
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return;
    };
    for (content_index, part) in content.iter().enumerate() {
        if part.get("type").and_then(Value::as_str) != Some("output_text") {
            continue;
        }
        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
        push_sse(
            stream,
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "output_index": output_index,
                "content_index": content_index,
                "part": part,
            }),
        );
        push_sse(
            stream,
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "output_index": output_index,
                "content_index": content_index,
                "delta": text,
            }),
        );
        push_sse(
            stream,
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "output_index": output_index,
                "content_index": content_index,
                "text": text,
            }),
        );
        push_sse(
            stream,
            "response.content_part.done",
            json!({
                "type": "response.content_part.done",
                "output_index": output_index,
                "content_index": content_index,
                "part": part,
            }),
        );
    }
}

struct MpscByteStream {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

impl futures_util::Stream for MpscByteStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

struct SseSink {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
}

impl SseSink {
    fn new(tx: mpsc::Sender<Result<Bytes, std::io::Error>>) -> Self {
        Self { tx }
    }

    async fn send_event(&mut self, event: &str, data: Value) {
        let mut chunk = String::new();
        push_sse(&mut chunk, event, data);
        let _ = self.tx.send(Ok(Bytes::from(chunk))).await;
    }

    async fn send_done(&mut self) {
        let _ = self
            .tx
            .send(Ok(Bytes::from_static(b"data: [DONE]\n\n")))
            .await;
    }
}

async fn emit_output_item(sink: &mut SseSink, output_index: usize, item: &Value) {
    sink.send_event(
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        }),
    )
    .await;
    let mut text_events = String::new();
    push_message_text_events(&mut text_events, output_index, item);
    if !text_events.is_empty() {
        let _ = sink.tx.send(Ok(Bytes::from(text_events))).await;
    }
    sink.send_event(
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }),
    )
    .await;
}

async fn emit_message_started(sink: &mut SseSink, output_index: usize) {
    let item = json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4()),
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": []
    });
    sink.send_event(
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        }),
    )
    .await;
    sink.send_event(
        "response.content_part.added",
        json!({
            "type": "response.content_part.added",
            "output_index": output_index,
            "content_index": 0,
            "part": { "type": "output_text", "text": "", "annotations": [] },
        }),
    )
    .await;
}

async fn emit_text_delta(
    sink: &mut SseSink,
    output_index: usize,
    content_index: usize,
    delta: &str,
) {
    sink.send_event(
        "response.output_text.delta",
        json!({
            "type": "response.output_text.delta",
            "output_index": output_index,
            "content_index": content_index,
            "delta": delta,
        }),
    )
    .await;
}

async fn emit_text_done(sink: &mut SseSink, output_index: usize, content_index: usize, text: &str) {
    sink.send_event(
        "response.output_text.done",
        json!({
            "type": "response.output_text.done",
            "output_index": output_index,
            "content_index": content_index,
            "text": text,
        }),
    )
    .await;
    sink.send_event(
        "response.content_part.done",
        json!({
            "type": "response.content_part.done",
            "output_index": output_index,
            "content_index": content_index,
            "part": { "type": "output_text", "text": text, "annotations": [] },
        }),
    )
    .await;
}

async fn emit_message_done(sink: &mut SseSink, output_index: usize, text: &str) {
    let item = responses::message_output_item(text);
    sink.send_event(
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }),
    )
    .await;
}

async fn emit_reasoning_started(sink: &mut SseSink, output_index: usize) {
    let item = json!({
        "id": format!("rs_{}", uuid::Uuid::new_v4()),
        "type": "reasoning",
        "status": "in_progress",
        "summary": []
    });
    sink.send_event(
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        }),
    )
    .await;
    sink.send_event(
        "response.reasoning_summary_part.added",
        json!({
            "type": "response.reasoning_summary_part.added",
            "output_index": output_index,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": "" }
        }),
    )
    .await;
}

async fn emit_reasoning_delta(sink: &mut SseSink, output_index: usize, delta: &str) {
    sink.send_event(
        "response.reasoning_summary_text.delta",
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": output_index,
            "summary_index": 0,
            "delta": delta
        }),
    )
    .await;
}

async fn emit_reasoning_done(sink: &mut SseSink, output_index: usize, reasoning: &str) {
    sink.send_event(
        "response.reasoning_summary_text.done",
        json!({
            "type": "response.reasoning_summary_text.done",
            "output_index": output_index,
            "summary_index": 0,
            "text": reasoning
        }),
    )
    .await;
    sink.send_event(
        "response.reasoning_summary_part.done",
        json!({
            "type": "response.reasoning_summary_part.done",
            "output_index": output_index,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": reasoning }
        }),
    )
    .await;
    sink.send_event(
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": responses::reasoning_output_item(reasoning),
        }),
    )
    .await;
}

async fn emit_reasoning_item(sink: &mut SseSink, output_index: usize, item: &Value) {
    let reasoning = item
        .get("summary")
        .and_then(Value::as_array)
        .and_then(|summary| summary.first())
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    sink.send_event(
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        }),
    )
    .await;
    sink.send_event(
        "response.reasoning_summary_part.added",
        json!({
            "type": "response.reasoning_summary_part.added",
            "output_index": output_index,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": "" }
        }),
    )
    .await;
    sink.send_event(
        "response.reasoning_summary_text.delta",
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": output_index,
            "summary_index": 0,
            "delta": reasoning
        }),
    )
    .await;
    sink.send_event(
        "response.reasoning_summary_text.done",
        json!({
            "type": "response.reasoning_summary_text.done",
            "output_index": output_index,
            "summary_index": 0,
            "text": reasoning
        }),
    )
    .await;
    sink.send_event(
        "response.reasoning_summary_part.done",
        json!({
            "type": "response.reasoning_summary_part.done",
            "output_index": output_index,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": reasoning }
        }),
    )
    .await;
    sink.send_event(
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }),
    )
    .await;
}

fn prepend_reasoning_output(reasoning: Option<&str>, mut output: Vec<Value>) -> Vec<Value> {
    let Some(reasoning) = reasoning.map(str::trim).filter(|value| !value.is_empty()) else {
        return output;
    };
    output.insert(0, responses::reasoning_output_item(reasoning));
    output
}

fn prepend_reasoning_item(reasoning_item: Option<Value>, mut output: Vec<Value>) -> Vec<Value> {
    let Some(reasoning_item) = reasoning_item else {
        return output;
    };
    output.insert(0, reasoning_item);
    output
}

async fn emit_response_error(sink: &mut SseSink, mut response: Value, message: impl Into<String>) {
    response["status"] = Value::String("failed".to_string());
    response["error"] = json!({
        "type": "adapter_error",
        "message": message.into(),
    });
    sink.send_event(
        "response.failed",
        json!({ "type": "response.failed", "response": response }),
    )
    .await;
    sink.send_done().await;
}

fn push_sse(stream: &mut String, event: &str, data: Value) {
    stream.push_str("event: ");
    stream.push_str(event);
    stream.push('\n');
    stream.push_str("data: ");
    stream.push_str(&data.to_string());
    stream.push_str("\n\n");
}

fn should_route_to_default_deepseek_model(
    requested_model: &str,
    upstream_model: &str,
    upstream_options: &UpstreamRequestOptions,
) -> bool {
    upstream_model.starts_with("deepseek-web/")
        && !requested_model.trim().starts_with("deepseek-web/")
        && upstream_options
            .provider
            .as_deref()
            .is_none_or(|provider| provider.eq_ignore_ascii_case("deepseek-web"))
        && upstream_options.base_url.is_none()
}

fn log_codex_request_summary(
    mode: WireMode,
    request: &UnifiedRequest,
    upstream_options: &UpstreamRequestOptions,
    conversation_key: Option<&str>,
) {
    let tool_names = request
        .tools
        .iter()
        .take(20)
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    let input_chars: usize = request
        .messages
        .iter()
        .map(|message| message.content_text().chars().count())
        .sum();
    tracing::info!(
        mode = ?mode,
        model = %request.model,
        stream = request.stream,
        previous_response_id = request.previous_response_id.as_deref(),
        conversation_key = conversation_key.map(|value| truncate_for_log(value, 96)),
        tool_count = request.tools.len(),
        tool_names = ?tool_names,
        tool_choice = ?request.tool_choice.to_wire_value(),
        parallel_tool_calls = request.parallel_tool_calls,
        input_messages = request.messages.len(),
        input_chars,
        upstream_options = ?upstream_options.redacted(),
        "codex request summary"
    );
}

fn log_response_summary(stage: &str, body: &Value) {
    let output = body
        .get("output")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let output_types = output
        .iter()
        .filter_map(|item| item.get("type").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    let tool_names = output
        .iter()
        .filter(|item| item.get("type").and_then(|value| value.as_str()) == Some("function_call"))
        .filter_map(|item| item.get("name").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    let tool_args = output
        .iter()
        .filter(|item| item.get("type").and_then(|value| value.as_str()) == Some("function_call"))
        .filter_map(|item| {
            let name = item.get("name").and_then(|value| value.as_str())?;
            let arguments = item
                .get("arguments")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            Some(format!("{name}:{}", truncate_for_log(arguments, 180)))
        })
        .collect::<Vec<_>>();
    let reasoning_chars: usize = output
        .iter()
        .filter(|item| item.get("type").and_then(|value| value.as_str()) == Some("reasoning"))
        .filter_map(|item| item.get("summary").and_then(|value| value.as_array()))
        .flatten()
        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
        .map(|text| text.chars().count())
        .sum();
    let message_chars: usize = output
        .iter()
        .filter(|item| item.get("type").and_then(|value| value.as_str()) == Some("message"))
        .filter_map(|item| item.get("content").and_then(|value| value.as_array()))
        .flatten()
        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
        .map(|text| text.chars().count())
        .sum();
    let response_id = body.get("id").and_then(|value| value.as_str());
    let status = body.get("status").and_then(|value| value.as_str());
    let model = body.get("model").and_then(|value| value.as_str());
    let output_text_chars = body
        .get("output_text")
        .and_then(|value| value.as_str())
        .map(|text| text.chars().count())
        .unwrap_or_default();
    let previous_response_id = body
        .get("previous_response_id")
        .and_then(|value| value.as_str());
    tracing::info!(
        stage,
        response_id,
        status,
        model,
        output_count = output.len(),
        output_types = ?output_types,
        tool_names = ?tool_names,
        tool_args = ?tool_args,
        output_text_chars,
        message_chars,
        reasoning_chars,
        previous_response_id,
        "adapter response summary"
    );
}

fn log_tool_call_candidates(response_id: Option<&str>, tool_calls: &[ParsedToolCall]) {
    if tool_calls.is_empty() {
        return;
    }
    let calls = tool_calls
        .iter()
        .map(|call| format!("{}:{}", call.name, truncate_for_log(&call.arguments, 220)))
        .collect::<Vec<_>>();
    tracing::info!(
        response_id,
        tool_call_count = tool_calls.len(),
        tool_calls = ?calls,
        "parsed adapter tool calls"
    );
}

fn attach_previous_provider_session(
    state: &AppState,
    request: &UnifiedRequest,
    conversation_key: Option<&str>,
    upstream_options: &mut UpstreamRequestOptions,
) {
    if upstream_options.provider_session_id.is_some() {
        return;
    }
    if let Some(previous_response_id) = request.previous_response_id.as_deref() {
        if let Some(provider_state) = state
            .responses
            .provider_session_id_for(previous_response_id)
        {
            tracing::info!(
                previous_response_id,
                provider_state = %provider_state,
                "reusing provider session from previous response"
            );
            upstream_options.provider_session_id = Some(provider_state);
            return;
        }
    }
    let Some(conversation_key) = conversation_key else {
        return;
    };
    if let Some(provider_state) = state
        .provider_conversations
        .lock()
        .expect("provider conversation map poisoned")
        .get(conversation_key)
        .cloned()
    {
        tracing::info!(
            conversation_key = %truncate_for_log(conversation_key, 96),
            provider_state = %provider_state,
            "reusing provider session from adapter conversation state"
        );
        upstream_options.provider_session_id = Some(provider_state);
    }
}

fn remember_provider_conversation_state(
    state: &AppState,
    conversation_key: Option<&str>,
    provider_state: Option<String>,
) {
    let (Some(conversation_key), Some(provider_state)) = (conversation_key, provider_state) else {
        return;
    };
    state
        .provider_conversations
        .lock()
        .expect("provider conversation map poisoned")
        .insert(conversation_key.to_string(), provider_state.clone());
    tracing::info!(
        conversation_key = %truncate_for_log(conversation_key, 96),
        provider_state = %provider_state,
        "remembered provider session for adapter conversation"
    );
}

struct ProviderRunGuard<'a> {
    state: &'a AppState,
    key: Option<String>,
}

impl Drop for ProviderRunGuard<'_> {
    fn drop(&mut self) {
        let Some(key) = self.key.as_deref() else {
            return;
        };
        self.state
            .active_provider_runs
            .lock()
            .expect("active provider run map poisoned")
            .remove(key);
    }
}

fn acquire_provider_run_guard<'a>(
    state: &'a AppState,
    conversation_key: Option<&str>,
) -> Result<ProviderRunGuard<'a>, AdapterError> {
    let Some(conversation_key) = conversation_key else {
        return Ok(ProviderRunGuard { state, key: None });
    };
    let mut guard = state
        .active_provider_runs
        .lock()
        .expect("active provider run map poisoned");
    if !try_mark_provider_run_active(&mut guard, conversation_key) {
        tracing::warn!(
            conversation_key = %truncate_for_log(conversation_key, 96),
            "duplicate in-flight provider run rejected"
        );
        return Err(AdapterError::InvalidRequest(
            "another upstream response for this Codex conversation is still running; retry after it completes".to_string(),
        ));
    }
    Ok(ProviderRunGuard {
        state,
        key: Some(conversation_key.to_string()),
    })
}

fn try_mark_provider_run_active(active: &mut HashSet<String>, conversation_key: &str) -> bool {
    active.insert(conversation_key.to_string())
}

fn adapter_conversation_key(payload: &Value) -> Option<String> {
    if let Some(key) = payload
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(format!("prompt_cache_key:{key}"));
    }
    if let Some(key) = payload
        .pointer("/client_metadata/conversation_id")
        .or_else(|| payload.pointer("/client_metadata/thread_id"))
        .or_else(|| payload.pointer("/metadata/conversation_id"))
        .or_else(|| payload.pointer("/metadata/thread_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(format!("metadata:{key}"));
    }
    let input = payload.get("input")?;
    let Value::Array(items) = input else {
        return None;
    };
    let first_user = items.iter().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("user"))
        .then(|| text_from_response_message_item(item))
        .flatten()
    })?;
    stable_text_fingerprint(&first_user).map(|fingerprint| format!("first_user:{fingerprint}"))
}

fn text_from_response_message_item(item: &Value) -> Option<String> {
    let content = item.get("content")?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .or_else(|| part.get("content"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        other => Some(other.to_string()),
    }
}

fn stable_text_fingerprint(text: &str) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.trim().is_empty() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

fn strip_adapter_private_fields(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("_adapter_provider_session_id");
    }
    value
}

fn deepseek_provider_state_json(session_id: &str, parent_message_id: Option<Value>) -> String {
    let mut state = json!({
        "chat_session_id": session_id,
    });
    if let Some(parent_message_id) = parent_message_id {
        state["parent_message_id"] = parent_message_id;
    }
    state.to_string()
}

fn restore_external_model(body: &mut Value, external_model: &str) {
    if !external_model.trim().is_empty() {
        body["model"] = Value::String(external_model.to_string());
    }
}

fn apply_tool_choice_to_tool_calls(
    request: &UnifiedRequest,
    model_text: &str,
    tool_calls: Vec<crate::types::ParsedToolCall>,
) -> Result<Vec<crate::types::ParsedToolCall>, AdapterError> {
    if tool_calls.is_empty() {
        return Ok(tool_calls);
    }
    if !request.tool_choice.allows_tools() {
        tracing::warn!(
            model_text_chars = model_text.chars().count(),
            "upstream emitted tool calls while tool_choice=none; treating output as plain text"
        );
        return Ok(Vec::new());
    }
    if let Some(required_name) = request.tool_choice.required_name() {
        if !tool_calls.iter().all(|call| call.name == required_name) {
            return Err(AdapterError::InvalidRequest(format!(
                "model emitted a tool other than required function {required_name}"
            )));
        }
    }
    Ok(tool_calls)
}

fn combined_tool_parse_text(reasoning: Option<&str>, model_text: &str) -> String {
    match reasoning.map(str::trim).filter(|value| !value.is_empty()) {
        Some(reasoning) if !model_text.trim().is_empty() => {
            format!("{reasoning}\n\n{model_text}")
        }
        Some(reasoning) => reasoning.to_string(),
        None => model_text.to_string(),
    }
}

fn clean_model_visible_text(text: &str) -> String {
    let mut value = text.trim_end();
    loop {
        let Some(stripped) = value.strip_suffix("FINISHED") else {
            break;
        };
        value = stripped.trim_end();
    }
    value.to_string()
}

fn fallback_tool_call_if_needed(
    request: &UnifiedRequest,
    model_text: &str,
    tool_calls: Vec<ParsedToolCall>,
) -> Result<Vec<ParsedToolCall>, AdapterError> {
    if !tool_calls.is_empty() || !request.tool_choice.allows_tools() {
        return Ok(tool_calls);
    }
    if request.tool_choice.requires_tool() {
        return Ok(tool_calls);
    }
    let Some(tool_name) = preferred_inspection_tool(request) else {
        return Ok(tool_calls);
    };
    let latest_user_text = latest_user_text(request);
    if !looks_like_explicit_project_inspection_request(&latest_user_text) {
        return Ok(tool_calls);
    }

    tracing::warn!(
        tool = %tool_name,
        latest_user_chars = latest_user_text.chars().count(),
        model_text = %truncate_for_log(model_text, 160),
        "upstream returned an inspection preamble without a tool call; synthesizing adapter tool call"
    );
    Ok(vec![ParsedToolCall {
        id: "call_adapter_inspect_1".to_string(),
        name: tool_name.to_string(),
        arguments: inspection_tool_arguments(tool_name).to_string(),
    }])
}

fn immediate_tool_response_if_needed(request: &UnifiedRequest) -> Option<Value> {
    if !request.tool_choice.allows_tools()
        || request.tool_choice.requires_tool()
        || has_recent_tool_result(request)
    {
        return None;
    }
    let latest_user_text = latest_user_text(request);
    if !looks_like_explicit_project_inspection_request(&latest_user_text) {
        return None;
    }
    let tool_name = preferred_inspection_tool(request)?;
    let call = ParsedToolCall {
        id: "call_adapter_inspect_1".to_string(),
        name: tool_name.to_string(),
        arguments: inspection_tool_arguments(tool_name).to_string(),
    };
    tracing::info!(
        tool = %call.name,
        latest_user_chars = latest_user_text.chars().count(),
        "synthesizing immediate inspection tool call before upstream request"
    );
    Some(responses::response_from_output(
        request,
        responses::function_call_items(&[call]),
        "",
    ))
}

fn should_buffer_text_until_tool_decision(request: &UnifiedRequest) -> bool {
    request.tool_choice.allows_tools() && !request.tools.is_empty()
}

fn has_recent_tool_result(request: &UnifiedRequest) -> bool {
    request.messages.iter().rev().any(|message| {
        message
            .content
            .iter()
            .any(|content| matches!(content, crate::types::UnifiedContent::ToolResult { .. }))
    })
}

fn preferred_inspection_tool(request: &UnifiedRequest) -> Option<&str> {
    let names = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    ["exec_command", "shell", "bash", "run_command", "terminal"]
        .into_iter()
        .find(|candidate| names.iter().any(|name| name == candidate))
        .or_else(|| names.first().copied())
}

fn inspection_tool_arguments(tool_name: &str) -> Value {
    match tool_name {
        "exec_command" | "shell" | "bash" | "run_command" | "terminal" => {
            json!({ "cmd": "pwd && printf '\\n--- files ---\\n' && find . -maxdepth 2 -type f | sed 's#^./##' | sort | head -200 && printf '\\n--- cargo ---\\n' && sed -n '1,220p' Cargo.toml 2>/dev/null && printf '\\n--- readme ---\\n' && sed -n '1,220p' README.md 2>/dev/null && printf '\\n--- src ---\\n' && find src -maxdepth 2 -type f | sort" })
        }
        _ => json!({}),
    }
}

fn latest_user_text(request: &UnifiedRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.content_text())
        .unwrap_or_default()
}

fn looks_like_explicit_project_inspection_request(text: &str) -> bool {
    let value = text.to_lowercase();
    let normalized = value.trim();
    if normalized.chars().count() < 8 {
        return false;
    }
    if looks_like_followup_or_execution_request(normalized) {
        return false;
    }
    let project_terms = [
        "当前项目",
        "这个项目",
        "项目",
        "代码",
        "仓库",
        "repo",
        "repository",
        "codebase",
    ];
    let inspection_terms = [
        "看看", "看下", "检查", "分析", "问题", "结构", "质量", "review", "inspect", "check",
        "analyze",
    ];
    project_terms.iter().any(|term| value.contains(term))
        && inspection_terms.iter().any(|term| value.contains(term))
}

fn looks_like_followup_or_execution_request(text: &str) -> bool {
    let normalized = text.trim();
    [
        "继续",
        "继续开始",
        "开始完善",
        "执行计划",
        "按计划",
        "开始实现",
        "继续实现",
        "继续补全",
        "接着",
        "go on",
        "continue",
        "proceed",
        "implement",
    ]
    .iter()
    .any(|term| normalized.starts_with(term))
}

fn truncate_for_log(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out = trimmed.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

fn constrain_parallel_tool_calls(
    request: &UnifiedRequest,
    tool_calls: Vec<crate::types::ParsedToolCall>,
) -> Vec<crate::types::ParsedToolCall> {
    if request.parallel_tool_calls {
        tool_calls
    } else {
        tool_calls.into_iter().take(1).collect()
    }
}

fn prepend_previous_response_context(
    state: &AppState,
    request: &mut UnifiedRequest,
) -> Result<(), AdapterError> {
    let Some(previous_response_id) = request.previous_response_id.as_deref() else {
        return Ok(());
    };
    let previous_items = state
        .responses
        .context_items_for(previous_response_id)
        .ok_or_else(|| {
            AdapterError::NotFound(format!(
                "previous response {previous_response_id} not found"
            ))
        })?;
    let before_messages = request.messages.len();
    let before_chars: usize = request
        .messages
        .iter()
        .map(|message| message.content_text().chars().count())
        .sum();
    let previous_item_types = previous_items
        .iter()
        .filter_map(|item| item.get("type").and_then(|value| value.as_str()))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut previous_messages = responses::parse_input(&Value::Array(previous_items));
    previous_messages.append(&mut request.messages);
    request.messages = previous_messages;
    let after_chars: usize = request
        .messages
        .iter()
        .map(|message| message.content_text().chars().count())
        .sum();
    tracing::info!(
        previous_response_id,
        previous_item_types = ?previous_item_types,
        before_messages,
        after_messages = request.messages.len(),
        before_chars,
        after_chars,
        "prepended previous response context"
    );
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseItemsQuery {
    pub after: Option<String>,
    pub limit: Option<usize>,
    pub order: Option<String>,
}

fn upstream_options_from_headers(headers: &HeaderMap) -> UpstreamRequestOptions {
    UpstreamRequestOptions {
        provider: header_string(headers, "x-upstream-provider"),
        base_url: header_string(headers, "x-upstream-base-url"),
        api_key: header_string(headers, "x-upstream-api-key"),
        deepseek_session: header_string(headers, "x-deepseek-session"),
        provider_session_id: header_string(headers, "x-provider-session-id"),
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use axum::http::HeaderMap;
    use serde_json::json;

    use super::{
        adapter_conversation_key, apply_tool_choice_to_tool_calls, clean_model_visible_text,
        codex_managed_provider_block, codex_managed_root_block, combined_tool_parse_text,
        fallback_tool_call_if_needed, find_deepseek_bearer, immediate_tool_response_if_needed,
        inspection_tool_arguments, prepend_reasoning_item, replace_codex_managed_blocks,
        should_buffer_text_until_tool_decision, should_route_to_default_deepseek_model,
        try_mark_provider_run_active, upstream_options_from_headers,
    };
    use crate::protocol::parse_tool_calls;
    use crate::types::{ParsedToolCall, ToolChoice, UnifiedRequest};
    use crate::upstream::UpstreamRequestOptions;
    use crate::wire::responses;

    #[test]
    fn extracts_per_request_upstream_options() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-upstream-base-url",
            "https://api.example.com/v1".parse().unwrap(),
        );
        headers.insert("x-upstream-api-key", "secret".parse().unwrap());
        headers.insert("x-upstream-provider", "deepseek-web".parse().unwrap());
        headers.insert("x-deepseek-session", "cookie=a".parse().unwrap());

        let options = upstream_options_from_headers(&headers);

        assert_eq!(options.provider.as_deref(), Some("deepseek-web"));
        assert_eq!(
            options.base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert_eq!(options.api_key.as_deref(), Some("secret"));
        assert_eq!(options.deepseek_session.as_deref(), Some("cookie=a"));
    }

    #[test]
    fn tool_choice_none_suppresses_model_emitted_tool_calls() {
        let request = UnifiedRequest {
            model: "m".to_string(),
            max_tokens: 16,
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            parallel_tool_calls: false,
            stream: false,
            background: false,
            previous_response_id: None,
        };

        let calls = apply_tool_choice_to_tool_calls(
            &request,
            "<tool_call name=\"search\">{}</tool_call>",
            vec![ParsedToolCall {
                id: "call_a".to_string(),
                name: "search".to_string(),
                arguments: json!({}).to_string(),
            }],
        )
        .unwrap();

        assert!(calls.is_empty());
    }

    #[test]
    fn codex_config_replaces_stale_root_model_keys() {
        let existing = r#"model_provider = "MyOpenAI"
model = "gpt-5.5"
review_model = "gpt-5.5"
network_access = "enabled"
windows_wsl_setup_acknowledged = true

[features]
goals = true

[model_providers.MyOpenAI]
name = "MyOpenAI"
"#;

        let next = replace_codex_managed_blocks(
            existing,
            &codex_managed_root_block("ModelToolCallAdapter", "deepseek-web/reasoner"),
            &codex_managed_provider_block("ModelToolCallAdapter", "http://127.0.0.1:8787/v1"),
        );

        assert!(next.contains("model_provider = \"ModelToolCallAdapter\""));
        assert!(next.contains("model = \"deepseek-web/reasoner\""));
        assert!(next.contains("windows_wsl_setup_acknowledged = true"));
        assert!(next.contains("[features]"));
        assert!(next.contains("[model_providers.MyOpenAI]"));
        assert!(!next.contains("model_provider = \"MyOpenAI\""));
        assert!(!next.contains("model = \"gpt-5.5\""));
    }

    #[test]
    fn deepseek_capture_prefers_user_token_value() {
        let storage = json!({
            "__tea_cache_tokens_20006317": "{\"web_id\":\"analytics\"}",
            "settingsJwt": "{\"value\":{\"ownerHash\":\"not-login-token\"}}",
            "userToken": "{\"value\":\"login-token-64\"}"
        });

        let bearer = find_deepseek_bearer(&storage);

        assert_eq!(bearer.as_deref(), Some("login-token-64"));
    }

    #[test]
    fn codex_model_names_route_to_selected_deepseek_upstream() {
        let options = UpstreamRequestOptions::default();

        assert!(should_route_to_default_deepseek_model(
            "gpt-5.4-mini",
            "deepseek-web/reasoner",
            &options
        ));
        assert!(!should_route_to_default_deepseek_model(
            "gpt-5.4-mini",
            "qwen3-coder",
            &options
        ));
    }

    #[test]
    fn derives_adapter_conversation_key_from_prompt_cache_key() {
        let key = adapter_conversation_key(&json!({
            "prompt_cache_key": "codex-thread-a",
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
        }));

        assert_eq!(key.as_deref(), Some("prompt_cache_key:codex-thread-a"));
    }

    #[test]
    fn derives_adapter_conversation_key_from_first_user_message() {
        let a = adapter_conversation_key(&json!({
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"分析当前项目"}]}]
        }));
        let b = adapter_conversation_key(&json!({
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"分析当前项目"}]},
                {"type":"function_call_output","call_id":"call_a","output":"ok"}
            ]
        }));

        assert_eq!(a, b);
        assert!(a.unwrap().starts_with("first_user:"));
    }

    #[test]
    fn provider_run_guard_rejects_duplicate_active_conversation() {
        let mut active = HashSet::new();

        assert!(try_mark_provider_run_active(&mut active, "conv-a"));
        assert!(!try_mark_provider_run_active(&mut active, "conv-a"));
        assert!(try_mark_provider_run_active(&mut active, "conv-b"));
    }

    #[test]
    fn prepends_same_reasoning_item_for_streaming_completion() {
        let reasoning_item = responses::reasoning_output_item("thinking");
        let reasoning_id = reasoning_item["id"].clone();
        let output = prepend_reasoning_item(
            Some(reasoning_item),
            vec![responses::message_output_item("answer")],
        );

        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["id"], reasoning_id);
        assert_eq!(output[1]["type"], "message");
    }

    #[test]
    fn parses_tool_calls_from_reasoning_when_visible_text_is_plain() {
        let text = combined_tool_parse_text(
            Some(r#"<tool_call id="call_a" name="exec_command">{"cmd":"ls"}</tool_call>"#),
            "我会先查看项目。",
        );
        let calls = parse_tool_calls(&text);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec_command");
    }

    #[test]
    fn strips_deepseek_finished_sentinel_from_visible_text() {
        assert_eq!(
            clean_model_visible_text("正在检查项目。\nFINISHEDFINISHED"),
            "正在检查项目。"
        );
    }

    #[test]
    fn synthesizes_inspection_tool_call_for_explicit_project_request() {
        let request = UnifiedRequest {
            model: "m".to_string(),
            max_tokens: 16,
            system: None,
            messages: vec![crate::types::UnifiedMessage::text(
                "user",
                "你看看当前项目有哪些问题",
            )],
            tools: vec![crate::types::ToolDefinition {
                name: "exec_command".to_string(),
                description: Some("Run command".to_string()),
                input_schema: json!({"type":"object"}),
            }],
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: false,
            stream: false,
            background: false,
            previous_response_id: None,
        };

        let calls =
            fallback_tool_call_if_needed(&request, "正在查看项目结构和代码质量。", Vec::new())
                .unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec_command");
        assert!(calls[0].arguments.contains("pwd"));
        assert!(calls[0].arguments.contains("Cargo.toml"));
        assert!(calls[0].arguments.contains("README.md"));
    }

    #[test]
    fn project_inspection_command_collects_actionable_context() {
        let args = inspection_tool_arguments("exec_command");
        let cmd = args["cmd"].as_str().unwrap();

        assert!(cmd.contains("find . -maxdepth 2"));
        assert!(cmd.contains("sed -n '1,220p' Cargo.toml"));
        assert!(cmd.contains("find src -maxdepth 2"));
    }

    #[test]
    fn immediately_returns_inspection_tool_call_for_project_request() {
        let request = inspection_request(vec![crate::types::UnifiedMessage::text(
            "user",
            "你看看当前项目有哪些问题",
        )]);

        let body = immediate_tool_response_if_needed(&request).unwrap();

        assert_eq!(body["output"][0]["type"], "function_call");
        assert_eq!(body["output"][0]["name"], "exec_command");
        assert_eq!(body["output_text"], "");
    }

    #[test]
    fn does_not_immediately_return_tool_call_after_tool_result() {
        let request = inspection_request(vec![
            crate::types::UnifiedMessage::text("user", "你看看当前项目有哪些问题"),
            crate::types::UnifiedMessage {
                role: "user".to_string(),
                content: vec![crate::types::UnifiedContent::ToolResult {
                    tool_use_id: "call_adapter_inspect_1".to_string(),
                    content: "Cargo.toml\nsrc".to_string(),
                    is_error: false,
                }],
            },
        ]);

        assert!(immediate_tool_response_if_needed(&request).is_none());
    }

    #[test]
    fn does_not_immediately_return_tool_call_for_followup_execution_request() {
        let request = inspection_request(vec![crate::types::UnifiedMessage::text(
            "user",
            "继续开始完善",
        )]);

        assert!(immediate_tool_response_if_needed(&request).is_none());
    }

    #[test]
    fn does_not_fallback_tool_call_for_followup_execution_request() {
        let request =
            inspection_request(vec![crate::types::UnifiedMessage::text("user", "执行计划")]);

        let calls =
            fallback_tool_call_if_needed(&request, "我会继续按计划实现。", Vec::new()).unwrap();

        assert!(calls.is_empty());
    }

    #[test]
    fn buffers_text_while_tools_are_available() {
        let mut request =
            inspection_request(vec![crate::types::UnifiedMessage::text("user", "普通问题")]);

        assert!(should_buffer_text_until_tool_decision(&request));
        request.tool_choice = ToolChoice::None;
        assert!(!should_buffer_text_until_tool_decision(&request));
        request.tool_choice = ToolChoice::Auto;
        request.tools.clear();
        assert!(!should_buffer_text_until_tool_decision(&request));
    }

    fn inspection_request(messages: Vec<crate::types::UnifiedMessage>) -> UnifiedRequest {
        UnifiedRequest {
            model: "m".to_string(),
            max_tokens: 16,
            system: None,
            messages,
            tools: vec![crate::types::ToolDefinition {
                name: "exec_command".to_string(),
                description: Some("Run command".to_string()),
                input_schema: json!({"type":"object"}),
            }],
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: false,
            stream: false,
            background: false,
            previous_response_id: None,
        }
    }
}
