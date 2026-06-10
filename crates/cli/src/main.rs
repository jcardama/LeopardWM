//! LeopardWM CLI
//!
//! Command-line interface for controlling the LeopardWM window manager.
//!
//! Commands are sent to the daemon via IPC (named pipe).

mod args;
mod command_map;
mod config_cmds;
mod daemon_cmds;
mod doctor;
mod ipc_client;
mod output;
#[cfg(test)]
mod tests;

use anyhow::Result;
use clap::Parser;

use args::{validate_set_width_fraction, Cli, Commands};
use command_map::to_ipc_command;
use config_cmds::handle_config;
use daemon_cmds::{
    handle_autostart, handle_emergency_uncloak, handle_panic_revert, handle_run, handle_status,
    handle_stop, handle_subscribe,
};
use doctor::{handle_collect_logs, handle_doctor};
use ipc_client::{is_non_success_response, send_command};
use output::print_response;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle locally-executed commands (do not use IPC command mapping)
    match cli.command {
        Commands::Run {
            no_apply,
            wait_ms,
            safe_mode,
            no_watchdog,
        } => return handle_run(no_apply, wait_ms, safe_mode, no_watchdog).await,
        Commands::Subscribe { events } => return handle_subscribe(events).await,
        Commands::Stop => return handle_stop().await,
        Commands::PanicRevert => return handle_panic_revert().await,
        Commands::EmergencyUncloak => return handle_emergency_uncloak(),
        Commands::Status => return handle_status().await,
        Commands::Doctor => return handle_doctor().await,
        Commands::Autostart { action } => return handle_autostart(action),
        Commands::CollectLogs => return handle_collect_logs(),
        Commands::Config { action } => return handle_config(action),
        _ => {}
    }

    if let Commands::SetWidth { fraction } = &cli.command {
        if let Err(message) = validate_set_width_fraction(*fraction) {
            anyhow::bail!(message);
        }
    }

    let ipc_cmd = to_ipc_command(&cli.command);
    let response = send_command(ipc_cmd).await?;
    print_response(&response);

    // Exit with error code if response was an error
    if is_non_success_response(&response) {
        std::process::exit(1);
    }

    Ok(())
}
