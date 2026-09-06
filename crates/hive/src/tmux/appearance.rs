//! What a pane is told its terminal background is.
//!
//! The hived keeps a control-mode client (`tmux -C attach`, the pane
//! monitor) on every team session. tmux never initialises a control
//! client's terminal colours (they read as ANSI 0, black, not "unknown"),
//! and it answers a pane's OSC 10/11 colour query from the first attached
//! client that has colours — the hived's, since it attaches before any
//! human. Every engine in a team pane that asks (codex, `hive view`) is
//! told the background is black and draws its dark theme on a light
//! terminal. Verified on tmux 3.4 and 3.7c with an isolated server: a
//! control client alone, or attached first, answers black; a human
//! client alone answers its real colour.
//!
//! tmux 3.5 added `refresh-client -r '<pane>:<OSC reply>'` — a stored,
//! per-pane answer that outlives the client that set it. Hive reports the
//! colours of its own configured appearance (`view.theme`, then
//! `HIVE_APPEARANCE` / `COLORFGBG`, light by default) through that very
//! client, for every pane of the session it monitors, on attach and on
//! every layout change; on 3.4 there is nothing to report through, so
//! the verbs that set a team up warn instead.

use super::run::run;
use crate::view_theme::Appearance;

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
            "warning: tmux {}.{} answers every team pane's background-colour query \
             with black while the hived's control client is attached, so codex and \
             `hive view` in a team pane draw dark on a light terminal; tmux {}.{}+ \
             lets hive report the real colours (refresh-client -r)",
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

/// The control-mode command lines that tell tmux what `pane_id` should
/// hear for OSC 10/11: hive's configured appearance. Written by the
/// hived into its own control client's stdin — the client that would
/// otherwise answer black. Empty on a tmux without `refresh-client -r`.
pub fn pane_colour_report_lines(pane_id: &str) -> Vec<String> {
    if pane_id.is_empty() || version().is_none_or(|v| v < PANE_COLOUR_REPORT_SINCE) {
        return Vec::new();
    }
    colour_replies(crate::view_theme::configured_appearance())
        .into_iter()
        // tmux's command parser reads the single-quoted argument, where a
        // backslash escapes: the ST's `\` must arrive doubled.
        .map(|reply| {
            format!(
                "refresh-client -r '{pane_id}:{}'\n",
                reply.replace('\\', "\\\\")
            )
        })
        .collect()
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
    fn test_pane_colour_report_lines_double_the_terminator_backslash() {
        // Pure: the reply text through tmux's single-quote parser.
        let line = format!(
            "refresh-client -r '%3:{}'\n",
            "\x1b]11;rgb:ffff/ffff/ffff\x1b\\".replace('\\', "\\\\")
        );
        assert_eq!(
            line,
            "refresh-client -r '%3:\x1b]11;rgb:ffff/ffff/ffff\x1b\\\\'\n"
        );
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
}
