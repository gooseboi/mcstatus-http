#![deny(
    clippy::enum_glob_use,
    clippy::pedantic,
    clippy::nursery,
    clippy::unwrap_used
)]

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use color_eyre::Result;
use moka::future::{Cache, CacheBuilder};
use serde::Serialize;
use std::{env, net::SocketAddr, sync::Arc, time::Duration};
use tokio::process::Command;
use tracing::{debug, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Clone)]
struct AppState {
    mc_monitor_executable: Arc<str>,
    cache: Cache<String, ServerStatus>,
}

impl AppState {
    fn new() -> Self {
        const MC_MONITOR_EXECUTABLE: &str = "MC_MONITOR_EXECUTABLE";

        let mc_monitor_executable = env::var(MC_MONITOR_EXECUTABLE)
            .unwrap_or_else(|_| "mc-monitor".to_owned())
            .into();

        Self {
            mc_monitor_executable,
            cache: CacheBuilder::new(100)
                .time_to_live(Duration::from_secs(30))
                .time_to_idle(Duration::from_secs(15))
                .build(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ServerStatus {
    requested_url: String,
    exit_code: u8,
    output: String,
    error: Option<String>,
}

async fn fetch_status_from_server(
    url: &str,
    mc_monitor_executable: &str,
) -> Result<ServerStatus, (StatusCode, String)> {
    let output = Command::new(mc_monitor_executable)
        .arg("status")
        .args(["-host", &url])
        .output()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to spawn mc-monitor: {e}"),
            )
        })?;

    let stderr = output.stderr;
    let stderr = String::from_utf8(stderr.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mc-monitor outputted {stderr:?} on stderr, which was not utf-8: {e}"),
        )
    })?;
    let stderr = if stderr.is_empty() {
        None
    } else {
        Some(stderr)
    };

    let stdout = output.stdout;
    let stdout = String::from_utf8(stdout.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mc-monitor outputted {stdout:?} on stdin, which was not utf-8: {e}"),
        )
    })?;

    let exit_code = output
        .status
        .code()
        .expect("mc-monitor should terminate normally");
    let exit_code = exit_code
        .try_into()
        .expect("Exit codes should fit into u8s");

    Ok(ServerStatus {
        requested_url: url.to_owned(),
        exit_code,
        output: stdout,
        error: stderr,
    })
}

async fn get_status_for_server(
    Path(url): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<ServerStatus>, (StatusCode, String)> {
    debug!(%url, "Requested from api");

    let cache = state.cache.clone();
    let mc_monitor_executable = state.mc_monitor_executable;

    let status = cache
        .try_get_with_by_ref(&url, async {
            fetch_status_from_server(&url, &mc_monitor_executable).await
        })
        .await
        .map_err(|e| (*e).clone())?;

    Ok(Json::from(status))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mcstatus_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
    color_eyre::install()?;

    let state = AppState::new();

    let quit_sig = async {
        _ = tokio::signal::ctrl_c().await;
        warn!("Initiating graceful shutdown");
    };

    let app = Router::new()
        .route("/:url", get(get_status_for_server))
        .with_state(state);
    let addr: SocketAddr = "0.0.0.0:3789".parse().expect("This is a valid address");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(quit_sig)
        .await?;

    Ok(())
}
