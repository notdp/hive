//! hive owns a team window's layout. A human dragging a pane border is
//! respected until the next *layout event*: the plan hive would compute
//! now (`plan.rs`, from the window size and the panes' roles) differs from
//! the plan it last applied, whose key sits on the window as
//! `@hive-layout`. `ensure` is that comparison; the explicit call sites
//! (spawn, retire, attach, mirror) and the two window hooks
//! (`hooks.rs`) all come through it, and only a differing key — or
//! `force`, the human's `hive layout auto` — writes to tmux at all.
//!
//! A drag outlives the display: the hook that finds the key unchanged and
//! the layout away from the plan's remembers it in the workspace
//! (`arrangement.rs`), and a window planned to the same key over the same
//! members — rebuilt by `hive attach` after the tmux server died, or back
//! to that member count — gets the drag instead of the plan. `hive layout
//! auto` forgets it.

mod arrangement;
mod hooks;
mod plan;

pub(crate) use arrangement::WindowIdentity;
pub use hooks::{hook_argv, install_hooks, remove_hooks, unhook_argv, LAYOUT_HOOKS};
pub use plan::{layout_checksum, plan, split_beside, Plan, MIN_COLS, MIN_ROWS};

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
    /// The remembered drag applied under the plan's key instead of the
    /// plan's own layout.
    Restored(Plan),
}

impl Outcome {
    pub fn plan(&self) -> Option<&Plan> {
        match self {
            Outcome::Skipped(_) => None,
            Outcome::Unchanged(plan) | Outcome::Applied(plan) | Outcome::Restored(plan) => {
                Some(plan)
            }
        }
    }

    /// Whether the window was written to.
    pub fn applied(&self) -> bool {
        matches!(self, Outcome::Applied(_) | Outcome::Restored(_))
    }

    pub fn reason(&self) -> &'static str {
        match self {
            Outcome::Skipped(reason) => reason,
            Outcome::Unchanged(_) => "unchanged",
            Outcome::Applied(_) => "",
            Outcome::Restored(_) => "restored",
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
    /// The window's `#{window_layout}` as tmux has it now.
    fn window_layout(&mut self, target: &str) -> Option<String>;
    /// The window's team tags; None for a window that is not a team's.
    fn window_identity(&mut self, target: &str) -> Option<WindowIdentity>;
}

struct RealTmux;

impl TmuxOps for RealTmux {
    fn window_id(&mut self, target: &str) -> Option<String> {
        crate::tmux::get_window_id(target)
    }
    fn window_layout(&mut self, target: &str) -> Option<String> {
        crate::tmux::display_value(target, "#{window_layout}")
    }
    fn window_identity(&mut self, target: &str) -> Option<WindowIdentity> {
        let row = crate::tmux::display_value(
            target,
            "#{@hive-team}\t#{@hive-workspace}\t#{@hive-created}",
        )?;
        let mut parts = row.split('\t');
        let team = parts.next().unwrap_or_default().to_string();
        if team.is_empty() {
            return None;
        }
        Some(WindowIdentity {
            team,
            workspace: parts.next().unwrap_or_default().to_string(),
            instance: parts.next().unwrap_or_default().to_string(),
        })
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

/// Cross-process lock for one window's apply. Two appliers racing (the
/// hook fired by one apply's own `select-layout` landing beside the next
/// spawn) would each see the mirror out of place and both swap it — a
/// double swap puts it back where it was — and the hook must see the key
/// its predecessor wrote. The lock file is named by the window's `@N` id (`lock_key`):
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
    crate::paths::locks_dir().ok()
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
/// a join, a mirror tag) and its apply must land after an apply in flight.
pub fn ensure(window_target: &str, force: bool) -> Outcome {
    if window_target.is_empty() {
        return Outcome::Skipped("no-window");
    }
    let mode = if force { Mode::Force } else { Mode::Explicit };
    let mut tmux = RealTmux;
    ensure_locked(window_target, mode, true, &mut tmux)
}

/// The window hooks' form of [`ensure`]: never forces, never waits — an
/// apply in flight gets a rerun marker instead (see `LockState`) — and,
/// finding the key unchanged, remembers the drag it is looking at.
pub fn ensure_hook(window_target: &str) -> Outcome {
    if window_target.is_empty() {
        return Outcome::Skipped("no-window");
    }
    let mut tmux = RealTmux;
    ensure_locked(window_target, Mode::Hook, false, &mut tmux)
}

/// The apply that closes a build (`hive attach` rebuilding the window, a
/// backfill adding panes): the remembered drag applies whenever it fits,
/// whatever key the window holds — the hooks fire per split and one of
/// them may have planned the half-built window, its last pane not yet
/// tagged for its member, and written the plan's key first.
pub fn ensure_built(window_target: &str) -> Outcome {
    if window_target.is_empty() {
        return Outcome::Skipped("no-window");
    }
    let mut tmux = RealTmux;
    ensure_locked(window_target, Mode::Built, true, &mut tmux)
}

/// Who is asking for the comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A verb that changed the window (spawn, retire, mirror): the plan
    /// when the key differs, nothing when it matches.
    Explicit,
    /// The window hooks: as `Explicit`, and a matching key remembers the
    /// drag the window shows.
    Hook,
    /// `hive layout auto`: the plan, the drag forgotten.
    Force,
    /// A window hive just built: the remembered drag when it fits, the
    /// key notwithstanding; else as `Explicit`.
    Built,
}

/// Remember the `hive mirror` choice for the window's team instance, under
/// the window's apply lock so it lands beside a drag the hook is writing.
pub(crate) fn remember_mirror(window_target: &str, on: bool) {
    let mut tmux = RealTmux;
    let Some(identity) = tmux.window_identity(window_target) else {
        return;
    };
    let key = lock_key(window_target, &mut tmux);
    let _lock = match window_lock(&key, true) {
        LockState::Held(lock) => Some(lock),
        _ => None,
    };
    arrangement::remember_mirror(&identity, on);
}

/// The remembered `hive mirror` choice of a team instance (`Some(false)`
/// withholds the mirror), for a window about to be built.
pub(crate) fn remembered_mirror(team: &str, workspace: &str, instance: &str) -> Option<bool> {
    arrangement::mirror_preference(&WindowIdentity {
        team: team.to_string(),
        workspace: workspace.to_string(),
        instance: instance.to_string(),
    })
}

/// A `hive mirror off` recorded for a team instance, for a test of the
/// window that is built after it.
#[cfg(test)]
pub(crate) fn remember_mirror_for_test(team: &str, workspace: &str, instance: &str) {
    arrangement::remember_mirror(
        &WindowIdentity {
            team: team.to_string(),
            workspace: workspace.to_string(),
            instance: instance.to_string(),
        },
        false,
    );
}

fn ensure_locked(window_target: &str, mode: Mode, wait: bool, tmux: &mut dyn TmuxOps) -> Outcome {
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
    let mut outcome = ensure_with(window_target, mode, tmux);
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
        let again = ensure_with(window_target, mode, tmux);
        // Report the apply that happened, not the no-op that confirmed it.
        if again.applied() || !outcome.applied() {
            outcome = again;
        }
    }
    outcome
}

fn ensure_with(window_target: &str, mode: Mode, tmux: &mut dyn TmuxOps) -> Outcome {
    if window_target.is_empty() {
        return Outcome::Skipped("no-window");
    }
    if tmux.window_zoomed(window_target) {
        // The human zoomed in on a member: a re-tile would both unzoom and
        // rearrange under them. Skip; the unzoom fires the hook.
        return Outcome::Skipped("zoomed");
    }
    if mode == Mode::Force {
        // `hive layout auto`: the human asked for the plan, so the drag
        // the window would otherwise get back is forgotten with it.
        if let Some(identity) = tmux.window_identity(window_target) {
            arrangement::forget_drag(&identity);
        }
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
    if mode != Mode::Force
        && tmux.layout_key(window_target).as_deref() == Some(planned.key.as_str())
    {
        match mode {
            Mode::Hook => remember_drag(window_target, &planned, size, &panes, tmux),
            Mode::Built => {
                let panes = cell_order(window_target, panes, tmux);
                if let Restore::Done(outcome) = restore(window_target, &planned, panes, tmux) {
                    return outcome;
                }
            }
            Mode::Explicit | Mode::Force => {}
        }
        return Outcome::Unchanged(planned);
    }
    // Cells apply in window order: the mirror must be first.
    let panes = cell_order(window_target, panes, tmux);
    let panes = if mode == Mode::Force {
        panes
    } else {
        match restore(window_target, &planned, panes, tmux) {
            Restore::Done(outcome) => return outcome,
            Restore::Plan(panes) => panes,
        }
    };
    let Some(planned) = plan(size, &panes) else {
        return Outcome::Skipped("no-plan");
    };
    if !tmux.select_layout(window_target, &planned.layout) {
        return Outcome::Skipped("rejected");
    }
    tmux.set_layout_key(window_target, &planned.key);
    Outcome::Applied(planned)
}

/// The hook found the plan's key on the window: a layout away from the
/// plan's is the human's drag (or a preset), remembered for the window's
/// next rebuild with its panes in window order — the order cells apply
/// in. A layout equal to the plan's is the plan, nothing to remember.
fn remember_drag(
    window_target: &str,
    planned: &Plan,
    size: (i64, i64),
    panes: &[PaneInfo],
    tmux: &mut dyn TmuxOps,
) {
    let Some(layout) = tmux.window_layout(window_target) else {
        return;
    };
    if layout == planned.layout {
        return;
    }
    // The panes and the layout are two reads; a split landing between
    // them (a window being built) makes a layout whose leaves are not the
    // panes listed. That is no arrangement of these panes.
    let ids = layout_pane_ids(&layout);
    if ids.len() != panes.len() || !ids.iter().all(|id| panes.iter().any(|p| p.pane_id == *id)) {
        return;
    }
    let Some(identity) = tmux.window_identity(window_target) else {
        return;
    };
    let drag = arrangement::Drag {
        plan_key: planned.key.clone(),
        size,
        layout,
        leaves: panes
            .iter()
            .map(|p| arrangement::Leaf {
                member: p.agent.clone(),
                role: p.role.clone(),
            })
            .collect(),
    };
    arrangement::remember_drag(&identity, &drag);
}

/// The pane ids on the leaves of a tmux layout string, in string order:
/// `<csum>,` then cells `WxH,x,y` each followed by `,<pane id>` (a leaf),
/// `{…}` (children side by side) or `[…]` (stacked).
fn layout_pane_ids(layout: &str) -> Vec<String> {
    let body = layout.split_once(',').map(|(_, body)| body).unwrap_or("");
    let mut ids = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        // WxH,x,y
        for _ in 0..3 {
            rest = rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == 'x');
            if let Some(after) = rest.strip_prefix(',') {
                rest = after;
            }
        }
        // the cell's own tail: a leaf's pane id, or a split's children
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            ids.push(format!("%{}", &rest[..digits]));
            rest = &rest[digits..];
        }
        // separators between siblings and closers of splits
        rest = rest.trim_start_matches([',', '{', '[', '}', ']']);
    }
    ids
}

enum Restore {
    Done(Outcome),
    /// No drag for this window: the panes, in their order now, for the plan.
    Plan(Vec<PaneInfo>),
}

/// The remembered drag, when it fits the window the plan describes: the
/// same plan key, and one leaf per pane naming its member and role. The
/// panes are swapped into leaf order and the drag's layout applied under
/// the plan's key — from then on it holds as a drag does, until the plan
/// changes.
fn restore(
    window_target: &str,
    planned: &Plan,
    mut panes: Vec<PaneInfo>,
    tmux: &mut dyn TmuxOps,
) -> Restore {
    let Some(identity) = tmux.window_identity(window_target) else {
        return Restore::Plan(panes);
    };
    let Some(drag) = arrangement::drag(&identity) else {
        return Restore::Plan(panes);
    };
    if drag.plan_key != planned.key {
        return Restore::Plan(panes);
    }
    let Some(order) = leaf_order(&drag.leaves, &panes) else {
        return Restore::Plan(panes);
    };
    for (at, want) in order.iter().enumerate() {
        if panes[at].pane_id == *want {
            continue;
        }
        // Re-read after the swap rather than trusting it (see `cell_order`).
        tmux.swap_pane(want, &panes[at].pane_id.clone());
        panes = tmux.list_panes_full(window_target);
        if panes.get(at).map(|p| p.pane_id.as_str()) != Some(want.as_str()) {
            return Restore::Plan(panes);
        }
    }
    if !tmux.select_layout(window_target, &drag.layout) {
        // A layout tmux refuses is no arrangement for this window.
        arrangement::forget_drag(&identity);
        return Restore::Plan(panes);
    }
    tmux.set_layout_key(window_target, &planned.key);
    Restore::Done(Outcome::Restored(planned.clone()))
}

/// Pane ids in leaf order: each leaf takes the first pane of its member
/// and role not taken yet. None unless every leaf and every pane pair up.
fn leaf_order(leaves: &[arrangement::Leaf], panes: &[PaneInfo]) -> Option<Vec<String>> {
    if leaves.len() != panes.len() {
        return None;
    }
    let mut taken = vec![false; panes.len()];
    let mut order = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        let at = panes
            .iter()
            .enumerate()
            .position(|(i, p)| !taken[i] && p.agent == leaf.member && p.role == leaf.role)?;
        taken[at] = true;
        order.push(panes[at].pane_id.clone());
    }
    Some(order)
}

/// Swap the mirror to the front of the window order, re-reading after
/// the swap rather than trusting it: the layout string is applied by
/// window order, so it must describe what tmux has now.
fn cell_order(window: &str, mut panes: Vec<PaneInfo>, tmux: &mut dyn TmuxOps) -> Vec<PaneInfo> {
    if let Some(at) = panes.iter().position(|p| p.role == "mirror") {
        if at != 0 {
            tmux.swap_pane(&panes[at].pane_id.clone(), &panes[0].pane_id.clone());
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
        /// One layout string tmux refuses (a bad checksum, say).
        reject_layout: Option<String>,
        /// `#{window_layout}`: what the last accepted apply wrote, or what
        /// a test says the human dragged it to.
        layout: Option<String>,
        identity: Option<WindowIdentity>,
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
            let accepted = !self.reject && self.reject_layout.as_deref() != Some(layout);
            if accepted {
                self.layout = Some(layout.to_string());
            }
            accepted
        }
        fn window_layout(&mut self, _target: &str) -> Option<String> {
            self.layout.clone()
        }
        fn window_identity(&mut self, _target: &str) -> Option<WindowIdentity> {
            self.identity.clone()
        }
    }

    /// mirror + two members, tagged, as `list-panes` reads them.
    const TEAM: [(&str, &str, &str); 3] = [
        ("%1", "mirror", "orch"),
        ("%2", "agent", "scout"),
        ("%3", "agent", "sage"),
    ];

    fn tagged(spec: &[(&str, &str, &str)]) -> Vec<PaneInfo> {
        spec.iter()
            .map(|(id, role, agent)| PaneInfo {
                pane_id: id.to_string(),
                role: role.to_string(),
                agent: agent.to_string(),
                ..Default::default()
            })
            .collect()
    }

    fn identity(tmp: &tempfile::TempDir, instance: &str) -> WindowIdentity {
        WindowIdentity {
            team: "honey".to_string(),
            workspace: tmp.path().to_string_lossy().into_owned(),
            instance: instance.to_string(),
        }
    }

    /// A team window whose arrangement store is a temp workspace.
    fn arranged(size: (i64, i64), spec: &[(&str, &str, &str)]) -> (tempfile::TempDir, FakeTmux) {
        let tmp = tempfile::tempdir().unwrap();
        let mut tmux = FakeTmux::new(size, &[]);
        tmux.panes = tagged(spec);
        tmux.identity = Some(identity(&tmp, "100.0"));
        (tmp, tmux)
    }

    /// A layout string a human could have dragged the 220x60 team window
    /// to: mirror 60 wide, scout squeezed to 10 rows.
    fn dragged() -> String {
        let body = "220x60,0,0{60x60,0,0,1,159x60,61,0[159x10,61,0,2,159x49,61,11,3]}";
        format!("{:04x},{body}", layout_checksum(body))
    }

    /// The drag the hook would have remembered for `TEAM` at 220x60.
    fn remembered(tmp: &tempfile::TempDir, leaves: &[(&str, &str)]) -> arrangement::Drag {
        let drag = arrangement::Drag {
            plan_key: expected_tagged((220, 60), &TEAM).key,
            size: (220, 60),
            layout: dragged(),
            leaves: leaves
                .iter()
                .map(|(member, role)| arrangement::Leaf {
                    member: member.to_string(),
                    role: role.to_string(),
                })
                .collect(),
        };
        assert!(arrangement::remember_drag(&identity(tmp, "100.0"), &drag));
        drag
    }

    fn expected_tagged(size: (i64, i64), spec: &[(&str, &str, &str)]) -> Plan {
        plan(size, &tagged(spec)).unwrap()
    }

    fn layouts(tmux: &FakeTmux) -> Vec<String> {
        tmux.calls
            .iter()
            .filter(|c| c[0] == "layout")
            .map(|c| c[2].clone())
            .collect()
    }

    #[test]
    fn test_the_hook_remembers_a_drag_but_not_the_plan_itself() {
        let (tmp, mut tmux) = arranged((220, 60), &TEAM);
        let planned = expected_tagged((220, 60), &TEAM);
        tmux.key = Some(planned.key.clone());
        let me = identity(&tmp, "100.0");

        // the window shows the plan: nothing to remember
        tmux.layout = Some(planned.layout.clone());
        assert_eq!(
            ensure_with("dev:0", Mode::Hook, &mut tmux),
            Outcome::Unchanged(planned.clone())
        );
        assert_eq!(arrangement::drag(&me), None);

        // the human dragged a border: the hook remembers the window as it is
        tmux.layout = Some(dragged());
        assert_eq!(
            ensure_with("dev:0", Mode::Hook, &mut tmux),
            Outcome::Unchanged(planned.clone())
        );
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
        let drag = arrangement::drag(&me).unwrap();
        assert_eq!(drag.plan_key, planned.key);
        assert_eq!(drag.size, (220, 60));
        assert_eq!(drag.layout, dragged());
        let leaves: Vec<(&str, &str)> = drag
            .leaves
            .iter()
            .map(|l| (l.member.as_str(), l.role.as_str()))
            .collect();
        assert_eq!(
            leaves,
            vec![("orch", "mirror"), ("scout", "agent"), ("sage", "agent")]
        );

        // an explicit ensure (spawn, attach) observes, it does not record
        let body = "220x60,0,0{100x60,0,0,1,119x60,101,0[119x30,101,0,2,119x29,101,31,3]}";
        tmux.layout = Some(format!("{:04x},{body}", layout_checksum(body)));
        assert_eq!(
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
            Outcome::Unchanged(planned)
        );
        assert_eq!(arrangement::drag(&me).unwrap().layout, dragged());
    }

    #[test]
    fn test_a_rebuilt_window_gets_the_drag_back_in_leaf_order() {
        let (tmp, _) = arranged((220, 60), &TEAM);
        remembered(
            &tmp,
            &[("orch", "mirror"), ("sage", "agent"), ("scout", "agent")],
        );
        // `hive attach` rebuilt the window: new pane ids, roster order, no key
        let mut tmux = FakeTmux::new((220, 60), &[]);
        tmux.panes = tagged(&[
            ("%7", "agent", "sage"),
            ("%8", "agent", "scout"),
            ("%9", "mirror", "orch"),
        ]);
        tmux.identity = Some(identity(&tmp, "100.0"));
        let planned = expected_tagged((220, 60), &TEAM);

        let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);

        // the plan is the one for these panes (their ids label its leaves);
        // its key is the drag's
        assert!(matches!(outcome, Outcome::Restored(_)), "{outcome:?}");
        assert_eq!(outcome.plan().unwrap().key, planned.key);
        assert_eq!(outcome.reason(), "restored");
        assert!(outcome.applied());
        // the mirror swaps first as always, then sage takes the second leaf
        assert_eq!(tmux.calls[0], vec!["swap", "%9", "%7"]);
        assert_eq!(tmux.calls[1], vec!["swap", "%7", "%8"]);
        assert_eq!(tmux.order(), vec!["%9", "%7", "%8"]);
        // the drag's layout lands, not the plan's, under the plan's key
        assert_eq!(layouts(&tmux), vec![dragged()]);
        assert_eq!(tmux.key, Some(planned.key));
    }

    #[test]
    fn test_a_built_window_gets_the_drag_back_even_when_a_hook_planned_it_first() {
        let (tmp, mut tmux) = arranged((220, 60), &TEAM);
        remembered(
            &tmp,
            &[("orch", "mirror"), ("sage", "agent"), ("scout", "agent")],
        );
        // The hook fired by the last split planned the window (its third
        // pane untagged then) and wrote the plan's key.
        let planned = expected_tagged((220, 60), &TEAM);
        tmux.key = Some(planned.key.clone());
        tmux.layout = Some(planned.layout.clone());

        // A verb's ensure leaves a window whose key matches alone…
        assert_eq!(
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
            Outcome::Unchanged(planned.clone())
        );
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
        // …the build's apply asks the store anyway.
        let outcome = ensure_with("dev:0", Mode::Built, &mut tmux);
        assert!(matches!(outcome, Outcome::Restored(_)), "{outcome:?}");
        assert_eq!(tmux.calls[0], vec!["swap", "%3", "%2"]);
        assert_eq!(layouts(&tmux), vec![dragged()]);
        assert_eq!(tmux.key, Some(planned.key));
    }

    #[test]
    fn test_a_member_killed_back_to_the_dragged_count_gets_the_drag_back() {
        let (tmp, mut tmux) = arranged((220, 60), &TEAM);
        remembered(
            &tmp,
            &[("orch", "mirror"), ("scout", "agent"), ("sage", "agent")],
        );
        // a fourth member was planned over the drag…
        tmux.panes.push(PaneInfo {
            pane_id: "%4".to_string(),
            role: "agent".to_string(),
            agent: "ant".to_string(),
            ..Default::default()
        });
        let four = ensure_with("dev:0", Mode::Explicit, &mut tmux);
        assert!(matches!(four, Outcome::Applied(_)), "{four:?}");
        tmux.calls.clear();
        // …and killed: the window is back to the plan the drag held under
        tmux.panes.truncate(3);
        let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);
        assert!(matches!(outcome, Outcome::Restored(_)), "{outcome:?}");
        assert_eq!(layouts(&tmux), vec![dragged()]);
    }

    #[test]
    fn test_the_drag_is_ignored_for_another_plan_other_members_or_another_instance() {
        let (tmp, _) = arranged((220, 60), &TEAM);
        remembered(
            &tmp,
            &[("orch", "mirror"), ("scout", "agent"), ("sage", "agent")],
        );
        let planned_only = |mut tmux: FakeTmux| {
            let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);
            assert!(matches!(outcome, Outcome::Applied(_)), "{outcome:?}");
            assert_eq!(layouts(&tmux), vec![outcome.plan().unwrap().layout.clone()]);
        };
        let window = |size: (i64, i64), spec: &[(&str, &str, &str)], instance: &str| {
            let mut tmux = FakeTmux::new(size, &[]);
            tmux.panes = tagged(spec);
            tmux.identity = Some(identity(&tmp, instance));
            tmux
        };
        // one member fewer: another plan
        planned_only(window((220, 60), &TEAM[..2], "100.0"));
        // the same count, another member on a leaf
        planned_only(window(
            (220, 60),
            &[
                ("%1", "mirror", "orch"),
                ("%2", "agent", "scout"),
                ("%3", "agent", "ghost"),
            ],
            "100.0",
        ));
        // the mirror withheld: another plan
        planned_only(window(
            (220, 60),
            &[
                ("%2", "agent", "scout"),
                ("%3", "agent", "sage"),
                ("%4", "agent", "orch"),
            ],
            "100.0",
        ));
        // a portrait window: another plan
        planned_only(window((100, 90), &TEAM, "100.0"));
        // a recycled name's successor
        planned_only(window((220, 60), &TEAM, "200.0"));
        // the drag itself is still there for the window it fits
        assert!(arrangement::drag(&identity(&tmp, "100.0")).is_some());
    }

    #[test]
    fn test_a_drag_tmux_refuses_is_forgotten_and_the_plan_applied() {
        let (tmp, mut tmux) = arranged((220, 60), &TEAM);
        remembered(
            &tmp,
            &[("orch", "mirror"), ("scout", "agent"), ("sage", "agent")],
        );
        tmux.reject_layout = Some(dragged());

        let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);

        let planned = expected_tagged((220, 60), &TEAM);
        assert_eq!(outcome, Outcome::Applied(planned.clone()));
        assert_eq!(layouts(&tmux), vec![dragged(), planned.layout]);
        assert_eq!(arrangement::drag(&identity(&tmp, "100.0")), None);
    }

    #[test]
    fn test_layout_auto_forgets_the_drag_and_applies_the_plan() {
        let (tmp, mut tmux) = arranged((220, 60), &TEAM);
        remembered(
            &tmp,
            &[("orch", "mirror"), ("scout", "agent"), ("sage", "agent")],
        );
        let planned = expected_tagged((220, 60), &TEAM);
        tmux.key = Some(planned.key.clone());
        tmux.layout = Some(dragged());

        assert_eq!(
            ensure_with("dev:0", Mode::Force, &mut tmux),
            Outcome::Applied(planned.clone())
        );

        assert_eq!(layouts(&tmux), vec![planned.layout.clone()]);
        assert_eq!(arrangement::drag(&identity(&tmp, "100.0")), None);
        // the hook that the apply fires sees the plan: still nothing remembered
        assert_eq!(
            ensure_with("dev:0", Mode::Hook, &mut tmux),
            Outcome::Unchanged(planned)
        );
        assert_eq!(arrangement::drag(&identity(&tmp, "100.0")), None);
    }

    #[test]
    fn test_layout_pane_ids_reads_the_leaves_of_a_layout_string() {
        assert_eq!(layout_pane_ids("b25d,80x24,0,0,0"), vec!["%0"]);
        assert_eq!(
            layout_pane_ids("c195,80x24,0,0[80x12,0,0,0,80x11,0,13,1]"),
            vec!["%0", "%1"]
        );
        assert_eq!(
            layout_pane_ids(
                "1c39,200x50,0,0{60x50,0,0,0,139x50,61,0[139x25,61,0,1,139x24,61,26{69x24,61,26,3,69x24,131,26,4}]}"
            ),
            vec!["%0", "%1", "%3", "%4"]
        );
        assert_eq!(layout_pane_ids(&dragged()), vec!["%1", "%2", "%3"]);
        assert!(layout_pane_ids("").is_empty());
    }

    #[test]
    fn test_the_hook_does_not_remember_a_layout_whose_leaves_are_not_the_panes_listed() {
        // A window being built: the hook listed two panes, the third split
        // landed, and the layout it then read has three leaves.
        let (tmp, mut tmux) = arranged((220, 60), &TEAM[..2]);
        let planned = expected_tagged((220, 60), &TEAM[..2]);
        tmux.key = Some(planned.key.clone());
        tmux.layout = Some(dragged());

        assert_eq!(
            ensure_with("dev:0", Mode::Hook, &mut tmux),
            Outcome::Unchanged(planned)
        );

        assert_eq!(arrangement::drag(&identity(&tmp, "100.0")), None);
    }

    #[test]
    fn test_leaf_order_pairs_leaves_with_panes_by_member_and_role() {
        let leaf = |member: &str, role: &str| arrangement::Leaf {
            member: member.to_string(),
            role: role.to_string(),
        };
        let panes = tagged(&[
            ("%1", "agent", "sage"),
            ("%2", "", ""),
            ("%3", "mirror", "orch"),
            ("%4", "", ""),
        ]);
        // two shell panes pair up positionally
        assert_eq!(
            leaf_order(
                &[
                    leaf("orch", "mirror"),
                    leaf("", ""),
                    leaf("sage", "agent"),
                    leaf("", "")
                ],
                &panes
            ),
            Some(vec![
                "%3".to_string(),
                "%2".to_string(),
                "%1".to_string(),
                "%4".to_string()
            ])
        );
        // a leaf with no pane, a pane with no leaf
        assert_eq!(
            leaf_order(&[leaf("orch", "mirror"), leaf("sage", "agent")], &panes),
            None
        );
        assert_eq!(
            leaf_order(
                &[
                    leaf("orch", "agent"),
                    leaf("", ""),
                    leaf("sage", "agent"),
                    leaf("", "")
                ],
                &panes
            ),
            None
        );
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
            ensure_locked("dev:0", Mode::Hook, false, &mut tmux),
            Outcome::Skipped("busy")
        );
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
        assert!(rerun_marker(&key).unwrap().exists());

        // The holder finishes: the next apply takes the lock, plans, and
        // consumes the marker.
        drop(held);
        let outcome = ensure_locked("dev:0", Mode::Hook, false, &mut tmux);
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

        let outcome = ensure_locked("dev:0", Mode::Explicit, true, &mut tmux);

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
            ensure_with("", Mode::Explicit, &mut tmux),
            Outcome::Skipped("no-window")
        );
        assert!(tmux.calls.is_empty());
    }

    #[test]
    fn test_ensure_single_pane_skips() {
        let mut tmux = FakeTmux::new((200, 50), &[("%1", "agent")]);
        assert_eq!(
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
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
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
            Outcome::Applied(pair.clone())
        );
        tmux.calls.clear();
        tmux.panes.truncate(1);
        assert_eq!(
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
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
        let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);
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
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
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
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
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
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
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
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
            Outcome::Unchanged(planned)
        );
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
        // A proportional resize keeps the key: still nothing to write.
        tmux.size = (200, 55);
        let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);
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
            ensure_with("dev:0", Mode::Force, &mut tmux),
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
        let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);
        assert!(outcome.applied(), "{outcome:?}");
        assert!(outcome.plan().unwrap().key.contains("/m3/"));
        // a flip changes it too
        tmux.calls.clear();
        tmux.size = (60, 80);
        let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);
        assert!(outcome.applied(), "{outcome:?}");
        assert_eq!(outcome.plan().unwrap().orientation, "portrait");
    }

    #[test]
    fn test_ensure_swaps_the_mirror_first_and_generates_layout() {
        let mut tmux = FakeTmux::new(
            (220, 60),
            &[("%1", "agent"), ("%2", "agent"), ("%3", "mirror")],
        );
        let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);
        assert!(outcome.applied(), "{outcome:?}");
        assert_eq!(tmux.calls.len(), 3);
        // the mirror (last in window order) swaps with the first pane…
        assert_eq!(tmux.calls[0], vec!["swap", "%3", "%1"]);
        assert_eq!(tmux.order(), vec!["%3", "%2", "%1"]);
        // …so the mirror's cell comes first and the members stack in the
        // new order beside it
        let layout = &tmux.calls[1][2];
        assert!(layout.contains("{109x60,0,0,3,"), "{layout}");
        assert!(
            layout.ends_with("[110x29,110,0,2,110x30,110,30,1]}"),
            "{layout}"
        );
    }

    #[test]
    fn test_ensure_with_mirror_already_first_does_not_swap() {
        let mut tmux = FakeTmux::new(
            (220, 60),
            &[("%1", "mirror"), ("%2", "agent"), ("%3", "agent")],
        );
        ensure_with("dev:0", Mode::Explicit, &mut tmux);
        assert_eq!(tmux.calls.len(), 2);
        assert_eq!(tmux.calls[0][0], "layout");
        assert!(
            tmux.calls[0][2].contains("{109x60,0,0,1,"),
            "{}",
            tmux.calls[0][2]
        );
    }

    #[test]
    fn test_ensure_matching_key_skips_the_swap_too() {
        let spec = [("%1", "mirror"), ("%2", "agent")];
        let mut tmux = FakeTmux::new((200, 50), &[("%1", "agent"), ("%2", "mirror")]);
        tmux.key = Some(expected((200, 50), &spec).key);
        let outcome = ensure_with("dev:0", Mode::Explicit, &mut tmux);
        assert!(!outcome.applied(), "{outcome:?}");
        assert!(tmux.calls.is_empty(), "{:?}", tmux.calls);
    }

    #[test]
    fn test_ensure_keeps_the_old_key_when_tmux_rejects_the_layout() {
        let mut tmux = FakeTmux::new((200, 50), &[("%1", "agent"), ("%2", "agent")]);
        tmux.reject = true;
        assert_eq!(
            ensure_with("dev:0", Mode::Explicit, &mut tmux),
            Outcome::Skipped("rejected")
        );
        assert_eq!(tmux.calls.len(), 1);
        assert_eq!(tmux.key, None);
    }
}
