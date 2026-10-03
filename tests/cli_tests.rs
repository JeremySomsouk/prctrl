use clap::{error::ErrorKind, Parser};
use prctrl::cli::{Cli, Commands, ConfigAction};

// Clap builds the full command tree; use the same stack size as the main thread.
fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, || Cli::try_parse_from(args))
            .unwrap()
            .join()
            .unwrap()
    })
}

#[test]
fn no_subcommand_defaults_to_tui_with_standard_interval() {
    let implicit = parse(&["prctrl"]).unwrap();
    let explicit = parse(&["prctrl", "tui"]).unwrap();
    for cli in [implicit, explicit] {
        assert!(matches!(
            cli.command.unwrap_or_default(),
            Commands::Tui { interval: 30 }
        ));
    }
}

#[test]
fn global_options_work_without_a_subcommand() {
    let cli = parse(&["prctrl", "--include-drafts"]).unwrap();
    assert!(cli.include_drafts);
    assert!(matches!(
        cli.command.unwrap_or_default(),
        Commands::Tui { .. }
    ));
}

#[test]
fn explicit_commands_and_tui_options_are_preserved() {
    let cli = parse(&["prctrl", "tui", "--interval", "0"]).unwrap();
    assert!(matches!(cli.command, Some(Commands::Tui { interval: 0 })));
    let cli = parse(&["prctrl", "config", "init"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::Config {
            action: ConfigAction::Init { force: false }
        })
    ));
    let cli = parse(&["prctrl", "list", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::List { json: true, .. })
    ));
}

#[test]
fn help_version_and_invalid_commands_do_not_launch_tui() {
    for (args, expected) in [
        (vec!["prctrl", "--help"], ErrorKind::DisplayHelp),
        (vec!["prctrl", "--version"], ErrorKind::DisplayVersion),
        (vec!["prctrl", "typo"], ErrorKind::InvalidSubcommand),
    ] {
        assert_eq!(parse(&args).unwrap_err().kind(), expected);
    }
}
