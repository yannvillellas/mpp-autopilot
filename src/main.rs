//! CLI entrypoint and process wiring.

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use chrono_tz::Europe::Paris;
use mpp_autopilot::cli::{self, Action, Command};
use mpp_autopilot::config::{self, Config};
use mpp_autopilot::elo::{EloRatings, FootballDataClient};
use mpp_autopilot::mpp::{MppApi, MppClient};
use mpp_autopilot::odds::OddsApiClient;
use mpp_autopilot::report;
use mpp_autopilot::run;
use mpp_autopilot::service;
use mpp_autopilot::state::{Mode, State};
use tracing::{Level, info, warn};
use tracing_subscriber::filter::LevelFilter;

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
        Command::Score => score(&config).await,
        Command::Run => run_service(&config, mode).await,
    }
}

fn init_tracing(level: &str) -> Result<()> {
    let level = parse_log_level(level)?;
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::from(level))
        .with_ansi(std::io::stdout().is_terminal())
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize tracing: {error}"))?;
    Ok(())
}

fn parse_log_level(level: &str) -> Result<Level> {
    match level.to_ascii_uppercase().as_str() {
        "TRACE" => Ok(Level::TRACE),
        "DEBUG" => Ok(Level::DEBUG),
        "INFO" => Ok(Level::INFO),
        "WARN" => Ok(Level::WARN),
        "ERROR" => Ok(Level::ERROR),
        _ => bail!("invalid LOG_LEVEL {level:?}; expected TRACE, DEBUG, INFO, WARN or ERROR"),
    }
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
    let _elo = FootballDataClient::new(EloRatings::default())
        .context("failed to build Football-Data client")?;
    let _market = OddsApiClient::new(config.odds_api_key.clone())
        .context("failed to build The Odds API client")?;

    if let Some(parent) = config.state_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            bail!("state directory {} does not exist", parent.display());
        }
    }

    probe_state_directory(&config.state_path)?;

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

fn probe_state_directory(state_path: &Path) -> Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let probe = state_path.with_file_name(format!(
        ".mpp-autopilot-check-{}-{nonce}",
        std::process::id()
    ));

    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|mut file| std::io::Write::write_all(&mut file, b"ok"));

    if let Err(error) = result {
        let _ = std::fs::remove_file(&probe);
        return Err(error)
            .with_context(|| format!("state directory is not writable: {}", probe.display()));
    }

    if let Err(error) = std::fs::remove_file(&probe) {
        warn!("failed to remove state probe {}: {error}", probe.display());
    }
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
    let state = State::load(&config.state_path, mode)
        .with_context(|| format!("failed to load state {}", config.state_path.display()))?;
    let elo = FootballDataClient::new(state.cached_elo())
        .context("failed to build Football-Data client")?;
    let market = OddsApiClient::new(config.odds_api_key.clone())
        .context("failed to build The Odds API client")?;

    let report = service::run_once(
        mode,
        state,
        config.state_path.clone(),
        &mpp,
        &elo,
        &market,
        Utc::now(),
    )
    .await
    .context("once failed")?;

    info!(
        matches = report.matches,
        results = report.results,
        triggers = report.triggers,
        writes = report.writes,
        unchanged = report.unchanged,
        dry_writes = report.dry_writes,
        finished = report.finished,
        skipped = report.skipped,
        "once complete"
    );
    Ok(())
}

async fn score(config: &Config) -> Result<()> {
    let mpp = MppClient::new(config.mpp_token.clone()).context("failed to build MPP client")?;
    let mut state = State::load_unmodified(&config.state_path)
        .with_context(|| format!("failed to load state {}", config.state_path.display()))?;

    match mpp.fetch_snapshot().await {
        Ok(snapshot) => {
            match run::refresh_results(
                &mpp,
                &mut state,
                &config.state_path,
                &snapshot.clubs,
                Utc::now(),
                run::MAX_REPORT_HISTORY_PAGES,
            )
            .await
            {
                Ok(results) => info!(results, "forecast history refreshed"),
                Err(error) => warn!(
                    error = %error,
                    "cannot refresh the forecast history; reporting the stored ledger"
                ),
            }
        }
        Err(error) => warn!(
            error = %error,
            "cannot read MPP clubs; reporting the stored ledger"
        ),
    }

    print!("{}", report::score_report(&state));
    Ok(())
}

async fn run_service(config: &Config, mode: Mode) -> Result<()> {
    let mpp = MppClient::new(config.mpp_token.clone()).context("failed to build MPP client")?;
    let state = State::load(&config.state_path, mode)
        .with_context(|| format!("failed to load state {}", config.state_path.display()))?;
    let elo = FootballDataClient::new(state.cached_elo())
        .context("failed to build Football-Data client")?;
    let market = OddsApiClient::new(config.odds_api_key.clone())
        .context("failed to build The Odds API client")?;

    service::run_forever(
        mode,
        config.daily_hour,
        state,
        config.state_path.clone(),
        &mpp,
        &elo,
        &market,
    )
    .await
    .context("service loop failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "mpp-autopilot-main-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn log_level_accepts_standard_levels() {
        assert_eq!(parse_log_level("TRACE").expect("trace"), Level::TRACE);
        assert_eq!(parse_log_level("debug").expect("debug"), Level::DEBUG);
        assert_eq!(parse_log_level("Info").expect("info"), Level::INFO);
        assert_eq!(parse_log_level("warn").expect("warn"), Level::WARN);
        assert_eq!(parse_log_level("ERROR").expect("error"), Level::ERROR);
    }

    #[test]
    fn log_level_rejects_unknown_values() {
        assert!(parse_log_level("verbose").is_err());
        assert!(parse_log_level("mpp=debug").is_err());
        assert!(parse_log_level("1").is_err());
        assert!(parse_log_level("5").is_err());
    }

    #[test]
    fn state_probe_writes_and_cleans_up() {
        let dir = temp_dir();
        let state_path = dir.join("state.json");

        probe_state_directory(&state_path).expect("writable state directory");

        assert!(!state_path.exists());
        assert!(
            std::fs::read_dir(&dir)
                .expect("read temp dir")
                .next()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_probe_rejects_unwritable_location() {
        let dir = temp_dir();
        let file = dir.join("not-a-directory");
        std::fs::write(&file, b"x").expect("write file");

        assert!(probe_state_directory(&file.join("state.json")).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
