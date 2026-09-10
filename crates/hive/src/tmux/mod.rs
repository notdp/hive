//! tmux operations: pane lifecycle, send_keys, capture_pane, layout.

mod appearance;
mod context;
mod control_mode;
mod listing;
mod pane;
mod run;
mod session;
mod status;

pub use appearance::*;
pub use context::*;
pub use control_mode::*;
pub use listing::*;
pub use pane::*;
pub use run::*;
pub use session::*;
pub use status::*;

#[cfg(test)]
mod tests;

fn pane_cwd(cwd: Option<&str>) -> anyhow::Result<Option<&str>> {
    if let Some(cwd) = cwd {
        anyhow::ensure!(
            std::path::Path::new(cwd).is_dir(),
            "cannot spawn pane: working directory {cwd:?} is unavailable or is not a directory"
        );
    }
    Ok(cwd)
}

/// Terminal type assigned to panes, independent of the caller's tool shell.
pub(crate) fn default_terminal() -> String {
    match run(&["show-options", "-gv", "default-terminal"], false, 5) {
        Ok(r) if r.returncode == 0 && !r.stdout.trim().is_empty() => r.stdout.trim().to_string(),
        _ => "tmux-256color".to_string(),
    }
}
