//! Interaction state machines for the `hive view` TUI — selection movement,
//! fold/density state, the slash palette, and `/find` matching. Pure logic
//! over entry ids and display order; rendering and key wiring live in the
//! parent module.

use std::collections::HashMap;

use crate::view_theme::{parse_theme_pref, ThemePref};

// ---------------------------------------------------------------------------
// Density & fold state
// ---------------------------------------------------------------------------

/// Which fold family an entry belongs to (drives its density default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldKind {
    /// Thinking blocks: expanded at `thinking` and `verbose` density.
    Thinking,
    /// Run / Tool / ToolGroup blocks: expanded at `verbose` density only.
    Tool,
    /// User bands: collapse to 3 lines by default at every density.
    User,
    /// Assistant text and turn markers: never fold.
    Fixed,
}

/// Transcript view density (hive's own design, session-local): `normal` is
/// today's rendering, `thinking` expands all thinking blocks, `verbose`
/// additionally expands all tool execution blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Normal,
    Thinking,
    Verbose,
}

impl Density {
    /// Ctrl+O cycle order: normal → thinking → verbose → normal.
    pub fn next(self) -> Self {
        match self {
            Density::Normal => Density::Thinking,
            Density::Thinking => Density::Verbose,
            Density::Verbose => Density::Normal,
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(Density::Normal),
            "thinking" => Some(Density::Thinking),
            "verbose" => Some(Density::Verbose),
            _ => None,
        }
    }
}

/// Per-block manual overrides layered over the global density default
/// (grok's `display_mode_pinned` idea). A density change clears overrides.
pub struct FoldState {
    pub density: Density,
    overrides: HashMap<u64, bool>,
}

impl FoldState {
    pub fn new() -> Self {
        FoldState {
            density: Density::Normal,
            overrides: HashMap::new(),
        }
    }

    /// The density default for a fold family (true = expanded).
    pub fn default_expanded(density: Density, kind: FoldKind) -> bool {
        match kind {
            FoldKind::Thinking => density != Density::Normal,
            FoldKind::Tool => density == Density::Verbose,
            FoldKind::User | FoldKind::Fixed => false,
        }
    }

    /// Effective expansion: manual override else density default.
    pub fn expanded(&self, id: u64, kind: FoldKind) -> bool {
        if kind == FoldKind::Fixed {
            return false;
        }
        self.overrides
            .get(&id)
            .copied()
            .unwrap_or_else(|| Self::default_expanded(self.density, kind))
    }

    /// Pin one block; a pin matching the density default just clears itself.
    pub fn set(&mut self, id: u64, kind: FoldKind, expanded: bool) {
        if kind == FoldKind::Fixed {
            return;
        }
        if Self::default_expanded(self.density, kind) == expanded {
            self.overrides.remove(&id);
        } else {
            self.overrides.insert(id, expanded);
        }
    }

    pub fn toggle(&mut self, id: u64, kind: FoldKind) {
        let expanded = self.expanded(id, kind);
        self.set(id, kind, !expanded);
    }

    /// `/view <density>`: overrides reset so the density speaks alone.
    pub fn set_density(&mut self, density: Density) {
        self.density = density;
        self.overrides.clear();
    }

    /// Ctrl+O.
    pub fn cycle_density(&mut self) {
        self.set_density(self.density.next());
    }

    /// Ctrl+E (grok expand_all_thinking): if ANY thinking block is
    /// effectively collapsed, expand them all; else collapse them all.
    /// Returns the state they were driven to (true = expanded).
    pub fn toggle_all_thinking(&mut self, ids: &[u64]) -> bool {
        let any_collapsed = ids.iter().any(|&id| !self.expanded(id, FoldKind::Thinking));
        for &id in ids {
            self.set(id, FoldKind::Thinking, any_collapsed);
        }
        any_collapsed
    }

    #[cfg(test)]
    pub fn override_count(&self) -> usize {
        self.overrides.len()
    }
}

// ---------------------------------------------------------------------------
// Selection movement (grok state/selection.rs + state/nav.rs)
// ---------------------------------------------------------------------------

/// The slice of layout state selection logic needs, in display order.
#[derive(Debug, Clone, Copy)]
pub struct EntryInfo {
    pub id: u64,
    pub selectable: bool,
    /// User-prompt block: the turn anchor Shift+Left/Right jumps between.
    pub is_turn: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectMove {
    To(u64),
    /// Down past the last selectable entry: re-engage follow + goto_bottom.
    Overscroll,
    Stay,
}

fn idx_of(entries: &[EntryInfo], id: u64) -> Option<usize> {
    entries.iter().position(|e| e.id == id)
}

fn last_selectable(entries: &[EntryInfo]) -> Option<u64> {
    entries.iter().rev().find(|e| e.selectable).map(|e| e.id)
}

/// Down: next selectable entry; unset selection engages at the tail (grok
/// auto-selects the last entry on pane activation).
pub fn select_next(entries: &[EntryInfo], current: Option<u64>) -> SelectMove {
    let Some(i) = current.and_then(|id| idx_of(entries, id)) else {
        return match last_selectable(entries) {
            Some(id) => SelectMove::To(id),
            None => SelectMove::Stay,
        };
    };
    match entries[i + 1..].iter().find(|e| e.selectable) {
        Some(e) => SelectMove::To(e.id),
        None => SelectMove::Overscroll,
    }
}

/// Up: previous selectable entry; unset selection engages at the tail.
pub fn select_prev(entries: &[EntryInfo], current: Option<u64>) -> SelectMove {
    let Some(i) = current.and_then(|id| idx_of(entries, id)) else {
        return match last_selectable(entries) {
            Some(id) => SelectMove::To(id),
            None => SelectMove::Stay,
        };
    };
    match entries[..i].iter().rev().find(|e| e.selectable) {
        Some(e) => SelectMove::To(e.id),
        None => SelectMove::Stay,
    }
}

/// Shift+Right (grok next_turn): the NEXT turn's prompt entry; at the last
/// turn, re-snap the current turn's prompt.
pub fn next_turn(entries: &[EntryInfo], current: Option<u64>) -> Option<u64> {
    match current.and_then(|id| idx_of(entries, id)) {
        Some(i) => entries[i + 1..]
            .iter()
            .find(|e| e.is_turn)
            .or_else(|| entries[..=i].iter().rev().find(|e| e.is_turn))
            .map(|e| e.id),
        None => entries.iter().rev().find(|e| e.is_turn).map(|e| e.id),
    }
}

/// Shift+Left (grok prev_turn, two-stage): from a response, the CURRENT
/// turn's prompt; from a prompt, the PREVIOUS turn's prompt; at the first
/// prompt, the first selectable pre-turn entry.
pub fn prev_turn(entries: &[EntryInfo], current: Option<u64>) -> Option<u64> {
    let Some(i) = current.and_then(|id| idx_of(entries, id)) else {
        return entries.iter().rev().find(|e| e.is_turn).map(|e| e.id);
    };
    if entries[i].is_turn {
        entries[..i]
            .iter()
            .rev()
            .find(|e| e.is_turn)
            .or_else(|| entries[..i].iter().find(|e| e.selectable))
            .map(|e| e.id)
    } else {
        entries[..i]
            .iter()
            .rev()
            .find(|e| e.is_turn)
            .or_else(|| entries.iter().find(|e| e.selectable))
            .map(|e| e.id)
    }
}

/// grok ensure_selected_visible, minimal scrolling: fully visible → no
/// scroll; top clipped (or taller than the viewport) → entry top at the
/// content top; bottom clipped and it fits → just enough to show the bottom.
pub fn scroll_into_view(
    offset: usize,
    viewport: usize,
    start: usize,
    height: usize,
) -> Option<usize> {
    if viewport == 0 {
        return None;
    }
    let end = start + height;
    if start >= offset && end <= offset + viewport {
        return None;
    }
    if start < offset || height >= viewport {
        return Some(start);
    }
    Some(end - viewport)
}

// ---------------------------------------------------------------------------
// /find matching
// ---------------------------------------------------------------------------

/// Case-insensitive substring search over `(id, content)` in display order,
/// starting after/before `current` (exclusive) and wrapping around.
pub fn find_match(
    entries: &[(u64, String)],
    current: Option<u64>,
    query: &str,
    forward: bool,
) -> Option<u64> {
    if entries.is_empty() || query.is_empty() {
        return None;
    }
    let needle = query.to_lowercase();
    let hit = |&(_, ref text): &(u64, String)| text.to_lowercase().contains(&needle);
    let pivot = current.and_then(|id| entries.iter().position(|&(eid, _)| eid == id));
    let n = entries.len();
    let order: Vec<usize> = if forward {
        let s = pivot.map(|i| i + 1).unwrap_or(0);
        (0..n).map(|k| (s + k) % n).collect()
    } else {
        let s = pivot.unwrap_or(0);
        (1..=n).map(|k| (s + n - k) % n).collect()
    };
    order
        .into_iter()
        .find(|&i| hit(&entries[i]))
        .map(|i| entries[i].0)
}

// ---------------------------------------------------------------------------
// Slash palette
// ---------------------------------------------------------------------------

pub struct PaletteCmd {
    pub name: &'static str,
    pub desc: &'static str,
}

pub const PALETTE_COMMANDS: [PaletteCmd; 4] = [
    PaletteCmd {
        name: "/theme",
        desc: "switch theme: light · dark · auto (persists)",
    },
    PaletteCmd {
        name: "/view",
        desc: "transcript density: normal · thinking · verbose",
    },
    PaletteCmd {
        name: "/find",
        desc: "jump to blocks matching text · n/N cycle",
    },
    PaletteCmd {
        name: "/quit",
        desc: "exit the viewer",
    },
];

/// Max dropdown rows (grok MAX_VISIBLE_SUGGESTIONS; ours never exceeds 4).
pub const MAX_PALETTE_ROWS: usize = 8;

/// Case-insensitive subsequence match of `needle` inside `hay`; returns the
/// matched char positions in `hay` for highlight.
pub fn fuzzy_positions(needle: &str, hay: &str) -> Option<Vec<usize>> {
    let mut positions = Vec::new();
    let mut hay_chars = hay.char_indices().enumerate().map(|(i, (_, c))| (i, c));
    for nc in needle.chars() {
        let nc = nc.to_ascii_lowercase();
        loop {
            let (i, hc) = hay_chars.next()?;
            if hc.to_ascii_lowercase() == nc {
                positions.push(i);
                break;
            }
        }
    }
    Some(positions)
}

/// What Enter in the palette resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteAction {
    /// `/theme [arg]`; `None` = cycle light↔dark.
    SwitchTheme(Option<ThemePref>),
    /// `/view <density>`.
    SetDensity(Density),
    /// `/find <text>`.
    Find(String),
    /// `/quit`.
    Quit,
    /// Autocomplete the input to `"<name> "` and stay open.
    Complete(&'static str),
    /// Nothing actionable; stay open.
    Noop,
}

pub struct Palette {
    /// Full input including the leading `/`.
    pub input: String,
    /// Index into [`Palette::filtered`], clamped by the renderer/mover.
    pub selected: usize,
}

impl Palette {
    pub fn open() -> Self {
        Palette {
            input: "/".to_string(),
            selected: 0,
        }
    }

    /// Command word after `/`, before the first space.
    pub fn token(&self) -> &str {
        self.input
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("")
    }

    /// Everything after the first space, trimmed.
    pub fn args(&self) -> &str {
        match self.input.find(' ') {
            Some(i) => self.input[i + 1..].trim(),
            None => "",
        }
    }

    /// Commands whose name fuzzy-matches the typed token, with the matched
    /// char positions (relative to the full `/name`) for highlighting.
    pub fn filtered(&self) -> Vec<(usize, Vec<usize>)> {
        let token = self.token();
        PALETTE_COMMANDS
            .iter()
            .enumerate()
            .filter_map(|(i, cmd)| {
                fuzzy_positions(token, &cmd.name[1..])
                    .map(|pos| (i, pos.into_iter().map(|p| p + 1).collect()))
            })
            .collect()
    }

    /// The clamped selected row within the current filter.
    pub fn selected_row(&self) -> usize {
        let n = self.filtered().len();
        self.selected.min(n.saturating_sub(1))
    }

    pub fn insert(&mut self, ch: char) {
        self.input.push(ch);
        self.selected = self.selected_row();
    }

    /// Backspace; popping the leading `/` closes (returns false = closed).
    pub fn backspace(&mut self) -> bool {
        self.input.pop();
        !self.input.is_empty()
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected_row().saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        let n = self.filtered().len();
        if n > 0 {
            self.selected = (self.selected_row() + 1).min(n - 1);
        }
    }

    /// Resolve a typed token to one command: exact name match, else a unique
    /// fuzzy match.
    fn resolve_token(&self) -> Option<&'static PaletteCmd> {
        let token = self.token();
        if token.is_empty() {
            return None;
        }
        if let Some(cmd) = PALETTE_COMMANDS.iter().find(|c| &c.name[1..] == token) {
            return Some(cmd);
        }
        let hits = self.filtered();
        if hits.len() == 1 {
            return Some(&PALETTE_COMMANDS[hits[0].0]);
        }
        None
    }

    pub fn enter(&self) -> PaletteAction {
        let args = self.args().to_string();
        if !args.is_empty() {
            let Some(cmd) = self.resolve_token() else {
                return PaletteAction::Noop;
            };
            return match cmd.name {
                "/theme" => match parse_theme_pref(&args) {
                    Some(pref) => PaletteAction::SwitchTheme(Some(pref)),
                    None => PaletteAction::Noop,
                },
                "/view" => match Density::parse(&args) {
                    Some(d) => PaletteAction::SetDensity(d),
                    None => PaletteAction::Noop,
                },
                "/find" => PaletteAction::Find(args),
                "/quit" => PaletteAction::Quit,
                _ => PaletteAction::Noop,
            };
        }
        let hits = self.filtered();
        let cmd = match hits.get(self.selected_row()) {
            Some(&(i, _)) => &PALETTE_COMMANDS[i],
            None => match self.resolve_token() {
                Some(cmd) => cmd,
                None => return PaletteAction::Noop,
            },
        };
        match cmd.name {
            "/theme" => PaletteAction::SwitchTheme(None),
            "/quit" => PaletteAction::Quit,
            "/view" => PaletteAction::Complete("/view"),
            "/find" => PaletteAction::Complete("/find"),
            _ => PaletteAction::Noop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: u64, selectable: bool, is_turn: bool) -> EntryInfo {
        EntryInfo {
            id,
            selectable,
            is_turn,
        }
    }

    /// user(0) group(1) thinking(2) assistant(3) workedfor(4, non-selectable)
    /// user(5) assistant(6)
    fn sample() -> Vec<EntryInfo> {
        vec![
            e(0, true, true),
            e(1, true, false),
            e(2, true, false),
            e(3, true, false),
            e(4, false, false),
            e(5, true, true),
            e(6, true, false),
        ]
    }

    // ---- selection movement --------------------------------------------

    #[test]
    fn test_select_engages_at_tail_and_walks_selectables() {
        let entries = sample();
        assert_eq!(select_next(&entries, None), SelectMove::To(6));
        assert_eq!(select_prev(&entries, None), SelectMove::To(6));
        assert_eq!(select_prev(&entries, Some(6)), SelectMove::To(5));
        // skips the non-selectable WorkedFor at index 4
        assert_eq!(select_prev(&entries, Some(5)), SelectMove::To(3));
        assert_eq!(select_next(&entries, Some(3)), SelectMove::To(5));
        assert_eq!(select_prev(&entries, Some(0)), SelectMove::Stay);
    }

    #[test]
    fn test_select_next_past_last_is_overscroll() {
        let entries = sample();
        assert_eq!(select_next(&entries, Some(6)), SelectMove::Overscroll);
        assert_eq!(select_next(&[], None), SelectMove::Stay);
    }

    // ---- turn jumps -----------------------------------------------------

    #[test]
    fn test_next_turn_jumps_prompts_and_resnap_at_last() {
        let entries = sample();
        assert_eq!(next_turn(&entries, Some(0)), Some(5));
        assert_eq!(next_turn(&entries, Some(2)), Some(5));
        // at the last turn: re-snap its own prompt
        assert_eq!(next_turn(&entries, Some(6)), Some(5));
        assert_eq!(next_turn(&entries, Some(5)), Some(5));
        // unset selection snaps the last prompt
        assert_eq!(next_turn(&entries, None), Some(5));
    }

    #[test]
    fn test_prev_turn_is_two_stage() {
        let entries = sample();
        // inside a response → the CURRENT turn's prompt first
        assert_eq!(prev_turn(&entries, Some(6)), Some(5));
        assert_eq!(prev_turn(&entries, Some(2)), Some(0));
        // on a prompt → the PREVIOUS turn's prompt
        assert_eq!(prev_turn(&entries, Some(5)), Some(0));
        // unset → last prompt
        assert_eq!(prev_turn(&entries, None), Some(5));
    }

    #[test]
    fn test_prev_turn_at_first_prompt_goes_to_preturn_entry() {
        // pre-turn selectable entry before the first prompt
        let entries = vec![e(0, true, false), e(1, true, true), e(2, true, false)];
        assert_eq!(prev_turn(&entries, Some(1)), Some(0));
        // no pre-turn entries → stay
        let entries = vec![e(0, true, true), e(1, true, false)];
        assert_eq!(prev_turn(&entries, Some(0)), None);
    }

    // ---- scroll into view ----------------------------------------------

    #[test]
    fn test_scroll_into_view_is_minimal() {
        // fully visible → no scroll
        assert_eq!(scroll_into_view(10, 20, 12, 5), None);
        // top clipped → top-align
        assert_eq!(scroll_into_view(10, 20, 8, 5), Some(8));
        // bottom clipped and fits → show the bottom
        assert_eq!(scroll_into_view(10, 20, 28, 5), Some(13));
        // taller than the viewport → show the top
        assert_eq!(scroll_into_view(10, 20, 28, 30), Some(28));
        assert_eq!(scroll_into_view(0, 0, 5, 2), None);
    }

    // ---- fold state ----------------------------------------------------

    #[test]
    fn test_density_cycle_order() {
        assert_eq!(Density::Normal.next(), Density::Thinking);
        assert_eq!(Density::Thinking.next(), Density::Verbose);
        assert_eq!(Density::Verbose.next(), Density::Normal);
        assert_eq!(Density::parse("Thinking"), Some(Density::Thinking));
        assert_eq!(Density::parse("bogus"), None);
    }

    #[test]
    fn test_density_defaults_per_kind() {
        use FoldKind::*;
        let d = FoldState::default_expanded;
        assert!(!d(Density::Normal, Thinking) && !d(Density::Normal, Tool));
        assert!(d(Density::Thinking, Thinking) && !d(Density::Thinking, Tool));
        assert!(d(Density::Verbose, Thinking) && d(Density::Verbose, Tool));
        for density in [Density::Normal, Density::Thinking, Density::Verbose] {
            assert!(!d(density, User) && !d(density, Fixed));
        }
    }

    #[test]
    fn test_overrides_layer_over_density_and_reset_on_change() {
        let mut f = FoldState::new();
        assert!(!f.expanded(1, FoldKind::Thinking));
        f.toggle(1, FoldKind::Thinking);
        assert!(f.expanded(1, FoldKind::Thinking));
        f.set_density(Density::Thinking);
        // density change cleared the override; default now expanded
        assert_eq!(f.override_count(), 0);
        assert!(f.expanded(1, FoldKind::Thinking));
        // collapsing under an expanded default pins an override
        f.toggle(1, FoldKind::Thinking);
        assert!(!f.expanded(1, FoldKind::Thinking));
        assert_eq!(f.override_count(), 1);
        // re-toggling back to the default clears the pin
        f.toggle(1, FoldKind::Thinking);
        assert_eq!(f.override_count(), 0);
        f.cycle_density();
        assert_eq!(f.density, Density::Verbose);
        assert!(f.expanded(2, FoldKind::Tool));
    }

    #[test]
    fn test_toggle_all_thinking_any_collapsed_rule() {
        let mut f = FoldState::new();
        let ids = [1, 2, 3];
        f.toggle(2, FoldKind::Thinking); // one expanded, two collapsed
        assert!(f.toggle_all_thinking(&ids), "any collapsed → expand all");
        for &id in &ids {
            assert!(f.expanded(id, FoldKind::Thinking));
        }
        assert!(!f.toggle_all_thinking(&ids), "all expanded → collapse all");
        for &id in &ids {
            assert!(!f.expanded(id, FoldKind::Thinking));
        }
    }

    // ---- find -----------------------------------------------------------

    fn corpus() -> Vec<(u64, String)> {
        vec![
            (0, "review the SPEC today".to_string()),
            (1, "cargo build output".to_string()),
            (2, "the spec is green".to_string()),
        ]
    }

    #[test]
    fn test_find_match_is_case_insensitive_and_wraps() {
        let c = corpus();
        assert_eq!(find_match(&c, None, "spec", true), Some(0));
        assert_eq!(find_match(&c, Some(0), "spec", true), Some(2));
        assert_eq!(find_match(&c, Some(2), "spec", true), Some(0), "wraps");
        assert_eq!(find_match(&c, Some(0), "spec", false), Some(2), "backward");
        assert_eq!(find_match(&c, Some(2), "SPEC", false), Some(0));
        assert_eq!(find_match(&c, None, "absent", true), None);
        assert_eq!(find_match(&c, None, "", true), None);
    }

    // ---- palette --------------------------------------------------------

    #[test]
    fn test_palette_filters_by_fuzzy_subsequence() {
        let mut p = Palette::open();
        assert_eq!(p.filtered().len(), 4, "bare / lists everything");
        p.insert('t');
        // "t" is a subsequence of "theme" and "quit"
        let names: Vec<&str> = p
            .filtered()
            .iter()
            .map(|&(i, _)| PALETTE_COMMANDS[i].name)
            .collect();
        assert_eq!(names, vec!["/theme", "/quit"]);
        p.insert('h');
        let hits = p.filtered();
        assert_eq!(hits.len(), 1);
        assert_eq!(PALETTE_COMMANDS[hits[0].0].name, "/theme");
        // positions are relative to the full "/theme": 't' at 1, 'h' at 2
        assert_eq!(hits[0].1, vec![1, 2]);
    }

    #[test]
    fn test_palette_enter_dispatch() {
        let cmd = |input: &str| Palette {
            input: input.to_string(),
            selected: 0,
        };
        assert_eq!(cmd("/theme").enter(), PaletteAction::SwitchTheme(None));
        assert_eq!(
            cmd("/theme dark").enter(),
            PaletteAction::SwitchTheme(Some(ThemePref::Dark))
        );
        assert_eq!(
            cmd("/theme auto").enter(),
            PaletteAction::SwitchTheme(Some(ThemePref::Auto))
        );
        assert_eq!(cmd("/theme solarized").enter(), PaletteAction::Noop);
        assert_eq!(
            cmd("/view thinking").enter(),
            PaletteAction::SetDensity(Density::Thinking)
        );
        assert_eq!(cmd("/view bogus").enter(), PaletteAction::Noop);
        assert_eq!(
            cmd("/find fn main").enter(),
            PaletteAction::Find("fn main".to_string())
        );
        assert_eq!(cmd("/quit").enter(), PaletteAction::Quit);
        // args + fuzzy-unique token resolve ("th" only matches /theme)
        assert_eq!(
            cmd("/th light").enter(),
            PaletteAction::SwitchTheme(Some(ThemePref::Light))
        );
        // no args on an args-taking command autocompletes
        assert_eq!(cmd("/view").enter(), PaletteAction::Complete("/view"));
        assert_eq!(cmd("/find").enter(), PaletteAction::Complete("/find"));
    }

    #[test]
    fn test_palette_enter_uses_selected_row_without_args() {
        let mut p = Palette::open();
        p.move_down(); // → /view
        assert_eq!(p.enter(), PaletteAction::Complete("/view"));
        p.move_down();
        p.move_down(); // → /quit
        assert_eq!(p.enter(), PaletteAction::Quit);
        p.move_down(); // clamped at the end
        assert_eq!(p.selected_row(), 3);
        p.move_up();
        assert_eq!(p.selected_row(), 2);
    }

    #[test]
    fn test_palette_backspace_past_slash_closes() {
        let mut p = Palette::open();
        p.insert('q');
        assert!(p.backspace(), "back to bare /");
        assert!(!p.backspace(), "popping the / closes");
    }
}
