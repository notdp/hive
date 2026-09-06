use super::*;
use crate::testenv::EnvGuard;
use crate::tmux::{Run, TmuxError};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

// ------------------------------------------------------------------
// Seam hook: what a test pins on this module's cross-module calls. An
// unset field runs the real call (`configure_hive_home` installs an
// empty hook, and points the real tmux facade at a no-server double).
// ------------------------------------------------------------------

type SpawnFn = Box<dyn Fn(usize, &SpawnCall) -> Agent>;
type TeamRuntimeFn = Box<dyn Fn(&str, &str) -> Option<Map<String, Value>>>;
type ProfileFn = Box<dyn Fn(&str) -> Option<String>>;

#[derive(Default)]
pub struct Hook {
    pub spawn_calls: Vec<SpawnCall>,
    pub spawn_fn: Option<SpawnFn>,
    pub session_id: Option<String>,
    pub team_runtime: Option<TeamRuntimeFn>,
    pub profile_for_pane: Option<ProfileFn>,
}

impl Hook {
    /// Record the spawn; answer it (1-based call number, the call) when
    /// `spawn_fn` is set, else let `Agent::spawn` run.
    pub fn spawn(&mut self, call: &SpawnCall) -> Option<Result<Agent>> {
        self.spawn_calls.push(call.clone());
        let n = self.spawn_calls.len();
        self.spawn_fn.as_ref().map(|f| Ok(f(n, call)))
    }
}

thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

pub fn hook<T>(f: impl FnOnce(&mut Hook) -> T) -> Option<T> {
    HOOK.with(|cell| cell.borrow_mut().as_mut().map(f))
}

// ------------------------------------------------------------------
// Fake state shared by fake_tmux / fake_layout; `configure_hive_home`
// resets it per test.
// ------------------------------------------------------------------

type RunFn = Box<dyn Fn(&[String], bool) -> Result<Run, TmuxError>>;
type ListPanesFn = Box<dyn Fn(&str) -> Vec<PaneInfo>>;
type ListPanesOrNoneFn = Box<dyn Fn(&str) -> Option<Vec<PaneInfo>>>;

pub struct FakeState {
    pub tmux_inside: bool,
    pub current_pane: String,
    pub session_name: String,
    pub current_window_target: Option<String>,
    /// Ordered so the rendered `list-windows` output is deterministic.
    pub window_options: BTreeMap<String, HashMap<String, String>>,
    pub pane_options: HashMap<String, HashMap<String, String>>,
    pub pane_alive: bool,
    pub tagged: Vec<(String, String, String, String, String, String)>,
    pub borders: Vec<String>,
    pub cleared: Vec<(String, String)>,
    pub display_values: HashMap<(String, String), String>,
    pub default_display_value: Option<String>,
    pub pane_window_targets: HashMap<String, String>,
    pub run_fn: Option<RunFn>,
    pub list_panes_full_fn: Option<ListPanesFn>,
    pub list_panes_full_or_none_fn: Option<ListPanesOrNoneFn>,
    pub window_size: (i64, i64),
    pub layout_panes: Vec<String>,
    pub layout_actions: Vec<(String, String, String, String)>,
}

impl Default for FakeState {
    fn default() -> Self {
        FakeState {
            tmux_inside: true,
            current_pane: "%0".to_string(),
            session_name: "dev".to_string(),
            current_window_target: Some("dev:0".to_string()),
            window_options: BTreeMap::new(),
            pane_options: HashMap::new(),
            pane_alive: true,
            tagged: Vec::new(),
            borders: Vec::new(),
            cleared: Vec::new(),
            display_values: HashMap::new(),
            default_display_value: None,
            pane_window_targets: HashMap::new(),
            run_fn: None,
            list_panes_full_fn: None,
            list_panes_full_or_none_fn: None,
            window_size: (0, 0),
            layout_panes: Vec::new(),
            layout_actions: Vec::new(),
        }
    }
}

thread_local! {
    static STATE: RefCell<FakeState> = RefCell::new(FakeState::default());
}

pub fn with_state<R>(f: impl FnOnce(&mut FakeState) -> R) -> R {
    STATE.with(|state| f(&mut state.borrow_mut()))
}

fn window_id_for_target(target: &str) -> String {
    let suffix = if target.contains(':') {
        target.rsplit(':').next().unwrap_or("0")
    } else {
        "0"
    };
    format!("@{suffix}")
}

/// What `list-windows -a -F <fmt>` prints for the fake window options:
/// `#{session_name}:#{window_index}` is the target, `#{window_id}` its
/// derived id, `#{@key}` the option — so `find_team_window` reads back
/// what `write_window_options` wrote. Any other `run` prints nothing.
fn fake_run_stdout(st: &FakeState, args: &[String]) -> String {
    if args.first().map(String::as_str) != Some("list-windows") {
        return String::new();
    }
    let Some(fmt) = args
        .iter()
        .position(|a| a == "-F")
        .and_then(|i| args.get(i + 1))
    else {
        return String::new();
    };
    let mut out = String::new();
    for (target, opts) in &st.window_options {
        // The hidden-window mask, as tmux would resolve it.
        let team_tag = if opts.get("hive-hidden").is_some_and(|h| !h.is_empty()) {
            ""
        } else {
            "#{@hive-team}"
        };
        let mut line = fmt
            .replace(crate::tmux::WINDOW_TEAM_FMT, team_tag)
            .replace("#{session_name}:#{window_index}", target)
            .replace("#{window_id}", &window_id_for_target(target));
        while let Some(start) = line.find("#{@") {
            let Some(len) = line[start..].find('}') else {
                break;
            };
            let key = line[start + 3..start + len].to_string();
            let value = opts.get(&key).cloned().unwrap_or_default();
            line.replace_range(start..start + len + 1, &value);
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub mod fake_identity {
    use super::with_state;

    pub fn is_inside_tmux() -> bool {
        with_state(|st| st.tmux_inside)
    }

    pub fn current_pane_id() -> Option<String> {
        with_state(|st| Some(st.current_pane.clone()))
    }

    pub fn current_session_name() -> Option<String> {
        with_state(|st| Some(st.session_name.clone()))
    }

    pub fn current_window_target() -> Option<String> {
        with_state(|st| st.current_window_target.clone())
    }
}

pub mod fake_tmux {
    use super::with_state;
    use crate::tmux::{PaneInfo, Run, TmuxError};

    fn strip(option: &str) -> String {
        option.trim_start_matches('@').to_string()
    }

    pub fn get_window_id(target: &str) -> Option<String> {
        Some(super::window_id_for_target(target))
    }

    pub fn get_window_option(target: &str, key: &str) -> Option<String> {
        with_state(|st| {
            st.window_options
                .get(target)
                .and_then(|opts| opts.get(key))
                .cloned()
        })
    }

    pub fn set_window_option(target: &str, option: &str, value: &str) {
        with_state(|st| {
            st.window_options
                .entry(target.to_string())
                .or_default()
                .insert(strip(option), value.to_string());
        });
    }

    pub fn clear_window_option(target: &str, option: &str) {
        with_state(|st| {
            st.cleared.push((target.to_string(), option.to_string()));
            if let Some(opts) = st.window_options.get_mut(target) {
                opts.remove(&strip(option));
            }
        });
    }

    pub fn enable_pane_border_status(target: &str) {
        with_state(|st| st.borders.push(target.to_string()));
    }

    pub fn configure_hive_window(target: &str) {
        enable_pane_border_status(target);
        set_window_option(target, "monitor-activity", "off");
        set_window_option(target, "monitor-bell", "off");
    }

    pub fn tag_pane(pane_id: &str, role: &str, agent: &str, team: &str, cli: &str, group: &str) {
        with_state(|st| {
            st.tagged.push((
                pane_id.to_string(),
                role.to_string(),
                agent.to_string(),
                team.to_string(),
                cli.to_string(),
                group.to_string(),
            ));
            let opts = st.pane_options.entry(pane_id.to_string()).or_default();
            opts.insert("hive-role".to_string(), role.to_string());
            opts.insert("hive-agent".to_string(), agent.to_string());
            opts.insert("hive-team".to_string(), team.to_string());
            if !cli.is_empty() {
                opts.insert("hive-cli".to_string(), cli.to_string());
            }
            if !group.is_empty() {
                opts.insert("hive-group".to_string(), group.to_string());
            }
        });
    }

    pub fn clear_pane_tags(pane_id: &str) {
        with_state(|st| {
            st.pane_options.remove(pane_id);
        });
    }

    pub fn is_pane_alive(_pane_id: &str) -> bool {
        with_state(|st| st.pane_alive)
    }

    pub fn get_pane_option(pane_id: &str, key: &str) -> Option<String> {
        with_state(|st| {
            st.pane_options
                .get(pane_id)
                .and_then(|opts| opts.get(key))
                .cloned()
        })
    }

    pub fn display_value(target: &str, fmt: &str) -> Option<String> {
        with_state(|st| {
            st.display_values
                .get(&(target.to_string(), fmt.to_string()))
                .cloned()
                .or_else(|| st.default_display_value.clone())
        })
    }

    pub fn get_pane_window_target(pane_id: &str) -> Option<String> {
        with_state(|st| st.pane_window_targets.get(pane_id).cloned())
    }

    pub fn list_panes_full(target: &str) -> Vec<PaneInfo> {
        with_state(|st| {
            if let Some(f) = &st.list_panes_full_fn {
                return f(target);
            }
            if target.is_empty() {
                return Vec::new();
            }
            let mut out = Vec::new();
            for (pane_id, opts) in &st.pane_options {
                if opts.get("hive-team").is_some_and(|t| !t.is_empty()) {
                    out.push(PaneInfo {
                        pane_id: pane_id.clone(),
                        title: String::new(),
                        command: "claude".to_string(),
                        role: opts.get("hive-role").cloned().unwrap_or_default(),
                        agent: opts.get("hive-agent").cloned().unwrap_or_default(),
                        team: opts.get("hive-team").cloned().unwrap_or_default(),
                        cli: opts.get("hive-cli").cloned().unwrap_or_default(),
                        group: opts.get("hive-group").cloned().unwrap_or_default(),
                    });
                }
            }
            out
        })
    }

    pub fn list_panes_full_or_none(target: &str) -> Option<Vec<PaneInfo>> {
        let via_override =
            with_state(|st| st.list_panes_full_or_none_fn.as_ref().map(|f| f(target)));
        if let Some(result) = via_override {
            return result;
        }
        let mut panes = list_panes_full(target);
        let current = with_state(|st| st.current_pane.clone());
        if !panes.iter().any(|p| p.pane_id == current) {
            panes.push(PaneInfo {
                pane_id: current,
                ..Default::default()
            });
        }
        Some(panes)
    }

    pub(crate) fn run(args: &[&str], check: bool, _timeout: u64) -> Result<Run, TmuxError> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        with_state(|st| match &st.run_fn {
            Some(f) => f(&owned, check),
            None => Ok(Run {
                returncode: 0,
                stdout: super::fake_run_stdout(st, &owned),
                stderr: String::new(),
            }),
        })
    }
}

/// Fake layout runs the REAL planner over fake tmux state
/// (`window_size`, `layout_panes` with their `hive-role` tags), so only
/// the tmux calls are faked, not the plan.
pub mod fake_layout {
    use super::with_state;
    use crate::tmux::PaneInfo;

    fn layout_panes() -> Vec<PaneInfo> {
        with_state(|st| {
            st.layout_panes
                .iter()
                .map(|pane_id| PaneInfo {
                    pane_id: pane_id.clone(),
                    role: st
                        .pane_options
                        .get(pane_id)
                        .and_then(|opts| opts.get("hive-role"))
                        .cloned()
                        .unwrap_or_default(),
                    ..Default::default()
                })
                .collect()
        })
    }

    pub fn split_horizontal(window_target: &str) -> bool {
        if window_target.is_empty() {
            return true;
        }
        let size = with_state(|st| st.window_size);
        let mut panes = layout_panes();
        panes.push(PaneInfo::default());
        crate::layout::split_beside(size, &panes)
    }

    /// Records the unset as a cleared `@hive-layout` and one row per
    /// hook on `st.cleared`.
    pub fn remove_hooks(window: &str) {
        with_state(|st| {
            for hook in crate::layout::LAYOUT_HOOKS {
                st.cleared.push((window.to_string(), hook.to_string()));
            }
        });
        super::fake_tmux::clear_window_option(window, crate::layout::LAYOUT_KEY_OPTION);
    }

    pub fn ensure(window_target: &str, force: bool) -> crate::layout::Outcome {
        if window_target.is_empty() {
            return crate::layout::Outcome::Skipped("no-window");
        }
        let size = with_state(|st| st.window_size);
        let Some(plan) = crate::layout::plan(size, &layout_panes()) else {
            return crate::layout::Outcome::Skipped("no-plan");
        };
        let stored = with_state(|st| {
            st.window_options
                .get(window_target)
                .and_then(|opts| opts.get("hive-layout"))
                .cloned()
        });
        if !force && stored.as_deref() == Some(plan.key.as_str()) {
            return crate::layout::Outcome::Unchanged(plan);
        }
        with_state(|st| {
            st.layout_actions.push((
                "layout".to_string(),
                window_target.to_string(),
                plan.key.clone(),
                plan.layout.clone(),
            ));
        });
        super::fake_tmux::set_window_option(window_target, "@hive-layout", &plan.key);
        crate::layout::Outcome::Applied(plan)
    }
}

// ------------------------------------------------------------------
// Test helpers
// ------------------------------------------------------------------

/// Isolate every engine home under a temp dir, reset the fake tmux
/// state, install an empty seam hook, and make every `crate::tmux::run`
/// the seams' real calls reach fail as it would with no server — holding
/// the env lock for the test's lifetime.
fn configure_hive_home(tmux_inside: bool, current_pane: &str) -> (tempfile::TempDir, EnvGuard) {
    let mut env = EnvGuard::cleared(&[
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CODEX_THREAD_ID",
        "GROK_SESSION_ID",
    ]);
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    env.set("CODEX_HOME", tmp.path().join(".codex"));
    env.set("CLAUDE_HOME", tmp.path().join(".claude"));
    env.set("GROK_HOME", tmp.path().join(".grok"));
    env.set("XDG_CACHE_HOME", tmp.path().join(".cache"));
    env.set("CLAUDE_CONFIG_DIR", tmp.path().join("claude-env-isolation"));
    with_state(|st| {
        *st = FakeState::default();
        st.tmux_inside = tmux_inside;
        st.current_pane = current_pane.to_string();
    });
    HOOK.with(|cell| *cell.borrow_mut() = Some(Hook::default()));
    real_tmux_pane_commands(&[]);
    (tmp, env)
}

fn no_tmux_server() -> TmuxError {
    TmuxError::Os("no tmux server in unit tests".to_string())
}

/// Answer the real tmux facade's `display-message -t <pane> -p
/// #{pane_current_command}` from *commands* (the read
/// `agent_cli::member_role_for_pane` probes first); every other real
/// tmux call fails as it would with no server.
fn real_tmux_pane_commands(commands: &[(&str, &str)]) {
    let commands: HashMap<String, String> = commands
        .iter()
        .map(|(pane, command)| (pane.to_string(), command.to_string()))
        .collect();
    crate::tmux::set_run_override(move |args, _check, _timeout| {
        let pane = args
            .iter()
            .position(|a| a == "-t")
            .and_then(|i| args.get(i + 1));
        let command = match (args.first().map(String::as_str), args.last()) {
            (Some("display-message"), Some(fmt)) if fmt == "#{pane_current_command}" => {
                pane.and_then(|p| commands.get(p))
            }
            _ => None,
        };
        match command {
            Some(command) => Ok(Run {
                returncode: 0,
                stdout: format!("{command}\n"),
                stderr: String::new(),
            }),
            None => Err(no_tmux_server()),
        }
    });
}

fn set_hive_window(target: &str, team: &str, workspace: &str, desc: &str, created: &str) {
    for (key, value) in [
        ("@hive-team", team),
        ("@hive-workspace", workspace),
        ("@hive-desc", desc),
        ("@hive-created", created),
    ] {
        fake_tmux::set_window_option(target, key, value);
    }
}

fn pane_info(pane_id: &str, command: &str, role: &str, agent: &str, team: &str) -> PaneInfo {
    PaneInfo {
        pane_id: pane_id.to_string(),
        title: String::new(),
        command: command.to_string(),
        role: role.to_string(),
        agent: agent.to_string(),
        team: team.to_string(),
        cli: String::new(),
        group: String::new(),
    }
}

fn obj(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(o) => o,
        _ => unreachable!(),
    }
}

fn run_stdout(stdout: &'static str) -> RunFn {
    Box::new(move |_args, _check| {
        Ok(Run {
            returncode: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    })
}

// ------------------------------------------------------------------
// Ported tests (tests/unit/test_team.py, plus the pure-logic
// validate_team_name test from tests/cli/test_message_commands.py)
// ------------------------------------------------------------------

#[test]
fn test_team_create_inside_tmux_tags_lead_and_detects_session() {
    let (_tmp, _guard) = configure_hive_home(true, "%7");
    hook(|h| h.session_id = Some("sess-123".to_string()));
    real_tmux_pane_commands(&[("%7", "claude")]);

    let team = Team::create("team-a", "demo", "/tmp/ws").unwrap();

    assert_eq!(team.lead_pane_id, "%7");
    assert_eq!(team.lead_session_id.as_deref(), Some("sess-123"));
    assert_eq!(team.tmux_session, "dev");
    assert_eq!(team.tmux_window, "dev:0");
    assert_eq!(team.tmux_window_id, "@0");
    let tagged = with_state(|st| st.tagged.clone());
    assert_eq!(
        tagged,
        vec![(
            "%7".to_string(),
            "agent".to_string(),
            "orch".to_string(),
            "team-a".to_string(),
            String::new(),
            String::new()
        )]
    );
    assert_eq!(
        with_state(|st| st.borders.clone()),
        vec!["dev:0".to_string()]
    );
    assert_eq!(
        fake_tmux::get_window_option("dev:0", "monitor-activity").as_deref(),
        Some("off")
    );
    assert_eq!(
        fake_tmux::get_window_option("dev:0", "monitor-bell").as_deref(),
        Some("off")
    );
}

#[test]
fn test_team_create_rejects_outside_tmux() {
    let (_tmp, _guard) = configure_hive_home(false, "%0");

    let err = Team::create("team-a", "", "").unwrap_err();
    assert!(err.to_string().contains("requires tmux"), "{err}");
}

#[test]
fn test_team_create_rejects_reserved_or_dotted_names() {
    // `hive send` parses `<team>.<member>` / `ccd.<session>`: a team named
    // ccd, or one carrying a dot, would be unaddressable
    let (_tmp, _guard) = configure_hive_home(true, "%0");

    for name in ["ccd", "ccd.desk", "a.b"] {
        let err = Team::create(name, "", "").unwrap_err();
        assert!(err.to_string().contains("invalid"), "{name}: {err}");
    }
}

#[test]
fn test_team_save_and_load_round_trip() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    let mut team = Team {
        name: "team-a".to_string(),
        description: "demo".to_string(),
        workspace: "/tmp/ws".to_string(),
        lead_pane_id: "%0".to_string(),
        lead_session_id: Some("sess-1".to_string()),
        tmux_session: "dev".to_string(),
        tmux_window: "dev:0".to_string(),
        ..Default::default()
    };
    team.agents.push(new_agent(
        "claude", "team-a", "%1", "claude", "/tmp", "m1", None,
    ));

    team.write_window_options();
    assert_eq!(
        with_state(|st| st.borders.clone()),
        vec!["dev:0".to_string()]
    );

    // Set up pane tags for load to find (in real usage, set during create/spawn)
    fake_tmux::tag_pane("%0", "agent", "orch", "team-a", "claude", "");
    fake_tmux::tag_pane("%1", "agent", "claude", "team-a", "claude", "");

    let loaded = Team::load("team-a", "").unwrap();

    assert_eq!(loaded.name, "team-a");
    assert_eq!(loaded.description, "demo");
    assert_eq!(loaded.tmux_window, "dev:0");
    assert_eq!(loaded.tmux_window_id, "@0");
    assert_eq!(loaded.agent_named("orch").unwrap().pane_id, "%0");
    assert_eq!(loaded.agent_named("claude").unwrap().pane_id, "%1");
}

#[test]
fn test_team_load_restores_agent_cwd_from_pane_current_path() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    set_hive_window("dev:0", "team-a", "/tmp/ws", "", "0");
    with_state(|st| {
        st.list_panes_full_fn = Some(Box::new(|_target| {
            let mut pane = pane_info("%1", "claude", "agent", "claude", "team-a");
            pane.cli = "claude".to_string();
            vec![pane]
        }));
        st.display_values.insert(
            ("%1".to_string(), "#{pane_current_path}".to_string()),
            "/repo".to_string(),
        );
    });

    let loaded = Team::load("team-a", "").unwrap();

    assert_eq!(loaded.agent_named("claude").unwrap().cwd, "/repo");
}

/// The mirror is the member's pane as much as an engine pane is: a verb
/// addressing the orch (kill, capture, inject) lands on it.
#[test]
fn test_team_load_binds_a_member_to_its_mirror_pane() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    set_hive_window("dev:0", "team-a", "/tmp/ws", "", "0");
    with_state(|st| {
        st.list_panes_full_fn = Some(Box::new(|_target| {
            let mut pane = pane_info("%1", "hive", "mirror", "orch", "team-a");
            pane.cli = "claude".to_string();
            vec![pane]
        }));
    });

    let loaded = Team::load("team-a", "").unwrap();

    assert_eq!(loaded.get("orch").unwrap().pane_id, "%1");
    assert_eq!(loaded.agent_named("orch").unwrap().cli, "claude");
}

#[test]
fn test_team_lead_agent_uses_persisted_session_id() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    let team = Team {
        name: "team-a".to_string(),
        lead_pane_id: "%0".to_string(),
        lead_session_id: Some("sess-1".to_string()),
        ..Default::default()
    };

    let lead = team.lead_agent();

    let lead = lead.expect("lead agent");
    assert_eq!(lead.name, "orch");
    assert_eq!(lead.session_id.as_deref(), Some("sess-1"));
}

#[test]
fn test_team_spawn_tags_agent_and_passes_skill() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    let agent = new_agent("claude", "team-a", "%9", "claude", "", "", None);
    let spawn_result = agent.clone();
    hook(move |h| h.spawn_fn = Some(Box::new(move |_n, _call| spawn_result.clone())));
    with_state(|st| {
        st.current_window_target = Some("dev:1".to_string());
        st.window_size = (200, 50);
        st.layout_panes = vec!["%1".to_string(), "%9".to_string()];
    });

    let mut team = Team {
        name: "team-a".to_string(),
        lead_pane_id: "%0".to_string(),
        ..Default::default()
    };
    let result = team
        .spawn(
            "claude",
            "",
            "start now",
            "",
            "demo-review",
            Some(&HashMap::from([("FOO".to_string(), "bar".to_string())])),
            "claude",
        )
        .unwrap();

    assert_eq!(result.pane_id, agent.pane_id);
    assert_eq!(result.name, agent.name);
    let calls = hook(|h| h.spawn_calls.clone()).unwrap();
    assert_eq!(calls[0].target_pane, "%0");
    assert_eq!(calls[0].skill, "demo-review");
    assert_eq!(calls[0].prompt, "start now");
    assert_eq!(calls[0].split_size.as_deref(), Some("50%"));
    assert_eq!(
        calls[0].extra_env.as_ref().and_then(|env| env.get("FOO")),
        Some(&"bar".to_string())
    );
    let tagged = with_state(|st| st.tagged.clone());
    assert_eq!(
        tagged,
        vec![(
            "%9".to_string(),
            "agent".to_string(),
            "claude".to_string(),
            "team-a".to_string(),
            "claude".to_string(),
            String::new()
        )]
    );
    assert!(with_state(|st| st.borders.clone()).contains(&"dev:1".to_string()));
}

/// Guards Bug 1 regression: a portrait window must end stacked, not on
/// the legacy hardcoded left-right split.
#[test]
fn test_team_spawn_portrait_window_stacks_the_panes() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    let agent = new_agent("claude", "team-a", "%9", "claude", "", "", None);
    hook(move |h| h.spawn_fn = Some(Box::new(move |_n, _call| agent.clone())));
    with_state(|st| {
        st.current_window_target = Some("dev:1".to_string());
        st.window_size = (191, 171);
        st.layout_panes = vec!["%0".to_string(), "%9".to_string()];
    });

    let mut team = Team {
        name: "team-a".to_string(),
        lead_pane_id: "%0".to_string(),
        ..Default::default()
    };
    team.spawn("claude", "", "", "", "hive", None, "claude")
        .unwrap();

    let layouts = with_state(|st| st.layout_actions.clone());
    let plan = crate::layout::plan(
        (191, 171),
        &["%0", "%9"].map(|id| crate::tmux::PaneInfo {
            pane_id: id.to_string(),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(plan.orientation, "portrait");
    assert_eq!(
        layouts,
        vec![(
            "layout".to_string(),
            "dev:1".to_string(),
            plan.key,
            plan.layout
        )]
    );
    // Pre-spawn split should also follow portrait orientation (vertical = False).
    let calls = hook(|h| h.spawn_calls.clone()).unwrap();
    assert!(!calls[0].split_horizontal);
}

#[test]
fn test_team_spawn_second_agent_splits_from_last_agent() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    hook(|h| {
        h.spawn_fn = Some(Box::new(|n, call| {
            super::new_agent(
                &call.name,
                "team-a",
                &format!("%{}", n + 8),
                "claude",
                "",
                "",
                None,
            )
        }))
    });
    with_state(|st| st.current_window_target = None);

    let mut team = Team {
        name: "team-a".to_string(),
        lead_pane_id: "%0".to_string(),
        ..Default::default()
    };
    team.agents
        .push(new_agent("claude", "team-a", "%9", "claude", "", "", None));
    team.spawn("gpt", "", "", "", "hive", None, "claude")
        .unwrap();

    let calls = hook(|h| h.spawn_calls.clone()).unwrap();
    assert_eq!(calls[0].target_pane, "%9");
    assert!(!calls[0].split_horizontal);
    assert_eq!(calls[0].skill, "hive");
}

#[test]
fn test_team_get_resolves_lead_and_members() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    let alive = new_agent("claude", "team-a", "%1", "claude", "", "", None);

    let mut team = Team {
        name: "team-a".to_string(),
        lead_pane_id: "%0".to_string(),
        ..Default::default()
    };
    team.agents.push(alive);

    assert_eq!(team.get("orch").unwrap().pane_id, "%0");
    assert_eq!(team.get("claude").unwrap().pane_id, "%1");
}

#[test]
fn test_team_status_stays_local_only() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");

    let mut team = Team {
        name: "team-a".to_string(),
        lead_pane_id: "%0".to_string(),
        ..Default::default()
    };
    team.agents
        .push(new_agent("claude", "team-a", "%1", "claude", "", "", None));

    let payload = team.status();

    let members = payload.get("members").unwrap().as_array().unwrap();
    let orch = members
        .iter()
        .find(|m| m.get("name").unwrap() == "orch")
        .unwrap()
        .as_object()
        .unwrap();
    let claude = members
        .iter()
        .find(|m| m.get("name").unwrap() == "claude")
        .unwrap()
        .as_object()
        .unwrap();
    for row in [orch, claude] {
        assert!(!row.contains_key("sessionId"));
        assert!(!row.contains_key("model"));
        assert!(!row.contains_key("alive"));
    }
}

/// When two windows claim the same team, the one containing prefer_pane wins.
///
/// The losing duplicate here is stale (no live member panes), so it is cleared.
#[test]
fn test_find_team_window_prefers_pane_window_on_duplicate() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    with_state(|st| {
        st.run_fn = Some(run_stdout(
            "dev:2\t@2\tmy-team\t/tmp/ws\tdesc\t0\ndev:3\t@3\tmy-team\t/tmp/ws\tdesc\t0\n",
        ));
        st.pane_window_targets
            .insert("%99".to_string(), "dev:3".to_string());
        // No live member panes anywhere → the losing window dev:2 is provably stale.
        st.list_panes_full_or_none_fn = Some(Box::new(|_target| Some(Vec::new())));
    });

    let (wt, _data) = find_team_window("my-team", "%99").unwrap();

    assert_eq!(wt, "dev:3");
    let cleared = with_state(|st| st.cleared.clone());
    assert!(cleared.iter().any(|(w, _)| w == "dev:2"));
}

/// Without prefer_pane, pick the window that actually has tagged panes.
#[test]
fn test_find_team_window_falls_back_to_tagged_panes() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    with_state(|st| {
        st.run_fn = Some(run_stdout(
            "dev:2\t@2\tmy-team\t/tmp/ws\tdesc\t0\ndev:3\t@3\tmy-team\t/tmp/ws\tdesc\t0\n",
        ));
        st.list_panes_full_or_none_fn = Some(Box::new(|target| {
            if target == "dev:3" {
                Some(vec![pane_info("%50", "codex", "agent", "rev-a", "my-team")])
            } else {
                Some(vec![pane_info("%40", "codex", "", "", "")])
            }
        }));
    });

    let (wt, _data) = find_team_window("my-team", "").unwrap();

    assert_eq!(wt, "dev:3");
    let cleared = with_state(|st| st.cleared.clone());
    assert!(cleared.iter().any(|(w, _)| w == "dev:2"));
}

#[test]
fn test_gc_stale_team_windows_clears_non_kept() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    // All duplicates are stale (no live member panes) → all non-kept get cleared.
    with_state(|st| {
        st.list_panes_full_or_none_fn = Some(Box::new(|_target| Some(Vec::new())));
    });

    gc_stale_team_windows(
        "my-team",
        "dev:3",
        &[
            "dev:2".to_string(),
            "dev:3".to_string(),
            "dev:4".to_string(),
        ],
    );

    let cleared = with_state(|st| st.cleared.clone());
    let stale_windows: std::collections::HashSet<String> =
        cleared.iter().map(|(w, _)| w.clone()).collect();
    assert_eq!(
        stale_windows,
        ["dev:2".to_string(), "dev:4".to_string()]
            .into_iter()
            .collect()
    );
    assert!(!cleared.contains(&("dev:3".to_string(), "@hive-team".to_string())));
}

/// A window that stops being hive's loses the layout hooks and the
/// plan key with its tags, or its next split would be re-tiled.
#[test]
fn test_clear_window_tags_unsets_the_layout_hooks_and_key() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    fake_tmux::set_window_option("dev:2", "@hive-team", "my-team");
    fake_tmux::set_window_option("dev:2", "@hive-layout", "landscape/m2/no-mirror/2x1");

    clear_window_tags("dev:2");

    let cleared = with_state(|st| st.cleared.clone());
    for hook in crate::layout::LAYOUT_HOOKS {
        assert!(
            cleared.contains(&("dev:2".to_string(), hook.to_string())),
            "{hook} left: {cleared:?}"
        );
    }
    assert!(cleared.contains(&("dev:2".to_string(), "@hive-layout".to_string())));
    assert!(cleared.contains(&("dev:2".to_string(), "@hive-team".to_string())));
    let left = with_state(|st| st.window_options.get("dev:2").cloned().unwrap_or_default());
    assert!(left.is_empty(), "{left:?}");
}

/// A duplicate window with live member panes is never cleared (Bug A safety).
#[test]
fn test_gc_stale_team_windows_skips_live_duplicate() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    with_state(|st| {
        st.list_panes_full_or_none_fn = Some(Box::new(|target| {
            if target == "dev:2" {
                Some(vec![pane_info(
                    "%40",
                    "codex",
                    "agent",
                    "validator",
                    "my-team",
                )])
            } else {
                Some(Vec::new())
            }
        }));
    });

    gc_stale_team_windows(
        "my-team",
        "dev:3",
        &[
            "dev:2".to_string(),
            "dev:3".to_string(),
            "dev:4".to_string(),
        ],
    );

    let cleared: Vec<String> = with_state(|st| st.cleared.iter().map(|(w, _)| w.clone()).collect());
    assert!(!cleared.contains(&"dev:2".to_string())); // live duplicate preserved
    assert!(cleared.contains(&"dev:4".to_string())); // stale duplicate still cleared
}

/// A failed pane listing is unknown, not proof of staleness — clear nothing.
#[test]
fn test_gc_stale_team_windows_skips_cleanup_on_tmux_failure() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    with_state(|st| {
        st.list_panes_full_or_none_fn = Some(Box::new(|_target| None));
    });

    gc_stale_team_windows(
        "my-team",
        "dev:3",
        &[
            "dev:2".to_string(),
            "dev:3".to_string(),
            "dev:4".to_string(),
        ],
    );

    assert!(with_state(|st| st.cleared.clone()).is_empty());
}

/// Two live windows share a team name; prefer_pane picks one for routing and
/// the other keeps its tags. Bug A: never clobber a live duplicate.
#[test]
fn test_find_team_window_keeps_live_duplicate() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    with_state(|st| {
        st.run_fn = Some(run_stdout(
            "dev:2\t@2\t0-2\t/tmp/ws2\tdesc\t0\ndev:3\t@3\t0-2\t/tmp/ws3\tdesc\t0\n",
        ));
        st.pane_window_targets
            .insert("%40".to_string(), "dev:3".to_string());
        st.list_panes_full_or_none_fn = Some(Box::new(|target| {
            if target == "dev:2" {
                Some(vec![
                    pane_info("%10", "claude", "agent", "worker", "0-2"),
                    pane_info("%11", "codex", "agent", "validator", "0-2"),
                ])
            } else if target == "dev:3" {
                Some(vec![
                    pane_info("%40", "claude", "agent", "worker", "0-2"),
                    pane_info("%41", "codex", "agent", "validator", "0-2"),
                ])
            } else {
                Some(Vec::new())
            }
        }));
    });

    let (wt, _data) = find_team_window("0-2", "%40").unwrap();

    assert_eq!(wt, "dev:3"); // prefer_pane window wins for routing
    let cleared: Vec<String> = with_state(|st| st.cleared.iter().map(|(w, _)| w.clone()).collect());
    assert!(!cleared.contains(&"dev:2".to_string())); // the other live duplicate keeps its tags
}

/// Two windows sharing a team name are reported with their ids + live members;
/// a uniquely-named team is not.
#[test]
fn test_duplicate_team_bindings_reports_only_collisions() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    with_state(|st| {
        st.run_fn = Some(run_stdout(
            "0:2\t@2\t0-2\t/tmp/hive-0-w2\n0:3\t@3\t0-2\t/tmp/hive-0-w3\n0:5\t@5\tsolo\t/tmp/hive-0-w5\n",
        ));
        st.list_panes_full_fn = Some(Box::new(|target| match target {
            "0:2" => vec![pane_info("%42", "claude", "agent", "worker", "0-2")],
            "0:3" => vec![pane_info("%10", "claude", "agent", "worker", "0-2")],
            "0:5" => vec![pane_info("%80", "claude", "agent", "worker", "solo")],
            _ => Vec::new(),
        }));
    });

    let dupes = duplicate_team_bindings().unwrap();

    assert_eq!(dupes.len(), 1); // only the colliding team, not the unique "solo"
    assert_eq!(dupes[0].get("team").unwrap(), "0-2");
    let windows = dupes[0].get("windows").unwrap().as_array().unwrap();
    let window_ids: std::collections::HashSet<String> = windows
        .iter()
        .map(|w| w.get("windowId").unwrap().as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        window_ids,
        ["@2".to_string(), "@3".to_string()].into_iter().collect()
    );
    assert_eq!(
        windows[0].get("liveMembers").unwrap().as_array().unwrap()[0]
            .get("name")
            .unwrap(),
        "worker"
    );
    assert!(dupes[0]
        .get("repair")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("manual"));
}

// --- registry-first load (tmux is display, not truth) ---

/// A team with a registry entry and no tmux window is loadable: members
/// come back pane-less with their recorded engine identity (was
/// FileNotFoundError when tmux was the truth layer).
#[test]
fn test_team_load_registry_only_team_loads_without_any_window() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    let members = vec![obj(json!({
        "name": "worker", "cli": "grok", "model": "grok-4.6",
        "sessionId": "sid-g", "cwd": "/repo",
    }))];
    assert_eq!(
        crate::registry::record_team("ghostteam", "/tmp/ws-g", "111.0", &members, "").unwrap(),
        "written"
    );

    let loaded = Team::load("ghostteam", "").unwrap();

    assert_eq!(loaded.workspace, "/tmp/ws-g");
    assert_eq!(loaded.created_at, 111.0);
    assert_eq!(loaded.tmux_window, "");
    let worker = loaded.agent_named("worker").unwrap();
    assert_eq!(worker.pane_id, "");
    assert_eq!(worker.cli, "grok");
    assert_eq!(worker.session_id.as_deref(), Some("sid-g"));
    assert_eq!(worker.cwd, "/repo");
}

#[test]
fn test_team_load_unknown_everywhere_still_raises() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");

    let err = Team::load("nosuchteam", "").unwrap_err();
    assert!(
        err.to_string().contains("Team 'nosuchteam' not found"),
        "{err}"
    );
}

/// Union semantics: a live pane binds onto its registry row (display wins
/// for pane-derived fields, the recorded engine identity survives a wiped
/// pane record), and a registry row without a pane stays as a member.
#[test]
fn test_team_load_pane_binds_onto_registry_roster() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    let members = vec![
        obj(json!({
            "name": "alive", "cli": "codex", "model": "m0",
            "sessionId": "sid-a", "cwd": "/old",
        })),
        obj(json!({
            "name": "headless", "cli": "claude", "sessionId": "sid-h", "cwd": "/repo",
        })),
    ];
    assert_eq!(
        crate::registry::record_team("team-u", "/tmp/ws-u", "5.0", &members, "").unwrap(),
        "written"
    );
    set_hive_window("dev:0", "team-u", "", "", "5.0");
    with_state(|st| {
        st.list_panes_full_fn = Some(Box::new(|_target| {
            let mut pane = pane_info("%1", "codex", "agent", "alive", "team-u");
            pane.cli = "codex".to_string();
            vec![pane]
        }));
        st.default_display_value = Some("/fresh".to_string());
    });

    let loaded = Team::load("team-u", "").unwrap();

    let names: std::collections::HashSet<String> =
        loaded.agents.iter().map(|a| a.name.clone()).collect();
    assert_eq!(
        names,
        ["alive".to_string(), "headless".to_string()]
            .into_iter()
            .collect()
    );
    let alive = loaded.agent_named("alive").unwrap();
    assert_eq!(alive.pane_id, "%1");
    assert_eq!(alive.cwd, "/fresh"); // live pane is fresher than the registry row
    assert_eq!(alive.session_id.as_deref(), Some("sid-a")); // wiped pane record falls back to registry
    assert_eq!(loaded.agent_named("headless").unwrap().pane_id, "");
}

/// A parked mirror's hidden window answers `@hive-team` through its
/// pane; every team-window scan masks it.
#[test]
fn test_window_scans_mask_the_hidden_mirror_window() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    with_state(|st| {
        st.window_options.insert(
            "dev:1".to_string(),
            HashMap::from([
                ("hive-team".to_string(), "honey".to_string()),
                ("hive-workspace".to_string(), "/tmp/ws".to_string()),
            ]),
        );
        st.window_options.insert(
            "honey:9".to_string(),
            HashMap::from([
                ("hive-hidden".to_string(), "honey".to_string()),
                ("hive-team".to_string(), "honey".to_string()),
            ]),
        );
        st.list_panes_full_fn = Some(Box::new(|target| match target {
            "dev:1" => vec![pane_info("%2", "grok", "agent", "sage", "honey")],
            "honey:9" => vec![pane_info("%1", "hive", "mirror", "orch", "honey")],
            _ => Vec::new(),
        }));
    });

    let teams = list_teams().unwrap();
    assert_eq!(teams.len(), 1);
    assert_eq!(teams[0].get("tmuxWindow").unwrap(), "dev:1");
    assert!(duplicate_team_bindings().unwrap().is_empty());
    let (wt, _) = find_team_window("honey", "").unwrap();
    assert_eq!(wt, "dev:1");
    assert!(with_state(|st| st.cleared.is_empty()));
}

#[test]
fn test_list_teams_unions_registry_and_windows() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    assert_eq!(
        crate::registry::record_team("headlessteam", "/tmp/ws-h", "1.0", &[], "").unwrap(),
        "written"
    );
    with_state(|st| {
        st.run_fn = Some(run_stdout("dev:0\twindowed\t/tmp/ws-w\n"));
    });

    let teams: HashMap<String, Map<String, Value>> = list_teams()
        .unwrap()
        .into_iter()
        .map(|t| (t.get("name").unwrap().as_str().unwrap().to_string(), t))
        .collect();

    assert_eq!(teams["headlessteam"].get("tmuxWindow").unwrap(), "");
    assert_eq!(teams["headlessteam"].get("workspace").unwrap(), "/tmp/ws-h");
    assert_eq!(teams["windowed"].get("tmuxWindow").unwrap(), "dev:0");
    assert_eq!(teams["windowed"].get("workspace").unwrap(), "/tmp/ws-w");
}

/// The registry is the name authority: a windowless team owns its name.
#[test]
fn test_create_refuses_a_registry_claimed_name() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    assert_eq!(
        crate::registry::record_team("team-h", "/tmp/ws", "1.0", &[], "").unwrap(),
        "written"
    );

    let err =
        Team::create_for_window("team-h", "dev:0", "", LEAD_AGENT_NAME, "", "", true).unwrap_err();
    assert!(
        err.to_string().contains("already exists in the registry"),
        "{err}"
    );
}

#[test]
fn test_team_status_payload_shape() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    real_tmux_pane_commands(&[("%0", "python3.12")]);
    let mut team = Team {
        name: "team-a".to_string(),
        workspace: "/tmp/ws".to_string(),
        lead_pane_id: "%0".to_string(),
        lead_session_id: Some("sess-1".to_string()),
        tmux_session: "dev".to_string(),
        ..Default::default()
    };
    team.agents.push(new_agent(
        "claude", "team-a", "%1", "claude", "", "m1", None,
    ));

    let payload = team.status();

    assert_eq!(payload.get("tmuxSession").unwrap(), "dev");
    assert_eq!(payload.get("tmuxWindow").unwrap(), "");
    let members = payload.get("members").unwrap().as_array().unwrap();
    let orch = members
        .iter()
        .find(|m| m.get("name").unwrap() == "orch")
        .unwrap();
    let claude = members
        .iter()
        .find(|m| m.get("name").unwrap() == "claude")
        .unwrap();
    assert_eq!(orch.get("role").unwrap(), "terminal");
    assert_eq!(claude.get("role").unwrap(), "agent");
}

fn team_with_pane_member(pane_id: &str) -> Team {
    let mut team = Team {
        name: "team-a".to_string(),
        workspace: "/ws".to_string(),
        ..Default::default()
    };
    team.agents.push(new_agent(
        "worker", "team-a", pane_id, "claude", "", "", None,
    ));
    team
}

fn stub_team_runtime(response: Value) {
    hook(|h| h.team_runtime = Some(Box::new(move |_, _| response.as_object().cloned())));
}

#[test]
fn test_usable_runtime_rejects_error_envelope_and_empty_body() {
    assert!(usable_runtime(None).is_none());
    assert!(usable_runtime(Some(Map::new())).is_none());
    let err = json!({"ok": false, "error": "team not found"});
    assert!(usable_runtime(err.as_object().cloned()).is_none());
    let ok = json!({"ok": true, "members": {}});
    assert_eq!(
        usable_runtime(ok.as_object().cloned()),
        ok.as_object().cloned()
    );
}

#[test]
fn test_member_alive_hived_error_falls_back_to_pane_liveness() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    stub_team_runtime(json!({"ok": false, "error": "load failed"}));

    assert!(team_with_pane_member("%1").member_alive("worker"));
    assert!(!team_with_pane_member("").member_alive("worker"));
}

#[test]
fn test_member_alive_hived_answer_is_authoritative() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    let team = team_with_pane_member("%1");

    stub_team_runtime(json!({"ok": true, "members": {"worker": {"cliAlive": false}}}));
    assert!(!team.member_alive("worker"));

    stub_team_runtime(json!({"ok": true, "members": {"worker": {"cliAlive": true}}}));
    assert!(team.member_alive("worker"));

    stub_team_runtime(json!({"ok": true, "members": {}}));
    assert!(!team.member_alive("worker"));
}

#[test]
fn test_member_alive_no_hived_uses_pane_liveness() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    // No hived answers: pin the seam instead of probing a real socket path.
    hook(|h| h.team_runtime = Some(Box::new(|_, _| None)));

    assert!(team_with_pane_member("%1").member_alive("worker"));
    assert!(!team_with_pane_member("").member_alive("worker"));
}

/// The real `resolve_member_cli` ladder: the pane tag answers first, then
/// the pane command (normalized), then the live profile probe, and
/// "claude" when the probe finds nothing. The probe runs only for the
/// last two rungs.
#[test]
fn test_resolve_member_cli_ladder_tag_then_command_then_probe_then_claude() {
    let (_tmp, _guard) = configure_hive_home(true, "%0");
    let probed: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&probed);
    hook(|h| {
        h.profile_for_pane = Some(Box::new(move |pane_id| {
            seen.borrow_mut().push(pane_id.to_string());
            (pane_id == "%3").then(|| "grok".to_string())
        }))
    });

    let mut tagged = pane_info("%1", "node", "agent", "a", "team-a");
    tagged.cli = "codex".to_string();
    let by_command = pane_info("%2", "/usr/local/bin/codex", "agent", "b", "team-a");
    let by_probe = pane_info("%3", "node", "agent", "c", "team-a");
    let unknown = pane_info("%4", "zsh", "agent", "d", "team-a");

    assert_eq!(resolve_member_cli(&tagged), "codex");
    assert_eq!(resolve_member_cli(&by_command), "codex");
    assert_eq!(resolve_member_cli(&by_probe), "grok");
    assert_eq!(resolve_member_cli(&unknown), "claude");
    assert_eq!(*probed.borrow(), vec!["%3".to_string(), "%4".to_string()]);
}
