use anyhow::Result;
use clap::Parser;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use wind_monitor::config::{Config, DEFAULT_CONFIG_PATH};
use wind_monitor::{notify, service};

/// Watches Windguru forecasts and sends push alerts when the wind is on.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file path.
    #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    /// State file path; overrides `state_file` from the config.
    #[arg(long, value_name = "PATH")]
    state_file: Option<PathBuf>,

    /// Run a single check, print matches and exit without notifying.
    #[arg(long, conflicts_with = "test_notify")]
    once: bool,

    /// Send a test notification and exit.
    #[arg(long)]
    test_notify: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    init_logging();
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let mut config = Config::load(&cli.config)?;
    if let Some(path) = cli.state_file {
        config.state_file = path;
    }

    if cli.once {
        let ok = service::run_once(&config, chrono::Utc::now()).await?;
        return Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }

    let notifiers = notify::from_config(&config.notify)?;
    if cli.test_notify {
        notify::send_all(&notifiers, &notify::test_message()).await?;
        println!("test notification sent");
        return Ok(ExitCode::SUCCESS);
    }

    let interval = Duration::from_secs(config.check_interval_min * 60);
    tracing::info!(
        spots = config.spots.len(),
        interval_min = config.check_interval_min,
        state = %config.state_file.display(),
        "wind-monitor started"
    );
    let mut svc = service::Service::new(config, notifiers)?;
    loop {
        svc.run_cycle(chrono::Utc::now()).await;
        let sleep = interval + jitter(interval);
        tokio::select! {
            _ = tokio::time::sleep(sleep) => {}
            _ = shutdown_signal() => break,
        }
    }
    tracing::info!("shutting down");
    Ok(ExitCode::SUCCESS)
}

/// Random delay of up to 10% of the interval (at most 5 minutes), so checks
/// don't hit the feed at fixed times.
fn jitter(interval: Duration) -> Duration {
    let max = (interval / 10).min(Duration::from_secs(300)).as_secs();
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    Duration::from_secs(if max == 0 { 0 } else { random % (max + 1) })
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    // journald adds its own timestamps.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().with_ansi(false).init();
    } else {
        builder.init();
    }
}
