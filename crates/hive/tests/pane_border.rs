//! Real-tmux rendering of the hive pane-border format. tmux itself evaluates
//! the format, so the assertions run the real renderer via
//! `display-message -p`, inside a detached session of the test's own.

mod common;
use common::{kill_session, private_server, require_tmux, run_tmux, PrivateServer};

struct BorderPane {
    // `kill_session` in Drop runs before any field drops, so the server
    // directory outlives the session whatever the field order.
    _server: PrivateServer,
    session: String,
    pane: String,
}

impl BorderPane {
    fn new(tag: &str) -> Self {
        let server = private_server();
        let session = format!("hive-e2e-border-{tag}-{}", std::process::id());
        let pane = run_tmux(&[
            "new-session",
            "-d",
            "-s",
            &session,
            "-x",
            "80",
            "-y",
            "20",
            "-P",
            "-F",
            "#{pane_id}",
        ]);
        BorderPane {
            _server: server,
            session,
            pane,
        }
    }

    fn set(&self, key: &str, value: &str) {
        run_tmux(&["set-option", "-p", "-t", &self.pane, key, value]);
    }

    fn render(&self) -> String {
        run_tmux(&[
            "display-message",
            "-p",
            "-t",
            &self.pane,
            hive::tmux::_HIVE_PANE_BORDER_FORMAT,
        ])
    }
}

impl Drop for BorderPane {
    fn drop(&mut self) {
        kill_session(&self.session);
    }
}

#[test]
fn test_border_follows_the_viewed_session() {
    require_tmux();
    let p = BorderPane::new("view");
    p.set("@hive-agent", "red");
    p.set("@hive-team", "probe");
    p.set("@hive-cli", "claude");
    // A drifted terminal title never speaks for itself: the probe does.
    run_tmux(&["select-pane", "-t", &p.pane, "-T", "whatever the TUI wrote"]);

    // On its own member (or nothing identifiable on screen): the member's
    // full name.
    p.set("@hive-view", "");
    assert_eq!(p.render(), " probe.red ");

    // Viewer switched to another member: dual display, both sides named.
    p.set("@hive-view", "comb.blue");
    assert_eq!(
        p.render(),
        " probe.red#[fg=colour220] -> comb.blue#[default] "
    );

    // Notify marker composes with the view suffix.
    p.set("@hive-notify-active", "1");
    assert!(p
        .render()
        .starts_with(" #[fg=colour220]#[bold][!] #[default]probe.red"));
}

#[test]
fn test_border_untagged_pane_falls_back_to_pane_title() {
    require_tmux();
    let p = BorderPane::new("title");
    run_tmux(&["select-pane", "-t", &p.pane, "-T", "plain shell"]);
    assert_eq!(p.render(), " plain shell ");
}

#[test]
fn test_border_without_a_team_tag_shows_the_bare_agent() {
    require_tmux();
    let p = BorderPane::new("bare");
    p.set("@hive-agent", "red");
    run_tmux(&["select-pane", "-t", &p.pane, "-T", "whatever the TUI wrote"]);
    assert_eq!(p.render(), " red ");
}
