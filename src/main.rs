use anyhow::Result;

mod agent;
mod app;
mod cli;
mod config;
mod daemon;
mod git;
mod install;
mod ipc;
mod kill;
mod notify;
mod session;
mod settings;
#[cfg(test)]
mod test_env;
mod tui;
mod ui;

use cli::{Cli, Command};

fn main() -> Result<()> {
    // Captured before argument parsing so a `status --from-hook` push's
    // timestamp reflects this process's actual invocation moment as closely
    // as possible, not delayed by clap parsing or `--from-hook`'s transcript
    // read (see `AgentRegistry::set_status`'s staleness guard, STATUS-RACE.md).
    let pushed_at = std::time::SystemTime::now();

    use clap::Parser;
    let parsed = Cli::parse();
    let socket = cli::resolve_socket(parsed.socket);

    match parsed.cmd {
        None => tui::run_tui(socket, parsed.mock),
        Some(Command::Install { agent, uninstall }) => install::run_install(&agent, uninstall),
        Some(Command::Daemon) => daemon::run_daemon(socket),
        Some(Command::Kill) => kill::run_kill(socket),
        Some(cmd) => cli::run_client(socket, cmd, pushed_at),
    }
}
