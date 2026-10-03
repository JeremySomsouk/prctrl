use clap::Parser;
use prctrl::cli::{Cli, Commands};
use prctrl::commands::dispatch::{dispatch, CommandContext};
use prctrl::commands::helpers;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Handle config commands without requiring env vars
    if let Some(Commands::Config { action }) = &cli.command {
        match action {
            prctrl::cli::ConfigAction::Init { force } => {
                helpers::run_config_init(*force)?;
            }
            prctrl::cli::ConfigAction::Show => {
                helpers::run_config_show()?;
            }
            prctrl::cli::ConfigAction::Update {
                token,
                username,
                org,
                repos,
                teams,
                ..
            } => {
                helpers::run_config_update(
                    token.as_deref(),
                    username.as_deref(),
                    org.as_deref(),
                    repos.as_deref(),
                    teams.as_deref(),
                )?;
            }
        }
        return Ok(());
    }

    let ctx = CommandContext::build(cli).await?;
    dispatch(ctx).await
}
