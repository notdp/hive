mod adapters;
mod agent;
mod agent_cli;
mod bus;
mod cli;
mod context;
mod core_hooks;
mod devlog;
mod draft_guard;
mod flow;
mod hived;
mod layout;
mod notify_debug;
mod notify_ui;
mod plugin_manager;
mod registry;
mod runtime_snapshot;
mod runtime_state;
mod settings;
mod team;
mod tmux;
mod transcript_view;
mod worktree;

fn main() {
    cli::main();
}
