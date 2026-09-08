//! What a pane is told its terminal background is.
//!
//! A tmux control client starts with ANSI black tty colours. When it is
//! the first eligible client and a pane has no background override, OSC
//! 11 queries therefore return black, even on a light terminal.
//! On tmux 3.5+, Hive supplies per-pane answers through `refresh-client -r`.
//! These override pane styles while any control client exists on the server.
//!
//! Explicit `HIVE_VIEW_THEME` / `view.theme` wins. Auto uses the first
//! non-control client in this session with a known `client_theme`, then
//! `HIVE_APPEARANCE`, `COLORFGBG`, and finally a provisional light fallback.
//! The monitor samples clients every two seconds and on client attachment;
//! only new panes or a changed appearance receive colour reports.
//!
//! ponytail: these are black/white approximations, not the terminal's RGB.
//! Linked windows share their overrides across sessions; differently themed
//! sessions can overwrite each other's reports. Updating the cache does not
//! notify running applications or undo an answer already consumed at startup.
//! A terminal without mode 2031 support can leave `client_theme` unknown.

use super::run::run;
use std::collections::BTreeMap;

use crate::view_theme::{
    parse_appearance_var, parse_colorfgbg, resolve_pref, Appearance, ThemePref,
};

/// The first tmux release with `refresh-client -r`.
pub const PANE_COLOUR_REPORT_SINCE: (u32, u32) = (3, 5);

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

/// The warning a team-building verb prints on a tmux that cannot take a
/// pane colour report, or None when it can (or tmux is absent — that
/// fails louder elsewhere).
pub fn stale_version_warning() -> Option<String> {
    let v = version()?;
    (v < PANE_COLOUR_REPORT_SINCE).then(|| {
        format!(
            "warning: tmux {}.{} answers default-background pane queries with black \
             when the hived control client is first; tmux {}.{}+ supports \
             pane colour reports (refresh-client -r)",
            v.0, v.1, PANE_COLOUR_REPORT_SINCE.0, PANE_COLOUR_REPORT_SINCE.1
        )
    })
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
    stamp: Option<&str>,
    colorfgbg: Option<&str>,
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
    PaneAppearance {
        appearance: parse_appearance_var(stamp)
            .or_else(|| parse_colorfgbg(colorfgbg))
            .unwrap_or(Appearance::Light),
        source: "fallback",
        client: None,
    }
}

pub(super) struct PaneColourSnapshot {
    pub selected: PaneAppearance,
    pub panes: Option<String>,
}

/// One subprocess per sample, regardless of pane count. Layout events add
/// list-panes to the same tmux command queue. A failed query preserves the
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
        "C\t#{client_control_mode}\t#{client_theme}\t#{client_name}",
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
    let panes = refresh_panes.then(|| {
        snapshot
            .stdout
            .lines()
            .filter_map(|line| line.strip_prefix("P\t"))
            .collect::<Vec<_>>()
            .join("\n")
    });
    let env_theme = std::env::var("HIVE_VIEW_THEME").ok();
    let config_theme =
        crate::settings::get_setting("view.theme").and_then(|v| v.as_str().map(str::to_string));
    let stamp = std::env::var("HIVE_APPEARANCE").ok();
    let colorfgbg = std::env::var("COLORFGBG").ok();
    Some(PaneColourSnapshot {
        selected: resolve_pane_appearance(
            env_theme.as_deref(),
            config_theme.as_deref(),
            &clients,
            stamp.as_deref(),
            colorfgbg.as_deref(),
        ),
        panes,
    })
}

/// Remember successful writes, so sampling and layout changes do not
/// repeatedly override panes whose reported appearance has not changed.
#[derive(Default)]
pub(super) struct PaneColourReports {
    panes: BTreeMap<String, Option<Appearance>>,
    pub selected: Option<PaneAppearance>,
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
            if *reported == Some(selected.appearance) {
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
            let got = resolve_pane_appearance(env, config, clients, stamp, colors);
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
                resolve_pane_appearance(Some(auto), Some("light"), "0\tdark\thuman", None, None)
                    .appearance,
                Appearance::Dark
            );
            assert_eq!(
                resolve_pane_appearance(Some("invalid"), Some(auto), "0\tdark\thuman", None, None)
                    .source,
                "client"
            );
        }
        assert_eq!(
            resolve_pane_appearance(Some("invalid"), Some("invalid"), "", Some("dark"), None)
                .appearance,
            Appearance::Dark
        );
    }

    #[test]
    fn test_client_selection_skips_control_unknown_and_malformed_rows() {
        let clients = "1\tlight\tcontrol\n0\t\tunknown\n0\tblue\tbad\n0\tdark\n0\tdark\t\n0\tdark\thuman with spaces\n0\tlight\tsecond";
        let selected = resolve_pane_appearance(None, None, clients, None, None);
        assert_eq!(selected.source, "client");
        assert_eq!(selected.appearance, Appearance::Dark);
        assert_eq!(selected.client.as_deref(), Some("human with spaces"));
        for clients in ["", "1\tdark\tcontrol", "0\t\thuman"] {
            assert_eq!(
                resolve_pane_appearance(None, None, clients, None, None).source,
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
        reports.selected = Some(resolve_pane_appearance(None, None, "", None, None));
        assert_eq!(writes(&mut reports).len(), 2);
        assert!(writes(&mut reports).is_empty());
        // Changing the source but keeping light does not repeat the overrides.
        reports.selected = Some(resolve_pane_appearance(
            None,
            None,
            "0\tlight\thuman",
            None,
            None,
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
        reports.selected = Some(resolve_pane_appearance(None, None, "", None, None));
        assert!(reports
            .write_pending(|_| Err(std::io::ErrorKind::BrokenPipe.into()))
            .is_err());
        assert_eq!(writes(&mut reports).len(), 1);
        assert!(writes(&mut reports).is_empty());
    }
}
