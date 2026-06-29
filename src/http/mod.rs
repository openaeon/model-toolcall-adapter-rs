use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::sync::Mutex;

use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::config::AppConfig;
use crate::responses_store::ResponseStore;
use crate::upstream::OpenAiChatUpstream;

mod routes;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub upstream: OpenAiChatUpstream,
    pub responses: ResponseStore,
    pub provider_conversations: Arc<Mutex<HashMap<String, String>>>,
    pub active_provider_runs: Arc<Mutex<HashSet<String>>>,
    pub setup: Arc<Mutex<SetupState>>,
}

#[derive(Default)]
pub struct SetupState {
    pub deepseek_browser: Option<DeepSeekBrowserProcess>,
}

pub struct DeepSeekBrowserProcess {
    pub port: u16,
    pub user_data_dir: PathBuf,
    pub child: Option<Child>,
}

pub async fn serve(config: AppConfig) -> anyhow::Result<()> {
    let bind: SocketAddr = config.bind.parse()?;
    let upstream = OpenAiChatUpstream::new(&config)?;
    let responses = ResponseStore::default();
    let state = Arc::new(AppState {
        config,
        upstream,
        responses,
        provider_conversations: Arc::new(Mutex::new(HashMap::new())),
        active_provider_runs: Arc::new(Mutex::new(HashSet::new())),
        setup: Arc::new(Mutex::new(SetupState::default())),
    });

    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/", get(routes::ui))
        .route("/ui", get(routes::ui))
        .route("/deepseek-web/login", post(routes::deepseek_web_login))
        .route(
            "/deepseek-web/session",
            post(routes::deepseek_web_session_save),
        )
        .route("/setup/state", get(routes::setup_state))
        .route("/setup/provider", post(routes::setup_provider))
        .route(
            "/setup/deepseek-browser/start",
            post(routes::setup_deepseek_browser_start),
        )
        .route(
            "/setup/deepseek-browser/capture",
            post(routes::setup_deepseek_browser_capture),
        )
        .route("/setup/codex/apply", post(routes::setup_codex_apply))
        .route("/v1/models", get(routes::models))
        .route("/v1/chat/completions", post(routes::chat_completions))
        .route("/v1/messages", post(routes::messages))
        .route("/responses", post(routes::responses))
        .route("/responses/compact", post(routes::responses_compact))
        .route("/responses/{response_id}", get(routes::responses_retrieve))
        .route(
            "/responses/{response_id}/input_items",
            get(routes::responses_input_items),
        )
        .route(
            "/responses/{response_id}/cancel",
            post(routes::responses_cancel),
        )
        .route("/v1/responses", post(routes::responses))
        .route("/v1/responses/compact", post(routes::responses_compact))
        .route(
            "/v1/responses/{response_id}",
            get(routes::responses_retrieve),
        )
        .route(
            "/v1/responses/{response_id}/input_items",
            get(routes::responses_input_items),
        )
        .route(
            "/v1/responses/{response_id}/cancel",
            post(routes::responses_cancel),
        )
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::anyhow!(
                "address http://{bind} is already in use; stop the existing adapter process or run with ADAPTER_BIND=127.0.0.1:8899"
            )
        } else {
            anyhow::Error::new(error)
        }
    })?;
    tracing::info!("model tool-call adapter listening on http://{bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
