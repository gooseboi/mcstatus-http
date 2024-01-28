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
use std::{env, net::SocketAddr, sync::Arc};
use tokio::process::Command;
use tracing::{info, debug, warn, Instrument};
use tracing::debug_span;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Clone)]
struct AppState {
    mc_monitor_executable: Arc<str>,
    use_mc_monitor: Arc<bool>,
    cache: Cache<String, ServerStatus>,
}

impl AppState {
    fn new() -> Self {
        const MC_MONITOR_EXECUTABLE: &str = "MC_MONITOR_EXECUTABLE";
        const CACHE_TTL: &str = "CACHE_TTL";
        const USE_MC_MONITOR: &str = "USE_MC_MONITOR";

        let mc_monitor_executable = env::var(MC_MONITOR_EXECUTABLE)
            .unwrap_or_else(|_| "mc-monitor".to_owned())
            .into();

        let cache_ttl = env::var(CACHE_TTL).unwrap_or_else(|_| "10 seconds".to_owned());
        let cache_ttl = parse_duration::parse(&cache_ttl)
            .unwrap_or_else(|_| panic!("Expected string {cache_ttl} to be a duration"));

        let use_mc_monitor = env::var(USE_MC_MONITOR)
            .unwrap_or_else(|_| "true".to_owned())
            .parse::<bool>()
            .unwrap_or_else(|_| panic!("Failed parsing variable {USE_MC_MONITOR} into bool"))
            .into();

        info!(%mc_monitor_executable);
        info!(%use_mc_monitor);
        info!(?cache_ttl);

        Self {
            mc_monitor_executable,
            cache: CacheBuilder::new(100).time_to_live(cache_ttl).build(),
            use_mc_monitor,
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

async fn fetch_status_with_mc_monitor(
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
        .expect("mc-monitor should terminate normally")
        .try_into()
        .expect("Exit codes should fit into u8s");

    Ok(ServerStatus {
        requested_url: url.to_owned(),
        exit_code,
        output: stdout,
        error: stderr,
    })
}

async fn fetch_status_from_server(
    url: &str,
    use_mc_monitor: bool,
    mc_monitor_executable: &str,
) -> Result<ServerStatus, (StatusCode, String)> {
    if use_mc_monitor {
        let span = debug_span!("mc_monitor_fetch", url = url);
        fetch_status_with_mc_monitor(url, mc_monitor_executable)
            .instrument(span)
            .await
    } else {
        todo!()
    }
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
            fetch_status_from_server(&url, *state.use_mc_monitor, &mc_monitor_executable).await
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
