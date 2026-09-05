//! Adaptive tmux layout for hive teams.
//!
//! Picks a preset from the window's aspect ratio (tmux cell ≈ 1:2 pixel,
//! so char-width >= 2*char-height ≈ landscape pixels) and current pane count.
//!
//! A window with a mirror rail (`@hive-role mirror`) or a dock pane
//! (`@hive-role dock`) gets a generated layout string instead of a preset:
//! rails stack in a `RAIL_COLS` column on the left, the dock stays a strip
//! at the bottom of the remaining column, members tile in between.

pub const LANDSCAPE_PRESET: &str = "main-vertical";
pub const PORTRAIT_PRESET: &str = "even-vertical";
pub const MAIN_PANE_FRACTION: &str = "50%";
// From this many panes, a main pane plus a single strip squeezes members
// into slivers — tile instead.
pub const TILED_THRESHOLD: usize = 5;
/// Rows the dock strip asks for; a short window gives it a third.
pub const DOCK_ROWS: i64 = 14;
pub const DOCK_PRESET: &str = "dock";
/// Columns of a mirror rail (`@hive-role mirror`): a `hive view` pane folded
/// to its status column. `transcript_tui::RAIL_MAX_WIDTH` must stay >= this
/// or the viewer would draw a transcript into the rail.
pub const RAIL_COLS: i64 = 14;
/// Width a clicked rail opens to (`resize-pane -x`); a second click folds it
/// back to `RAIL_COLS`.
pub const RAIL_OPEN_WIDTH: &str = "45%";
pub const RAIL_PRESET: &str = "rail";

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutChoice {
    pub orientation: &'static str,
    pub preset: &'static str,
    pub options: Vec<(&'static str, &'static str)>,
}

fn _is_landscape(width: i64, height: i64) -> bool {
    if width <= 0 || height <= 0 {
        return true;
    }
    width >= 2 * height
}

/// Return the layout choice for this window, or None when no apply should happen.
pub fn pick(window_size: (i64, i64), pane_count: usize) -> Option<LayoutChoice> {
    if pane_count < 2 {
        return None;
    }
    let (w, h) = window_size;
    let orientation = if _is_landscape(w, h) {
        "horizontal"
    } else {
        "vertical"
    };
    if pane_count >= TILED_THRESHOLD {
        return Some(LayoutChoice {
            orientation,
            preset: "tiled",
            options: Vec::new(),
        });
    }
    if orientation == "horizontal" {
        return Some(LayoutChoice {
            orientation: "horizontal",
            preset: LANDSCAPE_PRESET,
            options: vec![("main-pane-width", MAIN_PANE_FRACTION)],
        });
    }
    Some(LayoutChoice {
        orientation: "vertical",
        preset: PORTRAIT_PRESET,
        options: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// dock layout: tmux layout-string grammar
//
//   <csum>,<WxH>,<x>,<y>{a,b}   side by side     <…>[a,b]   stacked
//   leaf = WxH,x,y,<pane index>  (index = the pane id without '%')
//
// Consistency tmux checks: children of `{}` share the parent's height and
// their widths plus one separator each sum to the parent's width; `[]` the
// same with the axes swapped. The pane indices in the string are not
// honoured: tmux hands cells to panes in window order, so the rail cells —
// first in the string — land on whichever panes come first and the dock
// cell — last — on whichever pane is last; the apply swaps them there
// first.
// ---------------------------------------------------------------------------

/// tmux's `layout_checksum`.
pub fn layout_checksum(body: &str) -> u16 {
    let mut csum: u16 = 0;
    for b in body.bytes() {
        csum = (csum >> 1).wrapping_add((csum & 1) << 15);
        csum = csum.wrapping_add(u16::from(b));
    }
    csum
}

fn pane_index(pane_id: &str) -> &str {
    pane_id.trim_start_matches('%')
}

/// Members per row: landscape windows grid toward square, portrait ones
/// stack. The last row takes the remainder.
fn grid_rows(n: usize, landscape: bool) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let cols = if landscape {
        (n as f64).sqrt().ceil() as usize
    } else {
        1
    };
    let rows = n.div_ceil(cols);
    let mut out = vec![cols; rows];
    out[rows - 1] = n - cols * (rows - 1);
    out
}

/// Row cells for `members` tiled over the box at (x, y) of size (w, h):
/// one string per grid row — a leaf for a lone member, `{…}` for a row of
/// several — with absolute coordinates.
fn tile_rows(x: i64, y: i64, w: i64, h: i64, members: &[String], landscape: bool) -> Vec<String> {
    let rows = grid_rows(members.len(), landscape);
    let n_rows = rows.len() as i64;
    if n_rows == 0 {
        return Vec::new();
    }
    let base_h = (h - (n_rows - 1)) / n_rows;
    let mut cells: Vec<String> = Vec::new();
    let mut cy = y;
    let mut idx = 0;
    for (r, &count) in rows.iter().enumerate() {
        let row_h = if r + 1 == rows.len() {
            y + h - cy
        } else {
            base_h
        };
        if count == 1 {
            cells.push(format!(
                "{w}x{row_h},{x},{cy},{}",
                pane_index(&members[idx])
            ));
        } else {
            let base_w = (w - (count as i64 - 1)) / count as i64;
            let mut cx = x;
            let mut parts = Vec::new();
            for c in 0..count {
                let cw = if c + 1 == count { x + w - cx } else { base_w };
                parts.push(format!(
                    "{cw}x{row_h},{cx},{cy},{}",
                    pane_index(&members[idx + c])
                ));
                cx += cw + 1;
            }
            cells.push(format!("{w}x{row_h},{x},{cy}{{{}}}", parts.join(",")));
        }
        idx += count;
        cy += row_h + 1;
    }
    cells
}

/// The column right of the rails (the whole window without rails): members
/// tiled above an optional dock strip. A lone row is returned bare (a leaf
/// or a `{…}` row spanning the column); several rows, or a dock, wrap in
/// `[…]`. Members empty + dock: the dock leaf takes the column.
fn column_body(x: i64, w: i64, h: i64, members: &[String], dock: Option<&str>) -> String {
    let landscape = _is_landscape(w, h);
    match dock {
        Some(dock) => {
            if members.is_empty() {
                return format!("{w}x{h},{x},0,{}", pane_index(dock));
            }
            let dock_h = DOCK_ROWS.min(h / 3).max(2);
            let members_h = h - dock_h - 1;
            let rows = tile_rows(x, 0, w, members_h, members, landscape);
            format!(
                "{w}x{h},{x},0[{},{w}x{dock_h},{x},{},{}]",
                rows.join(","),
                members_h + 1,
                pane_index(dock)
            )
        }
        None => {
            let rows = tile_rows(x, 0, w, h, members, landscape);
            if rows.len() == 1 {
                return rows[0].clone();
            }
            format!("{w}x{h},{x},0[{}]", rows.join(","))
        }
    }
}

/// Layout string for a window with rails and/or a dock; None when neither
/// is present (presets apply), when there is nothing to tile beside the
/// rails, or when the window is too small.
pub fn window_layout(
    size: (i64, i64),
    rails: &[String],
    members: &[String],
    dock: Option<&str>,
) -> Option<String> {
    let (w, h) = size;
    if rails.is_empty() && dock.is_none() {
        return None;
    }
    if members.is_empty() && (dock.is_none() || rails.is_empty()) {
        return None;
    }
    if w < 4 || h < 6 || (!rails.is_empty() && w < RAIL_COLS + 6) {
        return None;
    }
    let body = if rails.is_empty() {
        column_body(0, w, h, members, dock)
    } else {
        let left = if rails.len() == 1 {
            format!("{RAIL_COLS}x{h},0,0,{}", pane_index(&rails[0]))
        } else {
            let n = rails.len() as i64;
            let base = (h - (n - 1)) / n;
            let mut cells = Vec::new();
            let mut cy = 0;
            for (i, rail) in rails.iter().enumerate() {
                let rh = if i + 1 == rails.len() { h - cy } else { base };
                cells.push(format!("{RAIL_COLS}x{rh},0,{cy},{}", pane_index(rail)));
                cy += rh + 1;
            }
            format!("{RAIL_COLS}x{h},0,0[{}]", cells.join(","))
        };
        let right = column_body(RAIL_COLS + 1, w - RAIL_COLS - 1, h, members, dock);
        format!("{w}x{h},0,0{{{left},{right}}}")
    };
    Some(format!("{:04x},{body}", layout_checksum(&body)))
}

// Seam so unit tests can record tmux calls without a tmux server.
trait TmuxOps {
    fn window_zoomed(&mut self, target: &str) -> bool;
    fn window_size(&mut self, target: &str) -> (i64, i64);
    fn list_panes(&mut self, target: &str) -> Vec<String>;
    /// `(pane_id, @hive-role)` in window order.
    fn pane_roles(&mut self, target: &str) -> Vec<(String, String)>;
    fn swap_pane(&mut self, src: &str, dst: &str);
    fn set_window_option(&mut self, target: &str, option: &str, value: &str);
    fn select_layout(&mut self, target: &str, layout: &str);
}

struct RealTmux;

impl TmuxOps for RealTmux {
    fn window_zoomed(&mut self, target: &str) -> bool {
        crate::tmux::window_zoomed(target)
    }
    fn window_size(&mut self, target: &str) -> (i64, i64) {
        let (w, h) = crate::tmux::window_size(target);
        (w as i64, h as i64)
    }
    fn list_panes(&mut self, target: &str) -> Vec<String> {
        crate::tmux::list_panes(target)
    }
    fn pane_roles(&mut self, target: &str) -> Vec<(String, String)> {
        crate::tmux::list_panes_full(target)
            .into_iter()
            .map(|p| (p.pane_id, p.role))
            .collect()
    }
    fn swap_pane(&mut self, src: &str, dst: &str) {
        crate::tmux::swap_pane(src, dst)
    }
    fn set_window_option(&mut self, target: &str, option: &str, value: &str) {
        crate::tmux::set_window_option(target, option, value)
    }
    fn select_layout(&mut self, target: &str, layout: &str) {
        crate::tmux::select_layout(target, layout)
    }
}

/// Cross-process lock for one window's apply. Two appliers racing (a
/// board starting while the rig splits its mirror, two `hive flow node
/// run` spawns landing together) would each see the dock out of place and
/// both swap it — a double swap puts it back where it was.
struct WindowLock(std::fs::File);

impl Drop for WindowLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn window_lock(window_target: &str) -> Option<WindowLock> {
    use std::os::unix::io::AsRawFd;
    let dir = crate::team::hive_home().join("state").join("locks");
    std::fs::create_dir_all(&dir).ok()?;
    let key: String = window_target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(format!("layout-{key}.lock")))
        .ok()?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return None;
    }
    Some(WindowLock(file))
}

/// Read window size + pane count from tmux, apply the matching preset.
pub fn apply_adaptive(window_target: &str) -> Option<LayoutChoice> {
    let _lock = window_lock(window_target);
    apply_adaptive_with(window_target, &mut RealTmux)
}

fn apply_adaptive_with(window_target: &str, tmux: &mut dyn TmuxOps) -> Option<LayoutChoice> {
    if window_target.is_empty() {
        return None;
    }
    if tmux.window_zoomed(window_target) {
        // The human zoomed in on a member: a re-tile would both unzoom and
        // rearrange under them. Skip; the next unzoomed apply catches up.
        return None;
    }
    let size = tmux.window_size(window_target);
    let panes = tmux.list_panes(window_target);
    let pane_count = panes.len();
    let roles = tmux.pane_roles(window_target);
    let rails: Vec<String> = roles
        .iter()
        .filter(|(_, role)| role == "mirror")
        .map(|(id, _)| id.clone())
        .collect();
    let dock: Option<String> = roles
        .iter()
        .find(|(_, role)| role == "dock")
        .map(|(id, _)| id.clone());
    if !rails.is_empty() || dock.is_some() {
        // Cells apply in window order: the rails must be the first panes
        // and the dock the last. Re-read after each swap rather than trust
        // it: the layout string must describe what tmux has now.
        let mut order = panes;
        for (i, rail) in rails.iter().enumerate() {
            if order.get(i) != Some(rail) {
                if let Some(at) = order.get(i) {
                    tmux.swap_pane(rail, at);
                    order = tmux.list_panes(window_target);
                }
            }
        }
        if let Some(dock) = dock.as_deref() {
            if let (Some(at), Some(last)) = (
                order.iter().position(|p| p == dock),
                order.len().checked_sub(1),
            ) {
                if at != last {
                    tmux.swap_pane(dock, &order[last]);
                    order = tmux.list_panes(window_target);
                }
            }
        }
        let members: Vec<String> = order
            .into_iter()
            .filter(|p| !rails.contains(p) && dock.as_deref() != Some(p.as_str()))
            .collect();
        // A window too small for the rail column (or with nothing beside
        // it) is tiled by a preset instead of not at all.
        if let Some(layout) = window_layout(size, &rails, &members, dock.as_deref()) {
            tmux.select_layout(window_target, &layout);
            return Some(LayoutChoice {
                orientation: if _is_landscape(size.0, size.1) {
                    "horizontal"
                } else {
                    "vertical"
                },
                preset: if rails.is_empty() {
                    DOCK_PRESET
                } else {
                    RAIL_PRESET
                },
                options: Vec::new(),
            });
        }
    }
    let choice = pick(size, pane_count)?;
    for (key, value) in &choice.options {
        tmux.set_window_option(window_target, key, value);
    }
    tmux.select_layout(window_target, choice.preset);
    Some(choice)
}

/// Pick pre-spawn tmux split direction to match the final adaptive layout.
///
/// Keeps the visible spawn geometry consistent with the post-spawn rebalance
/// so portrait windows don't show a squeezed left-right split while the new
/// CLI boots. Falls back to `true` (horizontal / `-h`) when window size
/// is unknown, matching the legacy default.
pub fn split_horizontal(window_target: &str, pane_count_after: usize) -> bool {
    if window_target.is_empty() {
        return true;
    }
    let (w, h) = crate::tmux::window_size(window_target);
    match pick((w as i64, h as i64), pane_count_after) {
        None => true,
        Some(choice) => choice.orientation == "horizontal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTmux {
        zoomed: bool,
        roles: Vec<(String, String)>,
        size: (i64, i64),
        panes: Vec<String>,
        calls: Vec<(String, String, String, String)>,
    }

    impl FakeTmux {
        fn panes(mut self, n: usize) -> Self {
            self.panes = (1..=n).map(|i| format!("%{i}")).collect();
            self
        }
    }

    impl TmuxOps for FakeTmux {
        fn window_zoomed(&mut self, _target: &str) -> bool {
            self.zoomed
        }
        fn window_size(&mut self, _target: &str) -> (i64, i64) {
            self.size
        }
        fn list_panes(&mut self, _target: &str) -> Vec<String> {
            self.panes.clone()
        }
        fn pane_roles(&mut self, _target: &str) -> Vec<(String, String)> {
            self.panes
                .iter()
                .map(|p| {
                    let role = self
                        .roles
                        .iter()
                        .find(|(id, _)| id == p)
                        .map(|(_, r)| r.clone())
                        .unwrap_or_default();
                    (p.clone(), role)
                })
                .collect()
        }
        fn swap_pane(&mut self, src: &str, dst: &str) {
            self.calls.push((
                "swap".to_string(),
                src.to_string(),
                dst.to_string(),
                String::new(),
            ));
            let a = self.panes.iter().position(|p| p == src);
            let b = self.panes.iter().position(|p| p == dst);
            if let (Some(a), Some(b)) = (a, b) {
                self.panes.swap(a, b);
            }
        }
        fn set_window_option(&mut self, target: &str, option: &str, value: &str) {
            self.calls.push((
                "opt".to_string(),
                target.to_string(),
                option.to_string(),
                value.to_string(),
            ));
        }
        fn select_layout(&mut self, target: &str, layout: &str) {
            self.calls.push((
                "layout".to_string(),
                target.to_string(),
                layout.to_string(),
                String::new(),
            ));
        }
    }

    #[test]
    fn test_pick_portrait_two_panes() {
        let choice = pick((191, 171), 2).unwrap();
        assert_eq!(choice.orientation, "vertical");
        assert_eq!(choice.preset, "even-vertical");
        assert!(choice.options.is_empty());
    }

    #[test]
    fn test_pick_portrait_three_panes() {
        let choice = pick((100, 100), 3).unwrap();
        assert_eq!(choice.orientation, "vertical");
        assert_eq!(choice.preset, "even-vertical");
    }

    #[test]
    fn test_pick_landscape_two_panes() {
        let choice = pick((200, 50), 2).unwrap();
        assert_eq!(choice.orientation, "horizontal");
        assert_eq!(choice.preset, "main-vertical");
        assert_eq!(choice.options, vec![("main-pane-width", "50%")]);
    }

    #[test]
    fn test_pick_landscape_exactly_two_x_threshold() {
        let choice = pick((200, 100), 2).unwrap();
        assert_eq!(choice.orientation, "horizontal");
    }

    #[test]
    fn test_pick_just_below_landscape_threshold() {
        let choice = pick((199, 100), 2).unwrap();
        assert_eq!(choice.orientation, "vertical");
    }

    #[test]
    fn test_pick_single_pane_returns_none() {
        assert_eq!(pick((200, 50), 1), None);
        assert_eq!(pick((100, 100), 1), None);
    }

    #[test]
    fn test_pick_zero_panes_returns_none() {
        assert_eq!(pick((200, 50), 0), None);
    }

    #[test]
    fn test_pick_unknown_window_size_falls_back_to_landscape() {
        let choice = pick((0, 0), 2).unwrap();
        assert_eq!(choice.orientation, "horizontal");
        assert_eq!(choice.preset, "main-vertical");
    }

    #[test]
    fn test_apply_adaptive_empty_window_target_is_noop() {
        let mut tmux = FakeTmux::default();
        assert_eq!(apply_adaptive_with("", &mut tmux), None);
        assert!(tmux.calls.is_empty());
    }

    #[test]
    fn test_apply_adaptive_portrait_applies_even_vertical() {
        let mut tmux = FakeTmux {
            size: (191, 171),
            ..FakeTmux::default()
        }
        .panes(2);
        let result = apply_adaptive_with("dev:0", &mut tmux).unwrap();
        assert_eq!(result.preset, "even-vertical");
        assert_eq!(
            tmux.calls,
            vec![(
                "layout".to_string(),
                "dev:0".to_string(),
                "even-vertical".to_string(),
                String::new(),
            )]
        );
    }

    #[test]
    fn test_apply_adaptive_landscape_sets_main_pane_width_before_select() {
        let mut tmux = FakeTmux {
            size: (200, 50),
            ..FakeTmux::default()
        }
        .panes(3);
        let result = apply_adaptive_with("dev:0", &mut tmux).unwrap();
        assert_eq!(result.preset, "main-vertical");
        assert_eq!(
            tmux.calls,
            vec![
                (
                    "opt".to_string(),
                    "dev:0".to_string(),
                    "main-pane-width".to_string(),
                    "50%".to_string(),
                ),
                (
                    "layout".to_string(),
                    "dev:0".to_string(),
                    "main-vertical".to_string(),
                    String::new(),
                ),
            ]
        );
    }

    #[test]
    fn test_apply_adaptive_single_pane_skips_select_layout() {
        let mut tmux = FakeTmux {
            size: (200, 50),
            ..FakeTmux::default()
        }
        .panes(1);
        assert_eq!(apply_adaptive_with("dev:0", &mut tmux), None);
        assert!(tmux.calls.is_empty());
    }

    #[test]
    fn test_pick_five_panes_tiles_regardless_of_orientation() {
        let landscape = pick((200, 50), 5).unwrap();
        assert_eq!(landscape.preset, "tiled");
        assert_eq!(landscape.orientation, "horizontal");
        let portrait = pick((100, 100), 6).unwrap();
        assert_eq!(portrait.preset, "tiled");
        assert_eq!(portrait.orientation, "vertical");
    }

    #[test]
    fn test_pick_four_panes_keeps_main_layout() {
        let choice = pick((200, 50), 4).unwrap();
        assert_eq!(choice.preset, "main-vertical");
    }

    #[test]
    fn test_apply_adaptive_skips_while_zoomed() {
        let mut tmux = FakeTmux {
            zoomed: true,
            size: (200, 50),
            ..FakeTmux::default()
        }
        .panes(2);
        assert_eq!(apply_adaptive_with("dev:0", &mut tmux), None);
        // a zoomed window is never re-tiled under the human
        assert!(tmux.calls.is_empty());
    }

    #[test]
    fn test_layout_checksum_matches_tmux() {
        // `#{window_layout}` of a fresh 80x24 window, and after one -v split
        assert_eq!(format!("{:04x}", layout_checksum("80x24,0,0,0")), "b25d");
        assert_eq!(
            format!(
                "{:04x}",
                layout_checksum("80x24,0,0[80x12,0,0,0,80x11,0,13,1]")
            ),
            "c195"
        );
    }

    #[test]
    fn test_grid_rows_landscape_squares_and_portrait_stacks() {
        assert_eq!(grid_rows(1, true), vec![1]);
        assert_eq!(grid_rows(2, true), vec![2]);
        assert_eq!(grid_rows(3, true), vec![2, 1]);
        assert_eq!(grid_rows(5, true), vec![3, 2]);
        assert_eq!(grid_rows(3, false), vec![1, 1, 1]);
        assert_eq!(grid_rows(0, true), Vec::<usize>::new());
    }

    #[test]
    fn test_dock_layout_two_members_landscape() {
        let members = vec!["%3".to_string(), "%5".to_string()];
        let layout = window_layout((200, 50), &[], &members, Some("%1")).unwrap();
        // members: 50 - 14 - 1 = 35 rows; widths 99 + 1 + 100 = 200
        let body = "200x50,0,0[200x35,0,0{99x35,0,0,3,100x35,100,0,5},200x14,0,36,1]";
        assert_eq!(layout, format!("{:04x},{body}", layout_checksum(body)));
    }

    #[test]
    fn test_dock_layout_three_members_landscape_wraps_last_row() {
        let members = vec!["%3".to_string(), "%5".to_string(), "%7".to_string()];
        let layout = window_layout((200, 50), &[], &members, Some("%1")).unwrap();
        // two rows over 35 lines: 17 + 1 + 17; the lone third pane spans the row
        let body = "200x50,0,0[200x17,0,0{99x17,0,0,3,100x17,100,0,5},200x17,0,18,7,200x14,0,36,1]";
        assert_eq!(layout, format!("{:04x},{body}", layout_checksum(body)));
    }

    #[test]
    fn test_dock_layout_portrait_stacks_and_short_window_shrinks_dock() {
        let members = vec!["%3".to_string(), "%5".to_string()];
        // 100x60 is portrait: stacked rows over 45 lines = 22 + 1 + 22
        let body = "100x60,0,0[100x22,0,0,3,100x22,0,23,5,100x14,0,46,1]";
        assert_eq!(
            window_layout((100, 60), &[], &members, Some("%1")).unwrap(),
            format!("{:04x},{body}", layout_checksum(body))
        );
        // 100x30 is landscape and short: the dock takes a third (10 rows)
        let body = "100x30,0,0[100x19,0,0{49x19,0,0,3,50x19,50,0,5},100x10,0,20,1]";
        assert_eq!(
            window_layout((100, 30), &[], &members, Some("%1")).unwrap(),
            format!("{:04x},{body}", layout_checksum(body))
        );
        assert_eq!(window_layout((200, 50), &[], &[], Some("%1")), None);
    }

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn checksummed(body: &str) -> String {
        format!("{:04x},{body}", layout_checksum(body))
    }

    #[test]
    fn test_window_layout_rail_and_one_member() {
        let body = "200x50,0,0{14x50,0,0,1,185x50,15,0,3}";
        assert_eq!(
            window_layout((200, 50), &ids(&["%1"]), &ids(&["%3"]), None).unwrap(),
            checksummed(body)
        );
    }

    #[test]
    fn test_window_layout_rail_and_two_members() {
        let body = "200x50,0,0{14x50,0,0,1,185x50,15,0{92x50,15,0,3,92x50,108,0,5}}";
        assert_eq!(
            window_layout((200, 50), &ids(&["%1"]), &ids(&["%3", "%5"]), None).unwrap(),
            checksummed(body)
        );
    }

    #[test]
    fn test_window_layout_rail_and_three_members_wraps_the_last_row() {
        let body = "200x50,0,0{14x50,0,0,1,185x50,15,0[185x24,15,0{92x24,15,0,3,92x24,108,0,5},185x25,15,25,7]}";
        assert_eq!(
            window_layout((200, 50), &ids(&["%1"]), &ids(&["%3", "%5", "%7"]), None).unwrap(),
            checksummed(body)
        );
    }

    #[test]
    fn test_window_layout_rail_members_and_dock() {
        let body = "200x50,0,0{14x50,0,0,1,185x50,15,0[185x35,15,0{92x35,15,0,3,92x35,108,0,5},185x14,15,36,9]}";
        assert_eq!(
            window_layout((200, 50), &ids(&["%1"]), &ids(&["%3", "%5"]), Some("%9")).unwrap(),
            checksummed(body)
        );
    }

    #[test]
    fn test_window_layout_rail_and_dock_without_members() {
        let body = "200x50,0,0{14x50,0,0,1,185x50,15,0,9}";
        assert_eq!(
            window_layout((200, 50), &ids(&["%1"]), &[], Some("%9")).unwrap(),
            checksummed(body)
        );
    }

    #[test]
    fn test_window_layout_two_rails_stack_on_the_left() {
        let body = "200x50,0,0{14x50,0,0[14x24,0,0,1,14x25,0,25,2],185x50,15,0,3}";
        assert_eq!(
            window_layout((200, 50), &ids(&["%1", "%2"]), &ids(&["%3"]), None).unwrap(),
            checksummed(body)
        );
    }

    #[test]
    fn test_window_layout_portrait_column_stacks_the_members() {
        // the column beside the rail is 85 wide: portrait, so rows stack
        let body = "100x60,0,0{14x60,0,0,1,85x60,15,0[85x29,15,0,3,85x30,15,30,5]}";
        assert_eq!(
            window_layout((100, 60), &ids(&["%1"]), &ids(&["%3", "%5"]), None).unwrap(),
            checksummed(body)
        );
    }

    #[test]
    fn test_window_layout_none_when_too_narrow_or_nothing_generated() {
        assert_eq!(
            window_layout((19, 50), &ids(&["%1"]), &ids(&["%3"]), None),
            None
        );
        assert_eq!(window_layout((200, 50), &[], &ids(&["%3"]), None), None);
        assert_eq!(window_layout((200, 50), &ids(&["%1"]), &[], None), None);
    }

    #[test]
    fn test_apply_adaptive_swaps_the_rail_first_and_generates_the_rail_layout() {
        let mut tmux = FakeTmux {
            roles: vec![("%3".to_string(), "mirror".to_string())],
            size: (200, 50),
            ..FakeTmux::default()
        }
        .panes(3);
        let choice = apply_adaptive_with("dev:0", &mut tmux).unwrap();
        assert_eq!(choice.preset, RAIL_PRESET);
        let body = "200x50,0,0{14x50,0,0,3,185x50,15,0{92x50,15,0,2,92x50,108,0,1}}";
        assert_eq!(
            tmux.calls,
            vec![
                (
                    "swap".to_string(),
                    "%3".to_string(),
                    "%1".to_string(),
                    String::new()
                ),
                (
                    "layout".to_string(),
                    "dev:0".to_string(),
                    checksummed(body),
                    String::new()
                ),
            ]
        );
    }

    #[test]
    fn test_apply_adaptive_rail_first_then_dock_last() {
        let mut tmux = FakeTmux {
            roles: vec![
                ("%1".to_string(), "dock".to_string()),
                ("%3".to_string(), "mirror".to_string()),
            ],
            size: (200, 50),
            ..FakeTmux::default()
        }
        .panes(3);
        apply_adaptive_with("dev:0", &mut tmux).unwrap();
        // after the rail swap the order is %3 %2 %1: the dock is already last
        let body = "200x50,0,0{14x50,0,0,3,185x50,15,0[185x35,15,0,2,185x14,15,36,1]}";
        assert_eq!(
            tmux.calls,
            vec![
                (
                    "swap".to_string(),
                    "%3".to_string(),
                    "%1".to_string(),
                    String::new()
                ),
                (
                    "layout".to_string(),
                    "dev:0".to_string(),
                    checksummed(body),
                    String::new()
                ),
            ]
        );
    }

    #[test]
    fn test_apply_adaptive_rail_alone_is_a_noop() {
        let mut tmux = FakeTmux {
            roles: vec![("%1".to_string(), "mirror".to_string())],
            size: (200, 50),
            ..FakeTmux::default()
        }
        .panes(1);
        assert_eq!(apply_adaptive_with("dev:0", &mut tmux), None);
        assert!(tmux.calls.is_empty());
    }

    #[test]
    fn test_apply_adaptive_too_narrow_for_the_rail_falls_back_to_a_preset() {
        let mut tmux = FakeTmux {
            roles: vec![("%1".to_string(), "mirror".to_string())],
            size: (18, 50),
            ..FakeTmux::default()
        }
        .panes(3);
        let choice = apply_adaptive_with("dev:0", &mut tmux).unwrap();
        assert_eq!(choice.preset, PORTRAIT_PRESET);
        assert_eq!(
            tmux.calls,
            vec![(
                "layout".to_string(),
                "dev:0".to_string(),
                "even-vertical".to_string(),
                String::new(),
            )]
        );
    }

    #[test]
    fn test_apply_adaptive_skips_a_zoomed_rail_window() {
        let mut tmux = FakeTmux {
            zoomed: true,
            roles: vec![("%1".to_string(), "mirror".to_string())],
            size: (200, 50),
            ..FakeTmux::default()
        }
        .panes(2);
        assert_eq!(apply_adaptive_with("dev:0", &mut tmux), None);
        assert!(tmux.calls.is_empty());
    }

    #[test]
    fn test_apply_adaptive_with_dock_pane_swaps_it_last_and_generates_layout() {
        let mut tmux = FakeTmux {
            roles: vec![("%1".to_string(), "dock".to_string())],
            size: (200, 50),
            ..FakeTmux::default()
        }
        .panes(3);
        let choice = apply_adaptive_with("dev:0", &mut tmux).unwrap();
        assert_eq!(choice.preset, DOCK_PRESET);
        assert_eq!(tmux.calls.len(), 2);
        // the dock (first in window order) swaps with the last pane…
        assert_eq!(
            tmux.calls[0],
            (
                "swap".to_string(),
                "%1".to_string(),
                "%3".to_string(),
                String::new()
            )
        );
        // …so the members tile in the new order and the dock cell is last
        let (kind, target, layout, _) = &tmux.calls[1];
        assert_eq!((kind.as_str(), target.as_str()), ("layout", "dev:0"));
        assert!(layout.contains("{99x35,0,0,3,100x35,100,0,2}"), "{layout}");
        assert!(layout.ends_with(",200x14,0,36,1]"), "{layout}");
    }

    #[test]
    fn test_apply_adaptive_with_dock_already_last_does_not_swap() {
        let mut tmux = FakeTmux {
            roles: vec![("%3".to_string(), "dock".to_string())],
            size: (200, 50),
            ..FakeTmux::default()
        }
        .panes(3);
        apply_adaptive_with("dev:0", &mut tmux).unwrap();
        assert_eq!(tmux.calls.len(), 1);
        assert_eq!(tmux.calls[0].0, "layout");
        assert!(
            tmux.calls[0].2.ends_with(",200x14,0,36,3]"),
            "{}",
            tmux.calls[0].2
        );
    }
}
