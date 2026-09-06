//! Worktree pool plumbing behind `hive worktree start/done/status`.
//!
//! Pool layout: `<main checkout root>/.claude/worktrees/<feature>` — one
//! worktree per feature branch (branch == feature), per repo. Git itself is
//! the source of truth: the worktree<->branch<->path mapping is read live
//! from `git worktree list` and ownership/audit marks live in
//! `git config branch.<feature>.hive-*`. Hive keeps no registry file.
//!
//! Hive only creates/removes worktrees and reads state. Entering/leaving a
//! worktree is the agent's own move (a CLI subprocess cannot change the agent
//! process cwd), and gh/PR work stays on the agent side.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub const POOL_SEGMENTS: [&str; 2] = [".claude", "worktrees"];

/// `branch.<feature>.<key>` written on a ready start; cleared by done.
pub const META_KEYS: [&str; 5] = [
    "hive-owner",
    "hive-team",
    "hive-base",
    "hive-base-oid",
    "hive-created",
];
pub const GH_MERGE_BASE_KEY: &str = "gh-merge-base";

const PR_BASE_PREFIXES: [&str; 3] = ["refs/remotes/origin/", "origin/", "refs/heads/"];

const IN_PROGRESS_MARKERS: [(&str, &str); 6] = [
    ("rebase-merge", "rebase"),
    ("rebase-apply", "rebase"),
    ("MERGE_HEAD", "merge"),
    ("CHERRY_PICK_HEAD", "cherry-pick"),
    ("REVERT_HEAD", "revert"),
    ("BISECT_LOG", "bisect"),
];

pub const READY_MODES: [&str; 4] = ["created", "existing", "attached", "adopted-existing-branch"];

/// User-actionable failure; the message is the CLI error text.
#[derive(Debug, Clone)]
pub struct WorktreeError(pub String);

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for WorktreeError {}

pub type Result<T> = std::result::Result<T, WorktreeError>;

struct Completed {
    code: i32,
    stdout: String,
    stderr: String,
}

fn git(args: &[&str], cwd: Option<&Path>, timeout: f64) -> Result<Completed> {
    let mut cmd = Command::new("git");
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(WorktreeError("git not found on PATH".to_string()))
        }
        Err(e) => return Err(WorktreeError(format!("git failed to spawn: {e}"))),
    };
    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    let out_h = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out_pipe.read_to_string(&mut s);
        s
    });
    let err_h = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let head: Vec<&str> = args.iter().take(2).copied().collect();
                    return Err(WorktreeError(format!(
                        "git {} timed out after {:.0}s",
                        head.join(" "),
                        timeout
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(WorktreeError(format!("git wait failed: {e}"))),
        }
    };
    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    Ok(Completed {
        code,
        stdout,
        stderr,
    })
}

fn git_ok(args: &[&str], cwd: Option<&Path>, timeout: f64) -> Result<String> {
    let r = git(args, cwd, timeout)?;
    if r.code != 0 {
        let detail = if r.stderr.is_empty() {
            &r.stdout
        } else {
            &r.stderr
        };
        return Err(WorktreeError(format!(
            "git {} failed: {}",
            args.join(" "),
            detail.trim()
        )));
    }
    Ok(r.stdout)
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Non-strict resolve: canonicalize the deepest existing
/// ancestor and append the remaining segments verbatim.
fn resolve_path(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    let mut base = abs.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !base.exists() {
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                base = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = std::fs::canonicalize(&base).unwrap_or(base);
    for seg in tail.iter().rev() {
        out.push(seg);
    }
    out
}

// ponytail: naive single-quoted repr (no escaping) — branch names with
// quotes/control chars would render oddly; none do.
fn py_repr(s: &str) -> String {
    format!("'{s}'")
}

// --- Repo anchoring -----------------------------------------------------------

/// Main checkout root, stable from inside any linked worktree.
///
/// Derived from the *common* git dir so that running `start` while standing
/// in another feature worktree still lands new worktrees in the main pool
/// instead of nesting them under the current worktree.
pub fn repo_anchor(cwd: Option<&Path>) -> Result<PathBuf> {
    let r = git(
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd,
        10.0,
    )?;
    if r.code != 0 {
        return Err(WorktreeError("not inside a git repository".to_string()));
    }
    let common = PathBuf::from(r.stdout.trim());
    Ok(match common.parent() {
        Some(p) => p.to_path_buf(),
        None => common.clone(),
    })
}

pub fn pool_root(anchor: &Path) -> PathBuf {
    let mut p = anchor.to_path_buf();
    for seg in POOL_SEGMENTS {
        p.push(seg);
    }
    p
}

pub fn feature_path(anchor: &Path, feature: &str) -> PathBuf {
    pool_root(anchor).join(feature)
}

pub fn validate_feature(feature: &str) -> Result<()> {
    if feature.is_empty() || feature.trim() != feature {
        return Err(WorktreeError(format!(
            "invalid feature name: {}",
            py_repr(feature)
        )));
    }
    let r = git(&["check-ref-format", "--branch", feature], None, 10.0)?;
    if r.code != 0 {
        return Err(WorktreeError(format!(
            "invalid feature name (not a valid branch name): {}",
            py_repr(feature)
        )));
    }
    Ok(())
}

// --- Git state reads ----------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct WorktreeInfo {
    pub path: String,
    pub head: String,
    /// short branch name; "" when detached/bare
    pub branch: String,
    pub prunable: bool,
    pub is_main: bool,
}

pub fn list_worktrees(anchor: &Path) -> Result<Vec<WorktreeInfo>> {
    let out = git_ok(&["worktree", "list", "--porcelain"], Some(anchor), 15.0)?;
    let mut items: Vec<WorktreeInfo> = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(c) = current.take() {
                items.push(c);
            }
            current = Some(WorktreeInfo {
                path: rest.to_string(),
                ..Default::default()
            });
        } else if let Some(c) = current.as_mut() {
            if let Some(rest) = line.strip_prefix("HEAD ") {
                c.head = rest.to_string();
            } else if let Some(refname) = line.strip_prefix("branch ") {
                c.branch = refname
                    .strip_prefix("refs/heads/")
                    .unwrap_or(refname)
                    .to_string();
            } else if line.starts_with("prunable") {
                c.prunable = true;
            }
        }
    }
    if let Some(c) = current.take() {
        items.push(c);
    }
    // git-worktree(1): the main worktree is listed first, linked ones after.
    if let Some(first) = items.first_mut() {
        first.is_main = true;
    }
    Ok(items)
}

pub fn find_feature_worktree<'a>(
    worktrees: &'a [WorktreeInfo],
    feature: &str,
) -> Option<&'a WorktreeInfo> {
    worktrees
        .iter()
        .find(|wt| wt.branch == feature && !wt.is_main)
}

pub fn branch_exists(anchor: &Path, feature: &str) -> Result<bool> {
    let refname = format!("refs/heads/{feature}");
    let r = git(
        &["show-ref", "--verify", "--quiet", &refname],
        Some(anchor),
        10.0,
    )?;
    Ok(r.code == 0)
}

pub fn rev_parse(anchor: &Path, refname: &str) -> Result<String> {
    let spec = format!("{refname}^{{commit}}");
    let r = git(
        &["rev-parse", "--verify", "--quiet", &spec],
        Some(anchor),
        10.0,
    )?;
    if r.code != 0 {
        return Err(WorktreeError(format!(
            "cannot resolve '{refname}' to a commit"
        )));
    }
    Ok(r.stdout.trim().to_string())
}

pub fn is_ancestor(anchor: &Path, ancestor_oid: &str, refname: &str) -> Result<bool> {
    let r = git(
        &["merge-base", "--is-ancestor", ancestor_oid, refname],
        Some(anchor),
        10.0,
    )?;
    Ok(r.code == 0)
}

pub fn worktree_dirty(wt_path: &str) -> Result<bool> {
    let out = git_ok(
        &["status", "--porcelain", "--untracked-files=all"],
        Some(Path::new(wt_path)),
        20.0,
    )?;
    Ok(!out.trim().is_empty())
}

/// Names of git operations mid-flight in *wt_path* (rebase, merge, ...).
pub fn in_progress_ops(wt_path: &str) -> Result<Vec<String>> {
    let mut args: Vec<&str> = vec!["rev-parse"];
    for (marker, _) in IN_PROGRESS_MARKERS.iter() {
        args.push("--git-path");
        args.push(marker);
    }
    let out = git_ok(&args, Some(Path::new(wt_path)), 10.0)?;
    let paths: Vec<&str> = out.lines().collect();
    let mut ops: Vec<String> = Vec::new();
    let base = Path::new(wt_path);
    for ((_, op), p) in IN_PROGRESS_MARKERS.iter().zip(paths.iter()) {
        let candidate = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            base.join(p)
        };
        if candidate.exists() && !ops.iter().any(|o| o == op) {
            ops.push((*op).to_string());
        }
    }
    Ok(ops)
}

// --- Branch metadata (git config branch.<feature>.hive-*) ----------------------

pub fn read_meta(anchor: &Path, feature: &str) -> Result<HashMap<String, String>> {
    let pattern = format!("^branch\\.{}\\.(hive-|gh-merge-base)", re_escape(feature));
    let r = git(&["config", "--get-regexp", &pattern], Some(anchor), 10.0)?;
    let mut meta: HashMap<String, String> = HashMap::new();
    if r.code != 0 {
        return Ok(meta);
    }
    let prefix = format!("branch.{feature}.");
    for line in r.stdout.lines() {
        let (key, value) = match line.split_once(' ') {
            Some((k, v)) => (k, v),
            None => (line, ""),
        };
        if let Some(short) = key.strip_prefix(prefix.as_str()) {
            meta.insert(short.to_string(), value.to_string());
        }
    }
    Ok(meta)
}

/// Python 3.7+ `re.escape`: backslash-escape the special set only.
fn re_escape(s: &str) -> String {
    const SPECIAL: &str = "()[]{}?*+-|^$\\.&~# \t\n\r\x0b\x0c";
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if SPECIAL.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn write_meta(anchor: &Path, feature: &str, meta: &HashMap<String, String>) -> Result<()> {
    for (key, value) in meta {
        let name = format!("branch.{feature}.{key}");
        git_ok(&["config", &name, value], Some(anchor), 10.0)?;
    }
    Ok(())
}

/// Remove hive-* keys for *feature*; keep gh-merge-base (the branch and its
/// PR remain alive after done — gh still resolves base from it).
pub fn clear_meta(anchor: &Path, feature: &str) -> Result<Vec<String>> {
    let mut cleared: Vec<String> = Vec::new();
    let existing = read_meta(anchor, feature)?;
    for key in META_KEYS {
        if existing.contains_key(key) {
            let name = format!("branch.{feature}.{key}");
            let _ = git(&["config", "--unset", &name], Some(anchor), 10.0)?;
            cleared.push(key.to_string());
        }
    }
    Ok(cleared)
}

pub fn hive_labeled_branches(anchor: &Path) -> Result<Vec<String>> {
    let r = git(
        &["config", "--get-regexp", "^branch\\..*\\.hive-owner$"],
        Some(anchor),
        10.0,
    )?;
    if r.code != 0 {
        return Ok(Vec::new());
    }
    let mut branches: Vec<String> = Vec::new();
    for line in r.stdout.lines() {
        let key = line.split_once(' ').map(|(k, _)| k).unwrap_or(line);
        // branch.<name>.hive-owner — <name> may itself contain dots/slashes.
        let name = key
            .strip_prefix("branch.")
            .and_then(|k| k.strip_suffix(".hive-owner"))
            .unwrap_or("");
        if !name.is_empty() {
            branches.push(name.to_string());
        }
    }
    Ok(branches)
}

// --- Base resolution -----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BaseResolution {
    pub r#ref: String,
    pub oid: String,
    /// explicit | integration | default-branch
    pub source: String,
}

/// Resolve the base ref for a new feature.
///
/// *integration* is the team window's declared integration branch
/// (`hive worktree set-base`), or None when the window never declared one —
/// then the repo's default branch is the base.
pub fn resolve_base(
    anchor: &Path,
    explicit: Option<&str>,
    integration: Option<&str>,
) -> Result<BaseResolution> {
    if let Some(explicit) = explicit.filter(|s| !s.is_empty()) {
        return Ok(BaseResolution {
            r#ref: explicit.to_string(),
            oid: rev_parse(anchor, explicit)?,
            source: "explicit".to_string(),
        });
    }
    if let Some(integration) = integration.filter(|s| !s.is_empty()) {
        return Ok(BaseResolution {
            r#ref: integration.to_string(),
            oid: rev_parse(anchor, integration)?,
            source: "integration".to_string(),
        });
    }
    let r = git(
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        Some(anchor),
        10.0,
    )?;
    let default = if r.code == 0 {
        r.stdout.trim().to_string()
    } else {
        String::new()
    };
    if default.is_empty() {
        return Err(WorktreeError(
            "cannot detect the default branch (no origin/HEAD — repo without a \
             remote, or origin/HEAD unset); pass --base <ref> explicitly"
                .to_string(),
        ));
    }
    let oid = rev_parse(anchor, &default)?;
    Ok(BaseResolution {
        r#ref: default,
        oid,
        source: "default-branch".to_string(),
    })
}

pub fn pr_merge_base_from_ref(base_ref: &str) -> String {
    for prefix in PR_BASE_PREFIXES {
        if let Some(rest) = base_ref.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    base_ref.to_string()
}

// --- start ----------------------------------------------------------------------

/// Field order is the JSON output order; serde_json's `preserve_order`
/// feature (enabled in Cargo.toml) keeps it through a `Value` round-trip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResult {
    pub feature: String,
    pub branch: String,
    pub path: String,
    /// created | existing | attached | adopted-existing-branch | needs-rebase
    pub mode: String,
    pub owner: String,
    pub team: String,
    pub base: String,
    pub base_oid: String,
    pub worktree_root: String,
    pub git_common_dir: String,
    pub warnings: Vec<String>,
}

impl StartResult {
    pub fn ready(&self) -> bool {
        READY_MODES.contains(&self.mode.as_str())
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("StartResult serializes")
    }
}

fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

fn owned_err(feature: &str, stored_owner: &str, owner: &str) -> WorktreeError {
    WorktreeError(format!(
        "feature '{feature}' is owned by '{stored_owner}' (current: '{owner}'); \
         pick another feature name or have the owner release it"
    ))
}

#[allow(clippy::too_many_arguments)]
pub fn start(
    anchor: &Path,
    feature: &str,
    base: &BaseResolution,
    owner: &str,
    team: &str,
    gh_merge_base: Option<&str>,
    now: Option<f64>,
) -> Result<StartResult> {
    validate_feature(feature)?;
    let expected = feature_path(anchor, feature);
    let common_dir = git_ok(
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        Some(anchor),
        10.0,
    )?
    .trim()
    .to_string();

    let mk = |mode: &str, path: Option<&str>, warnings: Vec<String>| -> StartResult {
        StartResult {
            feature: feature.to_string(),
            branch: feature.to_string(),
            path: path
                .map(str::to_string)
                .unwrap_or_else(|| path_str(&expected)),
            mode: mode.to_string(),
            owner: owner.to_string(),
            team: team.to_string(),
            base: base.r#ref.clone(),
            base_oid: base.oid.clone(),
            worktree_root: path_str(&pool_root(anchor)),
            git_common_dir: common_dir.clone(),
            warnings,
        }
    };

    let required_meta = || -> HashMap<String, String> {
        let mut meta: HashMap<String, String> = HashMap::new();
        meta.insert("hive-owner".to_string(), owner.to_string());
        meta.insert(
            "hive-team".to_string(),
            if team.is_empty() {
                owner.to_string()
            } else {
                team.to_string()
            },
        );
        meta.insert("hive-base".to_string(), base.r#ref.clone());
        meta.insert("hive-base-oid".to_string(), base.oid.clone());
        let created = now.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        });
        meta.insert(
            "hive-created".to_string(),
            crate::team::created_at_key(created),
        );
        let gmb = match gh_merge_base.filter(|s| !s.is_empty()) {
            Some(g) => g.to_string(),
            None => pr_merge_base_from_ref(&base.r#ref),
        };
        meta.insert(GH_MERGE_BASE_KEY.to_string(), gmb);
        meta
    };

    // Every ready start must leave the full required config current —
    // notably gh-merge-base when the team's integration branch moved.
    // The first-created timestamp is the only key that survives as-is.
    let sync_ready_meta = |existing: &HashMap<String, String>| -> Result<()> {
        let mut fresh = required_meta();
        if let Some(created) = existing.get("hive-created") {
            fresh.insert("hive-created".to_string(), created.clone());
        }
        let delta: HashMap<String, String> = fresh
            .into_iter()
            .filter(|(k, v)| existing.get(k) != Some(v))
            .collect();
        if !delta.is_empty() {
            write_meta(anchor, feature, &delta)?;
        }
        Ok(())
    };

    let worktrees = list_worktrees(anchor)?;
    // Case-D detection must see every checkout of the branch, the main
    // checkout included — only the expected pool path may proceed.
    let mut checkout: Option<&WorktreeInfo> = worktrees.iter().find(|w| w.branch == feature);
    if let Some(c) = checkout {
        if c.prunable || !Path::new(&c.path).exists() {
            // Stale registration (directory manually removed): prune, then
            // treat the feature as branch-only.
            git_ok(&["worktree", "prune"], Some(anchor), 15.0)?;
            checkout = None;
        }
    }
    if let Some(c) = checkout {
        if resolve_path(Path::new(&c.path)) != resolve_path(&expected) {
            return Err(WorktreeError(format!(
                "branch '{}' is already checked out at {}; \
                 free it there or pick another feature name",
                feature, c.path
            )));
        }
    }
    let wt = checkout;

    let has_branch = branch_exists(anchor, feature)?;
    let meta = if has_branch {
        read_meta(anchor, feature)?
    } else {
        HashMap::new()
    };
    let stored_owner = meta.get("hive-owner").cloned().unwrap_or_default();
    let foreign = !stored_owner.is_empty() && stored_owner != owner;

    if let Some(w) = wt {
        // Case B: worktree exists at the pool path.
        if foreign {
            return Err(owned_err(feature, &stored_owner, owner));
        }
        if !is_ancestor(anchor, &base.oid, feature)? {
            return Ok(mk(
                "needs-rebase",
                Some(&w.path),
                vec![format!(
                    "branch does not contain resolved base {} ({}); \
                     rebase onto it, then rerun start",
                    base.r#ref,
                    short_oid(&base.oid)
                )],
            ));
        }
        if stored_owner.is_empty() {
            write_meta(anchor, feature, &required_meta())?;
            return Ok(mk("adopted-existing-branch", Some(&w.path), Vec::new()));
        }
        sync_ready_meta(&meta)?;
        return Ok(mk("existing", Some(&w.path), Vec::new()));
    }

    if has_branch {
        // Case C: branch exists, no worktree (post-done rework is the common
        // path here — done keeps the branch for the PR's lifetime).
        if foreign {
            return Err(owned_err(feature, &stored_owner, owner));
        }
        let compatible = is_ancestor(anchor, &base.oid, feature)?;
        if let Some(parent) = expected.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| WorktreeError(format!("cannot create {}: {e}", parent.display())))?;
        }
        let expected_str = path_str(&expected);
        git_ok(
            &["worktree", "add", &expected_str, feature],
            Some(anchor),
            120.0,
        )?;
        if !compatible {
            return Ok(mk(
                "needs-rebase",
                None,
                vec![format!(
                    "branch does not contain resolved base {} ({}); \
                     worktree attached so you can rebase, then rerun start",
                    base.r#ref,
                    short_oid(&base.oid)
                )],
            ));
        }
        if stored_owner.is_empty() {
            write_meta(anchor, feature, &required_meta())?;
            return Ok(mk("adopted-existing-branch", None, Vec::new()));
        }
        sync_ready_meta(&meta)?;
        return Ok(mk("attached", None, Vec::new()));
    }

    // Case A: nothing exists yet.
    if let Some(parent) = expected.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| WorktreeError(format!("cannot create {}: {e}", parent.display())))?;
    }
    let expected_str = path_str(&expected);
    git_ok(
        &["worktree", "add", "-b", feature, &expected_str, &base.oid],
        Some(anchor),
        120.0,
    )?;
    write_meta(anchor, feature, &required_meta())?;
    Ok(mk("created", None, Vec::new()))
}

// --- done -----------------------------------------------------------------------

/// Field order is the JSON output order; serde_json's `preserve_order`
/// feature keeps it through a `Value` round-trip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoneResult {
    pub feature: String,
    pub branch: String,
    pub removed_path: String,
    pub branch_kept: bool,
    pub cleared_config_keys: Vec<String>,
    pub forced: bool,
    pub status_summary: String,
    pub warnings: Vec<String>,
}

impl DoneResult {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("DoneResult serializes")
    }
}

pub fn done(anchor: &Path, feature: &str, force: bool, caller_cwd: &str) -> Result<DoneResult> {
    validate_feature(feature)?;
    let worktrees = list_worktrees(anchor)?;
    let wt = match find_feature_worktree(&worktrees, feature) {
        Some(wt) => wt,
        None => {
            return Err(WorktreeError(format!(
                "no worktree found for feature '{feature}' \
                 (see `hive worktree status` — done only removes worktrees, never branches)"
            )))
        }
    };

    let wt_path = resolve_path(Path::new(&wt.path));
    if !caller_cwd.is_empty() && resolve_path(Path::new(caller_cwd)).starts_with(&wt_path) {
        return Err(WorktreeError(format!(
            "you are inside {}; leave the worktree first \
             (ExitWorktree action=keep, or cd back to the main checkout), then rerun done",
            wt.path
        )));
    }

    let mut summary = String::new();
    let stale = !wt_path.exists();
    if !stale {
        let ops = in_progress_ops(&wt.path)?;
        let dirty = worktree_dirty(&wt.path)?;
        if !force {
            if !ops.is_empty() {
                return Err(WorktreeError(format!(
                    "git {} in progress in {}; \
                     finish or abort it, or rerun with --force to discard",
                    ops.join("/"),
                    wt.path
                )));
            }
            if dirty {
                return Err(WorktreeError(format!(
                    "worktree {} has uncommitted changes; commit them, \
                     or rerun with --force to discard them (destructive)",
                    wt.path
                )));
            }
        } else {
            // --force always reports what it is about to discard — a clean
            // tree still gets the summary so the abandon decision is auditable.
            summary = git_ok(
                &["status", "--short", "--branch", "--untracked-files=all"],
                Some(Path::new(&wt.path)),
                20.0,
            )?
            .trim_end()
            .to_string();
            summary.push_str("\n(ignored files are not included in this summary)");
        }
    }

    let mut remove_args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        // Twice: git requires double --force for dirty + locked combinations.
        remove_args.extend(["--force", "--force"]);
    }
    remove_args.push(&wt.path);
    git_ok(&remove_args, Some(anchor), 60.0)?;

    let cleared = clear_meta(anchor, feature)?;
    Ok(DoneResult {
        feature: feature.to_string(),
        branch: feature.to_string(),
        removed_path: wt.path.clone(),
        branch_kept: true,
        cleared_config_keys: cleared,
        forced: force,
        status_summary: summary,
        warnings: Vec::new(),
    })
}

// --- status ----------------------------------------------------------------------

/// Field order is the JSON output order; serde_json's `preserve_order`
/// feature keeps it through a `Value` round-trip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureStatus {
    pub feature: String,
    pub branch_exists: bool,
    pub worktree_path: String,
    pub owner: String,
    pub base: String,
    pub base_oid: String,
    pub current_base_oid: String,
    pub state: String,
    pub dirty: bool,
    pub in_progress: Vec<String>,
    pub stale: bool,
    pub warnings: Vec<String>,
}

pub fn feature_status(anchor: &Path, feature: &str) -> Result<FeatureStatus> {
    validate_feature(feature)?;
    let worktrees = list_worktrees(anchor)?;
    let wt = find_feature_worktree(&worktrees, feature);
    let has_branch = branch_exists(anchor, feature)?;
    let meta = read_meta(anchor, feature)?;

    let base_ref = meta.get("hive-base").cloned().unwrap_or_default();
    let current_base_oid = if base_ref.is_empty() {
        String::new()
    } else {
        rev_parse(anchor, &base_ref).unwrap_or_default()
    };

    let stale = wt
        .map(|w| w.prunable || !Path::new(&w.path).exists())
        .unwrap_or(false);
    let mut dirty = false;
    let mut ops: Vec<String> = Vec::new();
    if let Some(w) = wt {
        if !stale {
            dirty = worktree_dirty(&w.path)?;
            ops = in_progress_ops(&w.path)?;
        }
    }

    let mut warnings: Vec<String> = Vec::new();
    let state = if !has_branch {
        "unknown-branch"
    } else if stale {
        warnings.push(
            "worktree directory is gone; `git worktree prune` (or rerun start) will clean the entry"
                .to_string(),
        );
        "stale"
    } else if wt.is_none() {
        "branch-only"
    } else if !current_base_oid.is_empty() && !is_ancestor(anchor, &current_base_oid, feature)? {
        "needs-rebase"
    } else {
        "active"
    };

    Ok(FeatureStatus {
        feature: feature.to_string(),
        branch_exists: has_branch,
        worktree_path: wt.map(|w| w.path.clone()).unwrap_or_default(),
        owner: meta.get("hive-owner").cloned().unwrap_or_default(),
        base: base_ref,
        base_oid: meta.get("hive-base-oid").cloned().unwrap_or_default(),
        current_base_oid,
        state: state.to_string(),
        dirty,
        in_progress: ops,
        stale,
        warnings,
    })
}

pub fn pool_status(anchor: &Path) -> Result<Vec<FeatureStatus>> {
    let mut features: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for wt in list_worktrees(anchor)? {
        if wt.is_main || wt.branch.is_empty() {
            continue;
        }
        if seen.insert(wt.branch.clone()) {
            features.push(wt.branch);
        }
    }
    for branch in hive_labeled_branches(anchor)? {
        if seen.insert(branch.clone()) {
            features.push(branch);
        }
    }
    features.iter().map(|f| feature_status(anchor, f)).collect()
}

#[cfg(test)]
mod tests {
    //! Real temp git repos, no tmux: this layer covers base resolution, the
    //! start compatibility matrix, done guards, and status classification
    //! against actual git state — git itself is the metadata source of truth,
    //! so faking it would test nothing.

    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

    fn run(args: &[&str], cwd: &Path) -> String {
        let out = Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .output()
            .expect("spawn");
        assert!(
            out.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(repo: &Path, msg: &str) -> String {
        run(&["git", "commit", "--allow-empty", "-m", msg], repo);
        run(&["git", "rev-parse", "HEAD"], repo)
    }

    /// Main checkout with a file remote and origin/HEAD set.
    fn make_repo(tmp: &Path) -> PathBuf {
        let remote = tmp.join("remote.git");
        run(
            &["git", "init", "-q", "--bare", remote.to_str().unwrap()],
            tmp,
        );
        let main = tmp.join("repo");
        let main_s = main.to_str().unwrap().to_string();
        run(&["git", "init", "-q", &main_s], tmp);
        run(
            &[
                "git",
                "-C",
                &main_s,
                "config",
                "user.email",
                "t@example.invalid",
            ],
            tmp,
        );
        run(&["git", "-C", &main_s, "config", "user.name", "t"], tmp);
        commit(&main, "init");
        let default = run(&["git", "symbolic-ref", "--short", "HEAD"], &main);
        run(
            &["git", "remote", "add", "origin", remote.to_str().unwrap()],
            &main,
        );
        run(&["git", "push", "-q", "-u", "origin", &default], &main);
        run(&["git", "remote", "set-head", "origin", "-a"], &main);
        main
    }

    fn repo_fixture() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = make_repo(&root);
        (tmp, repo)
    }

    fn base_of(repo: &Path) -> BaseResolution {
        resolve_base(repo, None, None).unwrap()
    }

    fn start_t(repo: &Path, feature: &str) -> StartResult {
        start(repo, feature, &base_of(repo), "team:t1", "t1", None, None).unwrap()
    }

    fn err_of<T: std::fmt::Debug>(r: Result<T>) -> String {
        match r {
            Err(e) => e.to_string(),
            Ok(v) => panic!("expected error, got {v:?}"),
        }
    }

    // --- anchoring -----------------------------------------------------------

    #[test]
    fn test_repo_anchor_resolves_main_root_from_linked_worktree() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        let anchor_from_inside = repo_anchor(Some(Path::new(&res.path))).unwrap();
        assert_eq!(anchor_from_inside, repo.canonicalize().unwrap());
    }

    #[test]
    fn test_start_from_inside_linked_worktree_lands_in_flat_pool() {
        let (_t, repo) = repo_fixture();
        let res_a = start_t(&repo, "feat-a");
        // as the CLI would, cwd inside feat-a
        let anchor = repo_anchor(Some(Path::new(&res_a.path))).unwrap();
        let res_b = start(
            &anchor,
            "feat-b",
            &resolve_base(&anchor, None, None).unwrap(),
            "team:t1",
            "t1",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            Path::new(&res_b.path).parent().unwrap(),
            pool_root(&repo.canonicalize().unwrap()).as_path()
        );
        assert!(
            !resolve_path(Path::new(&res_b.path)).starts_with(resolve_path(Path::new(&res_a.path)))
        );
    }

    // --- base resolution -----------------------------------------------------

    #[test]
    fn test_resolve_base_detects_default_branch() {
        let (_t, repo) = repo_fixture();
        let base = resolve_base(&repo, None, None).unwrap();
        assert_eq!(base.source, "default-branch");
        assert!(base.r#ref.starts_with("origin/"));
        assert_eq!(base.oid.len(), 40);
    }

    #[test]
    fn test_pr_base_strips_origin_refs_longest_prefix_first() {
        assert_eq!(pr_merge_base_from_ref("origin/main"), "main");
        assert_eq!(pr_merge_base_from_ref("refs/remotes/origin/main"), "main");
        assert_eq!(pr_merge_base_from_ref("refs/heads/develop"), "develop");
        assert_eq!(pr_merge_base_from_ref("main"), "main");
    }

    #[test]
    fn test_resolve_base_explicit_invalid_ref_fails() {
        let (_t, repo) = repo_fixture();
        let err = err_of(resolve_base(&repo, Some("no-such-ref"), None));
        assert!(err.contains("cannot resolve"), "{err}");
    }

    #[test]
    fn test_resolve_base_no_origin_head_hard_fails() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let bare = root.join("solo");
        let bare_s = bare.to_str().unwrap().to_string();
        run(&["git", "init", "-q", &bare_s], &root);
        run(
            &[
                "git",
                "-C",
                &bare_s,
                "config",
                "user.email",
                "t@example.invalid",
            ],
            &root,
        );
        run(&["git", "-C", &bare_s, "config", "user.name", "t"], &root);
        commit(&bare, "x");
        let err = err_of(resolve_base(&bare, None, None));
        assert!(err.contains("default branch"), "{err}");
    }

    #[test]
    fn test_resolve_base_integration_resolves() {
        let (_t, repo) = repo_fixture();
        run(&["git", "branch", "epic-integration"], &repo);
        let base = resolve_base(&repo, None, Some("epic-integration")).unwrap();
        assert_eq!(base.source, "integration");
        assert_eq!(base.r#ref, "epic-integration");
    }

    // --- start matrix --------------------------------------------------------

    #[test]
    fn test_start_creates_worktree_branch_and_meta() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        assert_eq!(res.mode, "created");
        assert!(res.ready());
        assert_eq!(
            PathBuf::from(&res.path),
            feature_path(&repo.canonicalize().unwrap(), "feat-a")
        );
        assert!(Path::new(&res.path).is_dir());
        let meta = read_meta(&repo, "feat-a").unwrap();
        assert_eq!(meta["hive-owner"], "team:t1");
        assert_eq!(meta["hive-base"], res.base);
        assert_eq!(meta["hive-base-oid"], res.base_oid);
        assert!(meta.contains_key("hive-created"));
    }

    #[test]
    fn test_start_is_idempotent_for_existing_worktree() {
        let (_t, repo) = repo_fixture();
        let first = start_t(&repo, "feat-a");
        let second = start_t(&repo, "feat-a");
        assert_eq!(second.mode, "existing");
        assert!(second.ready());
        assert_eq!(second.path, first.path);
    }

    #[test]
    fn test_start_foreign_owner_hard_fails_without_config_overwrite() {
        let (_t, repo) = repo_fixture();
        start_t(&repo, "feat-a");
        run(
            &["git", "config", "branch.feat-a.hive-owner", "team:other"],
            &repo,
        );
        let err = err_of(start(
            &repo,
            "feat-a",
            &base_of(&repo),
            "team:t1",
            "t1",
            None,
            None,
        ));
        assert!(err.contains("owned by 'team:other'"), "{err}");
        assert_eq!(
            read_meta(&repo, "feat-a").unwrap()["hive-owner"],
            "team:other"
        );
    }

    #[test]
    fn test_start_needs_rebase_when_base_advances() {
        let (_t, repo) = repo_fixture();
        start_t(&repo, "feat-a");
        let new_oid = commit(&repo, "advance");
        let stored_before = read_meta(&repo, "feat-a").unwrap()["hive-base-oid"].clone();
        let res = start(
            &repo,
            "feat-a",
            &resolve_base(&repo, Some(&new_oid), None).unwrap(),
            "team:t1",
            "t1",
            None,
            None,
        )
        .unwrap();
        assert_eq!(res.mode, "needs-rebase");
        assert!(!res.ready());
        assert!(!res.warnings.is_empty());
        // hive-base-oid must not advance until the branch actually contains it.
        assert_eq!(
            read_meta(&repo, "feat-a").unwrap()["hive-base-oid"],
            stored_before
        );
    }

    #[test]
    fn test_start_attaches_branch_left_by_manual_worktree_remove() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        run(&["git", "worktree", "remove", &res.path], &repo);
        // meta survives
        assert_eq!(read_meta(&repo, "feat-a").unwrap()["hive-owner"], "team:t1");
        let res2 = start_t(&repo, "feat-a");
        assert_eq!(res2.mode, "attached");
        assert!(res2.ready());
        assert!(Path::new(&res2.path).is_dir());
    }

    #[test]
    fn test_start_adopts_unlabeled_branch_after_done() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        done(&repo, "feat-a", false, repo.to_str().unwrap()).unwrap();
        let res2 = start_t(&repo, "feat-a");
        assert_eq!(res2.mode, "adopted-existing-branch");
        assert!(res2.ready());
        assert_eq!(read_meta(&repo, "feat-a").unwrap()["hive-owner"], "team:t1");
        assert_eq!(res2.path, res.path);
    }

    #[test]
    fn test_start_rejects_branch_checked_out_in_main_checkout() {
        let (_t, repo) = repo_fixture();
        run(&["git", "checkout", "-q", "-b", "feat-z"], &repo);
        let err = err_of(start(
            &repo,
            "feat-z",
            &base_of(&repo),
            "team:t1",
            "t1",
            None,
            None,
        ));
        assert!(err.contains("already checked out at"), "{err}");
    }

    #[test]
    fn test_start_recovers_stale_worktree_entry() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        // simulate manual rm -rf, leaving a prunable entry
        std::fs::remove_dir_all(&res.path).unwrap();
        let res2 = start_t(&repo, "feat-a");
        assert!(res2.ready());
        assert!(Path::new(&res2.path).is_dir());
    }

    #[test]
    fn test_start_integration_writes_gh_merge_base() {
        let (_t, repo) = repo_fixture();
        run(&["git", "branch", "epic-int"], &repo);
        let base = resolve_base(&repo, None, Some("epic-int")).unwrap();
        let res = start(
            &repo,
            "feat-a",
            &base,
            "team:epic",
            "",
            Some("epic-int"),
            None,
        )
        .unwrap();
        assert!(res.ready());
        let meta = read_meta(&repo, "feat-a").unwrap();
        assert_eq!(meta["gh-merge-base"], "epic-int");
    }

    #[test]
    fn test_start_standalone_writes_gh_merge_base_from_origin_base() {
        let (_t, repo) = repo_fixture();
        let base = BaseResolution {
            r#ref: "origin/main".to_string(),
            oid: run(&["git", "rev-parse", "HEAD"], &repo),
            source: "default-branch".to_string(),
        };
        let res = start(&repo, "feat-a", &base, "team:t1", "t1", None, None).unwrap();
        assert!(res.ready());
        assert_eq!(read_meta(&repo, "feat-a").unwrap()["gh-merge-base"], "main");
    }

    #[test]
    fn test_start_rejects_invalid_feature_name() {
        let (_t, repo) = repo_fixture();
        let err = err_of(start(
            &repo,
            "feat..bad",
            &base_of(&repo),
            "team:t1",
            "t1",
            None,
            None,
        ));
        assert!(err.contains("invalid feature name"), "{err}");
    }

    #[test]
    fn test_start_existing_refreshes_gh_merge_base_when_integration_moves() {
        let (_t, repo) = repo_fixture();
        run(&["git", "branch", "old-int"], &repo);
        run(&["git", "branch", "new-int"], &repo);
        let base_old = resolve_base(&repo, None, Some("old-int")).unwrap();
        start(
            &repo,
            "feat-a",
            &base_old,
            "team:epic",
            "",
            Some("old-int"),
            None,
        )
        .unwrap();
        let created = read_meta(&repo, "feat-a").unwrap()["hive-created"].clone();
        let base_new = resolve_base(&repo, None, Some("new-int")).unwrap();
        let res = start(
            &repo,
            "feat-a",
            &base_new,
            "team:epic",
            "",
            Some("new-int"),
            None,
        )
        .unwrap();
        assert_eq!(res.mode, "existing");
        let meta = read_meta(&repo, "feat-a").unwrap();
        assert_eq!(meta["gh-merge-base"], "new-int");
        assert_eq!(meta["hive-base"], "new-int");
        // first-created timestamp survives
        assert_eq!(meta["hive-created"], created);
    }

    #[test]
    fn test_start_attach_backfills_stale_gh_merge_base() {
        let (_t, repo) = repo_fixture();
        run(&["git", "branch", "old-int"], &repo);
        run(&["git", "branch", "new-int"], &repo);
        let base_old = resolve_base(&repo, None, Some("old-int")).unwrap();
        let res = start(
            &repo,
            "feat-a",
            &base_old,
            "team:epic",
            "",
            Some("old-int"),
            None,
        )
        .unwrap();
        // meta survives manual remove
        run(&["git", "worktree", "remove", &res.path], &repo);
        let base_new = resolve_base(&repo, None, Some("new-int")).unwrap();
        let res2 = start(
            &repo,
            "feat-a",
            &base_new,
            "team:epic",
            "",
            Some("new-int"),
            None,
        )
        .unwrap();
        assert_eq!(res2.mode, "attached");
        assert_eq!(
            read_meta(&repo, "feat-a").unwrap()["gh-merge-base"],
            "new-int"
        );
    }

    // --- done ----------------------------------------------------------------

    #[test]
    fn test_done_removes_worktree_keeps_branch_clears_hive_meta_only() {
        let (_t, repo) = repo_fixture();
        run(&["git", "branch", "epic-int"], &repo);
        let base = resolve_base(&repo, None, Some("epic-int")).unwrap();
        let res = start(
            &repo,
            "feat-a",
            &base,
            "team:epic",
            "",
            Some("epic-int"),
            None,
        )
        .unwrap();
        let out = done(&repo, "feat-a", false, repo.to_str().unwrap()).unwrap();
        assert!(out.branch_kept);
        assert!(!Path::new(&res.path).exists());
        assert!(out.cleared_config_keys.contains(&"hive-owner".to_string()));
        let meta = read_meta(&repo, "feat-a").unwrap();
        assert!(!meta.keys().any(|k| k.starts_with("hive-")));
        // gh-merge-base survives done: the branch and its PR are still alive.
        assert_eq!(
            meta.get("gh-merge-base").map(String::as_str),
            Some("epic-int")
        );
        assert!(!run(&["git", "branch", "--list", "feat-a"], &repo).is_empty());
    }

    #[test]
    fn test_done_refuses_from_inside_the_worktree() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        let err = err_of(done(&repo, "feat-a", false, &res.path));
        assert!(err.contains("leave the worktree first"), "{err}");
        assert!(Path::new(&res.path).exists());
    }

    #[test]
    fn test_done_refuses_dirty_without_force() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        std::fs::write(Path::new(&res.path).join("junk.txt"), "x").unwrap();
        let err = err_of(done(&repo, "feat-a", false, repo.to_str().unwrap()));
        assert!(err.contains("uncommitted changes"), "{err}");
    }

    #[test]
    fn test_done_refuses_in_progress_operation() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        let merge_head = run(
            &["git", "rev-parse", "--git-path", "MERGE_HEAD"],
            Path::new(&res.path),
        );
        let target = if Path::new(&merge_head).is_absolute() {
            PathBuf::from(&merge_head)
        } else {
            Path::new(&res.path).join(&merge_head)
        };
        std::fs::write(
            &target,
            format!("{}\n", run(&["git", "rev-parse", "HEAD"], &repo)),
        )
        .unwrap();
        let err = err_of(done(&repo, "feat-a", false, repo.to_str().unwrap()));
        assert!(err.contains("merge in progress"), "{err}");
    }

    #[test]
    fn test_done_force_emits_summary_with_untracked_and_ignored_note() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        std::fs::write(Path::new(&res.path).join("junk.txt"), "x").unwrap();
        let out = done(&repo, "feat-a", true, repo.to_str().unwrap()).unwrap();
        assert!(out.forced);
        assert!(out.status_summary.contains("junk.txt"));
        assert!(out
            .status_summary
            .contains("ignored files are not included"));
        assert!(!Path::new(&res.path).exists());
    }

    #[test]
    fn test_done_force_on_clean_worktree_still_emits_summary() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        let out = done(&repo, "feat-a", true, repo.to_str().unwrap()).unwrap();
        assert!(out.forced);
        // auditability: --force always reports
        assert!(!out.status_summary.is_empty());
        assert!(out
            .status_summary
            .contains("ignored files are not included"));
        assert!(!Path::new(&res.path).exists());
    }

    #[test]
    fn test_done_without_worktree_fails_with_status_hint() {
        let (_t, repo) = repo_fixture();
        run(&["git", "branch", "feat-only"], &repo);
        let err = err_of(done(&repo, "feat-only", false, repo.to_str().unwrap()));
        assert!(err.contains("no worktree found"), "{err}");
    }

    // --- status --------------------------------------------------------------

    #[test]
    fn test_status_active() {
        let (_t, repo) = repo_fixture();
        start_t(&repo, "feat-a");
        let s = feature_status(&repo, "feat-a").unwrap();
        assert_eq!(s.state, "active");
        assert!(s.branch_exists);
        assert!(!s.dirty);
        assert_eq!(s.owner, "team:t1");
        assert_eq!(s.base_oid, s.current_base_oid);
    }

    #[test]
    fn test_status_branch_only_after_done() {
        let (_t, repo) = repo_fixture();
        start_t(&repo, "feat-a");
        done(&repo, "feat-a", false, repo.to_str().unwrap()).unwrap();
        let s = feature_status(&repo, "feat-a").unwrap();
        assert_eq!(s.state, "branch-only");
        assert_eq!(s.worktree_path, "");
    }

    #[test]
    fn test_status_needs_rebase_after_base_advances() {
        let (_t, repo) = repo_fixture();
        start_t(&repo, "feat-a");
        commit(&repo, "advance");
        run(&["git", "push", "-q", "origin", "HEAD"], &repo);
        run(&["git", "fetch", "-q", "origin"], &repo);
        let s = feature_status(&repo, "feat-a").unwrap();
        assert_eq!(s.state, "needs-rebase");
        assert_ne!(s.base_oid, s.current_base_oid);
    }

    #[test]
    fn test_status_unknown_branch() {
        let (_t, repo) = repo_fixture();
        let s = feature_status(&repo, "ghost").unwrap();
        assert_eq!(s.state, "unknown-branch");
        assert!(!s.branch_exists);
    }

    #[test]
    fn test_status_dirty_and_in_progress_flags() {
        let (_t, repo) = repo_fixture();
        let res = start_t(&repo, "feat-a");
        std::fs::write(Path::new(&res.path).join("junk.txt"), "x").unwrap();
        let s = feature_status(&repo, "feat-a").unwrap();
        assert!(s.dirty);
    }

    #[test]
    fn test_pool_status_lists_labeled_and_checked_out() {
        let (_t, repo) = repo_fixture();
        start_t(&repo, "feat-a");
        start_t(&repo, "feat-b");
        // unlabeled now, but...
        done(&repo, "feat-b", false, repo.to_str().unwrap()).unwrap();
        // relabel
        run(
            &["git", "config", "branch.feat-b.hive-owner", "team:t1"],
            &repo,
        );
        let rows = pool_status(&repo).unwrap();
        let features: std::collections::HashSet<&str> =
            rows.iter().map(|r| r.feature.as_str()).collect();
        assert!(features.contains("feat-a"));
        assert!(features.contains("feat-b"));
    }
}
