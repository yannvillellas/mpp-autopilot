//! Command-line entry point for the backtest harness.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mpp_autopilot::backtest::{self, Options};

const HELP: &str = "\
Usage: backtest [OPTIONS]

Measures the Ligue 1 model on completed Football-Data seasons.

Options:
  --refresh   Download every season again instead of using backtest-data/
  --check     Compare the metrics with backtest-reference.json
  -h, --help  Print this help

Exit codes:
  0  success
  1  runtime error or failed check
  2  invalid arguments
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Args {
    refresh: bool,
    check: bool,
    help: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("error: {error}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };

    if args.help {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }

    let options = Options {
        data_dir: PathBuf::from(backtest::DATA_DIR),
        refresh: args.refresh,
    };
    let report = match backtest::run(&options).await {
        Ok(report) => report,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
    };
    report.print();

    let reference_path = Path::new(backtest::REFERENCE_PATH);
    if args.check {
        check(&report, reference_path)
    } else {
        write_reference_if_missing(&report, reference_path)
    }
}

fn check(report: &backtest::Report, path: &Path) -> ExitCode {
    let reference = match backtest::read_reference(path) {
        Ok(reference) => reference,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
    };

    match backtest::check(report, &reference) {
        Ok(outcome) => {
            for key in &outcome.skipped {
                eprintln!("warning: {key} is absent from the report; source not available");
            }
            println!(
                "check OK: {} configurations within {:.0e}",
                outcome.checked,
                backtest::TOLERANCE
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("check failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn write_reference_if_missing(report: &backtest::Report, path: &Path) -> ExitCode {
    if path.exists() {
        return ExitCode::SUCCESS;
    }
    match backtest::write_reference(report, path) {
        Ok(()) => {
            println!("wrote reference {}", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

fn parse_args<I>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = String>,
{
    let mut parsed = Args {
        refresh: false,
        check: false,
        help: false,
    };

    for arg in args {
        match arg.as_str() {
            "--refresh" => parsed.refresh = true,
            "--check" => parsed.check = true,
            "-h" | "--help" => parsed.help = true,
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Result<Args, String> {
        parse_args(values.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn parses_refresh_and_help() {
        assert_eq!(
            args(&[]).expect("no arguments"),
            Args {
                refresh: false,
                check: false,
                help: false
            }
        );
        assert_eq!(
            args(&["--refresh", "--check"]).expect("refresh and check"),
            Args {
                refresh: true,
                check: true,
                help: false
            }
        );
        assert_eq!(
            args(&["--refresh", "--help"]).expect("both flags"),
            Args {
                refresh: true,
                check: false,
                help: true
            }
        );
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(args(&["--now"]).is_err());
    }
}
