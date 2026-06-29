use std::path::{Path as FsPath, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::config::{update_local_config, LocalConfig};
use crate::error::AdapterError;
use crate::http::{AppState, DeepSeekBrowserProcess};
use crate::protocol::{parse_tool_calls, render_tool_protocol_prompt};
use crate::providers::deepseek_web::default_session_path as default_deepseek_session_path;
use crate::responses_store;
use crate::types::UnifiedRequest;
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
    Json(mut payload): Json<Value>,
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
        payload["stream"] = Value::Bool(false);
    }
    let body = handle_value(
        state,
        payload,
        WireMode::Responses,
        upstream_options_from_headers(&headers),
    )
    .await?;
    if stream {
        Ok(responses_sse(body))
    } else {
        Ok(Json(body).into_response())
    }
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
    upstream_options: UpstreamRequestOptions,
) -> Result<Json<Value>, AdapterError> {
    Ok(Json(
        handle_value(state, payload, mode, upstream_options).await?,
    ))
}

async fn handle_value(
    state: Arc<AppState>,
    payload: Value,
    mode: WireMode,
    upstream_options: UpstreamRequestOptions,
) -> Result<Value, AdapterError> {
    let mut request = UnifiedRequest::from_wire_payload(mode, payload)?;
    tracing::info!(
        "Received request with model={:?}, upstream_options={:?}",
        request.model,
        upstream_options.redacted()
    );
    if request.stream {
        return Err(AdapterError::StreamUnsupported);
    }
    let response_input_items = matches!(mode, WireMode::Responses)
        .then(|| responses_store::input_items_from_request(&request));
    if matches!(mode, WireMode::Responses) {
        prepend_previous_response_context(&state, &mut request)?;
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

    let protocol = render_tool_protocol_prompt(&request.tools);
    let body = match mode {
        WireMode::Responses => {
            let prompt = request.render_prompt_with_tool_protocol(&protocol);
            let upstream_response = state
                .upstream
                .complete(&request, &prompt, &upstream_options)
                .await?;
            if let Some(reasoning) = upstream_response.reasoning.as_deref() {
                tracing::debug!(
                    reasoning_chars = reasoning.chars().count(),
                    "upstream reasoning"
                );
            }
            let model_text = upstream_response.text;
            let tool_calls = apply_tool_choice_to_tool_calls(
                &request,
                &model_text,
                parse_tool_calls(&model_text),
            )?;
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
            let mut body = responses::response_from_output(&request, output, output_text);
            restore_external_model(&mut body, &external_model);
            body
        }
        WireMode::ChatCompletions | WireMode::Messages => {
            let prompt = request.render_prompt_with_tool_protocol(&protocol);
            let upstream_response = state
                .upstream
                .complete(&request, &prompt, &upstream_options)
                .await?;
            if let Some(reasoning) = upstream_response.reasoning.as_deref() {
                tracing::debug!(
                    reasoning_chars = reasoning.chars().count(),
                    "upstream reasoning"
                );
            }
            let model_text = upstream_response.text;
            let tool_calls = apply_tool_choice_to_tool_calls(
                &request,
                &model_text,
                parse_tool_calls(&model_text),
            )?;
            let mut body = match mode {
                WireMode::ChatCompletions => chat::response(&request, &model_text, &tool_calls),
                WireMode::Messages => messages::response(&request, &model_text, &tool_calls),
                WireMode::Responses => unreachable!(),
            };
            restore_external_model(&mut body, &external_model);
            body
        }
    };
    if matches!(mode, WireMode::Responses) {
        let context_items = responses_store::input_items_from_request(&request);
        state.responses.insert(
            body.clone(),
            response_input_items.unwrap_or_else(|| context_items.clone()),
            context_items,
            request.background,
        );
    }
    Ok(body)
}

fn responses_sse(body: Value) -> Response {
    let mut stream = String::new();
    let mut in_progress = body.clone();
    in_progress["status"] = Value::String("in_progress".to_string());
    in_progress["output"] = json!([]);

    push_sse(
        &mut stream,
        "response.created",
        json!({ "type": "response.created", "response": in_progress }),
    );
    push_sse(
        &mut stream,
        "response.in_progress",
        json!({ "type": "response.in_progress", "response": body_without_output(&body) }),
    );

    for (index, item) in body
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        push_sse(
            &mut stream,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": item,
            }),
        );
        push_message_text_events(&mut stream, index, item);
        push_sse(
            &mut stream,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item,
            }),
        );
    }

    push_sse(
        &mut stream,
        "response.completed",
        json!({ "type": "response.completed", "response": body }),
    );
    stream.push_str("data: [DONE]\n\n");

    ([(header::CONTENT_TYPE, "text/event-stream")], stream).into_response()
}

fn body_without_output(body: &Value) -> Value {
    let mut value = body.clone();
    value["output"] = json!([]);
    value
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
    let mut previous_messages = responses::parse_input(&Value::Array(previous_items));
    previous_messages.append(&mut request.messages);
    request.messages = previous_messages;
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
    use axum::http::HeaderMap;
    use serde_json::json;

    use super::{
        apply_tool_choice_to_tool_calls, codex_managed_provider_block, codex_managed_root_block,
        find_deepseek_bearer, replace_codex_managed_blocks, should_route_to_default_deepseek_model,
        upstream_options_from_headers,
    };
    use crate::types::{ParsedToolCall, ToolChoice, UnifiedRequest};
    use crate::upstream::UpstreamRequestOptions;

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
}
