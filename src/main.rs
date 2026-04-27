use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tokio::signal;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use amp_proxy::capture_pretty::{format_capture_file, write_pretty_file};
use amp_proxy::config::Config;
use amp_proxy::customproxy;
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
    /// Pretty-print a body_capture .log file as structured JSON.
    CapturePretty(CapturePrettyArgs),
}

#[derive(Debug, Args)]
struct CapturePrettyArgs {
    /// Path to the body_capture .log file.
    path: PathBuf,

    /// Write pretty JSON to this path instead of stdout.
    #[arg(long)]
    output: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Some(Cmd::Init(args)) => return run_init(args),
        Some(Cmd::CapturePretty(args)) => return run_capture_pretty(args),
        None => {}
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(cli.config))
}

fn run_capture_pretty(args: CapturePrettyArgs) -> anyhow::Result<()> {
    if let Some(output) = args.output {
        write_pretty_file(&args.path, &output)
    } else {
        println!("{}", format_capture_file(&args.path)?);
        Ok(())
    }
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
    let health_checker = tokio::spawn(provider_health_checker());

    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    watcher.abort();
    health_checker.abort();

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

async fn provider_health_checker() {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(10))
        .build()
        .expect("build provider health-check client");
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tick.tick().await;
        for provider in customproxy::global()
            .health_snapshots()
            .into_iter()
            .filter(|p| !p.healthy)
        {
            let url = provider_models_url(&provider.url);
            let mut req = client.get(&url);
            let bearer = provider.api_key.trim();
            if !bearer.is_empty() {
                req = req.bearer_auth(bearer);
            }
            match req.send().await {
                Ok(resp) => {
                    customproxy::global().record_success(&provider.name);
                    info!(
                        provider = %provider.name,
                        url = %url,
                        status = resp.status().as_u16(),
                        "custom provider health check recovered"
                    );
                }
                Err(e) => {
                    customproxy::global().record_failure(&provider.name, e.to_string());
                    warn!(
                        provider = %provider.name,
                        url = %url,
                        error = %e,
                        "custom provider health check failed"
                    );
                }
            }
        }
    }
}

fn provider_models_url(raw: &str) -> String {
    let base = raw.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
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
