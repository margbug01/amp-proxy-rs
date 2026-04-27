use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::signal;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use amp_proxy::config::Config;
use amp_proxy::init::{run as run_init, InitArgs};
use amp_proxy::server::{build_app, SharedState};

#[derive(Debug, Parser)]
#[command(name = "amp-proxy", version)]
struct Cli {
    /// Path to the YAML config file (when running as a server).
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Generate a ready-to-run config.yaml interactively.
    Init(InitArgs),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(Cmd::Init(args)) = cli.cmd {
        return run_init(args);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(cli.config))
}

async fn serve(config_path: PathBuf) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cfg = Config::load(&config_path).map_err(|e| anyhow::anyhow!("load config: {e}"))?;

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let (app, state) = build_app(&cfg).map_err(|e| anyhow::anyhow!("build app: {e}"))?;

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(%addr, "amp-proxy listening");

    // Spawn a config-file watcher that polls mtime once a second.
    let watcher = tokio::spawn(watch_config(config_path.clone(), state.clone()));

    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    watcher.abort();

    serve_result.map_err(|e| {
        error!("server: {e}");
        anyhow::anyhow!(e)
    })?;
    info!("amp-proxy shut down cleanly");
    Ok(())
}

async fn watch_config(path: PathBuf, state: SharedState) {
    let mut last_mod = match tokio::fs::metadata(&path).await {
        Ok(m) => m.modified().ok(),
        Err(e) => {
            warn!(error = %e, "config watcher: stat failed; reload disabled");
            return;
        }
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let modified = match tokio::fs::metadata(&path).await {
            Ok(m) => m.modified().ok(),
            Err(e) => {
                warn!(error = %e, "config watcher: stat failed");
                continue;
            }
        };
        if modified == last_mod {
            continue;
        }
        last_mod = modified;
        match Config::load(&path) {
            Ok(cfg) => {
                state.validator.set_keys(cfg.api_keys.clone());
                if let Err(e) = state.amp_module.on_config_updated(&cfg.ampcode) {
                    error!(error = %e, "config watcher: apply failed");
                    continue;
                }
                info!(path = %path.display(), "config reloaded");
            }
            Err(e) => {
                error!(error = %e, "config watcher: reload failed");
            }
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
