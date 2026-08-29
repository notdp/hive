//! Adaptive tmux layout for hive teams.
//!
//! Picks a preset from the window's aspect ratio (tmux cell ≈ 1:2 pixel,
//! so char-width >= 2*char-height ≈ landscape pixels) and current pane count.
//! Used by Team.spawn, hive kill, hive layout, and resume.

pub const LANDSCAPE_PRESET: &str = "main-vertical";
pub const PORTRAIT_PRESET: &str = "even-vertical";
pub const MAIN_PANE_FRACTION: &str = "50%";
// From this many panes, a main pane plus a single strip squeezes members
// into slivers — tile instead.
pub const TILED_THRESHOLD: usize = 5;

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

// Seam so unit tests can record tmux calls without a tmux server
// (mirrors the monkeypatching in tests/unit/test_layout.py).
trait TmuxOps {
    fn window_zoomed(&mut self, target: &str) -> bool;
    fn window_size(&mut self, target: &str) -> (i64, i64);
    fn list_panes(&mut self, target: &str) -> Vec<String>;
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
    fn set_window_option(&mut self, target: &str, option: &str, value: &str) {
        crate::tmux::set_window_option(target, option, value)
    }
    fn select_layout(&mut self, target: &str, layout: &str) {
        crate::tmux::select_layout(target, layout)
    }
}

/// Read window size + pane count from tmux, apply the matching preset.
pub fn apply_adaptive(window_target: &str) -> Option<LayoutChoice> {
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
    let pane_count = tmux.list_panes(window_target).len();
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
}
