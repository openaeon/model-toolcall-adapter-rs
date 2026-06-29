mod config;
mod error;
mod http;
mod protocol;
mod providers;
mod responses_store;
mod types;
mod ui;
mod upstream;
mod wire;

use clap::Parser;
use config::AppConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,tower_http=info".to_string()),
        )
        .init();

    let config = AppConfig::parse().load_or_init()?;
    http::serve(config).await
}
