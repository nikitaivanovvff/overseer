use anyhow::Result;

mod agent;
mod app;
mod cli;
mod config;
mod git;
mod install;
mod ipc;
mod session;
mod settings;
#[cfg(test)]
mod test_env;
mod tui;
mod ui;

use cli::{Cli, Command};

fn main() -> Result<()> {
    use clap::Parser;
    let parsed = Cli::parse();
    let socket = cli::resolve_socket(parsed.socket);

    match parsed.cmd {
        None => tui::run_tui(socket, parsed.mock),
        Some(Command::Install { agent, uninstall }) => install::run_install(&agent, uninstall),
        Some(cmd) => cli::run_client(socket, cmd),
    }
}
