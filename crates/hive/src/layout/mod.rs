//! hive owns a team window's layout. A human dragging a pane border is
//! respected until the next *layout event*: the plan hive would compute
//! now (`plan.rs`, from the window size and the panes' roles) differs from
//! the plan it last applied, whose key sits on the window as
//! `@hive-layout`. `ensure` is that comparison; the explicit call sites
//! (spawn, retire, attach, mirror, rig, board) and the two window hooks
//! (`hooks.rs`) all come through it, and only a differing key — or
//! `force`, the human's `hive layout auto` — writes to tmux at all.

mod hooks;
mod plan;

pub use hooks::{hook_argv, install_hooks, remove_hooks, unhook_argv, LAYOUT_HOOKS};
pub use plan::{layout_checksum, plan, split_beside, Plan, DOCK_ROWS, MIN_COLS, MIN_ROWS};

use std::path::PathBuf;

use crate::tmux::PaneInfo;

/// Window option holding the key of the last applied plan.
pub const LAYOUT_KEY_OPTION: &str = "@hive-layout";

/// What `ensure` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to compare: no window, zoomed, no plan, or tmux refused.
    Skipped(&'static str),
    /// The plan matches the window's key: no tmux write.
    Unchanged(Plan),
    Applied(Plan),
}

impl Outcome {
    pub fn plan(&self) -> Option<&Plan> {
        match self {
            Outcome::Skipped(_) => None,
            Outcome::Unchanged(plan) | Outcome::Applied(plan) => Some(plan),
        }
    }

    pub fn applied(&self) -> bool {
        matches!(self, Outcome::Applied(_))
    }

    pub fn reason(&self) -> &'static str {
        match self {
            Outcome::Skipped(reason) => reason,
            Outcome::Unchanged(_) => "unchanged",
            Outcome::Applied(_) => "",
        }
    }
}

// Seam so unit tests can record tmux calls without a tmux server.
trait TmuxOps {
    /// The window's `@N` id, None when tmux does not answer.
    fn window_id(&mut self, target: &str) -> Option<String>;
    fn window_zoomed(&mut self, target: &str) -> bool;
    fn window_size(&mut self, target: &str) -> (i64, i64);
    fn list_panes_full(&mut self, target: &str) -> Vec<PaneInfo>;
    fn layout_key(&mut self, target: &str) -> Option<String>;
    fn swap_pane(&mut self, src: &str, dst: &str);
    /// An empty `key` drops the option.
    fn set_layout_key(&mut self, target: &str, key: &str);
    /// Whether tmux accepted the layout.
    fn select_layout(&mut self, target: &str, layout: &str) -> bool;
}

struct RealTmux;

impl TmuxOps for RealTmux {
    fn window_id(&mut self, target: &str) -> Option<String> {
        crate::tmux::get_window_id(target)
    }
    fn window_zoomed(&mut self, target: &str) -> bool {
        crate::tmux::window_zoomed(target)
    }
    fn window_size(&mut self, target: &str) -> (i64, i64) {
        let (w, h) = crate::tmux::window_size(target);
        (w as i64, h as i64)
    }
    fn list_panes_full(&mut self, target: &str) -> Vec<PaneInfo> {
        crate::tmux::list_panes_full(target)
    }
    fn layout_key(&mut self, target: &str) -> Option<String> {
        crate::tmux::get_window_option(target, LAYOUT_KEY_OPTION.trim_start_matches('@'))
    }
    fn swap_pane(&mut self, src: &str, dst: &str) {
        crate::tmux::swap_pane(src, dst)
    }
    fn set_layout_key(&mut self, target: &str, key: &str) {
        if key.is_empty() {
            crate::tmux::clear_window_option(target, LAYOUT_KEY_OPTION)
        } else {
            crate::tmux::set_window_option(target, LAYOUT_KEY_OPTION, key)
        }
    }
    fn select_layout(&mut self, target: &str, layout: &str) -> bool {
        crate::tmux::run(&["select-layout", "-t", target, layout], false, 5)
            .is_ok_and(|r| r.returncode == 0)
    }
}

/// Cross-process lock for one window's apply. Two appliers racing (a
/// board starting while the rig splits its mirror, the hook fired by one
/// apply's own `select-layout` landing beside the next spawn) would each
/// see the dock out of place and both swap it — a double swap puts it
/// back where it was — and the hook must see the key its predecessor
/// wrote. The lock file is named by the window's `@N` id (`lock_key`):
/// the hooks address the window by id and every explicit site by
/// `session:index`, and both spellings must take the same lock.
struct WindowLock(std::fs::File);

impl Drop for WindowLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// The lock file stem for `window_target`: its window id when tmux
/// resolves one, else the target as spelled.
fn lock_key(window_target: &str, tmux: &mut dyn TmuxOps) -> String {
    tmux.window_id(window_target)
        .unwrap_or_else(|| window_target.to_string())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// How an apply waits for the window lock. In-process callers wait their
/// turn; the hook form must not: a border or terminal drag fires
/// `window-resized` per step, and a queue of blocked hook processes once
/// filled the process table. The hook tries the lock and, when another
/// apply holds it, leaves a rerun marker and exits; the holder plans once
/// more when it finds the marker, so a burst collapses into at most two
/// applies.
enum LockState {
    Held(WindowLock),
    Busy,
    Unavailable,
}

fn lock_dir() -> Option<PathBuf> {
    let dir = crate::team::hive_home().join("state").join("locks");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn window_lock(key: &str, wait: bool) -> LockState {
    use std::os::unix::io::AsRawFd;
    let Some(dir) = lock_dir() else {
        return LockState::Unavailable;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(format!("layout-{key}.lock")))
    else {
        return LockState::Unavailable;
    };
    let flags = if wait {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
        return LockState::Held(WindowLock(file));
    }
    if !wait && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        return LockState::Busy;
    }
    LockState::Unavailable
}

fn rerun_marker(key: &str) -> Option<PathBuf> {
    Some(lock_dir()?.join(format!("layout-{key}.rerun")))
}

fn leave_rerun(key: &str) {
    if let Some(marker) = rerun_marker(key) {
        let _ = std::fs::write(marker, b"");
    }
}

/// Take the rerun marker: true when a skipped apply left one.
fn take_rerun(key: &str) -> bool {
    rerun_marker(key).is_some_and(|marker| std::fs::remove_file(marker).is_ok())
}

/// Bring the window to the plan it should have: apply when `force` or when
/// the plan's key differs from `@hive-layout`, else touch nothing. Waits
/// for the window lock: the caller has just changed the window (a split,
/// a join, a dock tag) and its apply must land after an apply in flight.
pub fn ensure(window_target: &str, force: bool) -> Outcome {
    if window_target.is_empty() {
        return Outcome::Skipped("no-window");
    }
    let mut tmux = RealTmux;
    ensure_locked(window_target, force, true, &mut tmux)
}

/// The window hooks' form of [`ensure`]: never forces, never waits — an
/// apply in flight gets a rerun marker instead (see `LockState`).
pub fn ensure_hook(window_target: &str) -> Outcome {
    if window_target.is_empty() {
        return Outcome::Skipped("no-window");
    }
    let mut tmux = RealTmux;
    ensure_locked(window_target, false, false, &mut tmux)
}

fn ensure_locked(window_target: &str, force: bool, wait: bool, tmux: &mut dyn TmuxOps) -> Outcome {
    let key = lock_key(window_target, tmux);
    let mut lock = match window_lock(&key, wait) {
        LockState::Held(lock) => Some(lock),
        LockState::Busy => {
            leave_rerun(&key);
            // The holder may already have looked for markers: try once more,
            // and plan ourselves if it let go in between.
            match window_lock(&key, false) {
                LockState::Held(lock) => Some(lock),
                _ => return Outcome::Skipped("busy"),
            }
        }
        LockState::Unavailable => None,
    };
    let mut outcome = ensure_with(window_target, force, tmux);
    for _ in 0..3 {
        if !take_rerun(&key) {
            // Nobody asked for another pass while we held the lock. Release,
            // then look once more: a marker left between that check and the
            // release is ours to take while the lock is still free.
            drop(lock.take());
            if !rerun_marker(&key).is_some_and(|marker| marker.exists()) {
                break;
            }
            lock = match window_lock(&key, false) {
                LockState::Held(lock) => Some(lock),
                _ => break,
            };
            if !take_rerun(&key) {
                break;
            }
        }
        // An apply that found us holding the lock left the marker: its event
        // may postdate the state read above, so plan once more.
        let again = ensure_with(window_target, force, tmux);
        // Report the apply that happened, not the no-op that confirmed it.
        if again.applied() || !outcome.applied() {
            outcome = again;
        }
    }
    outcome
}

fn ensure_with(window_target: &str, force: bool, tmux: &mut dyn TmuxOps) -> Outcome {
    if window_target.is_empty() {
        return Outcome::Skipped("no-window");
    }
    if tmux.window_zoomed(window_target) {
        // The human zoomed in on a member: a re-tile would both unzoom and
        // rearrange under them. Skip; the unzoom fires the hook.
        return Outcome::Skipped("zoomed");
    }
    let size = tmux.window_size(window_target);
    let panes = tmux.list_panes_full(window_target);
    let Some(planned) = plan(size, &panes) else {
        // The key names a plan that no longer exists; left, a window back
        // to the same member count would match it and never be planned.
        if tmux.layout_key(window_target).is_some() {
            tmux.set_layout_key(window_target, "");
        }
        return Outcome::Skipped("no-plan");
    };
    if !force && tmux.layout_key(window_target).as_deref() == Some(planned.key.as_str()) {
        return Outcome::Unchanged(planned);
    }
    // Cells apply in window order: the mirror must be first, the dock last.
    let panes = cell_order(window_target, panes, tmux);
    let Some(planned) = plan(size, &panes) else {
        return Outcome::Skipped("no-plan");
    };
    if !tmux.select_layout(window_target, &planned.layout) {
        return Outcome::Skipped("rejected");
    }
    tmux.set_layout_key(window_target, &planned.key);
    Outcome::Applied(planned)
}

/// Swap the mirror to the front and the dock to the back of the window
/// order, re-reading after each swap rather than trusting it: the layout
/// string is applied by window order, so it must describe what tmux has
/// now.
fn cell_order(window: &str, mut panes: Vec<PaneInfo>, tmux: &mut dyn TmuxOps) -> Vec<PaneInfo> {
    if let Some(at) = panes.iter().position(|p| p.role == "mirror") {
        if at != 0 {
            tmux.swap_pane(&panes[at].pane_id.clone(), &panes[0].pane_id.clone());
            panes = tmux.list_panes_full(window);
        }
    }
    if let (Some(at), Some(last)) = (
        panes.iter().position(|p| p.role == "dock"),
        panes.len().checked_sub(1),
    ) {
        if at != last {
            tmux.swap_pane(&panes[at].pane_id.clone(), &panes[last].pane_id.clone());
            panes = tmux.list_panes_full(window);
        }
    }
    panes
}

/// Pre-spawn tmux split direction for one more member pane in
/// `window_target`, matching the plan that follows so a portrait window
/// never shows a squeezed left-right split while the new CLI boots.
/// `true` (`-h`, the legacy default) when the window is unknown.
pub fn split_horizontal(window_target: &str) -> bool {
    if window_target.is_empty() {
        return true;
    }
    let (w, h) = crate::tmux::window_size(window_target);
    let mut panes = crate::tmux::list_panes_full(window_target);
    panes.push(PaneInfo::default());
    split_beside((w as i64, h as i64), &panes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTmux {
        id: Option<String>,
        zoomed: bool,
        size: (i64, i64),
        panes: Vec<PaneInfo>,
        key: Option<String>,
        reject: bool,
        calls: Vec<Vec<String>>,
    }

    impl FakeTmux {
        fn new(size: (i64, i64), spec: &[(&str, &str)]) -> Self {
            FakeTmux {
                size,
                panes: spec
                    .iter()
                    .map(|(id, role)| PaneInfo {
                        pane_id: id.to_string(),
                        role: role.to_string(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }
        }

        fn record(&mut self, row: &[&str]) {
            self.calls.push(row.iter().map(|s| s.to_string()).collect());
        }

        fn order(&self) -> Vec<&str> {
            self.panes.iter().map(|p| p.pane_id.as_str()).collect()
        }
    }

    impl TmuxOps for FakeTmux {
        fn window_id(&mut self, _target: &str) -> Option<String> {
            self.id.clone()
        }
        fn window_zoomed(&mut self, _target: &str) -> bool {
            self.zoomed
        }
        fn window_size(&mut self, _target: &str) -> (i64, i64) {
            self.size
        }
        fn list_panes_full(&mut self, _target: &str) -> Vec<PaneInfo> {
            self.panes.clone()
        }
        fn layout_key(&mut self, _target: &str) -> Option<String> {
            self.key.clone()
        }
        fn swap_pane(&mut self, src: &str, dst: &str) {
            self.record(&["swap", src, dst]);
            let a = self.panes.iter().position(|p| p.pane_id == src);
            let b = self.panes.iter().position(|p| p.pane_id == dst);
            if let (Some(a), Some(b)) = (a, b) {
                self.panes.swap(a, b);
            }
        }
        fn set_layout_key(&mut self, target: &str, key: &str) {
            self.record(&["key", target, key]);
            self.key = (!key.is_empty()).then(|| key.to_string());
        }
        fn select_layout(&mut self, target: &str, layout: &str) -> bool {
            self.record(&["layout", target, layout]);
            !self.reject
        }
    }

    fn expected(size: (i64, i64), spec: &[(&str, &str)]) -> Plan {
        plan(size, &FakeTmux::new(size, spec).panes).unwrap()
    }

    fn locks_home() -> (tempfile::TempDir, crate::testenv::EnvGuard) {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = crate::testenv::EnvGuard::new();
        env.set("HIVE_HOME", tmp.path());
        (tmp, env)
    }

    #[test]
    fn test_a_hook_apply_yields_to_the_holder_and_leaves_a_rerun_marker() {
        let (_tmp, _env) = locks_home();
        let spec = [("%1", "agent"), ("%2", "agent")];
        let mut tmux = FakeTmux::new((200, 50), &spec);
        let key = lock_key("dev:0", &mut tmux);
        let held = match window_lock(&key, true) {
            LockState::Held(lock) => lock,
            _ => panic!("lock dir unavailable"),
        };

        assert_eq!(
            ensure_locked("dev:0", false, false, &mut tmux),
            Outcome::Skipped("busy")
        );
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
        assert!(rerun_marker(&key).unwrap().exists());

        // The holder finishes: the next apply takes the lock, plans, and
        // consumes the marker.
        drop(held);
        let outcome = ensure_locked("dev:0", false, false, &mut tmux);
        assert!(outcome.applied(), "{outcome:?}");
        assert!(!rerun_marker(&key).unwrap().exists());
    }

    #[test]
    fn test_the_holder_plans_once_more_when_a_rerun_marker_was_left() {
        let (_tmp, _env) = locks_home();
        let spec = [("%1", "agent"), ("%2", "agent")];
        let mut tmux = FakeTmux::new((200, 50), &spec);
        let key = lock_key("dev:0", &mut tmux);
        leave_rerun(&key);

        let outcome = ensure_locked("dev:0", false, true, &mut tmux);

        assert!(outcome.applied(), "{outcome:?}");
        assert!(!rerun_marker(&key).unwrap().exists());
        // The second pass found the key it had just written: one layout.
        let layouts = tmux.calls.iter().filter(|c| c[0] == "layout").count();
        assert_eq!(layouts, 1, "{:?}", tmux.calls);
    }

    #[test]
    fn test_a_forced_apply_waits_for_the_lock() {
        let (_tmp, _env) = locks_home();
        let key = "dev_0";
        let held = match window_lock(key, true) {
            LockState::Held(lock) => lock,
            _ => panic!("lock dir unavailable"),
        };
        assert!(matches!(window_lock(key, false), LockState::Busy));
        drop(held);
        assert!(matches!(window_lock(key, false), LockState::Held(_)));
    }

    #[test]
    fn test_ensure_empty_window_target_is_noop() {
        let mut tmux = FakeTmux::new((200, 50), &[("%1", "agent"), ("%2", "agent")]);
        assert_eq!(
            ensure_with("", false, &mut tmux),
            Outcome::Skipped("no-window")
        );
        assert!(tmux.calls.is_empty());
    }

    #[test]
    fn test_ensure_single_pane_skips() {
        let mut tmux = FakeTmux::new((200, 50), &[("%1", "agent")]);
        assert_eq!(
            ensure_with("dev:0", false, &mut tmux),
            Outcome::Skipped("no-plan")
        );
        assert!(tmux.calls.is_empty());
    }

    #[test]
    fn test_ensure_drops_a_stale_key_on_no_plan_so_the_next_member_is_planned() {
        // mirror + one member planned, then the member dies: one pane, no
        // plan, and the key that named the pair goes with it…
        let spec = [("%1", "mirror"), ("%2", "agent")];
        let mut tmux = FakeTmux::new((220, 60), &spec);
        let pair = expected((220, 60), &spec);
        assert_eq!(
            ensure_with("dev:0", false, &mut tmux),
            Outcome::Applied(pair.clone())
        );
        tmux.calls.clear();
        tmux.panes.truncate(1);
        assert_eq!(
            ensure_with("dev:0", false, &mut tmux),
            Outcome::Skipped("no-plan")
        );
        assert_eq!(tmux.calls, vec![vec!["key", "dev:0", ""]]);
        assert_eq!(tmux.key, None);
        // …so a new member with the same key as the old pair is planned,
        // not left in tmux's raw split.
        tmux.calls.clear();
        tmux.panes.push(PaneInfo {
            pane_id: "%3".to_string(),
            role: "agent".to_string(),
            ..Default::default()
        });
        let outcome = ensure_with("dev:0", false, &mut tmux);
        assert!(outcome.applied(), "{outcome:?}");
        assert_eq!(outcome.plan().unwrap().key, pair.key);
        assert_eq!(tmux.calls[0][0], "layout");
    }

    #[test]
    fn test_ensure_single_pane_with_no_key_writes_nothing() {
        // The hook fires on every resize of a one-pane window: no key, no
        // write.
        let mut tmux = FakeTmux::new((200, 50), &[("%1", "agent")]);
        tmux.size = (100, 90);
        assert_eq!(
            ensure_with("dev:0", false, &mut tmux),
            Outcome::Skipped("no-plan")
        );
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
    }

    #[test]
    fn test_lock_key_is_the_window_id_whatever_the_target_spelling() {
        // The hook says `@0`, a spawn says `dev:0`: one lock file.
        let mut tmux = FakeTmux::new((200, 50), &[]);
        tmux.id = Some("@0".to_string());
        assert_eq!(lock_key("dev:0", &mut tmux), "_0");
        assert_eq!(lock_key("@0", &mut tmux), "_0");
        assert_eq!(lock_key("dev:0", &mut tmux), lock_key("@0", &mut tmux));
        // tmux silent: the spelling as given
        tmux.id = None;
        assert_eq!(lock_key("dev:0", &mut tmux), "dev_0");
    }

    #[test]
    fn test_ensure_skips_while_zoomed() {
        let mut tmux = FakeTmux::new((200, 50), &[("%1", "agent"), ("%2", "agent")]);
        tmux.zoomed = true;
        assert_eq!(
            ensure_with("dev:0", false, &mut tmux),
            Outcome::Skipped("zoomed")
        );
        // a zoomed window is never re-tiled under the human
        assert!(tmux.calls.is_empty());
    }

    #[test]
    fn test_ensure_applies_the_plan_and_writes_its_key_on_a_fresh_window() {
        let spec = [("%1", "agent"), ("%2", "agent")];
        let mut tmux = FakeTmux::new((191, 171), &spec);
        let planned = expected((191, 171), &spec);
        assert_eq!(planned.orientation, "portrait");
        assert_eq!(
            ensure_with("dev:0", false, &mut tmux),
            Outcome::Applied(planned.clone())
        );
        assert_eq!(
            tmux.calls,
            vec![
                vec!["layout", "dev:0", planned.layout.as_str()],
                vec!["key", "dev:0", planned.key.as_str()],
            ]
        );
    }

    #[test]
    fn test_ensure_on_change_with_a_matching_key_writes_nothing() {
        let spec = [("%1", "mirror"), ("%2", "agent"), ("%3", "agent")];
        let mut tmux = FakeTmux::new((220, 60), &spec);
        let planned = expected((220, 60), &spec);
        tmux.key = Some(planned.key.clone());
        assert_eq!(
            ensure_with("dev:0", false, &mut tmux),
            Outcome::Unchanged(planned)
        );
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
        // A proportional resize keeps the key: still nothing to write.
        tmux.size = (200, 55);
        let outcome = ensure_with("dev:0", false, &mut tmux);
        assert!(!outcome.applied(), "{outcome:?}");
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
    }

    #[test]
    fn test_ensure_forced_applies_over_a_matching_key() {
        let spec = [("%1", "agent"), ("%2", "agent")];
        let mut tmux = FakeTmux::new((220, 60), &spec);
        let planned = expected((220, 60), &spec);
        tmux.key = Some(planned.key.clone());
        assert_eq!(
            ensure_with("dev:0", true, &mut tmux),
            Outcome::Applied(planned.clone())
        );
        assert_eq!(
            tmux.calls[0],
            vec!["layout", "dev:0", planned.layout.as_str()]
        );
    }

    #[test]
    fn test_ensure_reapplies_when_the_key_changes() {
        let spec = [("%1", "agent"), ("%2", "agent")];
        let mut tmux = FakeTmux::new((220, 60), &spec);
        tmux.key = Some(expected((220, 60), &spec).key);
        // a third member joins: the count is in the key
        tmux.panes.push(PaneInfo {
            pane_id: "%3".to_string(),
            role: "agent".to_string(),
            ..Default::default()
        });
        let outcome = ensure_with("dev:0", false, &mut tmux);
        assert!(outcome.applied(), "{outcome:?}");
        assert!(outcome.plan().unwrap().key.contains("/m3/"));
        // a flip changes it too
        tmux.calls.clear();
        tmux.size = (60, 80);
        let outcome = ensure_with("dev:0", false, &mut tmux);
        assert!(outcome.applied(), "{outcome:?}");
        assert_eq!(outcome.plan().unwrap().orientation, "portrait");
    }

    #[test]
    fn test_ensure_with_dock_pane_swaps_it_last_and_generates_layout() {
        let mut tmux = FakeTmux::new(
            (200, 50),
            &[("%1", "dock"), ("%2", "agent"), ("%3", "agent")],
        );
        let outcome = ensure_with("dev:0", false, &mut tmux);
        assert!(outcome.applied(), "{outcome:?}");
        assert_eq!(tmux.calls.len(), 3);
        // the dock (first in window order) swaps with the last pane…
        assert_eq!(tmux.calls[0], vec!["swap", "%1", "%3"]);
        assert_eq!(tmux.order(), vec!["%3", "%2", "%1"]);
        // …so the members tile in the new order and the dock cell is last
        let layout = &tmux.calls[1][2];
        assert!(layout.contains("{99x35,0,0,3,100x35,100,0,2}"), "{layout}");
        assert!(layout.ends_with(",200x14,0,36,1]"), "{layout}");
    }

    #[test]
    fn test_ensure_with_dock_already_last_does_not_swap() {
        let mut tmux = FakeTmux::new(
            (200, 50),
            &[("%1", "agent"), ("%2", "agent"), ("%3", "dock")],
        );
        ensure_with("dev:0", false, &mut tmux);
        assert_eq!(tmux.calls.len(), 2);
        assert_eq!(tmux.calls[0][0], "layout");
        assert!(
            tmux.calls[0][2].ends_with(",200x14,0,36,3]"),
            "{}",
            tmux.calls[0][2]
        );
    }

    #[test]
    fn test_ensure_swaps_the_mirror_first_then_the_dock_last() {
        let mut tmux = FakeTmux::new(
            (220, 60),
            &[("%1", "agent"), ("%2", "dock"), ("%3", "mirror")],
        );
        let outcome = ensure_with("dev:0", false, &mut tmux);
        assert!(outcome.applied(), "{outcome:?}");
        assert_eq!(tmux.calls[0], vec!["swap", "%3", "%1"]);
        assert_eq!(tmux.calls[1], vec!["swap", "%2", "%1"]);
        assert_eq!(tmux.order(), vec!["%3", "%1", "%2"]);
        let layout = &tmux.calls[2][2];
        assert!(layout.contains("{109x45,0,0,3,110x45,110,0,1}"), "{layout}");
        assert!(layout.ends_with(",220x14,0,46,2]"), "{layout}");
    }

    #[test]
    fn test_ensure_matching_key_skips_the_swaps_too() {
        let spec = [("%1", "agent"), ("%2", "dock")];
        let mut tmux = FakeTmux::new((200, 50), &[("%1", "dock"), ("%2", "agent")]);
        tmux.key = Some(expected((200, 50), &spec).key);
        let outcome = ensure_with("dev:0", false, &mut tmux);
        assert!(!outcome.applied(), "{outcome:?}");
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
    }

    #[test]
    fn test_ensure_keeps_the_old_key_when_tmux_rejects_the_layout() {
        let mut tmux = FakeTmux::new((200, 50), &[("%1", "agent"), ("%2", "agent")]);
        tmux.reject = true;
        assert_eq!(
            ensure_with("dev:0", false, &mut tmux),
            Outcome::Skipped("rejected")
        );
        assert_eq!(tmux.calls.len(), 1);
        assert_eq!(tmux.key, None);
    }
}
