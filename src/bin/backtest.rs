//! Command-line entry point for the backtest harness.

use std::path::PathBuf;
use std::process::ExitCode;

use mpp_autopilot::backtest::{self, Options};

const HELP: &str = "\
Usage: backtest [OPTIONS]

Measures the Ligue 1 model on completed Football-Data seasons.

Options:
  --refresh   Download every season again instead of using backtest-data/
  -h, --help  Print this help

Exit codes:
  0  success
  1  runtime error
  2  invalid arguments
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Args {
    refresh: bool,
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
    match backtest::run(&options).await {
        Ok(report) => {
            report.print();
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
        help: false,
    };

    for arg in args {
        match arg.as_str() {
            "--refresh" => parsed.refresh = true,
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
                help: false
            }
        );
        assert_eq!(
            args(&["--refresh"]).expect("refresh"),
            Args {
                refresh: true,
                help: false
            }
        );
        assert_eq!(
            args(&["--refresh", "--help"]).expect("both flags"),
            Args {
                refresh: true,
                help: true
            }
        );
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(args(&["--check"]).is_err());
    }
}
