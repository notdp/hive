use super::*;

// ---------------------------------------------------------------------------
// Chrome state scraped from raw transcript rows
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(super) struct Chrome {
    pub(super) branch: Option<String>,
    pub(super) cwd: Option<String>,
    /// Latest full-context usage (input + cache + output) from an assistant row.
    pub(super) context_used: i64,
}

impl Chrome {
    fn update(&mut self, row: &Value) {
        if let Some(b) = row.get("gitBranch").and_then(Value::as_str) {
            if !b.is_empty() {
                self.branch = Some(b.to_string());
            }
        }
        if let Some(c) = row.get("cwd").and_then(Value::as_str) {
            if !c.is_empty() {
                self.cwd = Some(c.to_string());
            }
        }
        if let Some(usage) = row.get("message").and_then(|m| m.get("usage")) {
            let n = |k: &str| usage.get(k).and_then(Value::as_i64).unwrap_or(0);
            let used = n("input_tokens")
                + n("cache_creation_input_tokens")
                + n("cache_read_input_tokens")
                + n("output_tokens");
            if used > 0 {
                self.context_used = used;
            }
        }
    }
}

/// `$HOME`-prefixed paths abbreviate to `~` on a component boundary.
pub(super) fn abbreviate_path(path: &str) -> String {
    let home = match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => h,
        _ => return path.to_string(),
    };
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(&home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// grok context_bar.rs::fmt_tokens (≤ 4 chars; 10K+ truncates, not rounds).
pub(super) fn fmt_tokens(n: i64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 10_000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}K", n / 1000)
    } else if n < 10_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{}M", n / 1_000_000)
    }
}

// ---------------------------------------------------------------------------
// Scroll/follow state (grok scrollback/state/nav.rs semantics)
// ---------------------------------------------------------------------------

pub(super) struct Scroll {
    pub(super) offset: usize,
    pub(super) max: usize,
    pub(super) follow: bool,
}

impl Scroll {
    fn new() -> Self {
        Scroll {
            offset: 0,
            max: 0,
            follow: true,
        }
    }

    /// The renderer feeds the fresh max offset; follow pins to the bottom.
    pub(super) fn sync(&mut self, max: usize) {
        self.max = max;
        if self.follow || self.offset > max {
            self.offset = max;
        }
    }

    /// Any upward scroll leaves follow mode (nav.rs scroll_up).
    pub(super) fn scroll_up(&mut self, rows: usize) {
        self.follow = false;
        self.offset = self.offset.saturating_sub(rows);
    }

    /// Downward scroll clamps to the bottom. Follow re-engages only on
    /// overscroll — a down gesture that was already pinned at the bottom and
    /// moved zero rows (nav.rs scroll_down + follow_by_overscroll; grok's
    /// j-on-last-entry rule collapses to the same gesture here). A scroll
    /// that merely lands at the bottom does NOT re-engage.
    pub(super) fn scroll_down(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        let before = self.offset;
        self.offset = (self.offset + rows).min(self.max);
        if self.offset == before && before == self.max {
            self.follow = true;
        }
    }

    /// `g` (nav.rs goto_top): jump to the top, follow off.
    pub(super) fn goto_top(&mut self) {
        self.follow = false;
        self.offset = 0;
    }

    /// `G` (nav.rs goto_bottom): jump to the bottom AND re-engage follow.
    pub(super) fn goto_bottom(&mut self) {
        self.follow = true;
        self.offset = self.max;
    }
}

/// Page scroll = viewport − 2 overlap rows, min 1 (nav.rs page_scroll_rows;
/// hive view has no sticky header).
pub(super) fn page_rows(viewport_h: usize) -> usize {
    viewport_h.saturating_sub(2).max(1)
}

/// Half page = viewport / 2, min 1 (nav.rs half_page_up/down).
pub(super) fn half_page_rows(viewport_h: usize) -> usize {
    (viewport_h / 2).max(1)
}

/// grok's scrollback-focused pager scroll bindings, minus the keys the
/// interaction layer now owns (Up/Down select, q quits at the app level):
/// j/k line scroll, Ctrl+J/K line scroll, Ctrl+D/U half page, PageUp/Down
/// page, g/G top/bottom.
pub(super) fn handle_scroll_key(
    scroll: &mut Scroll,
    viewport_h: usize,
    code: KeyCode,
    mods: KeyModifiers,
) {
    if mods.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('j') => scroll.scroll_down(1),
            KeyCode::Char('k') => scroll.scroll_up(1),
            KeyCode::Char('d') => scroll.scroll_down(half_page_rows(viewport_h)),
            KeyCode::Char('u') => scroll.scroll_up(half_page_rows(viewport_h)),
            _ => {}
        }
        return;
    }
    match code {
        KeyCode::Char('j') => scroll.scroll_down(1),
        KeyCode::Char('k') => scroll.scroll_up(1),
        KeyCode::Char('g') => scroll.goto_top(),
        KeyCode::Char('G') => scroll.goto_bottom(),
        KeyCode::PageDown => scroll.scroll_down(page_rows(viewport_h)),
        KeyCode::PageUp => scroll.scroll_up(page_rows(viewport_h)),
        _ => {}
    }
}

/// Wheel tick = 3 lines (grok mouse.rs), same follow rules as key scrolls.
pub(super) fn handle_mouse(scroll: &mut Scroll, kind: MouseEventKind) {
    match kind {
        MouseEventKind::ScrollDown => scroll.scroll_down(WHEEL_LINES),
        MouseEventKind::ScrollUp => scroll.scroll_up(WHEEL_LINES),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Block viewer overlay (grok views/block_viewer.rs + modal_window.rs)
// ---------------------------------------------------------------------------

pub(super) struct Viewer {
    pub(super) title: String,
    block: DisplayBlock,
    pub(super) scroll: usize,
    pub(super) lines: Vec<Line<'static>>,
    cache_w: usize,
    /// Content rows of the last draw, for page scrolling.
    pub(super) view_h: usize,
}

fn viewer_title(block: &DisplayBlock) -> String {
    match block {
        DisplayBlock::User(_) => "User".to_string(),
        DisplayBlock::Assistant(_) => "Assistant".to_string(),
        DisplayBlock::Thinking(_) => "Thinking".to_string(),
        DisplayBlock::Run(_) => "Run".to_string(),
        DisplayBlock::Tool(tool) => tool.name.clone(),
        DisplayBlock::ToolGroup(g) => g.label(),
        DisplayBlock::WorkedFor(_) => "Turn".to_string(),
    }
}

/// The viewer's tool output: the scrollback's rows, unindented.
fn viewer_outcome_lines(
    out: &mut Vec<Line<'static>>,
    t: &ViewTheme,
    width: usize,
    result: &Option<ToolOutcome>,
) {
    if let Some(res) = result {
        outcome_rows(t, out, "", res, width);
    }
}

fn viewer_lines(block: &DisplayBlock, t: &ViewTheme, width: usize) -> Vec<Line<'static>> {
    let width = width.max(4);
    let md = |text: &str| -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for line in grok_md::render_ratatui(text, t, width) {
            out.extend(wrap_line(&line, width));
        }
        out
    };
    let mut out = Vec::new();
    match block {
        DisplayBlock::User(u) => {
            for src in u.text.lines() {
                for piece in wrap_plain(src, width) {
                    out.push(Line::from(Span::styled(piece, fg(t.text_primary))));
                }
            }
        }
        DisplayBlock::Assistant(a) => out = md(&a.markdown),
        DisplayBlock::Thinking(tb) => out = md(&tb.text),
        DisplayBlock::Run(r) => {
            command_rows(t, &mut out, "", &r.command, width);
            if !out.is_empty() {
                out.push(Line::default());
            }
            viewer_outcome_lines(&mut out, t, width, &r.result);
        }
        DisplayBlock::Tool(tool) => {
            for src in tool.input_json.lines() {
                for piece in wrap_plain(src, width) {
                    out.push(Line::from(Span::styled(piece, fg(t.text_secondary))));
                }
            }
            if !out.is_empty() {
                out.push(Line::default());
            }
            viewer_outcome_lines(&mut out, t, width, &tool.result);
        }
        DisplayBlock::ToolGroup(g) => {
            for (i, member) in g.members.iter().enumerate() {
                if i > 0 {
                    out.push(Line::default());
                }
                out.push(Line::from(vec![
                    Span::styled("◆ ", fg(t.gray)),
                    Span::styled(member.name.clone(), bold(t.text_primary)),
                    Span::styled(format!("  {}", member.hint), fg(t.gray)),
                ]));
                viewer_outcome_lines(&mut out, t, width, &member.result);
            }
        }
        DisplayBlock::WorkedFor(w) => out.push(Line::from(Span::styled(w.label(), fg(t.gray)))),
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "(no content)".to_string(),
            fg(t.gray),
        )));
    }
    out
}

impl Viewer {
    fn new(block: DisplayBlock) -> Self {
        Viewer {
            title: viewer_title(&block),
            block,
            scroll: 0,
            lines: Vec::new(),
            cache_w: 0,
            view_h: 0,
        }
    }

    fn invalidate(&mut self) {
        self.cache_w = 0;
        self.lines.clear();
    }

    pub(super) fn build(&mut self, t: &ViewTheme, width: usize) {
        if self.cache_w == width && !self.lines.is_empty() {
            return;
        }
        self.lines = viewer_lines(&self.block, t, width);
        self.cache_w = width;
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// A cached per-entry render keyed by the states that change its pixels.
struct CachedEntry {
    expanded: bool,
    selected: bool,
    foldable: bool,
    lines: Vec<Line<'static>>,
}

/// Where an entry landed in the assembled scrollback, plus its capabilities.
#[derive(Clone, Copy)]
pub(super) struct LayoutEntry {
    pub(super) id: u64,
    pub(super) start: usize,
    pub(super) height: usize,
    selectable: bool,
    pub(super) is_turn: bool,
    foldable: bool,
    pub(super) kind: FoldKind,
}

pub(super) struct App {
    pub(super) theme: &'static ViewTheme,
    pub(super) parser: TranscriptParser,
    pub(super) chrome: Chrome,
    finalized: Vec<Entry>,
    pub(super) fold: FoldState,
    pub(super) selected: Option<u64>,
    pub(super) palette: Option<Palette>,
    pub(super) viewer: Option<Viewer>,
    find_query: Option<String>,
    last_click: Option<(Instant, u64)>,
    pub(super) scroll: Scroll,
    pub(super) viewport_h: usize,
    pub(super) scroll_rect: Rect,
    layout: Vec<LayoutEntry>,
    cache: HashMap<u64, CachedEntry>,
    cache_width: usize,
    cache_theme: ThemeKind,
}

impl App {
    pub(super) fn new(theme: &'static ViewTheme) -> Self {
        App {
            theme,
            parser: TranscriptParser::new(),
            chrome: Chrome::default(),
            finalized: Vec::new(),
            fold: FoldState::new(),
            selected: None,
            palette: None,
            viewer: None,
            find_query: None,
            last_click: None,
            scroll: Scroll::new(),
            viewport_h: 0,
            scroll_rect: Rect::default(),
            layout: Vec::new(),
            cache: HashMap::new(),
            cache_width: 0,
            cache_theme: theme.kind,
        }
    }

    pub(super) fn push_raw(&mut self, raw: &str) {
        let Ok(row) = serde_json::from_str::<Value>(raw) else {
            return;
        };
        self.chrome.update(&row);
        self.finalized.extend(self.parser.push_row(&row));
    }

    /// Assemble the scrollback: cached finalized entries (re-rendered when
    /// their width/fold/selection state changes) + freshly rendered pending,
    /// recording each entry's line range for selection and hit tests.
    pub(super) fn scrollback_lines(&mut self, inner_w: usize) -> Vec<Line<'static>> {
        if self.cache_width != inner_w || self.cache_theme != self.theme.kind {
            self.cache.clear();
            self.cache_width = inner_w;
            self.cache_theme = self.theme.kind;
        }
        let pending = self.parser.pending_entries();
        let n_final = self.finalized.len();
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut layout: Vec<LayoutEntry> = Vec::new();
        let mut last_dense = false;
        for (i, entry) in self.finalized.iter().chain(pending.iter()).enumerate() {
            let kind = fold_kind(&entry.block);
            let expanded = self.fold.expanded(entry.id, kind);
            let selected = self.selected == Some(entry.id);
            let dense = matches!(
                entry.block,
                DisplayBlock::ToolGroup(_)
                    | DisplayBlock::Run(_)
                    | DisplayBlock::Tool(_)
                    | DisplayBlock::Thinking(_)
            );
            if !lines.is_empty() && (!dense || !last_dense) {
                lines.push(Line::default());
            }
            last_dense = dense;
            let (entry_lines, foldable) = if i < n_final {
                match self.cache.get(&entry.id) {
                    Some(c) if c.expanded == expanded && c.selected == selected => {
                        (c.lines.clone(), c.foldable)
                    }
                    _ => {
                        let r = render_entry(self.theme, &entry.block, inner_w, expanded, selected);
                        self.cache.insert(
                            entry.id,
                            CachedEntry {
                                expanded,
                                selected,
                                foldable: r.foldable,
                                lines: r.lines.clone(),
                            },
                        );
                        (r.lines, r.foldable)
                    }
                }
            } else {
                let r = render_entry(self.theme, &entry.block, inner_w, expanded, selected);
                (r.lines, r.foldable)
            };
            layout.push(LayoutEntry {
                id: entry.id,
                start: lines.len(),
                height: entry_lines.len(),
                selectable: !matches!(entry.block, DisplayBlock::WorkedFor(_)),
                is_turn: entry.block.starts_turn(),
                foldable,
                kind,
            });
            lines.extend(entry_lines);
        }
        // The parser only emits WorkedFor when the NEXT user message closes
        // the turn; a live mirror wants the line as soon as the turn settles.
        if let Some(secs) = self.parser.open_turn_worked_secs() {
            let synth = DisplayBlock::WorkedFor(crate::transcript_view::WorkedForBlock {
                duration_secs: Some(secs),
            });
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let r = render_entry(self.theme, &synth, inner_w, false, false);
            lines.extend(r.lines);
        }
        self.layout = layout;
        lines
    }

    fn infos(&self) -> Vec<EntryInfo> {
        self.layout
            .iter()
            .map(|le| EntryInfo {
                id: le.id,
                selectable: le.selectable,
                is_turn: le.is_turn,
            })
            .collect()
    }

    pub(super) fn layout_of(&self, id: u64) -> Option<LayoutEntry> {
        self.layout.iter().copied().find(|le| le.id == id)
    }

    fn scroll_to_selected(&mut self) {
        let Some(le) = self.selected.and_then(|id| self.layout_of(id)) else {
            return;
        };
        if let Some(offset) =
            interact::scroll_into_view(self.scroll.offset, self.viewport_h, le.start, le.height)
        {
            self.scroll.offset = offset.min(self.scroll.max);
            self.scroll.follow = false;
        }
    }

    fn move_selection(&mut self, forward: bool) {
        let infos = self.infos();
        let mv = if forward {
            interact::select_next(&infos, self.selected)
        } else {
            interact::select_prev(&infos, self.selected)
        };
        match mv {
            SelectMove::To(id) => {
                self.selected = Some(id);
                self.scroll_to_selected();
            }
            SelectMove::Overscroll => self.scroll.goto_bottom(),
            SelectMove::Stay => {}
        }
    }

    /// Shift+Left/Right: select the turn's prompt and snap it to the top.
    fn jump_turn(&mut self, forward: bool) {
        let infos = self.infos();
        let target = if forward {
            interact::next_turn(&infos, self.selected)
        } else {
            interact::prev_turn(&infos, self.selected)
        };
        let Some(id) = target else { return };
        self.selected = Some(id);
        if let Some(le) = self.layout_of(id) {
            self.scroll.offset = le.start.min(self.scroll.max);
            self.scroll.follow = false;
        }
    }

    fn fold_selected(&mut self, expanded: bool) {
        let Some(le) = self.selected.and_then(|id| self.layout_of(id)) else {
            return;
        };
        if le.foldable {
            self.fold.set(le.id, le.kind, expanded);
            self.scroll_to_selected();
        }
    }

    fn toggle_selected_fold(&mut self) {
        let Some(le) = self.selected.and_then(|id| self.layout_of(id)) else {
            return;
        };
        if le.foldable {
            self.fold.toggle(le.id, le.kind);
        }
    }

    fn toggle_all_thinking(&mut self) {
        let ids: Vec<u64> = self
            .layout
            .iter()
            .filter(|le| le.kind == FoldKind::Thinking)
            .map(|le| le.id)
            .collect();
        self.fold.toggle_all_thinking(&ids);
    }

    fn block_of(&self, id: u64) -> Option<DisplayBlock> {
        self.finalized
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.block.clone())
            .or_else(|| {
                self.parser
                    .pending_entries()
                    .into_iter()
                    .find(|e| e.id == id)
                    .map(|e| e.block)
            })
    }

    fn open_viewer(&mut self) {
        let Some(block) = self.selected.and_then(|id| self.block_of(id)) else {
            return;
        };
        self.viewer = Some(Viewer::new(block));
    }

    /// `/theme`: switch live, then persist through the settings store the
    /// startup resolution reads (`view.theme`). `auto` re-detects from the
    /// env stamps only — the OSC 11 probe needs the raw tty, which crossterm
    /// owns mid-session.
    fn apply_theme(&mut self, pref: Option<ThemePref>) {
        use crate::view_theme::{parse_appearance_var, parse_colorfgbg, resolve_kind};
        let (kind, persist) = match pref {
            None => match self.theme.kind {
                ThemeKind::Dark => (ThemeKind::Light, "light"),
                ThemeKind::Light => (ThemeKind::Dark, "dark"),
            },
            Some(ThemePref::Light) => (ThemeKind::Light, "light"),
            Some(ThemePref::Dark) => (ThemeKind::Dark, "dark"),
            Some(ThemePref::Auto) => {
                let detected =
                    parse_appearance_var(std::env::var("HIVE_APPEARANCE").ok().as_deref())
                        .or_else(|| parse_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()));
                (resolve_kind(ThemePref::Auto, detected), "auto")
            }
        };
        self.theme = kind.theme();
        if let Some(v) = &mut self.viewer {
            v.invalidate();
        }
        let _ = crate::settings::set_setting("view.theme", serde_json::json!(persist));
    }

    fn run_find(&mut self, forward: bool) {
        let Some(query) = self.find_query.clone() else {
            return;
        };
        let pending = self.parser.pending_entries();
        let list: Vec<(u64, String)> = self
            .finalized
            .iter()
            .chain(pending.iter())
            .filter(|e| !matches!(e.block, DisplayBlock::WorkedFor(_)))
            .map(|e| (e.id, search_text(&e.block)))
            .collect();
        if let Some(id) = interact::find_match(&list, self.selected, &query, forward) {
            self.selected = Some(id);
            self.scroll_to_selected();
        }
    }

    // ---- key/mouse routing ---------------------------------------------

    /// Returns true when the app should quit.
    pub(super) fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('c') | KeyCode::Char('q'))
        {
            return true;
        }
        if self.viewer.is_some() {
            self.viewer_key(code, mods);
            return false;
        }
        if self.palette.is_some() {
            return self.palette_key(code, mods);
        }
        self.main_key(code, mods)
    }

    fn main_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('f') => self.open_viewer(),
                KeyCode::Char('e') => self.toggle_all_thinking(),
                KeyCode::Char('o') => self.fold.cycle_density(),
                _ => handle_scroll_key(&mut self.scroll, self.viewport_h, code, mods),
            }
            return false;
        }
        if mods.contains(KeyModifiers::SHIFT) && matches!(code, KeyCode::Left | KeyCode::Right) {
            self.jump_turn(code == KeyCode::Right);
            return false;
        }
        match code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('/') => self.palette = Some(Palette::open()),
            KeyCode::Up => self.move_selection(false),
            KeyCode::Down => self.move_selection(true),
            KeyCode::Left => self.fold_selected(false),
            KeyCode::Right => self.fold_selected(true),
            KeyCode::Enter => self.open_viewer(),
            KeyCode::Char('n') => self.run_find(true),
            KeyCode::Char('N') => self.run_find(false),
            _ => handle_scroll_key(&mut self.scroll, self.viewport_h, code, mods),
        }
        false
    }

    fn viewer_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        // Enter closes as well as opens — the key you reached for to get in
        // is the one you reach for to get out.
        let closes = matches!(code, KeyCode::Esc | KeyCode::Enter)
            || (!ctrl && code == KeyCode::Char('q'))
            || (ctrl && code == KeyCode::Char('f'));
        if closes {
            self.viewer = None;
            return;
        }
        let Some(v) = &mut self.viewer else { return };
        let page = page_rows(v.view_h);
        let half = half_page_rows(v.view_h);
        if ctrl {
            match code {
                KeyCode::Char('j') => v.scroll += 1,
                KeyCode::Char('k') => v.scroll = v.scroll.saturating_sub(1),
                KeyCode::Char('d') => v.scroll += half,
                KeyCode::Char('u') => v.scroll = v.scroll.saturating_sub(half),
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => v.scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => v.scroll = v.scroll.saturating_sub(1),
            KeyCode::PageDown => v.scroll += page,
            KeyCode::PageUp => v.scroll = v.scroll.saturating_sub(page),
            KeyCode::Char('g') => v.scroll = 0,
            // Past any real length: draw_viewer clamps to the last page each
            // frame, and the halved MAX leaves headroom for a `+= page` from
            // the same event batch, handled before that draw.
            KeyCode::Char('G') => v.scroll = usize::MAX / 2,
            _ => {}
        }
    }

    fn palette_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        match code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Enter => {
                let action = self.palette.as_ref().map(Palette::enter);
                match action {
                    Some(PaletteAction::SwitchTheme(pref)) => {
                        self.palette = None;
                        self.apply_theme(pref);
                    }
                    Some(PaletteAction::SetDensity(density)) => {
                        self.palette = None;
                        self.fold.set_density(density);
                    }
                    Some(PaletteAction::Find(query)) => {
                        self.palette = None;
                        self.find_query = Some(query);
                        self.run_find(true);
                    }
                    Some(PaletteAction::Quit) => return true,
                    Some(PaletteAction::Complete(name)) => {
                        if let Some(p) = &mut self.palette {
                            p.input = format!("{name} ");
                        }
                    }
                    Some(PaletteAction::Noop) | None => {}
                }
            }
            KeyCode::Backspace => {
                if let Some(p) = &mut self.palette {
                    if !p.backspace() {
                        self.palette = None;
                    }
                }
            }
            KeyCode::Up => {
                if let Some(p) = &mut self.palette {
                    p.move_up();
                }
            }
            KeyCode::Down => {
                if let Some(p) = &mut self.palette {
                    p.move_down();
                }
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                if let Some(p) = &mut self.palette {
                    p.insert(c);
                }
            }
            _ => {}
        }
        false
    }

    pub(super) fn on_mouse(&mut self, kind: MouseEventKind, x: u16, y: u16, now: Instant) {
        match kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                if let Some(v) = &mut self.viewer {
                    if kind == MouseEventKind::ScrollDown {
                        v.scroll += WHEEL_LINES;
                    } else {
                        v.scroll = v.scroll.saturating_sub(WHEEL_LINES);
                    }
                } else {
                    handle_mouse(&mut self.scroll, kind);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => self.on_click(x, y, now),
            _ => {}
        }
    }

    /// Single click selects the entry under the cursor; a second click on the
    /// same entry within the multi-click window toggles its fold.
    fn on_click(&mut self, x: u16, y: u16, now: Instant) {
        if self.viewer.is_some() || self.palette.is_some() {
            return;
        }
        let r = self.scroll_rect;
        if x < r.x || x >= r.x + r.width || y < r.y || y >= r.y + r.height {
            return;
        }
        let line_idx = self.scroll.offset + (y - r.y) as usize;
        let Some(le) =
            self.layout.iter().copied().find(|le| {
                le.selectable && line_idx >= le.start && line_idx < le.start + le.height
            })
        else {
            return;
        };
        let double = self
            .last_click
            .is_some_and(|(t0, id)| id == le.id && now.duration_since(t0) <= MULTI_CLICK);
        self.selected = Some(le.id);
        if double {
            self.toggle_selected_fold();
            self.last_click = None;
        } else {
            self.last_click = Some((now, le.id));
        }
    }
}
