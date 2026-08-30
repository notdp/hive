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
    // The spawned daemon re-enters this binary as `hive --hived <workspace>
    // <team> <tmux_window> <tmux_window_id>` — route before clap parsing.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--hived") {
        std::process::exit(hived::_run_spawned_hived(&args));
    }
    cli::main();
}
