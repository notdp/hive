//! What a pane is told its terminal background is.
//!
//! tmux answers a pane's OSC 10/11 query (a pane with no background of its
//! own) from the first client in its client list — creation order —
//! attached to a session holding the window. A terminal carries the
//! colours it answered tmux at attach, exactly; a control client carries
//! ANSI black. The hived's control client sits on every team session, so
//! when it is that first client the pane would hear black. Hive then
//! supplies per-pane answers through `refresh-client -r` (tmux 3.5, the
//! oldest hive supports), which override any client while a control client
//! exists on the server, and which tmux keeps for good. When a terminal is
//! the session's first client — attached before the hived's control client
//! — hive writes nothing and tmux answers with the terminal's own colours
//! (`reports_needed`). The hived's monitor and an attach's
//! `stamp_session_appearance` both follow that rule; a pane passed over is
//! written once the rule turns.
//!
//! Explicit `HIVE_VIEW_THEME` / `view.theme` wins. Auto uses the first
//! non-control client in this session with a known `client_theme`, then the
//! session's own `HIVE_APPEARANCE` (what `hive attach` heard from the
//! terminal it ran in, `stamp_session_appearance`), then the hived's
//! `HIVE_APPEARANCE`, `COLORFGBG`, the system's appearance (source `system`),
//! and finally a provisional light fallback.
//! The monitor samples every two seconds while a client theme is unknown or
//! for 30 seconds after a client event, otherwise every 60 seconds. Only new
//! panes or a changed appearance receive colour reports.
//!
//! ponytail: reports are black/white approximations, not the terminal's RGB.
//! Linked windows share their overrides across sessions; differently themed
//! sessions can overwrite each other's reports, and clients of another
//! session holding the window do not count in `reports_needed`. Updating the
//! cache does not notify running applications or undo an answer already
//! consumed at startup. A terminal without mode 2031 support (the desktop
//! app's xterm.js) leaves `client_theme` unknown; the session stamp is what
//! answers for it, and it is as old as the last `hive attach` from a
//! terminal that answered OSC 11.

use super::run::run;
use std::collections::BTreeMap;

use crate::view_theme::{
    parse_appearance_var, parse_colorfgbg, resolve_pref, Appearance, ThemePref,
};

/// `tmux -V` as (major, minor); None when tmux is missing or the string
/// is not one tmux prints (`tmux 3.4`, `tmux 3.7c`, `tmux next-3.8`).
pub fn version() -> Option<(u32, u32)> {
    let r = run(&["-V"], false, 5).ok()?;
    parse_version(&r.stdout)
}

pub(crate) fn parse_version(output: &str) -> Option<(u32, u32)> {
    let raw = output.trim().strip_prefix("tmux")?.trim();
    let raw = raw.strip_prefix("next-").unwrap_or(raw);
    let (major, rest) = raw.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The oldest tmux hive supports, the oldest its suite runs against: the
/// first with `refresh-client -r`, which is what tells a pane its
/// terminal's background before any terminal attached.
pub const TMUX_MIN_VERSION: (u32, u32) = (3, 5);

/// The tmux this server runs, as `3.3`, when it is older than hive
/// supports; None otherwise or with no tmux.
pub fn stale_version() -> Option<String> {
    version().and_then(stale)
}

fn stale(version: (u32, u32)) -> Option<String> {
    (version < TMUX_MIN_VERSION).then(|| format!("{}.{}", version.0, version.1))
}

/// The warning a human-facing verb prints on a tmux older than hive
/// supports, or None when it is supported (or absent — that fails louder
/// elsewhere).
pub fn stale_version_warning() -> Option<String> {
    let version = stale_version()?;
    let min = format!("{}.{}", TMUX_MIN_VERSION.0, TMUX_MIN_VERSION.1);
    Some(format!(
        "warning: tmux {version} is older than hive supports ({min}+), so team windows may \
         draw or behave wrongly; upgrade tmux to {min} or newer"
    ))
}

/// The OSC 10 (foreground) and OSC 11 (background) replies for an
/// appearance, as a pane would hear them from a plain terminal.
pub(crate) fn colour_replies(appearance: Appearance) -> [String; 2] {
    let (fg, bg) = match appearance {
        Appearance::Light => ("0000/0000/0000", "ffff/ffff/ffff"),
        Appearance::Dark => ("ffff/ffff/ffff", "0000/0000/0000"),
    };
    [
        format!("\x1b]10;rgb:{fg}\x1b\\"),
        format!("\x1b]11;rgb:{bg}\x1b\\"),
    ]
}

/// Pure control-mode commands; the monitor checks tmux support once per attach.
pub fn pane_colour_report_lines(pane_id: &str, appearance: Appearance) -> Vec<String> {
    if !pane_id
        .strip_prefix('%')
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    {
        return Vec::new();
    }
    colour_replies(appearance)
        .into_iter()
        // The tmux command parser consumes one backslash inside single quotes.
        .map(|reply| {
            format!(
                "refresh-client -r '{pane_id}:{}'\n",
                reply.replace('\\', "\\\\")
            )
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PaneAppearance {
    pub appearance: Appearance,
    pub source: &'static str,
    pub client: Option<String>,
}

fn resolve_pane_appearance(
    env_theme: Option<&str>,
    config_theme: Option<&str>,
    clients: &str,
    session_stamp: Option<&str>,
    stamp: Option<&str>,
    colorfgbg: Option<&str>,
    system: impl FnOnce() -> Option<Appearance>,
) -> PaneAppearance {
    let explicit = match resolve_pref(env_theme, config_theme) {
        ThemePref::Light => Some(Appearance::Light),
        ThemePref::Dark => Some(Appearance::Dark),
        ThemePref::Auto => None,
    };
    if let Some(appearance) = explicit {
        return PaneAppearance {
            appearance,
            source: "explicit",
            client: None,
        };
    }
    for line in clients.lines() {
        let mut fields = line.splitn(3, '\t');
        if fields.next() != Some("0") {
            continue;
        }
        let appearance = match fields.next() {
            Some("dark") => Appearance::Dark,
            Some("light") => Appearance::Light,
            _ => continue,
        };
        if let Some(client) = fields.next().filter(|name| !name.is_empty()) {
            return PaneAppearance {
                appearance,
                source: "client",
                client: Some(client.to_string()),
            };
        }
    }
    if let Some(appearance) = parse_appearance_var(session_stamp) {
        return PaneAppearance {
            appearance,
            source: "session",
            client: None,
        };
    }
    if let Some(appearance) = parse_appearance_var(stamp).or_else(|| parse_colorfgbg(colorfgbg)) {
        return PaneAppearance {
            appearance,
            source: "fallback",
            client: None,
        };
    }
    // Asked last and only when nothing nearer answered: it costs a process.
    match system() {
        Some(appearance) => PaneAppearance {
            appearance,
            source: "system",
            client: None,
        },
        None => PaneAppearance {
            appearance: Appearance::Light,
            source: "fallback",
            client: None,
        },
    }
}

/// Whether a colour query from a pane of this session is answered by a
/// human's terminal. Not in a session a control client is attached to
/// (whose blank tty answers black when it is first in the client list) nor
/// in a team session (hive's own report, read back): a plain session of
/// the human's relays what their terminal answered tmux at attach.
pub fn session_relays_terminal_colours(session_target: &str) -> bool {
    let snapshot = run(
        &[
            "list-clients",
            "-t",
            session_target,
            "-F",
            "C\t#{client_control_mode}",
            ";",
            "list-panes",
            "-s",
            "-t",
            session_target,
            "-F",
            "P\t#{@hive-team}#{@hive-hidden}",
        ],
        false,
        5,
    );
    match snapshot {
        Ok(r) if r.returncode == 0 => relays_terminal_colours(&r.stdout),
        _ => false,
    }
}

fn relays_terminal_colours(snapshot: &str) -> bool {
    snapshot.lines().all(|line| match line.split_once('\t') {
        Some(("C", control)) => control != "1",
        Some(("P", tags)) => tags.is_empty(),
        _ => true,
    })
}

/// The session environment variable `hive attach` leaves for the reporter
/// and for every pane the session starts afterwards (`hive view` reads it
/// before probing).
const SESSION_STAMP: &str = "HIVE_APPEARANCE";

/// Record what the attaching terminal said its background is. Display
/// state on the session, rewritten by every attach that got an answer: the
/// environment variable for the pane reports and later panes, and
/// `@hive-appearance`, which the team bar reads for a client that reports
/// no theme of its own (`status_palette`). The panes are told at once as well, through whichever client the
/// session has (a report is stored per pane, whoever writes it): an engine
/// started in the next moment asks before the monitor's next sample. Only
/// when tmux would answer them from a control client (`reports_needed`):
/// a terminal ahead of it answers with its own colours, exactly.
pub fn stamp_session_appearance(session_target: &str, appearance: Appearance) {
    let _ = run(
        &[
            "set-environment",
            "-t",
            session_target,
            SESSION_STAMP,
            appearance.stamp(),
            ";",
            "set-option",
            "-t",
            session_target,
            "@hive-appearance",
            appearance.stamp(),
        ],
        false,
        5,
    );
    let env_theme = std::env::var("HIVE_VIEW_THEME").ok();
    let config_theme =
        crate::settings::get_setting("view.theme").and_then(|v| v.as_str().map(str::to_string));
    if resolve_pref(env_theme.as_deref(), config_theme.as_deref()) != ThemePref::Auto {
        return;
    }
    let clients = match run(
        &[
            "list-clients",
            "-t",
            session_target,
            "-F",
            SESSION_CLIENT_FORMAT,
        ],
        false,
        5,
    ) {
        Ok(r) if r.returncode == 0 => r.stdout,
        _ => return,
    };
    let order = session_clients(&clients);
    if !reports_needed(&order) {
        return;
    }
    let Some(client) = order
        .iter()
        .map(|(_, name)| *name)
        .find(|name| !name.is_empty())
        .map(str::to_string)
    else {
        return;
    };
    let panes = match run(
        &["list-panes", "-s", "-t", session_target, "-F", "#{pane_id}"],
        false,
        5,
    ) {
        Ok(r) if r.returncode == 0 => r.stdout,
        _ => return,
    };
    for pane in panes.lines() {
        for reply in colour_replies(appearance) {
            let report = format!("{pane}:{reply}");
            let _ = run(&["refresh-client", "-t", &client, "-r", &report], false, 5);
        }
    }
}

/// A session's clients, `C\t<control>\t<theme>\t<name>`.
const SESSION_CLIENT_FORMAT: &str = "C\t#{client_control_mode}\t#{client_theme}\t#{client_name}";
/// The session's clients as (control, name), in tmux's own order: creation
/// order, the order it asks them for a pane's colours in (`switch-client`
/// does not move a client in it).
fn session_clients(snapshot: &str) -> Vec<(bool, &str)> {
    snapshot
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("C\t")?.splitn(3, '\t');
            let control = fields.next()? == "1";
            Some((control, fields.nth(1)?))
        })
        .collect()
}

/// Whether hive must tell the session's panes their colours: when the
/// session's first client is a control client, whose blank tty tmux would
/// answer them from (black). A terminal first answers with its own exact
/// colours, and a report would override it for good. A session nobody
/// watches is told too: an engine started there asks at once, and a
/// terminal attaching later sits behind the control client. A session with
/// no client has nothing to override.
///
/// ponytail: tmux skips a terminal that never answered its OSC 11 query
/// (or is suspended) and falls to the control client behind it, black; no
/// format tells hive either. A window linked into another session is
/// answered by that session's clients too; they are not counted. A
/// terminal attached elsewhere before the control client answers exactly
/// once it switches in, but only panes not reported while nobody watched.
pub(super) fn reports_needed(clients: &[(bool, &str)]) -> bool {
    clients.first().is_some_and(|(control, _)| *control)
}

pub(super) struct PaneColourSnapshot {
    pub selected: PaneAppearance,
    pub reports: bool,
    pub panes: Option<String>,
    pub clients: Vec<String>,
    pub has_unknown_client: bool,
}

/// One subprocess per sample, regardless of pane count: the session's
/// clients and environment. Layout events add list-panes to the same tmux
/// command queue. A failed query preserves the
/// previous snapshot instead of impersonating a headless session.
pub(super) fn session_colour_snapshot(
    session_target: &str,
    refresh_panes: bool,
) -> Option<PaneColourSnapshot> {
    let mut args = vec![
        "-u",
        "list-clients",
        "-t",
        session_target,
        "-F",
        SESSION_CLIENT_FORMAT,
        ";",
        "show-environment",
        "-t",
        session_target,
    ];
    if refresh_panes {
        args.extend([
            ";",
            "list-panes",
            "-s",
            "-t",
            session_target,
            "-F",
            "P\t#{pane_id}",
        ]);
    }
    let snapshot = run(&args, false, 1).ok()?;
    if snapshot.returncode != 0 {
        return None;
    }
    let clients = snapshot
        .stdout
        .lines()
        .filter_map(|line| line.strip_prefix("C\t"))
        .collect::<Vec<_>>()
        .join("\n");
    let human_clients: Vec<_> = clients
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            if fields.next() != Some("0") {
                return None;
            }
            let theme = fields.next()?;
            let name = fields.next().filter(|name| !name.is_empty())?;
            Some((name, theme.is_empty()))
        })
        .collect();
    let panes = refresh_panes.then(|| {
        snapshot
            .stdout
            .lines()
            .filter_map(|line| line.strip_prefix("P\t"))
            .collect::<Vec<_>>()
            .join("\n")
    });
    let session_stamp = snapshot
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix(SESSION_STAMP)?.strip_prefix('='));
    let env_theme = std::env::var("HIVE_VIEW_THEME").ok();
    let config_theme =
        crate::settings::get_setting("view.theme").and_then(|v| v.as_str().map(str::to_string));
    let stamp = std::env::var("HIVE_APPEARANCE").ok();
    let colorfgbg = std::env::var("COLORFGBG").ok();
    let mut order = session_clients(&snapshot.stdout);
    // The monitor's own control client is the newest: until tmux has run
    // its attach it is not listed, and it would be last.
    if !order.iter().any(|(control, _)| *control) {
        order.push((true, ""));
    }
    Some(PaneColourSnapshot {
        selected: resolve_pane_appearance(
            env_theme.as_deref(),
            config_theme.as_deref(),
            &clients,
            session_stamp,
            stamp.as_deref(),
            colorfgbg.as_deref(),
            crate::view_theme::system_appearance,
        ),
        reports: reports_needed(&order),
        panes,
        has_unknown_client: human_clients.iter().any(|(_, unknown)| *unknown),
        clients: human_clients
            .iter()
            .map(|(name, _)| name.to_string())
            .collect(),
    })
}

/// Remember successful writes, so sampling and layout changes do not
/// repeatedly override panes whose reported appearance has not changed.
/// While tmux answers natively (`reports_needed` false) a pane never
/// reported stays unreported, to be written if that changes; one already
/// reported keeps its report current, since tmux answers it from there.
///
/// ponytail: what this map does not know is left alone — a report from an
/// attach's `stamp_session_appearance` or from an earlier monitor (the map
/// starts empty at each control-client attach) holds while any control
/// client exists on the server, tmux has no way to clear it, and no format
/// tells whether a pane carries one.
#[derive(Default)]
pub(super) struct PaneColourReports {
    panes: BTreeMap<String, Option<Appearance>>,
    pub selected: Option<PaneAppearance>,
    pub terminal_answers: bool,
    pub clients: Vec<String>,
    pub has_unknown_client: bool,
}

impl PaneColourReports {
    pub fn set_panes(&mut self, snapshot: &str) {
        let mut next = BTreeMap::new();
        for pane in snapshot.lines().map(str::trim).filter(|p| !p.is_empty()) {
            next.insert(pane.to_string(), self.panes.get(pane).copied().flatten());
        }
        self.panes = next;
    }

    pub fn write_pending(
        &mut self,
        mut write: impl FnMut(&str) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let Some(selected) = &self.selected else {
            return Ok(());
        };
        for (pane, reported) in &mut self.panes {
            if *reported == Some(selected.appearance)
                || (reported.is_none() && self.terminal_answers)
            {
                continue;
            }
            let lines = pane_colour_report_lines(pane, selected.appearance).concat();
            write(&lines)?;
            *reported = Some(selected.appearance);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session's clients in order, one token each: `c` control, `h`
    /// terminal.
    fn order(spec: &str) -> Vec<(bool, &str)> {
        spec.split_whitespace()
            .map(|token| (token == "c", token))
            .collect()
    }

    #[test]
    fn test_reports_needed_only_where_a_control_client_would_answer() {
        // A terminal attached before the control client answers itself.
        assert!(!reports_needed(&order("h c")));
        assert!(!reports_needed(&order("h c h")));
        // The control client is first: it would answer black.
        assert!(reports_needed(&order("c h")));
        assert!(reports_needed(&order("c h h")));
        // Nobody watches: an engine started now would hear black.
        assert!(reports_needed(&order("c")));
        // No client: nothing to override.
        assert!(!reports_needed(&order("")));
    }

    #[test]
    fn test_session_clients_keep_tmux_order() {
        let snapshot = "C\t0\tdark\tterm one\nC\t1\t\tcontrol\nFOO=bar\nC\t0\t\tterm two\nP\t%1\n";
        let clients = session_clients(snapshot);
        assert_eq!(
            clients,
            vec![(false, "term one"), (true, "control"), (false, "term two")]
        );
        assert!(!reports_needed(&clients));
    }

    #[test]
    fn test_unreported_panes_wait_while_a_terminal_answers() {
        let mut reports = PaneColourReports::default();
        reports.set_panes("%1");
        reports.selected = Some(resolve_pane_appearance(
            None,
            None,
            "",
            None,
            None,
            None,
            || None,
        ));
        assert_eq!(writes(&mut reports).len(), 1);
        reports.terminal_answers = true;
        reports.set_panes("%1\n%2");
        assert!(writes(&mut reports).is_empty());
        // A pane already reported is answered from its report: keep it current.
        reports.selected = Some(resolve_pane_appearance(
            None,
            None,
            "0\tdark\thuman",
            None,
            None,
            None,
            || None,
        ));
        let current = writes(&mut reports);
        assert_eq!(current.len(), 1);
        assert!(current[0].starts_with("refresh-client -r '%1:"));
        reports.terminal_answers = false;
        let flipped = writes(&mut reports);
        assert_eq!(flipped.len(), 1);
        assert!(flipped[0].starts_with("refresh-client -r '%2:"));
    }

    #[test]
    fn test_relays_terminal_colours_only_in_a_plain_session_without_control_client() {
        assert!(relays_terminal_colours("C\t0\nP\t\nP\t\n"));
        assert!(relays_terminal_colours(""));
        assert!(!relays_terminal_colours("C\t0\nC\t1\nP\t\n"));
        assert!(!relays_terminal_colours("C\t0\nP\t\nP\thoney\n"));
        assert!(!relays_terminal_colours("P\t1\n"));
    }

    #[test]
    fn test_parse_version_reads_every_shape_tmux_prints() {
        assert_eq!(parse_version("tmux 3.4\n"), Some((3, 4)));
        assert_eq!(parse_version("tmux 3.5a"), Some((3, 5)));
        assert_eq!(parse_version("tmux 3.7c"), Some((3, 7)));
        assert_eq!(parse_version("tmux next-3.8"), Some((3, 8)));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("bash: tmux: command not found"), None);
    }

    #[test]
    fn test_pane_colour_report_lines_encode_both_colours_and_reject_invalid_panes() {
        assert_eq!(
            pane_colour_report_lines("%7", Appearance::Dark),
            vec![
                "refresh-client -r '%7:\x1b]10;rgb:ffff/ffff/ffff\x1b\\\\'\n",
                "refresh-client -r '%7:\x1b]11;rgb:0000/0000/0000\x1b\\\\'\n",
            ]
        );
        for invalid in ["", "%", "not-a-pane", "%7'", "%1\nkill-server"] {
            assert!(pane_colour_report_lines(invalid, Appearance::Light).is_empty());
        }
    }

    #[test]
    fn test_stale_names_only_a_tmux_older_than_3_5() {
        assert_eq!(stale((3, 4)), Some("3.4".to_string()));
        assert_eq!(stale((2, 9)), Some("2.9".to_string()));
        assert_eq!(stale((3, 5)), None);
        assert_eq!(stale((3, 7)), None);
        assert_eq!(stale((3, 10)), None);
        assert_eq!(stale((4, 0)), None);
    }

    #[test]
    fn test_colour_replies_follow_the_appearance() {
        let [fg, bg] = colour_replies(Appearance::Light);
        assert_eq!(fg, "\x1b]10;rgb:0000/0000/0000\x1b\\");
        assert_eq!(bg, "\x1b]11;rgb:ffff/ffff/ffff\x1b\\");
        let [fg, bg] = colour_replies(Appearance::Dark);
        assert_eq!(fg, "\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
        assert_eq!(bg, "\x1b]11;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn test_pane_appearance_resolves_each_precedence_level() {
        // Each winner disagrees with the next available source.
        let cases = [
            (
                Some("night"),
                Some("light"),
                "0\tlight\thuman",
                Some("light"),
                Some("0;15"),
                Appearance::Dark,
                "explicit",
            ),
            (
                Some("invalid"),
                Some("day"),
                "0\tdark\thuman",
                Some("dark"),
                Some("15;0"),
                Appearance::Light,
                "explicit",
            ),
            (
                None,
                None,
                "0\tdark\thuman",
                Some("light"),
                Some("0;15"),
                Appearance::Dark,
                "client",
            ),
            (
                None,
                None,
                "0\t\thuman",
                Some("light"),
                Some("0;15"),
                Appearance::Dark,
                "session",
            ),
            (
                None,
                None,
                "",
                Some("night"),
                Some("0;15"),
                Appearance::Dark,
                "fallback",
            ),
            (
                None,
                None,
                "",
                Some("invalid"),
                Some("15;0"),
                Appearance::Dark,
                "fallback",
            ),
            (
                None,
                None,
                "",
                None,
                Some("invalid"),
                Appearance::Light,
                "fallback",
            ),
        ];
        for (env, config, clients, stamp, colors, appearance, source) in cases {
            // The session's stamp disagrees with the hived's own.
            let session = (source == "session").then_some("dark");
            let got =
                resolve_pane_appearance(env, config, clients, session, stamp, colors, || None);
            assert_eq!(
                got.appearance, appearance,
                "{env:?} / {config:?} / {clients:?}"
            );
            assert_eq!(got.source, source);
            assert_eq!(
                got.client.as_deref(),
                (source == "client").then_some("human")
            );
        }
    }

    #[test]
    fn test_pane_appearance_preserves_auto_and_invalid_setting_semantics() {
        for auto in ["auto", "system", " AUTO "] {
            // An explicit auto in env overrides a fixed config value.
            assert_eq!(
                resolve_pane_appearance(
                    Some(auto),
                    Some("light"),
                    "0\tdark\thuman",
                    None,
                    None,
                    None,
                    || None,
                )
                .appearance,
                Appearance::Dark
            );
            assert_eq!(
                resolve_pane_appearance(
                    Some("invalid"),
                    Some(auto),
                    "0\tdark\thuman",
                    None,
                    None,
                    None,
                    || None,
                )
                .source,
                "client"
            );
        }
        assert_eq!(
            resolve_pane_appearance(
                Some("invalid"),
                Some("invalid"),
                "",
                None,
                Some("dark"),
                None,
                || None,
            )
            .appearance,
            Appearance::Dark
        );
    }

    #[test]
    fn test_system_appearance_answers_only_when_nothing_nearer_does() {
        let dark = || Some(Appearance::Dark);
        let got = resolve_pane_appearance(None, None, "0\t\thuman", None, None, None, dark);
        assert_eq!((got.appearance, got.source), (Appearance::Dark, "system"));
        let got = resolve_pane_appearance(None, None, "", None, None, Some("0;15"), dark);
        assert_eq!(
            (got.appearance, got.source),
            (Appearance::Light, "fallback")
        );
        let got = resolve_pane_appearance(None, None, "", Some("light"), None, None, || {
            panic!("the session stamp answers first")
        });
        assert_eq!(got.source, "session");
    }

    #[test]
    fn test_client_selection_skips_control_unknown_and_malformed_rows() {
        let clients = "1\tlight\tcontrol\n0\t\tunknown\n0\tblue\tbad\n0\tdark\n0\tdark\t\n0\tdark\thuman with spaces\n0\tlight\tsecond";
        let selected = resolve_pane_appearance(None, None, clients, None, None, None, || None);
        assert_eq!(selected.source, "client");
        assert_eq!(selected.appearance, Appearance::Dark);
        assert_eq!(selected.client.as_deref(), Some("human with spaces"));
        for clients in ["", "1\tdark\tcontrol", "0\t\thuman"] {
            assert_eq!(
                resolve_pane_appearance(None, None, clients, None, None, None, || None).source,
                "fallback"
            );
        }
    }

    fn writes(reports: &mut PaneColourReports) -> Vec<String> {
        let mut writes = Vec::new();
        reports
            .write_pending(|lines| {
                writes.push(lines.to_string());
                Ok(())
            })
            .unwrap();
        writes
    }

    #[test]
    fn test_colour_reports_write_only_new_panes_or_changed_appearance() {
        let mut reports = PaneColourReports::default();
        reports.set_panes("%1\n%2");
        assert!(writes(&mut reports).is_empty());
        reports.selected = Some(resolve_pane_appearance(
            None,
            None,
            "",
            None,
            None,
            None,
            || None,
        ));
        assert_eq!(writes(&mut reports).len(), 2);
        assert!(writes(&mut reports).is_empty());
        // Changing the source but keeping light does not repeat the overrides.
        reports.selected = Some(resolve_pane_appearance(
            None,
            None,
            "0\tlight\thuman",
            None,
            None,
            None,
            || None,
        ));
        reports.set_panes("%2\n%1\n%3");
        let added = writes(&mut reports);
        assert_eq!(added.len(), 1);
        assert!(added[0].starts_with("refresh-client -r '%3:"));
        reports.selected = Some(resolve_pane_appearance(
            None,
            None,
            "0\tdark\thuman",
            None,
            None,
            None,
            || None,
        ));
        assert_eq!(writes(&mut reports).len(), 3);
        assert!(writes(&mut reports).is_empty());
        // Removed panes are forgotten; a subsequent arrival needs a report.
        reports.set_panes("%1");
        reports.set_panes("%1\n%2");
        assert_eq!(writes(&mut reports).len(), 1);
    }

    #[test]
    fn test_failed_colour_write_remains_pending() {
        let mut reports = PaneColourReports::default();
        reports.set_panes("%1");
        reports.selected = Some(resolve_pane_appearance(
            None,
            None,
            "",
            None,
            None,
            None,
            || None,
        ));
        assert!(reports
            .write_pending(|_| Err(std::io::ErrorKind::BrokenPipe.into()))
            .is_err());
        assert_eq!(writes(&mut reports).len(), 1);
        assert!(writes(&mut reports).is_empty());
    }
}
