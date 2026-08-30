//! Dual theme for `hive view` — grok's grokday (light) / groknight (dark)
//! palettes plus grok-style appearance resolution.
//!
//! Resolution order (grok cache.rs::resolve_initial_theme, with dp's two
//! deltas): explicit setting first — env `HIVE_VIEW_THEME`, then config
//! `view.theme` — then auto-detection, then **grokday (light)**. Grok
//! defaults to dark and makes auto opt-in; hive view defaults to auto and
//! falls back light.
//!
//! Auto-detection is grok's minimal chain for a tmux pane
//! (system_appearance.rs::detect_with_osc11_fallback, desktop API dropped):
//! explicit `HIVE_APPEARANCE` stamp → OSC 11 background query (bare, tmux
//! ≥ 3.2 answers it; 500ms) → `COLORFGBG` polarity guess → None. The OSC 11
//! probe owns raw stdin, so [`active_theme_kind`] must run before crossterm
//! takes the terminal (alternate screen / event reads).

use std::time::Duration;

use ratatui::style::Color;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    Light,
    Dark,
}

/// Every color the `hive view` TUI and its grok_md markdown renderer read.
/// Field names follow grok's Theme struct (groknight.rs / grokday.rs).
pub struct ViewTheme {
    pub kind: ThemeKind,
    // frame & chrome
    pub bg_base: Color,
    /// User-prompt band background.
    pub bg_light: Color,
    pub text_primary: Color,
    /// Bold hint keys on the bottom row.
    pub text_secondary: Color,
    /// `❯ ` prefix, `worktree ` label, usage gradient 50-65%.
    pub accent_user: Color,
    /// Muted labels, timestamps, `Worked for …`.
    pub gray: Color,
    /// cwd, separators, dim details.
    pub gray_dim: Color,
    /// Aggregate `◈` labels.
    pub gray_bright: Color,
    /// Usage gradient 75-85%.
    pub warning: Color,
    pub accent_error: Color,
    pub accent_model: Color,
    // interaction layer (grok selection / fold / palette chrome)
    /// Selection bracket-frame color (fg-only corners and sides).
    pub selection_border: Color,
    /// Tool-output panel strip and selected-collapsed-header patch.
    pub bg_dark: Color,
    /// Selected palette row background.
    pub bg_visual: Color,
    /// Thinking accent gutter (grok accent_thinking, magenta family).
    pub accent_thinking: Color,
    /// Successful run accent gutter.
    pub accent_success: Color,
    /// Palette fuzzy-match character highlight.
    pub fuzzy_accent: Color,
    // markdown (grok_md)
    /// H1..H6 heading colors; H1-H5 render bold, H6 plain (both themes).
    pub md_heading: [Color; 6],
    pub md_code: Color,
    pub md_code_bg: Color,
    /// Code-fence language tag (hidden span); grok uses the purple family.
    pub md_code_language: Color,
    /// Table chrome (hidden span); grok uses the blue family.
    pub md_table: Color,
    pub md_task_checked: Color,
    pub md_task_unchecked: Color,
    pub md_muted: Color,
    pub md_text: Color,
    pub link_fg: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// groknight (grok-build xai-grok-pager-render/src/theme/groknight.rs).
pub static GROKNIGHT: ViewTheme = ViewTheme {
    kind: ThemeKind::Dark,
    bg_base: rgb(20, 20, 20),
    bg_light: rgb(36, 36, 36),
    text_primary: rgb(225, 225, 225),
    text_secondary: rgb(200, 200, 200),
    accent_user: rgb(200, 200, 200),
    gray: rgb(108, 108, 108),
    gray_dim: rgb(88, 88, 88),
    gray_bright: rgb(120, 120, 120),
    warning: rgb(224, 175, 104),
    accent_error: rgb(247, 118, 142),
    accent_model: rgb(26, 188, 156),
    selection_border: rgb(60, 60, 65),
    bg_dark: rgb(28, 28, 28),
    bg_visual: rgb(54, 54, 54),
    accent_thinking: rgb(187, 154, 247),
    accent_success: rgb(158, 206, 106),
    fuzzy_accent: rgb(122, 162, 247),
    md_heading: [
        rgb(26, 188, 156),  // teal
        rgb(122, 162, 247), // blue
        rgb(157, 124, 216), // purple
        rgb(120, 120, 120), // dark5
        rgb(108, 108, 108), // comment
        rgb(90, 90, 90),    // dark3
    ],
    md_code: rgb(58, 149, 171),
    md_code_bg: rgb(28, 28, 28),
    md_code_language: rgb(157, 124, 216),
    md_table: rgb(122, 162, 247),
    md_task_checked: rgb(158, 206, 106),
    md_task_unchecked: rgb(200, 200, 200),
    md_muted: rgb(108, 108, 108),
    md_text: rgb(200, 200, 200),
    link_fg: rgb(122, 166, 218),
};

/// grokday (grok-build xai-grok-pager-render/src/theme/grokday.rs) —
/// neutral light grays, same hue families deepened for contrast.
pub static GROKDAY: ViewTheme = ViewTheme {
    kind: ThemeKind::Light,
    bg_base: rgb(238, 238, 238),
    bg_light: rgb(222, 222, 222),
    text_primary: rgb(38, 38, 38),
    text_secondary: rgb(68, 68, 68),
    accent_user: rgb(68, 68, 68),
    gray: rgb(118, 118, 118),
    gray_dim: rgb(165, 165, 165),
    gray_bright: rgb(98, 98, 98),
    warning: rgb(162, 118, 18),
    accent_error: rgb(205, 48, 72),
    accent_model: rgb(10, 142, 112),
    selection_border: rgb(185, 185, 190),
    bg_dark: rgb(228, 228, 228),
    bg_visual: rgb(198, 198, 198),
    accent_thinking: rgb(125, 75, 198),
    accent_success: rgb(55, 142, 35),
    fuzzy_accent: rgb(47, 100, 210),
    md_heading: [
        rgb(10, 142, 112),  // teal
        rgb(47, 100, 210),  // blue
        rgb(108, 62, 178),  // purple
        rgb(98, 98, 98),    // dark5
        rgb(118, 118, 118), // comment
        rgb(142, 142, 142), // dark3
    ],
    md_code: rgb(15, 135, 162),
    md_code_bg: rgb(228, 228, 228),
    md_code_language: rgb(108, 62, 178),
    md_table: rgb(47, 100, 210),
    md_task_checked: rgb(55, 142, 35),
    md_task_unchecked: rgb(68, 68, 68),
    md_muted: rgb(118, 118, 118),
    md_text: rgb(68, 68, 68),
    link_fg: rgb(47, 100, 210),
};

impl ThemeKind {
    pub fn theme(self) -> &'static ViewTheme {
        match self {
            ThemeKind::Light => &GROKDAY,
            ThemeKind::Dark => &GROKNIGHT,
        }
    }
}

fn rgb_of(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    }
}

impl ViewTheme {
    /// Token-counter gradient (grok context_bar.rs::default_breakpoints):
    /// 0% text_primary → 50-65% accent_user → 75-85% warning → 95%+
    /// accent_error, per-channel lerp rounded between breakpoints.
    pub fn usage_color(&self, pct: f64) -> Color {
        let stops: [(f64, (u8, u8, u8)); 7] = [
            (0.0, rgb_of(self.text_primary)),
            (50.0, rgb_of(self.accent_user)),
            (65.0, rgb_of(self.accent_user)),
            (75.0, rgb_of(self.warning)),
            (85.0, rgb_of(self.warning)),
            (95.0, rgb_of(self.accent_error)),
            (100.0, rgb_of(self.accent_error)),
        ];
        let pct = pct.clamp(0.0, 100.0);
        for pair in stops.windows(2) {
            let (p0, c0) = pair[0];
            let (p1, c1) = pair[1];
            if pct <= p1 {
                let t = if p1 > p0 { (pct - p0) / (p1 - p0) } else { 0.0 };
                let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
                return Color::Rgb(lerp(c0.0, c1.0), lerp(c0.1, c1.1), lerp(c0.2, c1.2));
            }
        }
        let (_, c) = stops[stops.len() - 1];
        Color::Rgb(c.0, c.1, c.2)
    }
}

// ---------------------------------------------------------------------------
// Preference resolution (explicit setting > auto > light)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePref {
    Light,
    Dark,
    Auto,
}

/// Theme-name parsing (grok theme/mod.rs::from_name aliases, case-insensitive).
/// Unknown values yield `None` so the caller falls through to the next source.
pub fn parse_theme_pref(raw: &str) -> Option<ThemePref> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" | "system" => Some(ThemePref::Auto),
        "dark" | "night" | "groknight" | "grok-night" => Some(ThemePref::Dark),
        "light" | "day" | "grokday" | "grok-day" => Some(ThemePref::Light),
        _ => None,
    }
}

/// Env beats config; an invalid or missing value falls through; with neither
/// set hive view defaults to auto-detection (grok defaults to dark here).
pub fn resolve_pref(env_val: Option<&str>, config_val: Option<&str>) -> ThemePref {
    env_val
        .and_then(parse_theme_pref)
        .or_else(|| config_val.and_then(parse_theme_pref))
        .unwrap_or(ThemePref::Auto)
}

/// Auto resolves through the detected appearance; detection failure or
/// ambiguity falls back to **light** (dp's preference; grok falls dark).
pub fn resolve_kind(pref: ThemePref, detected: Option<Appearance>) -> ThemeKind {
    match pref {
        ThemePref::Light => ThemeKind::Light,
        ThemePref::Dark => ThemeKind::Dark,
        ThemePref::Auto => match detected {
            Some(Appearance::Dark) => ThemeKind::Dark,
            Some(Appearance::Light) | None => ThemeKind::Light,
        },
    }
}

/// Full startup resolution. Runs the detection chain only when no explicit
/// setting decided the theme; MUST be called before crossterm owns the
/// terminal (the OSC 11 probe reads raw stdin).
pub fn active_theme_kind() -> ThemeKind {
    let env_val = std::env::var("HIVE_VIEW_THEME").ok();
    let config_val =
        crate::settings::get_setting("view.theme").and_then(|v| v.as_str().map(str::to_string));
    let pref = resolve_pref(env_val.as_deref(), config_val.as_deref());
    let detected = match pref {
        ThemePref::Auto => detect_appearance(),
        _ => None,
    };
    resolve_kind(pref, detected)
}

// ---------------------------------------------------------------------------
// Appearance detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

/// Startup chain: explicit `HIVE_APPEARANCE` stamp → OSC 11 → `COLORFGBG`.
fn detect_appearance() -> Option<Appearance> {
    parse_appearance_var(std::env::var("HIVE_APPEARANCE").ok().as_deref())
        .or_else(detect_via_osc11)
        .or_else(|| parse_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()))
}

/// `dark`/`night` and `light`/`day` stamps (grok env_appearance.rs);
/// unknown values are ignored.
pub fn parse_appearance_var(raw: Option<&str>) -> Option<Appearance> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "dark" | "night" => Some(Appearance::Dark),
        "light" | "day" => Some(Appearance::Light),
        _ => None,
    }
}

/// `COLORFGBG` polarity guess (grok env_appearance.rs::parse_colorfgbg):
/// last field is bg (`fg;bg` or `fg;default;bg`); bg 0-6 and 8 are dark,
/// 7 and 9-15 light; `default` or out-of-range yields `None`.
pub fn parse_colorfgbg(raw: Option<&str>) -> Option<Appearance> {
    let bg = raw?.split(';').next_back()?.trim().parse::<u8>().ok()?;
    match bg {
        0..=6 | 8 => Some(Appearance::Dark),
        7 | 9..=15 => Some(Appearance::Light),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// OSC 11 background probe (grok theme/osc11.rs, bare query only)
// ---------------------------------------------------------------------------

/// Backgrounds with BT.709 relative luminance below this are dark.
const LUMINANCE_THRESHOLD: f64 = 0.5;
/// Reply deadline for the bare query (grok OSC11_TIMEOUT).
const OSC11_TIMEOUT: Duration = Duration::from_millis(500);
const OSC11_QUERY: &[u8] = b"\x1b]11;?\x07";
/// Bounds the reply buffer against terminals that stream without a terminator.
const MAX_PROBE_RESPONSE: usize = 256;
/// Hard cap on post-deadline consumption of an in-flight reply.
const LATE_REPLY_GRACE: Duration = Duration::from_millis(100);
/// Per-byte quiet window during the grace period.
const LATE_REPLY_QUIET_MS: i32 = 25;

/// Query the terminal's background color over OSC 11 and classify it.
/// `None` when stdin/stdout is not a TTY, nothing answers in time, or the
/// reply cannot be parsed. tmux ≥ 3.2 answers the bare query from a pane, so
/// no DCS-passthrough retry is needed in hive's tmux-resident world.
fn detect_via_osc11() -> Option<Appearance> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }
    {
        let mut out = std::io::stdout();
        out.write_all(OSC11_QUERY).ok()?;
        out.flush().ok()?;
    }
    let response = read_osc_response(OSC11_TIMEOUT)?;
    let (r, g, b) = parse_osc11_rgb(&response)?;
    Some(classify_luminance(r, g, b))
}

/// sRGB → dark/light via BT.709 luminance with sRGB gamma (grok
/// osc11.rs::classify_luminance): Y < 0.5 → dark, else light.
pub fn classify_luminance(r: u8, g: u8, b: u8) -> Appearance {
    let y = 0.2126 * srgb_to_linear(r) + 0.7152 * srgb_to_linear(g) + 0.0722 * srgb_to_linear(b);
    if y < LUMINANCE_THRESHOLD {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

fn srgb_to_linear(c: u8) -> f64 {
    let s = c as f64 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Parse `rgb:RRRR/GGGG/BBBB` (1-4 hex digits per channel; >2 digits take
/// the high byte) out of an OSC 11 reply.
pub fn parse_osc11_rgb(response: &str) -> Option<(u8, u8, u8)> {
    let rgb_start = response.find("rgb:")? + 4;
    let parts: Vec<&str> = response[rgb_start..]
        .split(['/', '\x07', '\x1b'])
        .take(3)
        .collect();
    if parts.len() < 3 {
        return None;
    }
    Some((
        parse_channel(parts[0])?,
        parse_channel(parts[1])?,
        parse_channel(parts[2])?,
    ))
}

fn parse_channel(s: &str) -> Option<u8> {
    let trimmed = s.trim();
    let val = u16::from_str_radix(trimmed, 16).ok()?;
    Some(if trimmed.len() > 2 {
        (val >> 8) as u8
    } else {
        val as u8
    })
}

fn ends_with_osc_terminator(buf: &[u8]) -> bool {
    buf.last() == Some(&0x07) || buf.ends_with(b"\x1b\\")
}

/// Restores the original termios on drop. Local to the probe — crossterm's
/// raw mode has not been entered yet and its snapshot is untouched.
struct TermiosGuard {
    fd: libc::c_int,
    original: libc::termios,
}

impl Drop for TermiosGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

/// POSIX-portable subset of `cfmakeraw(3)`: clear only the lflags that block
/// a single-byte read (canonical mode, echo, signals, extended processing).
fn make_raw_termios(snapshot: &libc::termios) -> libc::termios {
    let mut raw = *snapshot;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
    raw
}

fn read_osc_response(timeout: Duration) -> Option<String> {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
        return None;
    }
    let raw = make_raw_termios(&original);
    let _guard = TermiosGuard { fd, original };
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    let buf = read_tty_reply(fd, timeout)?;
    // Reject partial buffers: a reply truncated mid-channel would mis-parse,
    // since channel width is inferred from digit count.
    if !ends_with_osc_terminator(&buf) {
        return None;
    }
    String::from_utf8(buf).ok()
}

/// Read stdin until BEL / ST, the size cap, or the deadline (grok
/// terminal/probe.rs::read_tty_reply with the OSC terminator predicate).
fn read_tty_reply(fd: libc::c_int, timeout: Duration) -> Option<Vec<u8>> {
    let start = std::time::Instant::now();
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    loop {
        let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
            return finish_after_deadline(fd, buf);
        };
        let remaining_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        match poll_read_byte(fd, remaining_ms) {
            PollRead::Byte(byte) => {
                buf.push(byte);
                if buf.len() >= MAX_PROBE_RESPONSE || ends_with_osc_terminator(&buf) {
                    return Some(buf);
                }
            }
            // Re-entry recomputes the deadline, so EINTR cannot extend it.
            PollRead::Interrupted => continue,
            PollRead::Timeout => return finish_after_deadline(fd, buf),
            PollRead::Error => return if buf.is_empty() { None } else { Some(buf) },
        }
    }
}

/// Deadline expiry: an in-flight reply (ESC byte seen) is consumed until
/// quiet so its tail can't reach crossterm's event stream as typed garbage;
/// otherwise return immediately to avoid eating keystrokes.
fn finish_after_deadline(fd: libc::c_int, mut buf: Vec<u8>) -> Option<Vec<u8>> {
    if buf.is_empty() {
        return None;
    }
    if !buf.contains(&0x1b) {
        return Some(buf);
    }
    let grace_start = std::time::Instant::now();
    while grace_start.elapsed() < LATE_REPLY_GRACE {
        match poll_read_byte(fd, LATE_REPLY_QUIET_MS) {
            PollRead::Byte(byte) => {
                buf.push(byte);
                if buf.len() >= MAX_PROBE_RESPONSE || ends_with_osc_terminator(&buf) {
                    break;
                }
            }
            PollRead::Interrupted => continue,
            PollRead::Timeout | PollRead::Error => break,
        }
    }
    Some(buf)
}

enum PollRead {
    Byte(u8),
    Interrupted,
    Timeout,
    Error,
}

/// One EINTR-retrying poll-then-read step for a single byte.
fn poll_read_byte(fd: libc::c_int, timeout_ms: i32) -> PollRead {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if ret == 0 {
        return PollRead::Timeout;
    }
    if ret < 0 {
        return if last_errno_is_eintr() {
            PollRead::Interrupted
        } else {
            PollRead::Error
        };
    }
    loop {
        let mut byte = [0u8; 1];
        let n = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
        if n == 1 {
            return PollRead::Byte(byte[0]);
        }
        if n < 0 && last_errno_is_eintr() {
            continue;
        }
        return PollRead::Error;
    }
}

fn last_errno_is_eintr() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TEST_ENV_LOCK;
    use serde_json::json;

    // ---- palette spot checks -------------------------------------------

    #[test]
    fn test_grokday_palette_spot_checks() {
        let t = &GROKDAY;
        assert_eq!(t.kind, ThemeKind::Light);
        assert_eq!(t.bg_base, Color::Rgb(238, 238, 238)); // #eeeeee
        assert_eq!(t.bg_light, Color::Rgb(222, 222, 222)); // #dedede band
        assert_eq!(t.text_primary, Color::Rgb(38, 38, 38)); // #262626
        assert_eq!(t.gray, Color::Rgb(118, 118, 118)); // #767676
        assert_eq!(t.gray_dim, Color::Rgb(165, 165, 165)); // #a5a5a5
        assert_eq!(t.accent_error, Color::Rgb(205, 48, 72)); // #CD3048
        assert_eq!(t.accent_model, Color::Rgb(10, 142, 112)); // #0A8E70
        assert_eq!(t.md_heading[0], Color::Rgb(10, 142, 112)); // H1 teal
        assert_eq!(t.md_heading[1], Color::Rgb(47, 100, 210)); // H2 blue
        assert_eq!(t.md_code, Color::Rgb(15, 135, 162)); // #0F87A2
        assert_eq!(t.md_code_bg, Color::Rgb(228, 228, 228)); // #e4e4e4
        assert_eq!(t.md_text, Color::Rgb(68, 68, 68)); // #444444
        assert_eq!(t.link_fg, Color::Rgb(47, 100, 210)); // #2F64D2
        assert_eq!(t.selection_border, Color::Rgb(185, 185, 190)); // #b9b9be
        assert_eq!(t.bg_dark, Color::Rgb(228, 228, 228)); // #e4e4e4
        assert_eq!(t.bg_visual, Color::Rgb(198, 198, 198)); // #c6c6c6
        assert_eq!(t.accent_thinking, Color::Rgb(125, 75, 198)); // #7d4bc6
        assert_eq!(t.accent_success, Color::Rgb(55, 142, 35)); // #378e23
        assert_eq!(t.fuzzy_accent, Color::Rgb(47, 100, 210)); // #2f64d2
    }

    #[test]
    fn test_grokday_dim_is_lighter_than_muted() {
        // Light-theme polarity inversion: dim (#a5a5a5) is LIGHTER than
        // muted (#767676); brighter (#626262) is darker. Fields are roles,
        // not a brightness ramp.
        let dim = rgb_of(GROKDAY.gray_dim);
        let muted = rgb_of(GROKDAY.gray);
        let bright = rgb_of(GROKDAY.gray_bright);
        assert!(dim.0 > muted.0, "{dim:?} vs {muted:?}");
        assert!(bright.0 < muted.0, "{bright:?} vs {muted:?}");
    }

    #[test]
    fn test_groknight_palette_spot_checks() {
        let t = &GROKNIGHT;
        assert_eq!(t.kind, ThemeKind::Dark);
        assert_eq!(t.bg_base, Color::Rgb(20, 20, 20));
        assert_eq!(t.bg_light, Color::Rgb(36, 36, 36));
        assert_eq!(t.text_primary, Color::Rgb(225, 225, 225));
        assert_eq!(t.accent_error, Color::Rgb(247, 118, 142));
        assert_eq!(t.md_heading[0], Color::Rgb(26, 188, 156));
        assert_eq!(t.selection_border, Color::Rgb(60, 60, 65)); // #3c3c41
        assert_eq!(t.bg_dark, Color::Rgb(28, 28, 28)); // #1c1c1c
        assert_eq!(t.bg_visual, Color::Rgb(54, 54, 54)); // #363636
        assert_eq!(t.accent_thinking, Color::Rgb(187, 154, 247)); // #bb9af7
        assert_eq!(t.accent_success, Color::Rgb(158, 206, 106)); // #9ece6a
        assert_eq!(t.fuzzy_accent, Color::Rgb(122, 162, 247)); // #7aa2f7
    }

    #[test]
    fn test_usage_gradient_endpoints_follow_theme_fields() {
        assert_eq!(GROKNIGHT.usage_color(0.0), GROKNIGHT.text_primary);
        assert_eq!(GROKNIGHT.usage_color(60.0), GROKNIGHT.accent_user);
        assert_eq!(GROKNIGHT.usage_color(80.0), GROKNIGHT.warning);
        assert_eq!(GROKNIGHT.usage_color(99.0), GROKNIGHT.accent_error);
        assert_eq!(GROKDAY.usage_color(0.0), GROKDAY.text_primary);
        assert_eq!(GROKDAY.usage_color(99.0), GROKDAY.accent_error);
        // 13.8% lerps text_primary → accent_user (identical in groknight's
        // grayscale pair only at the ends; check an interior blend).
        let Color::Rgb(r, _, _) = GROKNIGHT.usage_color(13.8) else {
            panic!("rgb expected");
        };
        assert_eq!(r, 218); // 225 → 200 at t=0.276 ≈ #dadada
    }

    // ---- preference resolution -----------------------------------------

    #[test]
    fn test_parse_theme_pref_accepts_grok_aliases() {
        assert_eq!(parse_theme_pref("light"), Some(ThemePref::Light));
        assert_eq!(parse_theme_pref("Day"), Some(ThemePref::Light));
        assert_eq!(parse_theme_pref("grokday"), Some(ThemePref::Light));
        assert_eq!(parse_theme_pref("dark"), Some(ThemePref::Dark));
        assert_eq!(parse_theme_pref("night"), Some(ThemePref::Dark));
        assert_eq!(parse_theme_pref("grok-night"), Some(ThemePref::Dark));
        assert_eq!(parse_theme_pref("AUTO"), Some(ThemePref::Auto));
        assert_eq!(parse_theme_pref("system"), Some(ThemePref::Auto));
        assert_eq!(parse_theme_pref("solarized"), None);
        assert_eq!(parse_theme_pref(""), None);
    }

    #[test]
    fn test_env_beats_config_and_invalid_env_falls_through() {
        assert_eq!(
            resolve_pref(Some("dark"), Some("light")),
            ThemePref::Dark,
            "env wins"
        );
        assert_eq!(
            resolve_pref(Some("bogus"), Some("dark")),
            ThemePref::Dark,
            "invalid env falls through to config"
        );
        assert_eq!(resolve_pref(None, Some("light")), ThemePref::Light);
        assert_eq!(resolve_pref(None, Some("auto")), ThemePref::Auto);
        assert_eq!(resolve_pref(None, None), ThemePref::Auto, "default is auto");
        assert_eq!(resolve_pref(Some("bogus"), Some("junk")), ThemePref::Auto);
    }

    #[test]
    fn test_auto_resolves_detection_and_falls_back_light() {
        assert_eq!(
            resolve_kind(ThemePref::Auto, Some(Appearance::Dark)),
            ThemeKind::Dark
        );
        assert_eq!(
            resolve_kind(ThemePref::Auto, Some(Appearance::Light)),
            ThemeKind::Light
        );
        // dp's delta from grok: detection failure falls LIGHT, not dark.
        assert_eq!(resolve_kind(ThemePref::Auto, None), ThemeKind::Light);
        // Explicit prefs ignore detection entirely.
        assert_eq!(
            resolve_kind(ThemePref::Dark, Some(Appearance::Light)),
            ThemeKind::Dark
        );
        assert_eq!(
            resolve_kind(ThemePref::Light, Some(Appearance::Dark)),
            ThemeKind::Light
        );
    }

    #[test]
    fn test_active_theme_kind_reads_env_then_config_then_falls_light() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path());
        std::env::remove_var("HIVE_VIEW_THEME");
        std::env::remove_var("HIVE_APPEARANCE");
        std::env::remove_var("COLORFGBG");
        // No setting, no detection signal (stdin is not a tty under the
        // test runner) → light fallback.
        assert_eq!(active_theme_kind(), ThemeKind::Light);
        // Config round-trip through the real settings store (the layer
        // `hive config set view.theme dark` writes).
        crate::settings::set_setting("view.theme", json!("dark")).unwrap();
        assert_eq!(
            crate::settings::get_setting("view.theme"),
            Some(json!("dark"))
        );
        assert_eq!(active_theme_kind(), ThemeKind::Dark);
        // Env overrides the config file.
        std::env::set_var("HIVE_VIEW_THEME", "light");
        assert_eq!(active_theme_kind(), ThemeKind::Light);
        // Invalid env falls through to config.
        std::env::set_var("HIVE_VIEW_THEME", "bogus");
        assert_eq!(active_theme_kind(), ThemeKind::Dark);
        // auto in config + explicit appearance stamp.
        crate::settings::set_setting("view.theme", json!("auto")).unwrap();
        std::env::remove_var("HIVE_VIEW_THEME");
        std::env::set_var("HIVE_APPEARANCE", "dark");
        assert_eq!(active_theme_kind(), ThemeKind::Dark);
        std::env::set_var("HIVE_APPEARANCE", "light");
        assert_eq!(active_theme_kind(), ThemeKind::Light);
    }

    #[test]
    fn test_colorfgbg_feeds_auto_detection() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path());
        std::env::remove_var("HIVE_VIEW_THEME");
        std::env::remove_var("HIVE_APPEARANCE");
        std::env::set_var("COLORFGBG", "15;0");
        assert_eq!(active_theme_kind(), ThemeKind::Dark);
        std::env::set_var("COLORFGBG", "0;15");
        assert_eq!(active_theme_kind(), ThemeKind::Light);
        std::env::set_var("COLORFGBG", "15;default");
        assert_eq!(active_theme_kind(), ThemeKind::Light, "unknown → light");
    }

    // ---- appearance parsing (grok env_appearance.rs semantics) ---------

    #[test]
    fn test_parse_appearance_var_stamps() {
        assert_eq!(parse_appearance_var(Some("dark")), Some(Appearance::Dark));
        assert_eq!(parse_appearance_var(Some("night")), Some(Appearance::Dark));
        assert_eq!(parse_appearance_var(Some("Light")), Some(Appearance::Light));
        assert_eq!(parse_appearance_var(Some("day")), Some(Appearance::Light));
        assert_eq!(parse_appearance_var(Some("solarized")), None);
        assert_eq!(parse_appearance_var(None), None);
    }

    #[test]
    fn test_parse_colorfgbg_vim_heuristic() {
        assert_eq!(parse_colorfgbg(Some("15;0")), Some(Appearance::Dark));
        assert_eq!(parse_colorfgbg(Some("0;15")), Some(Appearance::Light));
        assert_eq!(
            parse_colorfgbg(Some("15;default;0")),
            Some(Appearance::Dark)
        );
        assert_eq!(parse_colorfgbg(Some("7;8")), Some(Appearance::Dark));
        assert_eq!(parse_colorfgbg(Some("0;7")), Some(Appearance::Light));
        assert_eq!(parse_colorfgbg(Some("15;default")), None);
        assert_eq!(parse_colorfgbg(Some("1;99")), None);
        assert_eq!(parse_colorfgbg(Some("")), None);
        assert_eq!(parse_colorfgbg(None), None);
    }

    // ---- OSC 11 parsing & classification -------------------------------

    #[test]
    fn test_parse_osc11_rgb_channel_widths() {
        assert_eq!(
            parse_osc11_rgb("\x1b]11;rgb:ffff/ffff/ffff\x07"),
            Some((255, 255, 255))
        );
        assert_eq!(
            parse_osc11_rgb("\x1b]11;rgb:1a/1b/26\x07"),
            Some((0x1a, 0x1b, 0x26))
        );
        assert_eq!(
            parse_osc11_rgb("\x1b]11;rgb:8080/8080/8080\x1b\\"),
            Some((128, 128, 128))
        );
        assert_eq!(
            parse_osc11_rgb("\x1b]11;rgb:fff/fff/fff\x07"),
            Some((15, 15, 15))
        );
        assert_eq!(parse_osc11_rgb("\x1b]11;rgb:ffff/ffff\x07"), None);
        assert_eq!(parse_osc11_rgb("\x1b]11;color:ffff/ffff/ffff\x07"), None);
        assert_eq!(parse_osc11_rgb(""), None);
    }

    #[test]
    fn test_classify_luminance_boundary() {
        assert_eq!(classify_luminance(0, 0, 0), Appearance::Dark);
        assert_eq!(classify_luminance(255, 255, 255), Appearance::Light);
        assert_eq!(classify_luminance(0x1a, 0x1b, 0x26), Appearance::Dark);
        assert_eq!(classify_luminance(0xee, 0xee, 0xee), Appearance::Light);
        // sRGB 186 gray ≈ Y 0.497 (dark); 188 ≈ 0.508 (light).
        assert_eq!(classify_luminance(186, 186, 186), Appearance::Dark);
        assert_eq!(classify_luminance(188, 188, 188), Appearance::Light);
    }

    #[test]
    fn test_unterminated_osc_reply_is_rejected() {
        assert!(!ends_with_osc_terminator(b"\x1b]11;rgb:ffff/ffff/00"));
        assert!(ends_with_osc_terminator(b"\x1b]11;rgb:ffff/ffff/ffff\x07"));
        assert!(ends_with_osc_terminator(
            b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"
        ));
        assert!(!ends_with_osc_terminator(b""));
    }
}
