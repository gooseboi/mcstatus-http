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
use color_eyre::{
    eyre::{bail, ensure},
    Result,
};
use moka::future::{Cache, CacheBuilder};
use serde::Serialize;
use std::{
    env,
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};
use tokio::{net::TcpStream, process::Command};
use tracing::{debug, debug_span, info, warn, Instrument};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Clone, PartialEq, Eq, Hash)]
struct ServerAddr {
    domain_name: Option<String>,
    address: SocketAddr,
}

#[derive(Clone)]
struct AppState {
    mc_monitor_executable: Arc<str>,
    use_mc_monitor: Arc<bool>,
    cache: Cache<ServerAddr, ServerStatus>,
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
struct MonitorOutput {
    version: String,
    online_player_count: u16,
    max_player_count: u16,
    motd: String,
}

impl MonitorOutput {
    fn parse(output: &str) -> Result<Self> {
        let Some((_host, rest)) = output.split_once(" : ") else {
            bail!("Command output was not separated by `:`: {output}");
        };

        let Some((version, rest)) = rest.split_once(' ') else {
            bail!("Command output finished unexpectedly, expected ` `, found nothing: {rest}");
        };

        let Some((version_str, version)) = version.split_once('=') else {
            bail!("Version did not contain `=`. Found: {rest}");
        };
        ensure!(
            version_str == "version",
            "Version string was invalid: {version_str}"
        );
        let version = version.to_owned();

        let Some((online, rest)) = rest.split_once(' ') else {
            bail!("Command output finished unexpectedly, expected ` `, found nothing: {rest}");
        };

        let Some((online_str, online_player_count)) = online.split_once('=') else {
            bail!("Online player count did not contain `=`. Found: {rest}");
        };
        ensure!(
            online_str == "online",
            "Online string was invalid: {online_str}"
        );
        let online_player_count = match online_player_count.parse() {
            Ok(c) => c,
            Err(e) => bail!("Failed parsing player count {online_player_count}: {e}"),
        };

        let Some((max, rest)) = rest.split_once(' ') else {
            bail!("Command output finished unexpectedly, expected ` `, found nothing: {rest}");
        };

        let Some((max_str, max_player_count)) = max.split_once('=') else {
            bail!("Max player count did not contain `=`. Found: {rest}");
        };
        ensure!(max_str == "max", "Max string was invalid: {max_str}");
        let max_player_count = match max_player_count.parse() {
            Ok(c) => c,
            Err(e) => bail!("Failed parsing player count {max_player_count}: {e}"),
        };

        let Some((motd_str, motd)) = rest.split_once('=') else {
            bail!("motd did not contain `=`. Found: {rest}");
        };
        ensure!(motd_str == "motd", "motd string was invalid: {motd_str}");
        // motd='Minecraft server'
        let l = motd.len();
        let motd = motd[1..(l - 1)].to_owned();

        Ok(Self {
            version,
            online_player_count,
            max_player_count,
            motd,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
struct ServerStatus {
    requested_url: SocketAddr,
    exit_code: u8,
    output: Option<MonitorOutput>,
    error: Option<String>,
}

async fn fetch_status_with_mc_monitor(
    url: &SocketAddr,
    mc_monitor_executable: &str,
) -> Result<ServerStatus, (StatusCode, String)> {
    let child = Command::new(mc_monitor_executable)
        .arg("status")
        .args([
            "-host",
            &url.ip().to_string(),
            "-port",
            &url.port().to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to spawn mc-monitor: {e}"),
            )
        })?;
    info!("Spawned mc_monitor");
    let output = child.wait_with_output().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed running mc_monitor: {e}"),
        )
    })?;
    info!("mc_monitor exited");

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

    if stderr.is_some() {
        info!("mc_monitor returned an error");
    } else {
        info!("mc_monitor return successfully");
    }

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

    let output = if stderr.is_none() {
        let output = MonitorOutput::parse(&stdout).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed parsing mc_monitor output: {e}"),
            )
        })?;
        Some(output)
    } else {
        None
    };

    Ok(ServerStatus {
        requested_url: url.to_owned(),
        exit_code,
        output,
        error: stderr,
    })
}

async fn fetch_status_from_server(
    url: &SocketAddr,
    use_mc_monitor: bool,
    mc_monitor_executable: &str,
) -> Result<ServerStatus, (StatusCode, String)> {
    // FIXME: Make sure this url is actually valid
    let url_str = format!("{ip}:{port}", ip = url.ip(), port = url.port());
    if use_mc_monitor {
        let span = debug_span!("mc_monitor_fetch", url = url_str);
        fetch_status_with_mc_monitor(url, mc_monitor_executable)
            .instrument(span)
            .await
    } else {
        todo!()
    }
}

async fn get_status_for_server(
    Path(addr): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<ServerStatus>, (StatusCode, String)> {
    debug!(%addr, "Requested from api");

    let cache = state.cache.clone();
    let mc_monitor_executable = state.mc_monitor_executable;
    let domain_name = if addr.contains(|c| char::is_ascii_alphabetic(&c)) {
        let s = if addr.split(':').count() == 1 {
            addr.clone()
        } else {
            addr.split(':')
                .next()
                .ok_or((StatusCode::BAD_REQUEST, "Input url was empty".to_owned()))?
                .to_owned()
        };
        Some(s)
    } else {
        None
    };

    let addr = {
        let addr = match addr.split(':').count() {
            0 => unreachable!(),
            1 => format!("{addr}:25565"),
            2 => addr,
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("Invalid address {addr} for server, had too many `:`"),
                ))
            }
        };
        addr.to_socket_addrs()
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("addr {addr} was invalid: {e}"),
                )
            })?
            .next()
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("{addr} was addr, no addr was there"),
                )
            })?
    };

    let addr = ServerAddr {
        domain_name,
        address: addr,
    };
    // This is spawned in a task so the fetch isn't killed if the request is stopped This makes it
    // so repeated requests to the endpoint, while killing the previous request (like browser
    // refreshes) don't hammer the mc server.
    let handle = tokio::spawn(async move {
        cache
            .try_get_with_by_ref(&addr, async {
                fetch_status_from_server(
                    &addr.address,
                    *state.use_mc_monitor,
                    &mc_monitor_executable,
                )
                .await
            })
            .await
            .map_err(|e| (*e).clone())
    });
    let status = handle.await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to join cache thread: {e}"),
        )
    })?;
    let status = status?;

    Ok(Json::from(status))
}

#[tokio::main(flavor = "current_thread")]
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
        .route("/favicon.ico", get(favicon))
        .route("/:url", get(get_status_for_server))
        .with_state(state);
    let addr: SocketAddr = "0.0.0.0:3789".parse().expect("This is a valid address");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(quit_sig)
        .await?;

    Ok(())
}

async fn favicon(State(_): State<AppState>) -> StatusCode {
    StatusCode::NOT_FOUND
}
