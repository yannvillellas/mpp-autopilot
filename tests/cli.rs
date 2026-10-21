//! CLI parsing tests.

use mpp_autopilot::cli::{Action, CliError, Command, parse_args};

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn no_argument_runs_continuously() {
    assert_eq!(
        parse_args(Vec::<String>::new()),
        Ok(Action::Command(Command::Run))
    );
}

#[test]
fn subcommands_are_recognized() {
    assert_eq!(
        parse_args(args(&["once"])),
        Ok(Action::Command(Command::Once))
    );
    assert_eq!(
        parse_args(args(&["list"])),
        Ok(Action::Command(Command::List))
    );
    assert_eq!(
        parse_args(args(&["check"])),
        Ok(Action::Command(Command::Check))
    );
}

#[test]
fn help_flags_are_recognized() {
    assert_eq!(parse_args(args(&["--help"])), Ok(Action::Help));
    assert_eq!(parse_args(args(&["-h"])), Ok(Action::Help));
}

#[test]
fn unknown_arguments_are_rejected() {
    assert_eq!(
        parse_args(args(&["unknown"])),
        Err(CliError::UnknownArgument("unknown".to_owned()))
    );
}

#[test]
fn extra_arguments_are_rejected() {
    assert_eq!(
        parse_args(args(&["once", "extra"])),
        Err(CliError::TooManyArguments)
    );
}

#[test]
fn binary_help_exits_zero() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mpp-autopilot"))
        .arg("--help")
        .output()
        .expect("run mpp-autopilot");

    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: mpp-autopilot"));
}

#[test]
fn binary_unknown_argument_exits_two() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mpp-autopilot"))
        .arg("unknown")
        .output()
        .expect("run mpp-autopilot");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown argument"));
}
