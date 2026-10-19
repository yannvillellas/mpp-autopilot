//! CLI entrypoint and process wiring.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use chrono_tz::Europe::Paris;
use mpp::cli::{self, Action, Command};
use mpp::config::{self, Config};
use mpp::elo::ClubEloClient;
use mpp::mpp::{MppApi, MppClient};
use mpp::service;
use mpp::state::{Mode, State};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    match cli::parse_args(std::env::args().skip(1)) {
        Ok(Action::Help) => {
            print!("{}", cli::HELP);
            ExitCode::SUCCESS
        }
        Ok(Action::Command(command)) => match run_command(command).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error:#}");
                ExitCode::from(1)
            }
        },
        Err(error) => {
            eprintln!("error: {error}\n\n{}", cli::USAGE);
            ExitCode::from(2)
        }
    }
}

async fn run_command(command: Command) -> Result<()> {
    let config = Config::load().context("failed to load configuration")?;
    init_tracing(&config.log_level)?;
    log_startup(&config)?;

    let mode = Mode::from_write(config.write);

    match command {
        Command::Check => check(&config, mode),
        Command::List => list(&config).await,
        Command::Once => once(&config, mode).await,
        Command::Run => run_service(&config, mode).await,
    }
}

fn init_tracing(level: &str) -> Result<()> {
    let filter =
        EnvFilter::try_new(level).with_context(|| format!("invalid LOG_LEVEL {level:?}"))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal())
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize tracing: {error}"))?;
    Ok(())
}

fn log_startup(config: &Config) -> Result<()> {
    info!(
        write = config.write,
        daily_hour = config.daily_hour,
        timezone = config::TIMEZONE,
        state_path = %config.state_path.display(),
        "configuration loaded"
    );

    let expires_at =
        config::token_expiry(&config.mpp_token).context("MPP_TOKEN is not a valid JWT")?;
    info!(expires_at = %expires_at, "MPP token expiry");
    if config::token_expires_soon(expires_at, Utc::now()) {
        warn!(expires_at = %expires_at, "MPP token expires within 3 days");
    }

    Ok(())
}

fn check(config: &Config, mode: Mode) -> Result<()> {
    let _mpp = MppClient::new(config.mpp_token.clone()).context("failed to build MPP client")?;
    let _elo = ClubEloClient::new().context("failed to build ClubElo client")?;

    if let Some(parent) = config.state_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            bail!("state directory {} does not exist", parent.display());
        }
    }

    let state = State::load(&config.state_path, mode)
        .with_context(|| format!("failed to load state {}", config.state_path.display()))?;

    println!(
        "configuration OK: {} state matches, mode {:?}, state {}",
        state.matches.len(),
        mode,
        config.state_path.display()
    );
    Ok(())
}

async fn list(config: &Config) -> Result<()> {
    let mpp = MppClient::new(config.mpp_token.clone()).context("failed to build MPP client")?;
    let snapshot = mpp
        .fetch_snapshot()
        .await
        .context("failed to fetch MPP matches")?;

    if snapshot.matches.is_empty() {
        println!("No Ligue 1 matches.");
        return Ok(());
    }

    for fixture in snapshot.matches {
        let kickoff = fixture.kickoff.with_timezone(&Paris);
        let week = fixture
            .game_week
            .map(|week| format!("GW{week} "))
            .unwrap_or_default();
        println!(
            "{}  {}{} - {}  [{}/{}/{}]  {}",
            kickoff.format("%Y-%m-%d %H:%M"),
            week,
            fixture.home.name,
            fixture.away.name,
            fixture.odds.home,
            fixture.odds.draw,
            fixture.odds.away,
            fixture.id
        );
    }

    Ok(())
}

async fn once(config: &Config, mode: Mode) -> Result<()> {
    let mpp = MppClient::new(config.mpp_token.clone()).context("failed to build MPP client")?;
    let elo = ClubEloClient::new().context("failed to build ClubElo client")?;
    let state = State::load(&config.state_path, mode)
        .with_context(|| format!("failed to load state {}", config.state_path.display()))?;

    let report = service::run_once(
        mode,
        state,
        config.state_path.clone(),
        &mpp,
        &elo,
        Utc::now(),
    )
    .await
    .context("once failed")?;

    info!(
        matches = report.matches,
        triggers = report.triggers,
        writes = report.writes,
        unchanged = report.unchanged,
        dry_writes = report.dry_writes,
        skipped = report.skipped,
        "once complete"
    );
    Ok(())
}

async fn run_service(config: &Config, mode: Mode) -> Result<()> {
    let mpp = MppClient::new(config.mpp_token.clone()).context("failed to build MPP client")?;
    let elo = ClubEloClient::new().context("failed to build ClubElo client")?;
    let state = State::load(&config.state_path, mode)
        .with_context(|| format!("failed to load state {}", config.state_path.display()))?;

    service::run_forever(
        mode,
        config.daily_hour,
        state,
        config.state_path.clone(),
        &mpp,
        &elo,
    )
    .await
    .context("service loop failed")?;
    Ok(())
}
