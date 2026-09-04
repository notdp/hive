use std::collections::HashMap;

use crate::adapters::claude_bg::{EngineSession, KeyResult};
use crate::adapters::claude_sessions;

use super::testhook::{self, fake_engine, Hook};
use super::*;

fn setup() -> testhook::Guard {
    testhook::install(Hook::new())
}

fn hook<T>(f: impl FnOnce(&mut Hook) -> T) -> T {
    testhook::with(f).expect("test hook installed")
}

fn spawn_opts(f: impl FnOnce(&mut SpawnOptions)) -> SpawnOptions {
    let mut opts = SpawnOptions {
        cwd: "/tmp".to_string(),
        ..SpawnOptions::default()
    };
    f(&mut opts);
    opts
}

fn member(name: &str, team: &str, pane: &str, cli: &str) -> Agent {
    Agent {
        name: name.to_string(),
        team_name: team.to_string(),
        pane_id: pane.to_string(),
        model: String::new(),
        prompt: String::new(),
        cwd: "/tmp".to_string(),
        session_id: None,
        spawned_at: 0.0,
        cli: cli.to_string(),
    }
}

fn headless(cli: &str, session_id: Option<&str>) -> Agent {
    let mut agent = member("rex", "honey", "", cli);
    agent.cwd = "/repo".to_string();
    agent.session_id = session_id.map(|s| s.to_string());
    agent
}

/// Python `_mock_claude_bg_up`.
fn mock_claude_bg_up(job_id: &str, session_id: &str) {
    let engine = fake_engine(4321, job_id, session_id);
    hook(|h| {
        h.spawn_job_result = Some(job_id.to_string());
        h.wait_engine_entry = Some(engine.clone());
        h.ensure_engine = Some(Some(engine.clone()));
    });
}

/// Python `_mock_daemon_up`.
fn mock_daemon_up() {
    hook(|h| h.codex_spawn_daemon = true);
}

/// Python `_mock_grok_leader_up`.
fn mock_grok_leader_up() {
    hook(|h| {
        h.grok_spawn_daemon = true;
        h.wait_grok_ready = Some(true);
    });
}

/// Python `_pin_cli_probe`: "" pins "no live CLI process".
fn pin_cli_probe(name: &str) {
    hook(|h| h.cli_probe = Some(name.to_string()));
}

/// Python `_pin_job`: pane record -> engine entry.
fn pin_job(job_id: &str, engine: EngineSession) {
    hook(|h| {
        h.job_id_for_pane = Some(job_id.to_string());
        h.engines_by_job.insert(job_id.to_string(), engine);
    });
}

/// Python `_stale_claude_record`.
fn stale_claude_record() {
    pin_job("beef4321", fake_engine(4321, "beef4321", "sess-registry"));
    hook(|h| h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE));
}

fn calls() -> Vec<String> {
    hook(|h| h.calls.clone())
}

fn launch_of(cmd: &str) -> String {
    cmd.split(" && ")
        .last()
        .unwrap()
        .split("; hive resume-hint")
        .next()
        .unwrap()
        .to_string()
}

fn err_of<T: std::fmt::Debug>(result: anyhow::Result<T>) -> String {
    format!("{:#}", result.expect_err("expected an error"))
}

// --- spawn -------------------------------------------------------------

#[test]
fn test_spawn_rejects_outside_tmux_without_a_target() {
    let _guard = setup();
    hook(|h| h.is_inside_tmux = false);
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "",
        spawn_opts(|o| o.skill = "none".into()),
    ));
    assert!(err.contains("requires tmux"), "{err}");
}

#[test]
fn test_spawn_outside_tmux_with_target_pane_proceeds() {
    // An external orchestrator (workflow proxy, desktop session) has no
    // $TMUX and no member env markers, but a registry-known target pane
    // addresses the tmux server fine.
    let _guard = setup();
    hook(|h| h.is_inside_tmux = false);
    mock_claude_bg_up("abcd1234", "sess-registry");
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "none".into();
        }),
    )
    .unwrap();
}

#[test]
fn test_spawn_loads_specified_skill() {
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "demo-review".into();
        }),
    )
    .unwrap();
    // The skill activation rides the bg spawn's prompt, not the pane command.
    assert_eq!(hook(|h| h.spawns[0].prompt.clone()), "/demo-review t");
    assert!(!calls().iter().any(|c| c.contains("hive teammate")));
}

#[test]
fn test_spawn_skips_skill_when_none() {
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "none".into();
        }),
    )
    .unwrap();
    assert_eq!(hook(|h| h.spawns[0].prompt.clone()), "");
    assert!(!calls()
        .iter()
        .any(|c| c.starts_with('/') && !c.starts_with("/tmp")));
}

#[test]
fn test_spawn_passes_extra_env() {
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "none".into();
            o.extra_env = Some(vec![("CR_WORKSPACE".into(), "/tmp/cr-test".into())]);
        }),
    )
    .unwrap();
    let startup_cmd = calls()[0].clone();
    assert!(startup_cmd.contains("CR_WORKSPACE="));
    assert!(startup_cmd.contains("/tmp/cr-test"));
    // The engine runs outside the pane, so the env must reach the bg spawn —
    // and carry nothing else: identity is the engine's own session id, never
    // a variable hive hands it.
    let env_map = hook(|h| h.spawns[0].extra_env.clone());
    assert!(
        env_map.keys().all(|key| !key.starts_with("HIVE_")),
        "{env_map:?}"
    );
    let expected: HashMap<String, String> =
        [("CR_WORKSPACE".to_string(), "/tmp/cr-test".to_string())]
            .into_iter()
            .collect();
    assert_eq!(env_map, expected);
}

#[test]
fn test_spawn_without_extra_env_exports_nothing_in_the_pane() {
    let _guard = setup();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "none".into();
        }),
    )
    .unwrap();
    let startup_cmd = calls()[0].clone();
    assert!(!startup_cmd.contains("export "));
}

#[test]
fn test_spawn_hive_loads_skill_and_sends_prompt() {
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "hive".into();
            o.prompt = "Please check your inbox.".into();
        }),
    )
    .unwrap();
    // Skill activation + user prompt ride the bg spawn's positional prompt.
    assert_eq!(
        hook(|h| h.spawns[0].prompt.clone()),
        "/hive t\n\nPlease check your inbox."
    );
    assert_eq!(hook(|h| h.spawns[0].name.clone()), "t.w1");
}

#[test]
fn test_spawn_codex_hive_loads_skill_and_sends_prompt() {
    let _guard = setup();
    mock_daemon_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "hive".into();
            o.prompt = "Please check your inbox.".into();
            o.cli = "codex".into();
        }),
    )
    .unwrap();
    // Skill activation + user prompt are passed as the [PROMPT] positional
    // arg (avoids TUI keystroke race against the codex skill picker).
    let calls = calls();
    let startup_cmd = calls[0].clone();
    assert!(startup_cmd.contains("$hive"));
    assert!(startup_cmd.contains("Please check your inbox."));
    // Only the initial `cd ... && codex` Enter — no follow-up TUI inject.
    assert_eq!(calls.iter().filter(|c| *c == "<Enter>").count(), 1);
}

#[test]
fn test_spawn_claude_mints_job_records_pane_and_attaches() {
    // the job (and its engine entry) exist BEFORE the pane command is typed:
    // readiness is the engine registering, never screen text
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");
    let agent = Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.cli = "claude".into();
        }),
    )
    .unwrap();
    assert_eq!(agent.pane_id, "%0");
    assert!(hook(|h| h.captured.clone()).is_empty()); // no screen scraping anywhere in the spawn
    assert_eq!(hook(|h| h.spawns[0].name.clone()), "t.w1");
    assert_eq!(
        hook(|h| h.records.clone()),
        vec![(
            "%0".to_string(),
            "abcd1234".to_string(),
            "sess-registry".to_string(),
            "/tmp".to_string()
        )]
    );
    let launch = launch_of(&calls()[0]);
    assert_eq!(
        launch.split_whitespace().collect::<Vec<_>>(),
        vec!["hive", "claude", "--resume", "'abcd1234'"]
    );
}

#[test]
fn test_spawn_claude_mint_failure_kills_pane_and_fails() {
    let _guard = setup();
    hook(|h| h.spawn_job_result = None);
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.cli = "claude".into();
        }),
    ));
    assert!(err.contains("job identity"), "{err}");
    assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
    assert!(calls().is_empty()); // no startup command was ever sent
}

#[test]
fn test_spawn_claude_engine_never_registers_stops_job_and_fails() {
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");
    hook(|h| h.wait_engine_entry = None);
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.cli = "claude".into();
        }),
    ));
    assert!(err.contains("inbox-only"), "{err}");
    assert_eq!(hook(|h| h.stopped.clone()), vec!["abcd1234"]); // the half-born job is parked
    assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
    assert!(calls().is_empty());
}

#[test]
fn test_spawn_rejects_prompt_starting_with_dash() {
    // the launch goes through `hive <cli>`, whose parser strips any `--`
    // separator, so a dashed prompt would be read as a flag: refuse it
    for cli_name in ["claude", "codex", "grok"] {
        let _guard = setup();
        mock_daemon_up();
        mock_grok_leader_up();
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.cli = cli_name.into();
                o.skill = "none".into();
                o.prompt = "--edge prompt".into();
            }),
        ));
        assert!(err.contains("must not start with '-'"), "{err}");
    }
}

#[test]
fn test_spawn_pane_command_runs_hive_launcher_then_resume_hint() {
    // the pane runs hive's managed launcher as the binary (never the rc's
    // hclaude/hcodex/hgrok function) and prints the cd-ready hint once the
    // CLI exits
    for cli_name in ["claude", "codex", "grok"] {
        let _guard = setup();
        mock_daemon_up();
        mock_grok_leader_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.cli = cli_name.into();
                o.skill = "none".into();
            }),
        )
        .unwrap();
        let launch = calls()[0].split(" && ").last().unwrap().to_string();
        let tail = format!("; hive resume-hint {cli_name} 2>/dev/null || true");
        assert!(launch.ends_with(&tail), "{launch}");
        // token check, not a prefix: a bare claude launch now carries no flags
        let head = &launch[..launch.len() - tail.len()];
        assert_eq!(
            head.split_whitespace().take(2).collect::<Vec<_>>(),
            vec!["hive", cli_name]
        );
    }
}

#[test]
fn test_spawn_claude_resume_wakes_the_job_and_rebinds_the_pane() {
    // resume of a claude member is just waking its durable job: nothing is
    // minted, the pane record points at the same jobId
    let _guard = setup();
    mock_claude_bg_up("cafe0123", "sess-registry");
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.cli = "claude".into();
            o.session_id = Some("cafe0123".into());
            o.session_mode = "resume".into();
        }),
    )
    .unwrap();
    assert!(hook(|h| h.spawns.clone()).is_empty()); // nothing minted on resume
    assert_eq!(hook(|h| h.wakes.clone()), vec!["cafe0123"]);
    assert_eq!(
        hook(|h| h.records.clone()),
        vec![(
            "%0".to_string(),
            "cafe0123".to_string(),
            "sess-registry".to_string(),
            "/tmp".to_string()
        )]
    );
    assert!(calls()[0].contains("--resume 'cafe0123'"));
}

#[test]
fn test_spawn_claude_resume_of_a_gone_job_fails_and_gives_the_pane_back() {
    let _guard = setup();
    hook(|h| h.ensure_engine = Some(None));
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.cli = "claude".into();
            o.session_id = Some("cafe0123".into());
            o.session_mode = "resume".into();
        }),
    ));
    assert!(err.contains("did not come back"), "{err}");
    assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
    assert!(calls().is_empty());
}

#[test]
fn test_spawn_tags_pane_before_waiting_for_ready() {
    let _guard = setup();
    Agent::spawn(
        "w1",
        "t",
        "%9",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "claude".into();
        }),
    )
    .unwrap();
    assert!(
        !calls().is_empty(),
        "spawn should still start the CLI process"
    );
    assert_eq!(
        hook(|h| h.tags.clone()),
        vec![(
            "%9".to_string(),
            "agent".to_string(),
            "w1".to_string(),
            "t".to_string()
        )]
    );
}

#[test]
fn test_spawn_claude_pins_model_at_bg_spawn_not_pane_flag() {
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.model = "opus".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "claude".into();
        }),
    )
    .unwrap();
    // model is a bg-spawn flag (durable in respawnFlags), not a viewer flag
    assert_eq!(
        hook(|h| h.spawns[0].extra_args.clone()),
        vec!["--model", "opus"]
    );
    assert!(!calls()[0].contains("--model"));
}

#[test]
fn test_spawn_codex_pins_model_at_mint_not_flag() {
    let _guard = setup();
    mock_daemon_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.model = "gpt-5.2".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
        }),
    )
    .unwrap();
    let startup_cmd = calls()[0].clone();
    // model is a thread/start property, not a resume flag
    assert!(!startup_cmd.contains("-m 'gpt-5.2'"));
    assert_eq!(
        hook(|h| h.codex_minted.clone()),
        vec![(
            "/tmp".to_string(),
            "t.w1".to_string(),
            "gpt-5.2".to_string()
        )]
    );
}

#[test]
fn test_spawn_rejects_unknown_cli() {
    let _guard = setup();
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.skill = "none".into();
            o.cli = "vim".into();
        }),
    ));
    assert!(err.contains("unsupported cli"), "{err}");
}

#[test]
fn test_spawn_claude_fork_mints_a_new_job_from_the_session() {
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "claude".into();
            o.session_id = Some("sess-abc".into());
        }),
    )
    .unwrap();
    // fork mode: a NEW bg job branches the source session server-side
    assert_eq!(
        hook(|h| h.spawns[0].extra_args.clone()),
        vec!["-r", "sess-abc", "--fork-session"]
    );
    assert!(calls()[0].contains("--resume 'abcd1234'")); // the pane attaches to the fork
}

#[test]
fn test_spawn_codex_resume_uses_fork_subcommand() {
    let _guard = setup();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
            o.session_id = Some("sess-abc".into());
        }),
    )
    .unwrap();
    let startup_cmd = calls()[0].clone();
    assert!(startup_cmd
        .split(" && ")
        .last()
        .unwrap()
        .starts_with("hive codex -c check_for_update_on_startup=false fork 'sess-abc'"));
    // codex fork does not take --model; model flag should not appear
    assert!(!startup_cmd.contains("-m"));
}

#[test]
fn test_spawn_codex_new_session_resumes_minted_thread() {
    let _guard = setup();
    mock_daemon_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
        }),
    )
    .unwrap();
    let startup_cmd = calls()[0].clone();
    // hive minted the thread, recorded the pane binding, trusted the cwd, and
    // the pane attaches with `resume <tid>` — the managed launcher injects
    // --remote/--cd itself, so the spawn command carries neither.
    assert!(startup_cmd.contains("resume 'tid-minted'"));
    assert!(!startup_cmd.contains("--remote"));
    assert!(!startup_cmd.contains("--cd"));
    assert_eq!(
        hook(|h| h.codex_minted.clone()),
        vec![("/work/dir".to_string(), "t.w1".to_string(), "".to_string())]
    );
    assert_eq!(hook(|h| h.codex_trusted.clone()), vec!["/work/dir"]);
    assert_eq!(
        hook(|h| h.codex_records.clone()),
        vec![(
            "%0".to_string(),
            "tid-minted".to_string(),
            "/work/dir".to_string()
        )]
    );
}

#[test]
fn test_spawn_codex_mint_failure_kills_pane_and_fails() {
    let _guard = setup();
    mock_daemon_up();
    hook(|h| h.start_member_thread = None);
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
        }),
    ));
    assert!(err.contains("thread identity"), "{err}");
    assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
    assert!(calls().is_empty()); // no startup command was ever sent
}

#[test]
fn test_spawn_codex_preconnects_2nd_client_with_workspace() {
    // With a workspace, spawn asks the hived to bring its client online
    // before the member's first turn.
    let _guard = setup();
    mock_daemon_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
            o.workspace = "/tmp/ws".into();
        }),
    )
    .unwrap();
    assert_eq!(hook(|h| h.connects_codex.clone()), vec!["/tmp/ws"]);
}

#[test]
fn test_spawn_codex_skips_preconnect_without_workspace() {
    let _guard = setup();
    mock_daemon_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
        }),
    )
    .unwrap(); // no workspace → no eager preconnect, lazy tick covers it
    assert!(hook(|h| h.connects_codex.clone()).is_empty());
}

#[test]
fn test_spawn_codex_new_session_refuses_when_daemon_fails() {
    // Embedded codex is unsupported: if the shared daemon cannot bind, spawn
    // must not launch a raw codex as a team member — it kills the pane it
    // just split and raises instead of leaving a stateless tagged member.
    let _guard = setup(); // spawn_daemon defaults to false
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
        }),
    ));
    assert!(err.contains("daemon-only"), "{err}");
    assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]); // the split pane is cleaned up
    assert!(calls().is_empty()); // no startup command was ever sent
}

#[test]
fn test_spawn_codex_daemon_fail_in_place_clears_tags_instead_of_killing() {
    // split_window=false spawns into the caller's own shell pane: on daemon
    // failure that pane must survive, but the hive tags just written are
    // undone.
    let _guard = setup();
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
            o.split_window = false;
        }),
    ));
    assert!(err.contains("daemon-only"), "{err}");
    assert!(hook(|h| h.killed.clone()).is_empty());
    assert_eq!(hook(|h| h.cleared_tags.clone()), vec!["%0"]);
    assert!(calls().is_empty());
}

#[test]
fn test_spawn_codex_fork_does_not_start_daemon() {
    // The pane's `hive codex fork <sid>` binds the daemon, forks server-side
    // and records the pane's thread itself; spawn stays out of it.
    let _guard = setup();
    hook(|h| h.codex_spawn_daemon = true);
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
            o.session_id = Some("sess-abc".into());
        }),
    )
    .unwrap();
    let startup_cmd = calls()[0].clone();
    assert!(startup_cmd.contains("fork") && startup_cmd.contains("sess-abc"));
    assert!(!startup_cmd.contains("--remote")); // the launcher injects it
    assert!(hook(|h| h.codex_started.clone()).is_empty()); // daemon not started by spawn for a fork
}

#[test]
fn test_spawn_grok_launches_with_minted_session_id_and_model_flag() {
    let _guard = setup();
    mock_grok_leader_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.model = "grok-4.6".into();
            o.cwd = "/work/dir".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "grok".into();
        }),
    )
    .unwrap();
    let launch = launch_of(&calls()[0]);
    let (pane, session_id, cwd) = hook(|h| h.grok_sessions[0].clone());
    assert_eq!((pane.as_str(), cwd.as_str()), ("%0", "/work/dir"));
    assert_eq!(
        launch.split_whitespace().collect::<Vec<_>>(),
        vec![
            "hive",
            "grok",
            "--session-id",
            session_id.as_str(),
            "-m",
            "'grok-4.6'"
        ]
    );
}

#[test]
fn test_spawn_grok_resume_keeps_the_session_id_and_drops_fork_flag() {
    let _guard = setup();
    mock_grok_leader_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.skill = "none".into();
            o.cli = "grok".into();
            o.session_id = Some("sess-abc".into());
            o.session_mode = "resume".into();
        }),
    )
    .unwrap();
    let launch = launch_of(&calls()[0]);
    assert_eq!(
        launch.split_whitespace().collect::<Vec<_>>(),
        vec!["hive", "grok", "--resume", "'sess-abc'"]
    );
    // the pane drives the resumed session itself — no new id is minted
    assert_eq!(
        hook(|h| h.grok_sessions.clone()),
        vec![("%0".to_string(), "sess-abc".to_string(), "/tmp".to_string())]
    );
}

#[test]
fn test_spawn_grok_fork_mints_a_new_session_id_for_the_branch() {
    let _guard = setup();
    mock_grok_leader_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.skill = "none".into();
            o.cli = "grok".into();
            o.session_id = Some("sess-abc".into());
        }),
    )
    .unwrap();
    let launch = launch_of(&calls()[0]);
    let forked_id = hook(|h| h.grok_sessions[0].1.clone());
    assert_ne!(forked_id, "sess-abc");
    assert_eq!(
        launch.split_whitespace().collect::<Vec<_>>(),
        vec![
            "hive",
            "grok",
            "--session-id",
            forked_id.as_str(),
            "--resume",
            "'sess-abc'",
            "--fork-session"
        ]
    );
}

#[test]
fn test_spawn_grok_refuses_when_leader_daemon_fails() {
    // Grok runtime lives on the per-pane leader: without one the pane would
    // run a grok nobody can reach, so spawn gives the pane back and raises.
    let _guard = setup(); // grok spawn_daemon defaults to false
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.skill = "none".into();
            o.cli = "grok".into();
        }),
    ));
    assert!(err.contains("leader-only"), "{err}");
    assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
    assert!(calls().is_empty()); // no launch command was ever sent
    assert!(hook(|h| h.grok_sessions.clone()).is_empty()); // and no session record left behind
}

#[test]
fn test_spawn_grok_leader_fail_in_place_clears_tags_instead_of_killing() {
    let _guard = setup();
    let err = err_of(Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.skill = "none".into();
            o.cli = "grok".into();
            o.split_window = false;
        }),
    ));
    assert!(err.contains("leader-only"), "{err}");
    assert!(hook(|h| h.killed.clone()).is_empty());
    assert_eq!(hook(|h| h.cleared_tags.clone()), vec!["%0"]);
    assert!(calls().is_empty());
}

#[test]
fn test_spawn_grok_connects_the_2nd_client_once_the_session_is_ready() {
    // the client can only load a session the TUI has opened, so the connect
    // follows readiness instead of racing it
    let _guard = setup();
    mock_grok_leader_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.skill = "none".into();
            o.cli = "grok".into();
            o.workspace = "/tmp/ws".into();
        }),
    )
    .unwrap();
    assert_eq!(
        hook(|h| h.event_order.clone()),
        vec!["ready:%0", "connect:/tmp/ws:%0"]
    );
}

#[test]
fn test_spawn_grok_skips_the_connect_when_readiness_times_out() {
    let _guard = setup();
    mock_grok_leader_up();
    hook(|h| h.wait_grok_ready = Some(false));
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.skill = "none".into();
            o.cli = "grok".into();
            o.workspace = "/tmp/ws".into();
        }),
    )
    .unwrap();
    assert!(hook(|h| h.connects_grok.clone()).is_empty()); // nothing to load yet; the lazy connect retries
}

#[test]
fn test_spawn_grok_skips_preconnect_without_workspace() {
    let _guard = setup();
    mock_grok_leader_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.skill = "none".into();
            o.cli = "grok".into();
        }),
    )
    .unwrap(); // lazy connect on the next tick covers it
    assert!(hook(|h| h.connects_grok.clone()).is_empty());
}

// --- send --------------------------------------------------------------

#[test]
fn test_send_codex_uses_turn_start_when_daemon_accepts() {
    // pin the process probe: the real one inspects the live tmux pane "%3",
    // which detects whatever CLI happens to run there on this machine
    let _guard = setup();
    pin_cli_probe("codex");
    hook(|h| h.codex_send_to_pane = Some("turnStartAccepted"));
    member("w", "t", "%3", "codex").send("hi").unwrap();
    assert_eq!(
        hook(|h| h.codex_sent.clone()),
        vec![("%3".to_string(), "hi".to_string())]
    );
    assert!(calls().is_empty()); // no keystroke fallback when daemon accepts
}

#[test]
fn test_send_uses_detected_codex_daemon_when_stored_cli_is_stale() {
    let _guard = setup();
    pin_cli_probe("codex");
    hook(|h| h.codex_send_to_pane = Some("turnStartAccepted"));
    member("w", "t", "%3", "claude").send("hi").unwrap();
    assert_eq!(
        hook(|h| h.codex_sent.clone()),
        vec![("%3".to_string(), "hi".to_string())]
    );
    assert!(calls().is_empty());
}

#[test]
fn test_send_codex_accepted_returns_classification_without_keystrokes() {
    let _guard = setup();
    pin_cli_probe("codex");
    hook(|h| h.codex_send_to_pane = Some("turnStartAccepted"));
    let accepted = member("w", "t", "%3", "codex").send("hi").unwrap();
    assert_eq!(accepted, "turnStartAccepted");
    assert!(calls().is_empty()); // native transport only — the composer is never touched
}

#[test]
fn test_send_codex_transport_failure_raises_without_keystrokes() {
    // VAL-5: any codex transport failure (no daemon, no thread, RPC error,
    // exception — the adapter folds them all to None) raises DeliveryError
    // and never falls back to keystroke injection.
    let _guard = setup();
    pin_cli_probe("codex");
    hook(|h| h.codex_send_to_pane = None);
    assert!(member("w", "t", "%3", "codex").send("hi").is_err());
    assert!(calls().is_empty());
}

#[test]
fn test_send_grok_queues_the_prompt_on_the_leader() {
    let _guard = setup();
    pin_cli_probe("grok");
    hook(|h| h.grok_send_to_pane = Some("sessionPromptQueued"));
    let accepted = member("w", "t", "%3", "grok").send("hi").unwrap();
    assert_eq!(accepted, "sessionPromptQueued");
    assert_eq!(
        hook(|h| h.grok_sent.clone()),
        vec![("%3".to_string(), "hi".to_string())]
    );
    assert!(calls().is_empty()); // native transport only — the composer is never touched
}

#[test]
fn test_send_grok_transport_failure_raises_without_keystrokes() {
    // Every grok transport failure (no leader, no session record, RPC error,
    // ack timeout — the adapter folds them all to None) raises DeliveryError
    // and never falls back to keystroke injection.
    let _guard = setup();
    pin_cli_probe("grok");
    hook(|h| h.grok_send_to_pane = None);
    assert!(member("w", "t", "%3", "grok").send("hi").is_err());
    assert!(calls().is_empty());
}

#[test]
fn test_send_claude_writes_to_the_engine_inbox_as_the_member_address() {
    let _guard = setup();
    pin_cli_probe("claude");
    let engine = fake_engine(4321, "abcd1234", "sess-registry");
    pin_job(&engine.job_id.clone(), engine.clone());
    hook(|h| h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE));
    let accepted = member("w", "t", "%3", "claude").send("hi").unwrap();
    assert_eq!(accepted, "udsWriteAccepted");
    // the engine's own session id rides the frame: claude drops a
    // mismatching one, so a recycled socket cannot take a dead session's
    // mail
    // no author named: the frame's origin is the team, never the recipient
    assert_eq!(
        hook(|h| h.inbox_writes.clone()),
        vec![(
            engine.socket_path.clone(),
            "hi".to_string(),
            "t".to_string(),
            engine.session_id.clone()
        )]
    );
    assert!(calls().is_empty()); // native transport only — the composer is never touched
}

#[test]
fn test_send_from_labels_the_inbox_frame_with_the_author_not_the_recipient() {
    // The frame's `from` is what the human's message card shows. It used to
    // be the recipient's own address, so every card read "Message from
    // <me>" whoever wrote it.
    let _guard = setup();
    pin_cli_probe("claude");
    let engine = fake_engine(4321, "abcd1234", "sess-registry");
    pin_job(&engine.job_id.clone(), engine.clone());
    hook(|h| h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE));
    member("w", "t", "%3", "claude")
        .send_from("hi", "t.author")
        .unwrap();
    let writes = hook(|h| h.inbox_writes.clone());
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].2, "t.author");
    assert_ne!(writes[0].2, "t.w");
}

#[test]
fn test_send_claude_resolves_the_engine_from_the_pane_job_record() {
    // the delivery address is derived pane -> job record -> engine entry;
    // nothing on the pane tty (the attach viewer!) is ever what gets
    // messaged
    let _guard = setup();
    pin_cli_probe("claude");
    hook(|h| {
        h.job_id_for_pane = Some("beef4321".to_string());
        h.engines_by_job.insert(
            "beef4321".to_string(),
            fake_engine(4321, "beef4321", "sess-registry"),
        );
        h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE);
    });
    member("w", "t", "%3", "claude").send("hi").unwrap();
    assert_eq!(hook(|h| h.pane_job_lookups.clone()), vec!["%3"]);
    assert_eq!(hook(|h| h.seen_jobs.clone()), vec!["beef4321"]); // the pane's own record keys the engine
}

#[test]
fn test_send_claude_pane_without_job_delivers_to_interactive_session() {
    // hive render draws an interactive member (a joined ccd) as a
    // read-only mirror pane tagged with the member's name; the roster
    // sessionId — not the mirror pane — is the delivery address.
    let _guard = setup();
    pin_cli_probe("claude");
    hook(|h| h.daemon_reply = Some("udsWriteAccepted"));
    let mut orch = member("orch", "t", "%690", "claude");
    orch.session_id = Some("ccd-sess-1".to_string());
    assert_eq!(orch.send("hi").unwrap(), "udsWriteAccepted");
    assert_eq!(
        hook(|h| h.daemon_replies.clone()),
        vec![("ccd-sess-1".to_string(), "hi".to_string())]
    );
}

#[test]
fn test_send_claude_without_job_record_raises() {
    let _guard = setup();
    pin_cli_probe("claude");
    let err = member("w", "t", "%3", "claude").send("hi").unwrap_err();
    assert!(err.0.contains("no bg job record"), "{err}");
    assert!(hook(|h| h.inbox_writes.clone()).is_empty()); // no socket to write to; nothing was attempted
    assert!(calls().is_empty());
}

#[test]
fn test_send_claude_asleep_engine_is_woken_then_delivered() {
    // a parked engine (supervisor idles jobs after ~1h) is not a dead
    // member: the ledger still lists the job, the wake revives it, delivery
    // proceeds
    let _guard = setup();
    pin_cli_probe(""); // no viewer on the pane either
    let engine = fake_engine(4321, "beef4321", "sess-registry");
    hook(|h| {
        h.job_id_for_pane = Some("beef4321".to_string());
        h.job_row_ids = vec!["beef4321".to_string()];
        h.ensure_engine = Some(Some(engine.clone()));
        h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE);
    });
    let accepted = member("w", "t", "%3", "claude").send("hi").unwrap();
    assert_eq!(accepted, "udsWriteAccepted");
    assert_eq!(hook(|h| h.wakes.clone()), vec!["beef4321"]);
    assert_eq!(
        hook(|h| h.inbox_writes.clone()),
        vec![(
            engine.socket_path.clone(),
            "hi".to_string(),
            "t".to_string(), // no author named: the team is the origin
            engine.session_id.clone()
        )]
    );
    assert!(calls().is_empty());
}

#[test]
fn test_send_claude_gone_job_raises() {
    // the ledger no longer lists the job (removed): nothing to wake
    let _guard = setup();
    pin_cli_probe("");
    hook(|h| h.job_id_for_pane = Some("beef4321".to_string()));
    let err = member("w", "t", "%3", "claude").send("hi").unwrap_err();
    assert!(err.0.contains("gone"), "{err}");
    assert!(hook(|h| h.wakes.clone()).is_empty()); // nothing listed → no wake attempt
    assert!(calls().is_empty());
}

#[test]
fn test_send_claude_not_listening_raises_without_keystrokes() {
    let _guard = setup();
    pin_cli_probe("claude");
    pin_job("abcd1234", fake_engine(4321, "abcd1234", "sess-registry"));
    hook(|h| h.sessions_send = None);
    let err = member("w", "t", "%3", "claude").send("hi").unwrap_err();
    assert!(err.0.contains("not listening"), "{err}");
    assert!(calls().is_empty());
}

#[test]
fn test_send_claude_write_timeout_raises_and_is_not_an_accept() {
    // the listener took the connection but never read the frame: a stalled
    // session, reported as a failure rather than returned as a
    // classification
    let _guard = setup();
    pin_cli_probe("claude");
    pin_job("abcd1234", fake_engine(4321, "abcd1234", "sess-registry"));
    hook(|h| h.sessions_send = Some(claude_sessions::WRITE_TIMED_OUT));
    let err = member("w", "t", "%3", "claude").send("hi").unwrap_err();
    assert!(err.0.contains("did not drain the message in time"), "{err}");
    assert!(calls().is_empty());
}

#[test]
fn test_send_unknown_profile_raises_without_keystrokes() {
    // no CLI process on the pane TTY: the send gate refuses before any
    // transport
    let _guard = setup();
    pin_cli_probe("");
    assert!(member("w", "t", "%3", "mystery").send("hi").is_err());
    assert!(calls().is_empty());
}

#[test]
fn test_send_claude_never_uses_codex_daemon() {
    let _guard = setup();
    pin_cli_probe("claude");
    pin_job("abcd1234", fake_engine(4321, "abcd1234", "sess-registry"));
    hook(|h| {
        h.codex_send_to_pane = Some("turnStartAccepted");
        h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE);
    });
    member("w", "t", "%3", "claude").send("hi").unwrap();
    assert!(hook(|h| h.codex_sent.clone()).is_empty()); // codex daemon path not taken for claude
    assert_eq!(hook(|h| h.inbox_writes.clone()).len(), 1); // claude delivers over its session inbox
}

#[test]
fn test_send_codex_member_never_routes_into_a_stale_claude_record() {
    // A blind probe (tmux busy, nothing on the pane tty) must not hand a
    // codex member's message to whatever claude job the pane id used to
    // host.
    let _guard = setup();
    pin_cli_probe("");
    stale_claude_record();
    let err = member("w", "t", "%3", "codex").send("hi").unwrap_err();
    assert!(err.0.contains("no live CLI process"), "{err}");
    assert!(hook(|h| h.inbox_writes.clone()).is_empty()); // the other member's inbox was never opened
    assert!(calls().is_empty());
}

#[test]
fn test_send_codex_member_refuses_a_pane_probed_as_claude() {
    // The probe itself reads the stale job record as evidence of a live
    // claude, so 'the probe said claude' is not enough — the member hive
    // spawned on this pane is codex, and its transport is the daemon.
    let _guard = setup();
    pin_cli_probe("claude");
    stale_claude_record();
    hook(|h| h.codex_send_to_pane = Some("turnStartAccepted"));
    let err = member("w", "t", "%3", "codex").send("hi").unwrap_err();
    assert!(err.0.contains("does not deliver across CLIs"), "{err}");
    assert!(hook(|h| h.inbox_writes.clone()).is_empty());
    assert!(hook(|h| h.codex_sent.clone()).is_empty()); // a claude-looking pane is not a codex thread either
    assert!(calls().is_empty());
}

// --- draft guard ---------------------------------------------------------

#[test]
fn test_save_and_clear_draft_keeps_the_draft_when_the_buffer_save_fails() {
    // tmux never took the buffer: clearing the composer now would destroy
    // the only copy of the user's draft.
    let _guard = setup();
    hook(|h| {
        h.supported_profile = true;
        h.parse_draft = Some("unsent thought".to_string());
        h.load_buffer_fails = true;
    });
    assert_eq!(_save_and_clear_draft("%3", "claude"), "");
    assert!(hook(|h| h.draft_cleared.clone()).is_empty());
}

#[test]
fn test_save_and_clear_draft_still_restores_when_the_clear_fails() {
    // The buffer holds the draft, so a half-done clear must still hand the
    // restore its buffer name.
    let _guard = setup();
    hook(|h| {
        h.supported_profile = true;
        h.parse_draft = Some("unsent thought".to_string());
        h.clear_input_fails = true;
    });
    assert_eq!(_save_and_clear_draft("%3", "claude"), "hive_draft_3");
}

// --- session detection ---------------------------------------------------

#[test]
fn test_spawn_claude_skips_session_detection() {
    let _guard = setup();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "claude".into();
        }),
    )
    .unwrap();
    assert!(
        hook(|h| h.resolved_session_panes.clone()).is_empty(),
        "should not resolve session for claude"
    );
}

#[test]
fn test_detect_current_session_id_delegates_to_resolve() {
    let _guard = setup();
    hook(|h| {
        h.session_ids_by_pane
            .insert("%11".to_string(), "map-sess-1".to_string());
    });
    assert_eq!(
        detect_current_session_id("/tmp/test", "", "%11"),
        Some("map-sess-1".to_string())
    );
    assert_eq!(detect_current_session_id("/tmp/test", "", "%99"), None);
}

// --- session_mode: fork vs resume (VAL B5-B7) ----------------------------

#[test]
fn test_spawn_claude_fork_and_resume_session_semantics() {
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");

    // fork: a new bg job branches the source session
    Agent::spawn(
        "w",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "claude".into();
            o.session_id = Some("sess-1".into());
        }),
    )
    .unwrap();
    assert_eq!(
        hook(|h| h.spawns.last().unwrap().extra_args.clone()),
        vec!["-r", "sess-1", "--fork-session"]
    );
    assert!(hook(|h| h.wakes.clone()).is_empty());

    // resume: the id is the durable jobId — wake it, mint nothing
    hook(|h| h.calls.clear());
    Agent::spawn(
        "w",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "claude".into();
            o.session_id = Some("cafe0123".into());
            o.session_mode = "resume".into();
        }),
    )
    .unwrap();
    assert_eq!(hook(|h| h.wakes.clone()), vec!["cafe0123"]);
    assert_eq!(hook(|h| h.spawns.len()), 1); // unchanged from the fork above
}

#[test]
fn test_spawn_codex_fork_delegates_to_hive_codex() {
    let _guard = setup();
    // spawn itself never touches the daemon for a fork (the pane's `hive
    // codex` binds it); the default spawn_daemon mock returning false must
    // not matter.
    Agent::spawn(
        "w",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "codex".into();
            o.session_id = Some("roll-1".into());
        }),
    )
    .unwrap();
    let launch = launch_of(&calls()[0]);
    assert!(launch.starts_with("hive codex "), "{launch}");
    assert!(launch.contains("fork 'roll-1'"));
    assert!(!launch.contains("--remote")); // the daemon binding is `hive codex`'s job
    assert!(!launch.contains("resume"));
}

#[test]
fn test_spawn_codex_resume_records_thread_and_resumes_it() {
    let _guard = setup();
    mock_daemon_up();
    Agent::spawn(
        "w",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/repo".into();
            o.cli = "codex".into();
            o.session_id = Some("roll-1".into());
            o.session_mode = "resume".into();
            o.skill = "none".into();
            o.workspace = "/ws".into();
        }),
    )
    .unwrap();
    let cmd = calls()[0].clone();
    // the resumed session's id IS its threadId: recorded, then resumed
    // through the managed launcher (which injects --remote/--cd itself)
    assert!(cmd.contains("resume 'roll-1'"));
    assert!(!cmd.contains("fork"));
    assert!(!cmd.contains("--remote"));
    assert!(hook(|h| h.codex_minted.clone()).is_empty()); // nothing minted on resume
    assert_eq!(
        hook(|h| h.codex_records.clone()),
        vec![("%0".to_string(), "roll-1".to_string(), "/repo".to_string())]
    );
    assert_eq!(hook(|h| h.connects_codex.clone()), vec!["/ws"]);
}

#[test]
fn test_spawn_codex_resume_daemon_failure_never_falls_back_embedded() {
    let _guard = setup();

    // split path: new pane is killed
    let err = err_of(Agent::spawn(
        "w",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "codex".into();
            o.session_id = Some("roll-1".into());
            o.session_mode = "resume".into();
        }),
    ));
    assert!(err.contains("daemon"), "{err}");
    assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
    assert!(calls().is_empty()); // no command was ever typed — no embedded fallback

    // in-place path: tags/title cleared instead
    let err = err_of(Agent::spawn(
        "w",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "codex".into();
            o.session_id = Some("roll-1".into());
            o.session_mode = "resume".into();
            o.split_window = false;
        }),
    ));
    assert!(err.contains("daemon"), "{err}");
    assert_eq!(hook(|h| h.cleared_tags.clone()), vec!["%0"]);
    assert!(hook(|h| h.titles.clone()).contains(&("%0".to_string(), "".to_string())));
    assert!(calls().is_empty());
}

#[test]
fn test_spawn_rejects_unknown_session_mode() {
    let _guard = setup();
    let err = err_of(Agent::spawn(
        "w",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "claude".into();
            o.session_id = Some("s".into());
            o.session_mode = "clone".into();
        }),
    ));
    assert!(err.contains("session_mode"), "{err}");
}

// --- readiness oracles: runtime signals, not screen text (VAL 1-7) ------

#[test]
fn test_spawn_claude_engine_readiness_skips_banner_and_settle() {
    let _guard = setup();

    // fresh and resume: the engine's registry entry is the oracle, the
    // banner (the pane only shows an attach viewer) is not consulted at all
    Agent::spawn("w", "t", "%0", spawn_opts(|o| o.cli = "claude".into())).unwrap();
    Agent::spawn(
        "w",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "claude".into();
            o.session_id = Some("cafe0123".into());
            o.session_mode = "resume".into();
        }),
    )
    .unwrap();

    assert!(!hook(|h| h.sleeps.clone()).contains(&1.0)); // no fixed 1s settle either
}

#[test]
fn test_spawn_codex_waits_on_process_not_banner() {
    let _guard = setup();
    mock_daemon_up();
    Agent::spawn(
        "v",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "codex".into();
            o.skill = "none".into();
            o.session_id = Some("roll-1".into());
            o.session_mode = "resume".into();
        }),
    )
    .unwrap();
    assert_eq!(hook(|h| h.waited_codex.clone()), vec!["%0"]);
}

#[test]
fn test_wait_codex_attached_polls_for_the_codex_process() {
    let _guard = setup();
    hook(|h| {
        h.cli_probe_seq = vec![None, Some("claude".to_string()), Some("codex".to_string())];
    });
    // None and a non-codex profile are both "not attached yet"
    assert!(_wait_codex_attached("%9", 60.0, 0.0));
}

#[test]
fn test_wait_codex_attached_timeout_is_deterministic_and_nonfatal() {
    let _guard = setup();
    assert!(!_wait_codex_attached("%9", 0.0, 0.0));

    // spawn survives a readiness timeout and still completes
    mock_daemon_up();
    hook(|h| h.wait_codex_attached = Some(false));
    let agent = Agent::spawn(
        "v",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "codex".into();
            o.skill = "hive".into();
        }),
    )
    .unwrap();
    assert_eq!(agent.pane_id, "%0");
}

#[test]
fn test_spawn_grok_waits_on_the_minted_session_dir_not_the_banner() {
    let _guard = setup();
    hook(|h| h.grok_spawn_daemon = true);
    Agent::spawn(
        "w",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "grok".into();
            o.skill = "none".into();
        }),
    )
    .unwrap();
    let waited = hook(|h| h.waited_grok.clone());
    let minted = hook(|h| h.grok_sessions[0].1.clone());
    assert_eq!(waited, vec![("%0".to_string(), minted)]); // the id hive minted, not the pane's cwd
}

#[test]
fn test_wait_grok_session_ready_sees_the_session_dir_and_is_nonfatal() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::env::set_var("GROK_HOME", tmp.path());
    {
        let _guard = setup();
        hook(|h| h.wait_grok_ready = None); // run the real wait
        pin_cli_probe("grok");
        assert!(!_wait_grok_session_ready("%0", "sess-x", 0.0, 0.0));

        // grok creates $GROK_HOME/sessions/<quoted cwd>/<sid>/ at startup
        std::fs::create_dir_all(tmp.path().join("sessions").join("%2Ftmp").join("sess-x")).unwrap();
        assert!(_wait_grok_session_ready("%0", "sess-x", 0.0, 0.0));

        // on resume the dir predates the launch, so the pane's own grok
        // must be up
        pin_cli_probe("");
        assert!(!_wait_grok_session_ready("%0", "sess-x", 0.0, 0.0));
    }

    // a readiness timeout is not fatal: spawn still completes
    let _guard = setup();
    mock_grok_leader_up();
    hook(|h| h.wait_grok_ready = Some(false));
    let agent = Agent::spawn(
        "v",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "grok".into();
            o.skill = "hive".into();
        }),
    )
    .unwrap();
    assert_eq!(agent.pane_id, "%0");
}

#[test]
fn test_spawn_codex_fork_waits_on_process_not_banner() {
    let _guard = setup();
    Agent::spawn(
        "f",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cli = "codex".into();
            o.session_id = Some("roll-1".into()); // fork mode
        }),
    )
    .unwrap();
    assert_eq!(hook(|h| h.waited_codex.clone()), vec!["%0"]);
}

// --- V1: the launch never execs — the pane shell must survive the CLI ---

/// Single-quote-aware tokenizer (hive quotes with single quotes only), so
/// quoted prompt text cannot green this on substrings.
fn sq_tokens(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in segment.chars() {
        match c {
            '\'' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// The CLI must run as the shell's foreground child: no `exec` token may
/// appear in the launch pipeline.
fn assert_launch_keeps_shell(startup_cmd: &str) {
    for segment in startup_cmd.split("&&") {
        assert!(
            !sq_tokens(segment).iter().any(|t| t == "exec"),
            "{startup_cmd}"
        );
    }
}

#[test]
fn test_launch_guard_catches_the_old_exec_form() {
    // negative control: the pre-change launch shape must trip the assertion
    let result = std::panic::catch_unwind(|| {
        assert_launch_keeps_shell("cd '/w' && exec /bin/codex --remote 'unix:///s'")
    });
    assert!(result.is_err());
}

#[test]
fn test_spawn_claude_fresh_launch_keeps_shell() {
    let _guard = setup();
    Agent::spawn("w1", "t", "%0", spawn_opts(|o| o.skill = "none".into())).unwrap();
    let startup_cmd = calls()[0].clone();
    assert_launch_keeps_shell(&startup_cmd);
    assert!(startup_cmd.contains("claude"));
}

#[test]
fn test_spawn_claude_resume_launch_keeps_shell() {
    let _guard = setup();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.skill = "none".into();
            o.session_id = Some("cafe0123".into());
            o.session_mode = "resume".into();
        }),
    )
    .unwrap();
    let startup_cmd = calls()[0].clone();
    assert_launch_keeps_shell(&startup_cmd);
    assert!(startup_cmd.contains("--resume 'cafe0123'")); // the pane reattaches the job
}

#[test]
fn test_spawn_codex_daemon_native_launch_keeps_shell() {
    let _guard = setup();
    mock_daemon_up();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.cwd = "/work/dir".into();
            o.is_first = true;
            o.skill = "none".into();
            o.cli = "codex".into();
        }),
    )
    .unwrap();
    let startup_cmd = calls()[0].clone();
    assert_launch_keeps_shell(&startup_cmd);
    assert!(startup_cmd.contains("resume 'tid-minted'")); // minted-thread attach shape
}

#[test]
fn test_spawn_codex_fork_shortcut_launch_keeps_shell() {
    let _guard = setup();
    Agent::spawn(
        "w1",
        "t",
        "%0",
        spawn_opts(|o| {
            o.skill = "none".into();
            o.cli = "codex".into();
            o.session_id = Some("sess-abc".into());
        }),
    )
    .unwrap();
    let startup_cmd = calls()[0].clone();
    assert_launch_keeps_shell(&startup_cmd);
    assert!(startup_cmd.contains("fork") && startup_cmd.contains("sess-abc"));
}

#[test]
fn test_spawn_skill_ref_is_bare_for_grok_and_qualified_for_claude() {
    // grok/codex register plugin skills by bare name (/hive, $hive); claude
    // addresses them fully qualified (/hive:hive). /skills in grok only
    // opens the picker — never format the grok launch with it.
    let _guard = setup();
    mock_claude_bg_up("abcd1234", "sess-registry");
    hook(|h| {
        h.grok_spawn_daemon = true;
        h.wait_grok_ready = Some(true);
    });

    Agent::spawn(
        "g",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.cli = "grok".into();
            o.skill = "hive:hive".into();
        }),
    )
    .unwrap();
    let grok_all = calls().join(" ");
    assert!(grok_all.contains("/hive"));
    assert!(!grok_all.contains("/skills") && !grok_all.contains("/hive:hive"));

    hook(|h| h.calls.clear());
    Agent::spawn(
        "c",
        "t",
        "%0",
        spawn_opts(|o| {
            o.is_first = true;
            o.cli = "claude".into();
            o.skill = "hive:hive".into();
        }),
    )
    .unwrap();
    // claude's skill rides the bg spawn prompt, fully qualified
    assert!(hook(|h| h.spawns.last().unwrap().prompt.clone()).starts_with("/hive:hive"));
}

// --- headless members (tests/unit/test_agent_headless.py) ----------------

#[test]
fn test_headless_codex_send_routes_by_thread() {
    let _guard = setup();
    hook(|h| h.codex_send_to_thread = Some("turnStartAccepted"));
    assert_eq!(
        headless("codex", Some("sid-1")).send("hi").unwrap(),
        "turnStartAccepted"
    );
    assert_eq!(
        hook(|h| h.codex_sent_thread.clone()),
        vec![("sid-1".to_string(), "hi".to_string())]
    );
}

#[test]
fn test_headless_codex_send_without_thread_refuses() {
    let _guard = setup();
    assert!(headless("codex", None).send("hi").is_err());
}

#[test]
fn test_headless_grok_send_routes_by_member_key() {
    let _guard = setup();
    hook(|h| h.grok_send_to_key = Some("sessionPromptQueued"));
    assert_eq!(
        headless("grok", Some("sid-1")).send("hi").unwrap(),
        "sessionPromptQueued"
    );
    assert_eq!(
        hook(|h| h.grok_sent_key.clone()),
        vec![("m-honey.rex".to_string(), "hi".to_string())]
    );
}

#[test]
fn test_headless_claude_send_delivers_to_job() {
    let _guard = setup();
    hook(|h| {
        h.job_row_ids = vec!["job-1".to_string()];
        h.engines_by_job
            .insert("job-1".to_string(), fake_engine(4321, "job-1", "sess-9"));
        h.daemon_reply = Some("udsWriteAccepted");
    });
    assert_eq!(
        headless("claude", Some("job-1")).send("hi").unwrap(),
        "udsWriteAccepted"
    );
    assert_eq!(
        hook(|h| h.daemon_replies.clone()),
        vec![("sess-9".to_string(), "hi".to_string())]
    );
}

#[test]
fn test_headless_grok_interrupt_routes_by_member_key() {
    let _guard = setup();
    hook(|h| h.grok_interrupt_key = Some("sessionCancelSent"));
    headless("grok", Some("sid-1")).interrupt().unwrap();
    assert_eq!(
        hook(|h| h.grok_interrupted_keys.clone()),
        vec!["m-honey.rex"]
    );
}

#[test]
fn test_headless_codex_interrupt_routes_by_thread() {
    let _guard = setup();
    hook(|h| h.codex_interrupt_thread = Some("turnInterruptAccepted"));
    headless("codex", Some("sid-1")).interrupt().unwrap();
    assert_eq!(hook(|h| h.codex_interrupted_threads.clone()), vec!["sid-1"]);
}

#[test]
fn test_headless_is_alive_probes_the_engine() {
    let _guard = setup();
    hook(|h| h.codex_daemon_alive = Some(true));
    assert!(headless("codex", Some("sid-1")).is_alive());
    hook(|h| h.codex_daemon_alive = Some(false));
    assert!(!headless("codex", Some("sid-1")).is_alive());

    hook(|h| h.grok_probe_socket = Some(true));
    assert!(headless("grok", Some("sid-1")).is_alive());

    hook(|h| h.job_row_ids = vec!["job-1".to_string()]);
    assert!(headless("claude", Some("job-1")).is_alive()); // asleep is not dead
    hook(|h| h.job_row_ids.clear());
    assert!(!headless("claude", Some("job-1")).is_alive());
}

#[test]
fn test_headless_claude_send_falls_back_to_interactive_session() {
    let _guard = setup();
    hook(|h| h.daemon_reply = Some("udsWriteAccepted"));
    assert_eq!(
        headless("claude", Some("ccd-sid-1")).send("hi").unwrap(),
        "udsWriteAccepted"
    );
    assert_eq!(
        hook(|h| h.daemon_replies.clone()),
        vec![("ccd-sid-1".to_string(), "hi".to_string())]
    );
}

#[test]
fn test_headless_claude_session_send_uses_inbox_socket_fallback() {
    let _guard = setup();
    hook(|h| {
        h.list_sessions = vec![claude_sessions::ClaudeSession {
            name: String::new(),
            pid: 1,
            cwd: String::new(),
            kind: String::new(),
            socket_path: "/tmp/ccd.sock".to_string(),
            session_id: "ccd-sid-1".to_string(),
            title: String::new(),
        }];
        h.sessions_send = Some("accepted");
    });
    assert_eq!(
        headless("claude", Some("ccd-sid-1"))
            .send_from("hi", "t.orch")
            .unwrap(),
        "accepted"
    );
    let writes = hook(|h| h.inbox_writes.clone());
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, "/tmp/ccd.sock");
    assert_eq!(writes[0].2, "t.orch"); // the author, not the headless recipient
    assert_eq!(writes[0].3, "ccd-sid-1");
}

#[test]
fn test_grok_kill_takes_the_pane_down_before_the_leader() {
    // The pane's TUI is a leader client: kill the leader first and the
    // dying TUI raises a replacement on the same socket. The pid is read
    // ahead of the kill because tmux has no pane to answer about after it.
    // Nothing follows the daemon kill — clearing the key of its remaining
    // clients belongs to `kill_daemon_key`, not to a watch out here.
    let _guard = setup();
    member("bee", "hornet", "%3", "grok").kill();
    assert_eq!(
        hook(|h| h.event_order.clone()),
        vec![
            "pid:%3".to_string(),
            "pane:%3".to_string(),
            "wait:%3".to_string(),
            "pool:p3".to_string(),
            "daemon:p3".to_string(),
        ]
    );
}

#[test]
fn test_headless_grok_kill_addresses_the_member_key_without_a_pane() {
    let _guard = setup();
    headless("grok", None).kill();
    assert!(hook(|h| h.killed.clone()).is_empty());
    assert!(hook(|h| h.waited_pane_gone.clone()).is_empty());
    assert_eq!(
        hook(|h| h.grok_killed_keys.clone()),
        vec!["m-honey.rex".to_string()]
    );
}

#[test]
fn test_headless_claude_kill_never_stops_an_interactive_session() {
    let _guard = setup();
    headless("claude", Some("ccd-sid-1")).kill();
    assert!(hook(|h| h.stopped.clone()).is_empty());
}

// --- the hive paths that ride the key pipe (test_claude_key_pipe.py) -----

/// Python `_member_pane`.
fn member_pane(job_id: Option<&str>) {
    hook(|h| {
        h.resolve_profile_name = Some("claude".to_string());
        h.job_id_for_pane = job_id.map(|j| j.to_string());
    });
}

#[test]
fn test_submit_on_a_member_pane_pipes_into_the_job() {
    let _guard = setup();
    member_pane(Some("cafe1234"));
    hook(|h| {
        h.type_into_job_result = Some(KeyResult {
            ok: true,
            confirmed: "transcript".to_string(),
            why: String::new(),
        })
    });
    _submit_interactive_text("%1", "hello", "claude").unwrap();
    assert_eq!(
        hook(|h| h.typed.clone()),
        vec![("cafe1234".to_string(), "hello".to_string())]
    );
    assert!(calls().is_empty()); // a claude member's keyboard must not touch tmux
}

#[test]
fn test_submit_raises_when_the_job_did_not_take_the_text() {
    let _guard = setup();
    member_pane(Some("cafe1234"));
    hook(|h| {
        h.type_into_job_result = Some(KeyResult {
            ok: false,
            confirmed: String::new(),
            why: "never echoed".to_string(),
        })
    });
    let err = err_of(_submit_interactive_text("%1", "hello", "claude"));
    assert!(err.contains("never echoed"), "{err}");
}

#[test]
fn test_a_non_member_claude_pane_still_goes_through_tmux() {
    // No job record: a plain interactive claude TUI, typed at like any
    // other CLI — and refused when that TUI is not running.
    let _guard = setup();
    member_pane(None);
    hook(|h| h.interactive_claude_pid = Some(456));
    _submit_interactive_text("%1", "hello", "claude").unwrap();
    assert_eq!(calls(), vec!["hello", "<Enter>"]);

    hook(|h| h.interactive_claude_pid = None);
    let err = err_of(_submit_interactive_text("%1", "hello", "claude"));
    assert!(err.contains("no interactive claude"), "{err}");
}

#[test]
fn test_a_pane_whose_claude_is_an_attach_viewer_is_refused() {
    // A lost job record must not fall back onto the pane: the claude
    // process there is a viewer, and its composer belongs to whatever
    // session it shows — another member's, or a stranger's.
    let _guard = setup();
    member_pane(None);
    hook(|h| h.interactive_claude_pid = None); // the viewer is not an interactive claude
    let err = err_of(_submit_interactive_text("%1", "hello", "claude"));
    assert!(err.contains("no interactive claude"), "{err}");
    assert!(calls().is_empty());
}

#[test]
fn test_member_interrupt_pipes_escape_into_the_job() {
    let _guard = setup();
    member_pane(Some("cafe1234"));
    hook(|h| {
        h.interrupt_job_result = Some(KeyResult {
            ok: true,
            confirmed: "transcript".to_string(),
            why: String::new(),
        })
    });
    member("red", "probe", "%1", "claude").interrupt().unwrap();
    assert_eq!(hook(|h| h.interrupted_jobs.clone()), vec!["cafe1234"]);
    assert!(calls().is_empty());
}

#[test]
fn test_member_interrupt_without_a_job_record_is_refused() {
    // A lost job record leaves nothing addressable: Escape into the pane
    // would land in whatever session its viewer is showing, so hive
    // refuses instead.
    let _guard = setup();
    member_pane(None);
    let err = err_of(member("red", "probe", "%1", "claude").interrupt());
    assert!(err.contains("no bg job record"), "{err}");
    assert!(calls().is_empty());
}

#[test]
fn test_codex_interrupt_goes_to_the_thread_not_the_pane() {
    let _guard = setup();
    hook(|h| h.codex_interrupt_pane = Some("turnInterruptAccepted"));
    member("blue", "probe", "%2", "codex").interrupt().unwrap();
    assert_eq!(hook(|h| h.codex_interrupted_panes.clone()), vec!["%2"]);
    assert!(calls().is_empty());
}

#[test]
fn test_codex_interrupt_is_refused_when_the_rpc_is_not_accepted() {
    let _guard = setup();
    hook(|h| h.codex_interrupt_pane = None);
    let err = err_of(member("blue", "probe", "%2", "codex").interrupt());
    assert!(err.contains("turn/interrupt"), "{err}");
}

#[test]
fn test_grok_interrupt_goes_to_the_session_not_the_pane() {
    let _guard = setup();
    hook(|h| h.grok_interrupt_pane = Some("sessionCancelSent"));
    member("grey", "probe", "%3", "grok").interrupt().unwrap();
    assert_eq!(hook(|h| h.grok_interrupted_panes.clone()), vec!["%3"]);
    assert!(calls().is_empty());
}

#[test]
fn test_grok_interrupt_is_refused_when_the_cancel_is_not_accepted() {
    let _guard = setup();
    hook(|h| h.grok_interrupt_pane = None);
    let err = err_of(member("grey", "probe", "%3", "grok").interrupt());
    assert!(err.contains("session/cancel"), "{err}");
}

#[test]
fn test_interrupt_of_an_unsupported_cli_is_refused() {
    let _guard = setup();
    let err = err_of(member("odd", "probe", "%4", "cursor").interrupt());
    assert!(err.contains("no native interrupt"), "{err}");
    assert!(calls().is_empty());
}
