//! The layout planner: pure, no tmux. A window's size and its panes in
//! window order (each with its `@hive-role`) go in; a tmux layout string
//! and the key that names the decisions behind it come out.
//!
//! Space rules:
//! - Landscape = `w >= 2*h` (a tmux cell is about 1:2 in pixels).
//! - The mirror (`@hive-role mirror`) is a left column in landscape, a
//!   top row in portrait. Its share is half the window, unless
//!   the members' best grid at half scores below 1.0 and shrinking the
//!   mirror to `MIN_COLS`/`MIN_ROWS` scores them better.
//! - The members take a grid over what is left: for every row count, the
//!   equal-split cell is scored `min(cell_w / MIN_COLS, cell_h / MIN_ROWS)`
//!   and the best score wins (ties: fewer columns in portrait, columns
//!   nearest `sqrt(n)` in landscape). The last row takes the remainder.
//!
//! Layout-string grammar (`<csum>,<WxH>,<x>,<y>{a,b}` side by side,
//! `[a,b]` stacked, leaf `WxH,x,y,<pane index>`): children of `{}` share
//! the parent's height and their widths plus one separator each sum to
//! the parent's width; `[]` the same with the axes swapped. tmux ignores
//! the pane indices and hands cells to panes in window order, depth-first
//! through the string — so the leaves come out mirror then members, and
//! the apply (`layout::ensure`) puts the window in that order first.

use crate::tmux::PaneInfo;

/// The smallest cell a member's TUI is worth: the classic terminal. A
/// grid whose cells fall below it scores under 1.0. 24 rows, not fewer:
/// at 12 two members in a 220x60 window stack (1x2 scores 2.42 against
/// 2x1's 1.36) instead of sitting side by side.
pub const MIN_COLS: i64 = 80;
pub const MIN_ROWS: i64 = 24;

/// What `plan` decided: `key` names every decision except absolute sizes
/// (a proportional resize keeps it), `layout` is the `select-layout`
/// argument for this exact size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub key: String,
    pub layout: String,
    pub orientation: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

/// A layout tree; a split's children carry their extent along the split
/// axis (widths when `beside`, heights otherwise).
enum Node {
    Leaf(String),
    Split {
        beside: bool,
        children: Vec<(i64, Node)>,
    },
}

fn is_landscape(width: i64, height: i64) -> bool {
    if width <= 0 || height <= 0 {
        return true;
    }
    width >= 2 * height
}

/// tmux's `layout_checksum`.
pub fn layout_checksum(body: &str) -> u16 {
    let mut csum: u16 = 0;
    for b in body.bytes() {
        csum = (csum >> 1).wrapping_add((csum & 1) << 15);
        csum = csum.wrapping_add(u16::from(b));
    }
    csum
}

fn pane_index(pane_id: &str) -> String {
    pane_id.trim_start_matches('%').to_string()
}

/// `n` extents over `total` with one separator between neighbours: equal
/// shares, the remainder to the last.
fn equal(n: usize, total: i64) -> Vec<i64> {
    let n_i = n as i64;
    let base = (total - (n_i - 1)) / n_i;
    let mut out = vec![base; n];
    out[n - 1] = total - (n_i - 1) - base * (n_i - 1);
    out
}

/// Members per row for `n` members in `cols` columns; the last row takes
/// the remainder.
fn grid_rows(n: usize, cols: usize) -> Vec<usize> {
    let rows = n.div_ceil(cols);
    let mut out = vec![cols; rows];
    out[rows - 1] = n - cols * (rows - 1);
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Grid {
    cols: usize,
    rows: usize,
    score: f64,
}

fn grid_score(cols: usize, rows: usize, area_w: i64, area_h: i64) -> f64 {
    let cell_w = (area_w - (cols as i64 - 1)) / cols as i64;
    let cell_h = (area_h - (rows as i64 - 1)) / rows as i64;
    (cell_w as f64 / MIN_COLS as f64).min(cell_h as f64 / MIN_ROWS as f64)
}

/// Tie-break between two grids of equal score.
fn prefer(cols: usize, over: usize, n: usize, landscape: bool) -> bool {
    if !landscape {
        return cols < over;
    }
    let root = (n as f64).sqrt();
    (cols as f64 - root).abs() < (over as f64 - root).abs()
}

/// The best grid for `n` members over `area_w` x `area_h`.
fn best_grid(n: usize, area_w: i64, area_h: i64, landscape: bool) -> Grid {
    let mut best: Option<Grid> = None;
    for rows in 1..=n {
        let cols = n.div_ceil(rows);
        // A shorter grid of the same width already holds them all.
        if cols * (rows - 1) >= n {
            continue;
        }
        let score = grid_score(cols, rows, area_w, area_h);
        let better = match best {
            None => true,
            Some(b) => score > b.score || (score == b.score && prefer(cols, b.cols, n, landscape)),
        };
        if better {
            best = Some(Grid { cols, rows, score });
        }
    }
    best.unwrap_or(Grid {
        cols: 0,
        rows: 0,
        score: 0.0,
    })
}

fn grid_node(members: &[String], grid: Grid, area_w: i64, area_h: i64) -> Node {
    if members.len() == 1 {
        return Node::Leaf(pane_index(&members[0]));
    }
    let counts = grid_rows(members.len(), grid.cols);
    let heights = equal(counts.len(), area_h);
    let mut rows = Vec::new();
    let mut idx = 0;
    for (count, height) in counts.into_iter().zip(heights) {
        let row = if count == 1 {
            Node::Leaf(pane_index(&members[idx]))
        } else {
            let widths = equal(count, area_w);
            Node::Split {
                beside: true,
                children: members[idx..idx + count]
                    .iter()
                    .zip(widths)
                    .map(|(pane, width)| (width, Node::Leaf(pane_index(pane))))
                    .collect(),
            }
        };
        rows.push((height, row));
        idx += count;
    }
    if rows.len() == 1 {
        return rows.pop().unwrap().1;
    }
    Node::Split {
        beside: false,
        children: rows,
    }
}

/// The mirror's extent along the split axis and the members' grid beside
/// it: `(extent, variant, grid)`. `area` maps the members' remaining
/// extent to their `(w, h)`.
fn mirror_share(
    total: i64,
    min: i64,
    n: usize,
    landscape: bool,
    area: impl Fn(i64) -> (i64, i64),
) -> (i64, &'static str, Grid) {
    let shares = equal(2, total);
    let (half, rest) = (shares[0], shares[1]);
    let (w, h) = area(rest);
    let at_half = best_grid(n, w, h, landscape);
    if at_half.score >= 1.0 || min >= half {
        return (half, "half", at_half);
    }
    let (w, h) = area(total - 1 - min);
    let at_min = best_grid(n, w, h, landscape);
    if at_min.score > at_half.score {
        return (min, "min", at_min);
    }
    (half, "half", at_half)
}

/// Render `node` into the layout grammar over `rect`, collecting every
/// leaf's cell in leaf order.
fn render(node: &Node, rect: Rect, cells: &mut Vec<Rect>) -> String {
    let Rect { x, y, w, h } = rect;
    match node {
        Node::Leaf(index) => {
            cells.push(rect);
            format!("{w}x{h},{x},{y},{index}")
        }
        Node::Split { beside, children } => {
            let mut parts = Vec::new();
            let mut offset = 0;
            for (extent, child) in children {
                let child_rect = if *beside {
                    Rect {
                        x: x + offset,
                        y,
                        w: *extent,
                        h,
                    }
                } else {
                    Rect {
                        x,
                        y: y + offset,
                        w,
                        h: *extent,
                    }
                };
                parts.push(render(child, child_rect, cells));
                offset += extent + 1;
            }
            let (open, close) = if *beside { ('{', '}') } else { ('[', ']') };
            format!("{w}x{h},{x},{y}{open}{}{close}", parts.join(","))
        }
    }
}

/// The plan for `size` over `panes` (window order, roles read), with the
/// cell of every leaf in leaf order. None with fewer than two panes, a
/// window too small, or a cell squeezed below one column or row.
fn plan_cells(size: (i64, i64), panes: &[PaneInfo]) -> Option<(Plan, Vec<Rect>)> {
    let (w, h) = size;
    if panes.len() < 2 || w < 4 || h < 6 {
        return None;
    }
    let landscape = is_landscape(w, h);
    let mirror = panes
        .iter()
        .find(|p| p.role == "mirror")
        .map(|p| &p.pane_id);
    let members: Vec<String> = panes
        .iter()
        .filter(|p| Some(&p.pane_id) != mirror)
        .map(|p| p.pane_id.clone())
        .collect();
    let n = members.len();

    let (root, variant, grid) = match mirror {
        None => {
            let grid = best_grid(n, w, h, landscape);
            (grid_node(&members, grid, w, h), "no-mirror", grid)
        }
        Some(mirror) if landscape => {
            let (extent, variant, grid) = mirror_share(w, MIN_COLS, n, landscape, |rest| (rest, h));
            let node = Node::Split {
                beside: true,
                children: vec![
                    (extent, Node::Leaf(pane_index(mirror))),
                    (w - 1 - extent, grid_node(&members, grid, w - 1 - extent, h)),
                ],
            };
            (
                node,
                if variant == "half" {
                    "mirror-half"
                } else {
                    "mirror-min"
                },
                grid,
            )
        }
        Some(mirror) => {
            let (extent, variant, grid) = mirror_share(h, MIN_ROWS, n, landscape, |rest| (w, rest));
            let node = Node::Split {
                beside: false,
                children: vec![
                    (extent, Node::Leaf(pane_index(mirror))),
                    (h - 1 - extent, grid_node(&members, grid, w, h - 1 - extent)),
                ],
            };
            (
                node,
                if variant == "half" {
                    "mirror-half"
                } else {
                    "mirror-min"
                },
                grid,
            )
        }
    };
    let mut cells = Vec::new();
    let body_text = render(&root, Rect { x: 0, y: 0, w, h }, &mut cells);
    if cells.iter().any(|c| c.w < 1 || c.h < 1) {
        return None;
    }
    let orientation = if landscape { "landscape" } else { "portrait" };
    let key = format!("{orientation}/m{n}/{variant}/{}x{}", grid.cols, grid.rows);
    let plan = Plan {
        key,
        layout: format!("{:04x},{body_text}", layout_checksum(&body_text)),
        orientation,
    };
    Some((plan, cells))
}

/// The plan for `size` over `panes` in window order, or None when there is
/// nothing to lay out (see `plan_cells`).
pub fn plan(size: (i64, i64), panes: &[PaneInfo]) -> Option<Plan> {
    plan_cells(size, panes).map(|(plan, _)| plan)
}

/// Whether a member pane split off the window's last member (or mirror)
/// pane should go beside it (`-h`) rather than below (`-v`) to match the
/// plan for the window with that pane in it: `panes` are the window's
/// panes plus the one about to be split. The new member's cell is the
/// last leaf. `true` (the legacy default) when there is no plan.
pub fn split_beside(size: (i64, i64), panes: &[PaneInfo]) -> bool {
    match plan_cells(size, panes) {
        Some((_, cells)) if cells.len() >= 2 => {
            let last = cells[cells.len() - 1];
            let before = cells[cells.len() - 2];
            last.y == before.y
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panes in window order from `(id, role)` pairs.
    fn panes(spec: &[(&str, &str)]) -> Vec<PaneInfo> {
        spec.iter()
            .map(|(id, role)| PaneInfo {
                pane_id: id.to_string(),
                role: role.to_string(),
                ..Default::default()
            })
            .collect()
    }

    /// `n` member panes, a mirror first when `mirror`.
    fn window(n: usize, mirror: bool) -> Vec<PaneInfo> {
        let mut spec: Vec<(String, &str)> = Vec::new();
        if mirror {
            spec.push(("%1".to_string(), "mirror"));
        }
        for i in 0..n {
            spec.push((format!("%{}", 10 + i), "agent"));
        }
        let borrowed: Vec<(&str, &str)> = spec.iter().map(|(id, r)| (id.as_str(), *r)).collect();
        panes(&borrowed)
    }

    fn checked(body: &str) -> String {
        format!("{:04x},{body}", layout_checksum(body))
    }

    /// The `cols x rows` tail of a plan's key.
    fn grid_of(plan: &Plan) -> String {
        plan.key.rsplit('/').next().unwrap().to_string()
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
    fn test_plan_none_below_two_panes_or_a_tiny_window() {
        assert_eq!(plan((220, 60), &window(1, false)), None);
        assert_eq!(plan((220, 60), &[]), None);
        assert_eq!(plan((3, 60), &window(2, false)), None);
        assert_eq!(plan((220, 5), &window(2, false)), None);
        // eight members over six rows: a cell would be shorter than a row
        assert_eq!(plan((4, 6), &window(8, false)), None);
    }

    #[test]
    fn test_plan_two_members_landscape_side_by_side_portrait_stacked() {
        let landscape = plan((220, 60), &window(2, false)).unwrap();
        assert_eq!(landscape.orientation, "landscape");
        assert_eq!(landscape.key, "landscape/m2/no-mirror/2x1");
        // 109 + 1 + 110 = 220
        assert_eq!(
            landscape.layout,
            checked("220x60,0,0{109x60,0,0,10,110x60,110,0,11}")
        );
        let portrait = plan((100, 90), &window(2, false)).unwrap();
        assert_eq!(portrait.orientation, "portrait");
        assert_eq!(portrait.key, "portrait/m2/no-mirror/1x2");
        assert_eq!(
            portrait.layout,
            checked("100x90,0,0[100x44,0,0,10,100x45,0,45,11]")
        );
    }

    #[test]
    fn test_plan_grid_choice_landscape_220x60_two_to_six_members() {
        let grids: Vec<String> = (2..=6)
            .map(|n| grid_of(&plan((220, 60), &window(n, false)).unwrap()))
            .collect();
        // two side by side (109x60 scores 1.36 over 220x29's 1.21); three
        // to four in 2x2 (109x29 = 1.21 beats 72x60 = 0.9 and 54x60 =
        // 0.68); five and six in 3x2 (72x29 = 0.9 against 54x29's 0.68 in
        // 4x2).
        assert_eq!(grids, vec!["2x1", "2x2", "2x2", "3x2", "3x2"]);
    }

    #[test]
    fn test_plan_grid_choice_portrait_100x90_stacks_until_rows_run_out() {
        let grids: Vec<String> = (2..=6)
            .map(|n| grid_of(&plan((100, 90), &window(n, false)).unwrap()))
            .collect();
        // 100 columns fit one 80-column cell, so members stack until the
        // strips get shorter than two 49-column cells are narrow: at six,
        // 100x14 scores 0.58 and 49x29 in 2x3 scores 0.61.
        assert_eq!(grids, vec!["1x2", "1x3", "1x4", "1x5", "2x3"]);
    }

    #[test]
    fn test_plan_grid_choice_narrow_portrait_80x200_never_splits_columns() {
        for n in 2..=6 {
            let plan = plan((80, 200), &window(n, false)).unwrap();
            assert_eq!(grid_of(&plan), format!("1x{n}"), "{}", plan.key);
        }
    }

    #[test]
    fn test_plan_mirror_column_landscape_220x60_two_members_stack_beside_it() {
        let plan = plan((220, 60), &window(2, true)).unwrap();
        assert_eq!(plan.key, "landscape/m2/mirror-half/1x2");
        // mirror 109 wide, members 110x29 + 110x30 stacked in the rest
        assert_eq!(
            plan.layout,
            checked("220x60,0,0{109x60,0,0,1,110x60,110,0[110x29,110,0,10,110x30,110,30,11]}")
        );
    }

    #[test]
    fn test_plan_mirror_row_portrait_100x90_two_members_stack_below_it() {
        let plan = plan((100, 90), &window(2, true)).unwrap();
        // Half the window (44 rows) leaves the members 100x22 (0.92); a
        // 24-row mirror leaves them 100x32 (1.25), so the mirror shrinks.
        assert_eq!(plan.key, "portrait/m2/mirror-min/1x2");
        assert_eq!(
            plan.layout,
            checked("100x90,0,0[100x24,0,0,1,100x65,0,25[100x32,0,25,10,100x32,0,58,11]]")
        );
    }

    #[test]
    fn test_plan_mirror_shrinks_to_min_cols_when_that_scores_members_better() {
        // Four members beside a half mirror get 54x29 (0.68); beside an
        // 80-column mirror 69x29 (0.86).
        let plan = plan((220, 60), &window(4, true)).unwrap();
        assert_eq!(plan.key, "landscape/m4/mirror-min/2x2");
        assert!(
            plan.layout.contains("{80x60,0,0,1,139x60,81,0["),
            "{}",
            plan.layout
        );
    }

    #[test]
    fn test_plan_mirror_keeps_half_when_min_is_no_better() {
        // 20 rows bound the members' score (0.83) whatever the mirror's
        // width: 99x20 beside a half mirror, 159x20 beside an 80-column
        // one — no gain, so the mirror keeps its half.
        let plan = plan((400, 20), &window(2, true)).unwrap();
        assert_eq!(plan.key, "landscape/m2/mirror-half/2x1");
        assert!(
            plan.layout.contains("{199x20,0,0,1,200x20,200,0{"),
            "{}",
            plan.layout
        );
    }

    #[test]
    fn test_plan_three_members_go_side_by_side_in_a_short_window() {
        // 35 rows halve to 17 (0.71): three 66-column cells (0.83) win
        let plan = plan((200, 35), &window(3, false)).unwrap();
        assert_eq!(grid_of(&plan), "3x1");
        assert_eq!(
            plan.layout,
            checked("200x35,0,0{66x35,0,0,10,66x35,67,0,11,66x35,134,0,12}")
        );
    }

    #[test]
    fn test_plan_three_members_wrap_the_last_row_in_a_tall_window() {
        let plan = plan((220, 60), &window(3, false)).unwrap();
        assert_eq!(grid_of(&plan), "2x2");
        // two rows over 60 lines: 29 + 1 + 30; the lone third pane spans the row
        assert_eq!(
            plan.layout,
            checked("220x60,0,0[220x29,0,0{109x29,0,0,10,110x29,110,0,11},220x30,0,30,12]")
        );
    }

    #[test]
    fn test_plan_leaves_come_out_mirror_then_members_whatever_the_window_order() {
        let ordered = plan((220, 60), &window(2, true)).unwrap();
        let shuffled = plan(
            (220, 60),
            &panes(&[("%10", "agent"), ("%1", "mirror"), ("%11", "agent")]),
        )
        .unwrap();
        assert_eq!(ordered, shuffled);
        assert!(
            ordered.layout.contains("{109x60,0,0,1,"),
            "{}",
            ordered.layout
        );
    }

    #[test]
    fn test_plan_key_survives_a_proportional_resize() {
        let before = plan((220, 60), &window(2, true)).unwrap();
        let after = plan((200, 55), &window(2, true)).unwrap();
        assert_eq!(before.key, after.key);
        assert_ne!(before.layout, after.layout);
        let before = plan((220, 60), &window(2, false)).unwrap();
        let after = plan((200, 55), &window(2, false)).unwrap();
        assert_eq!(before.key, after.key);
    }

    #[test]
    fn test_plan_key_changes_on_flip_member_count_and_mirror() {
        let base = plan((220, 60), &window(2, true)).unwrap();
        let flipped = plan((60, 80), &window(2, true)).unwrap();
        assert_ne!(base.key, flipped.key);
        assert_eq!(flipped.orientation, "portrait");
        assert_ne!(base.key, plan((220, 60), &window(3, true)).unwrap().key);
        assert_ne!(base.key, plan((220, 60), &window(2, false)).unwrap().key);
    }

    #[test]
    fn test_split_beside_follows_the_plan_for_the_pane_about_to_split() {
        // landscape, two members: beside; portrait: below
        assert!(split_beside((220, 60), &window(2, false)));
        assert!(!split_beside((100, 90), &window(2, false)));
        // a member beside a landscape mirror
        assert!(split_beside((220, 60), &window(1, true)));
        // a second member stacks under the first in the mirror's column
        assert!(!split_beside((220, 60), &window(2, true)));
        // no plan: the legacy default
        assert!(split_beside((0, 0), &window(1, false)));
    }

    #[test]
    fn test_grid_helpers_tie_break_rows_and_shares() {
        assert!(prefer(2, 4, 4, true));
        assert!(prefer(1, 2, 4, false));
        assert!(!prefer(2, 1, 4, false));
        assert_eq!(grid_rows(5, 3), vec![3, 2]);
        assert_eq!(grid_rows(3, 1), vec![1, 1, 1]);
        assert_eq!(equal(3, 100), vec![32, 32, 34]);
        assert_eq!(equal(1, 7), vec![7]);
    }
}
