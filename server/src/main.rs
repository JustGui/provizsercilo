mod app;
mod catalog;
mod config;
mod error;
mod executor;
mod handlers;
mod stats;

use tracing::warn;
use tracing_subscriber::{fmt, EnvFilter};

use crate::config::{Config, LogFormat};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env();

    // Logging setup
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    match config.log_format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(filter)
                .init();
        }
    }

    if config.admin_token.is_none() {
        warn!("ADMIN_TOKEN not set — admin endpoints are disabled");
    }

    if !config.profiles_path.exists() {
        warn!(
            path = %config.profiles_path.display(),
            "profiles.toml not found — using empty language profile set"
        );
    }

    tracing::info!(
        port = config.port,
        db = %config.database_path.display(),
        "starting proviz-sercilo"
    );

    let (router, _state) = app::build_app(config.clone()).await?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!(addr = %addr, "listening");
    axum::serve(listener, router).await?;

    Ok(())
}
