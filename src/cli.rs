//! Manual command-line parsing.

use thiserror::Error;

/// Command selected on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Run the long-running service.
    Run,
    /// Run one daily fetch and due triggers.
    Once,
    /// List Ligue 1 matches and quotations.
    List,
    /// Validate configuration and dependencies.
    Check,
}

/// Parsed command-line action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Execute a command.
    Command(Command),
    /// Print help and exit successfully.
    Help,
}

/// CLI parsing error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CliError {
    /// The argument is not a known command or flag.
    #[error("unknown argument: {0}")]
    UnknownArgument(String),
    /// More than one argument was provided.
    #[error("too many arguments")]
    TooManyArguments,
}

/// Short usage line.
pub const USAGE: &str = "Usage: mpp [once|list|check]";

/// Full help text.
pub const HELP: &str = "\
Usage: mpp [COMMAND]

Commands:
  once    Run one daily fetch and the due triggers, then exit
  list    List Ligue 1 matches and quotations
  check   Validate configuration and dependencies
  (none)  Run continuously

Options:
  -h, --help  Print this help

Exit codes:
  0  success
  1  runtime or configuration error
  2  invalid arguments
";

/// Parses arguments, excluding the program name.
pub fn parse_args<I>(args: I) -> Result<Action, CliError>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();

    match args.as_slice() {
        [] => Ok(Action::Command(Command::Run)),
        [arg] if arg == "-h" || arg == "--help" => Ok(Action::Help),
        [arg] if arg == "once" => Ok(Action::Command(Command::Once)),
        [arg] if arg == "list" => Ok(Action::Command(Command::List)),
        [arg] if arg == "check" => Ok(Action::Command(Command::Check)),
        [arg] => Err(CliError::UnknownArgument(arg.clone())),
        _ => Err(CliError::TooManyArguments),
    }
}
