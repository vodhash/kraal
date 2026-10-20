//! kraal: a terminal workspace manager for SSH fleets and coding agents.

mod account_status;
mod accounts;
mod agent;
mod agent_registry;
mod ansi;
mod attachments;
mod buffers;
mod cli;
mod client;
mod clipboard;
mod codex_usage;
mod config;
mod context;
mod edit;
mod exec;
mod files;
mod fleet;
#[cfg(test)]
mod fuzz;
mod git;
mod graphics;
mod hints;
mod identity;
mod input;
mod integration;
mod keymap;
mod laptop;
mod layout;
mod pane;
mod paths;
mod persist;
mod preview;
mod process;
mod protocol;
mod rewrite;
mod runbooks;
mod server;
mod servers;
mod shell;
#[cfg(test)]
mod testing;
mod text;
mod theme;
mod threads;
mod tmux;
mod tools;
mod transfer;
mod unknown_keys;
mod watch;
mod workspace;
mod worktree_setup;

fn main() {
    cli::main();
}
