//! Dispatch journal. A nonterminal record from another process is ambiguous:
//! it is evidence of possible execution, never permission to submit again.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use super::*;
use crate::agent::TurnHandle;

#[derive(Debug)]
pub(super) struct ExistingOperation;
impl std::fmt::Display for ExistingOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("dispatch already recorded; execution may have started; do not resend")
    }
}
impl std::error::Error for ExistingOperation {}

struct Operation {
    record: Value,
    handle: Option<TurnHandle>,
    last_result: Option<Map<String, Value>>,
    persisted: bool,
    write_error: Option<String>,
}

fn active() -> &'static Mutex<HashMap<PathBuf, Operation>> {
    static CELL: OnceLock<Mutex<HashMap<PathBuf, Operation>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_path(workspace: &str, incarnation: &str, id: &str) -> PathBuf {
    hooked_run_dir(workspace)
        .join("operations")
        .join(incarnation)
        .join(format!("{id}.json"))
}

fn validate_key(incarnation: &str, id: &str) -> Result<()> {
    let epoch = incarnation
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n > 0.0);
    if epoch.is_none()
        || incarnation.contains(['/', '\\'])
        || id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid operation incarnation or dispatchId");
    }
    Ok(())
}

fn registry_incarnation(team: &str) -> Result<String> {
    let entry =
        crate::registry::load(team).ok_or_else(|| anyhow::anyhow!("team registry unavailable"))?;
    let created = entry
        .get("createdAt")
        .ok_or_else(|| anyhow::anyhow!("team incarnation missing"))?;
    let epoch = created
        .as_f64()
        .or_else(|| created.as_str()?.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .ok_or_else(|| anyhow::anyhow!("invalid team incarnation"))?;
    Ok(crate::team::created_at_key(epoch))
}

fn completed(operation: &Operation) -> bool {
    operation.record["state"] == "terminal" && operation.persisted
}

fn write_record(path: &Path, record: &Value) -> Result<()> {
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.tmp", crate::agent::uuid4()));
    let result = (|| {
        fs::write(&tmp, serde_json::to_vec(record)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(tmp);
    result
}

fn persist_operation(path: &Path, operation: &mut Operation) -> Result<()> {
    if operation.record["kind"] != "node" {
        operation.persisted = true;
        return Ok(());
    }
    match write_record(path, &operation.record) {
        Ok(()) => {
            operation.persisted = true;
            operation.write_error = None;
            Ok(())
        }
        Err(error) => {
            operation.persisted = false;
            let reason = error.to_string();
            if operation.write_error.as_deref() != Some(reason.as_str()) {
                eprintln!(
                    "hived: operation {} journal write failed: {reason}",
                    operation.record["dispatchId"].as_str().unwrap_or_default()
                );
            }
            operation.write_error = Some(reason);
            Err(error)
        }
    }
}

pub(super) fn prepare_operation(
    workspace: &str,
    team: &str,
    incarnation: &str,
    id: &str,
    target: &str,
    kind: &str,
) -> Result<PathBuf> {
    validate_key(incarnation, id)?;
    let path = record_path(workspace, incarnation, id);
    let mut operations = active().lock().unwrap_or_else(|e| e.into_inner());
    let previous = if let Some(operation) = operations.get(&path) {
        Some(operation.record.clone())
    } else if kind != "node" {
        None
    } else {
        match fs::read(&path) {
            Ok(bytes) => Some(serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(ExistingOperation.into()),
        }
    };
    if let Some(record) = &previous {
        // Only a definite pre-acceptance refusal can be retried. A prepared,
        // running, corrupt or completed record never permits another submit.
        if record["state"] != "terminal" || record["result"]["status"] != "refused" {
            return Err(ExistingOperation.into());
        }
    }
    let attempt = previous
        .as_ref()
        .and_then(|r| r["attempt"].as_u64())
        .unwrap_or(0)
        + 1;
    let record = json!({"team":team,"incarnation":incarnation,"dispatchId":id,"attempt":attempt,
        "target":target,"kind":kind,"state":"prepared",
        "registryBacked": crate::registry::load(team).is_some()});
    if kind == "node" {
        write_record(&path, &record)?;
    }
    operations.insert(
        path.clone(),
        Operation {
            record,
            handle: None,
            last_result: None,
            persisted: true,
            write_error: None,
        },
    );
    Ok(path)
}

pub(super) fn operation_handle(path: &Path, handle: TurnHandle) -> Result<()> {
    let mut operations = active().lock().unwrap_or_else(|e| e.into_inner());
    let operation = operations
        .get_mut(path)
        .ok_or_else(|| anyhow::anyhow!("operation retired"))?;
    if operation.record["state"] == "terminal" {
        bail!("operation interrupted");
    }
    operation.record["state"] = json!("running");
    operation.record["handle"] = match &handle {
        TurnHandle::Codex { thread_id, turn_id } => {
            json!({"cli":"codex","threadId":thread_id,"turnId":turn_id})
        }
        TurnHandle::Grok { key, prompt_id } => {
            json!({"cli":"grok","key":key,"generation":prompt_id.generation,"promptId":prompt_id.rid})
        }
        TurnHandle::Unknown(reason) | TurnHandle::Untracked(reason) => json!({"reason":reason}),
    };
    operation.handle = Some(handle);
    operation.persisted = false;
    persist_operation(path, operation)
}

pub(super) fn operation_terminal(path: &Path, mut result: Map<String, Value>) -> Result<()> {
    let mut operations = active().lock().unwrap_or_else(|e| e.into_inner());
    let operation = operations
        .get_mut(path)
        .ok_or_else(|| anyhow::anyhow!("operation retired"))?;
    if operation.record["state"] == "terminal" {
        bail!("operation interrupted");
    }
    result.entry("state").or_insert(json!("ended"));
    result.entry("status").or_insert(json!("refused"));
    result.entry("text").or_insert(json!(""));
    result.entry("error").or_insert(Value::Null);
    result
        .entry("dispatchId")
        .or_insert(operation.record["dispatchId"].clone());
    operation.record["state"] = json!("terminal");
    operation.record["result"] = Value::Object(result);
    operation.persisted = false;
    persist_operation(path, operation)?;
    operations.remove(path);
    Ok(())
}

fn retired_reason(record: &Value) -> Option<&'static str> {
    let team = record["team"].as_str()?;
    let entry = match crate::registry::load(team) {
        Some(entry) => entry,
        None => {
            if record["registryBacked"] == true {
                let path = crate::registry::entry_path(team)?;
                if fs::metadata(path).err()?.kind() == std::io::ErrorKind::NotFound {
                    return Some("team explicitly removed from the registry");
                }
            }
            return None;
        }
    };
    let created = entry.get("createdAt")?;
    let incarnation = created
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| created.to_string());
    let incarnation = incarnation
        .parse::<f64>()
        .ok()
        .map(crate::team::created_at_key)?;
    if record["incarnation"].as_str() != Some(incarnation.as_str()) {
        return Some("team incarnation replaced");
    }
    let members = entry.get("members")?.as_array()?;
    if !members
        .iter()
        .any(|member| member["name"] == record["target"])
    {
        return Some("member removed from the registry");
    }
    None
}

pub(super) fn interrupt_operations(workspace: &str, reason: &str) {
    let root = hooked_run_dir(workspace).join("operations");
    let mut operations = active().lock().unwrap_or_else(|e| e.into_inner());
    for (path, operation) in operations.iter_mut().filter(|(p, _)| p.starts_with(&root)) {
        if operation.record["state"] == "terminal" {
            continue;
        }
        let id = operation.record["dispatchId"].as_str().unwrap_or_default();
        let mut result = ambiguous(id, reason);
        result.insert("status".into(), json!("interrupted"));
        operation.record["result"] = Value::Object(result);
        operation.record["state"] = json!("terminal");
        let _ = persist_operation(path, operation);
    }
    operations.retain(|_, operation| !completed(operation));
}

/// Called by the serving/draining coordinator even when no runner polls.
/// An unresolved node handle keeps its result obligation and defers exec.
pub(super) fn flush_operations(workspace: &str) -> bool {
    let root = hooked_run_dir(workspace).join("operations");
    let mut operations = active().lock().unwrap_or_else(|e| e.into_inner());
    let mut ready = true;
    for (path, operation) in operations.iter_mut().filter(|(p, _)| p.starts_with(&root)) {
        if operation.record["state"] != "terminal" {
            if let Some(handle) = operation.handle.clone() {
                let id = operation.record["dispatchId"].as_str().unwrap_or_default();
                let result = turn_result_payload(id, Some(handle));
                operation.last_result = Some(result.clone());
                if result.get("state").and_then(Value::as_str) == Some("ended") {
                    operation.record["state"] = json!("terminal");
                    operation.record["result"] = Value::Object(result);
                    operation.persisted = false;
                }
            }
        }
        if operation.handle.is_some() && operation.record["state"] != "terminal" {
            if let Some(reason) = retired_reason(&operation.record) {
                let id = operation.record["dispatchId"].as_str().unwrap_or_default();
                operation.record["result"] = json!({"ok":true,"dispatchId":id,"state":"ended",
                    "status":"interrupted","text":"","error":reason});
                operation.record["state"] = json!("terminal");
                operation.persisted = false;
            }
        }
        if !operation.persisted {
            let _ = persist_operation(path, operation);
        }
        if operation.record["kind"] == "node" {
            ready &= completed(operation);
        }
    }
    operations.retain(|_, operation| !completed(operation));
    ready
}

pub(super) fn pending_operations(workspace: &str) -> usize {
    flush_operations(workspace);
    let root = hooked_run_dir(workspace).join("operations");
    active()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(path, operation)| {
            path.starts_with(&root) && operation.record["kind"] == "node" && !completed(operation)
        })
        .count()
}

pub(super) fn durable_node_result(workspace: &str, team: &str, id: &str) -> Map<String, Value> {
    let incarnation = match registry_incarnation(team) {
        Ok(incarnation) => incarnation,
        Err(error) => return err_response(error),
    };
    if let Err(error) = validate_key(&incarnation, id) {
        return err_response(error);
    }
    let path = record_path(workspace, &incarnation, id);
    flush_operations(workspace);
    let operations = active().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(operation) = operations.get(&path) {
        if operation.record["state"] == "terminal" {
            return operation.record["result"].as_object().unwrap().clone();
        }
        if operation.persisted {
            if let Some(result) = &operation.last_result {
                if result.get("state").and_then(Value::as_str) == Some("running") {
                    return result.clone();
                }
            }
        }
        return ambiguous(id, "operation accepted but its outcome is unresolved");
    }
    match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(record) if record["state"] == "terminal" => record["result"]
                .as_object()
                .cloned()
                .unwrap_or_else(|| ambiguous(id, "invalid terminal record")),
            _ => ambiguous(
                id,
                "hived restarted with an unfinished operation; do not resend",
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => turn_result_payload(id, None),
        Err(_) => ambiguous(id, "operation journal is unreadable; do not resend"),
    }
}

fn ambiguous(id: &str, reason: &str) -> Map<String, Value> {
    json!({"ok":true,"dispatchId":id,"state":"ambiguous","reason":reason})
        .as_object()
        .unwrap()
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::codex_app_server::TurnResult;
    use crate::hived::testhook::{self, Hook};
    use crate::team::Team;
    use crate::testenv::EnvGuard;
    use std::sync::Arc;

    fn team(workspace: &str) -> Team {
        Team {
            name: "journal".into(),
            created_at: 123.0,
            workspace: workspace.into(),
            ..Default::default()
        }
    }

    fn hooks(workspace: &str) -> Hook {
        if crate::registry::load("journal").is_none() {
            crate::registry::record_team(
                "journal",
                workspace,
                "123",
                &[json!({"name":"worker","cli":"codex"})
                    .as_object()
                    .unwrap()
                    .clone()],
                "",
            )
            .unwrap();
        }
        let resolved = team(workspace);
        Hook {
            team_load: Some(Arc::new(|_| panic!("node-result must not load Team"))),
            resolve_live_agent: Some(Arc::new(move |_, _| {
                Ok((
                    resolved.clone(),
                    crate::agent::testhook::fake_agent("worker", "journal", "", "codex"),
                ))
            })),
            check_send_gate: Some(Arc::new(|_| Ok(()))),
            cas_turn_result: Some(Arc::new(|_| {
                Some(TurnResult {
                    thread_id: "thread".into(),
                    status: Some("completed".into()),
                    error: None,
                    messages: vec!["result from native engine".into()],
                })
            })),
            ..Default::default()
        }
    }

    fn codex_handle() -> TurnHandle {
        TurnHandle::Codex {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
        }
    }

    // The child stops after the fake engine accepts, before the handle can
    // return to the journal writer. The parent has no in-memory operation.
    #[test]
    fn test_operation_survives_process_exit_at_acceptance_and_terminal_boundaries() {
        const CHILD: &str = "HIVE_JOURNAL_CRASH_TEST";
        if let Ok(mode) = std::env::var(CHILD) {
            let workspace = std::env::var("HIVE_JOURNAL_TEST_WORKSPACE").unwrap();
            let mut hook = hooks(&workspace);
            let crash_at_accept = mode == "accepted";
            hook.agent_dispatch_turn = Some(Arc::new(move |_, _| {
                if crash_at_accept {
                    std::process::exit(86);
                }
                Ok(codex_handle())
            }));
            let _guard = testhook::install(hook);
            send_payload(
                &workspace,
                "journal",
                SendOrigin::Node {
                    dispatch_id: "nd-crash",
                },
                "worker",
                "task",
                "",
            )
            .unwrap();
            assert!(flush_operations(&workspace));
            std::process::exit(86);
        }
        for mode in ["accepted", "terminal"] {
            let tmp = tempfile::tempdir().unwrap();
            let mut env = EnvGuard::new();
            env.set("HIVE_HOME", tmp.path().join("home"));
            let workspace = tmp.path().join("workspace");
            crate::bus::init_workspace(&workspace).unwrap();
            let workspace = workspace.to_str().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "hived::operations::tests::test_operation_survives_process_exit_at_acceptance_and_terminal_boundaries", "--nocapture"])
                .env(CHILD, mode).env("HIVE_JOURNAL_TEST_WORKSPACE", workspace)
                .status().unwrap();
            assert_eq!(status.code(), Some(86));
            let _guard = testhook::install(hooks(workspace));
            let result = durable_node_result(workspace, "journal", "nd-crash");
            if mode == "accepted" {
                assert_eq!(result["state"], "ambiguous");
            } else {
                assert_eq!(result["state"], "ended");
                assert_eq!(result["status"], "completed");
                assert_eq!(result["text"], "result from native engine");
                assert_eq!(
                    result,
                    durable_node_result(workspace, "journal", "nd-crash")
                );
            }
            assert!(
                prepare_operation(workspace, "journal", "123", "nd-crash", "worker", "node")
                    .unwrap_err()
                    .is::<ExistingOperation>()
            );
        }
    }

    #[test]
    fn test_failed_handle_write_keeps_prepared_record_and_blocks_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::new();
        env.set("HIVE_HOME", tmp.path().join("home"));
        let workspace = tmp.path().to_str().unwrap();
        let mut hook = hooks(workspace);
        hook.cas_turn_result = Some(Arc::new(|_| None));
        let _guard = testhook::install(hook);
        let path =
            prepare_operation(workspace, "journal", "123", "nd-write", "worker", "node").unwrap();
        let dir = path.parent().unwrap();
        let saved = dir.with_file_name("saved");
        fs::rename(dir, &saved).unwrap();
        fs::write(dir, "blocks journal writes").unwrap();
        assert!(operation_handle(&path, codex_handle()).is_err());
        assert!(!flush_operations(workspace));
        assert_eq!(
            durable_node_result(workspace, "journal", "nd-write")["state"],
            "ambiguous"
        );
        fs::remove_file(dir).unwrap();
        fs::rename(&saved, dir).unwrap();
        // Model loss of this process's clients and operation memory.
        active().lock().unwrap().remove(&path);
        assert_eq!(
            durable_node_result(workspace, "journal", "nd-write")["state"],
            "ambiguous"
        );
    }

    #[test]
    fn test_terminal_write_failure_retains_result_until_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::new();
        env.set("HIVE_HOME", tmp.path().join("home"));
        let workspace = tmp.path().to_str().unwrap();
        let _guard = testhook::install(hooks(workspace));
        let path =
            prepare_operation(workspace, "journal", "123", "nd-done", "worker", "node").unwrap();
        operation_handle(&path, codex_handle()).unwrap();
        let dir = path.parent().unwrap();
        let saved = dir.with_file_name("saved");
        fs::rename(dir, &saved).unwrap();
        fs::write(dir, "blocks writes").unwrap();
        assert!(!flush_operations(workspace));
        assert_eq!(
            durable_node_result(workspace, "journal", "nd-done")["text"],
            "result from native engine"
        );
        fs::remove_file(dir).unwrap();
        fs::rename(saved, dir).unwrap();
        assert!(flush_operations(workspace));
        assert!(!active().lock().unwrap().contains_key(&path));
        assert_eq!(
            durable_node_result(workspace, "journal", "nd-done")["text"],
            "result from native engine"
        );
    }

    #[test]
    fn test_new_team_incarnation_cannot_read_prior_dispatch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::new();
        env.set("HIVE_HOME", tmp.path().join("home"));
        let workspace = tmp.path().to_str().unwrap();
        let _guard = testhook::install(hooks(workspace));
        let path =
            prepare_operation(workspace, "journal", "122", "nd-old", "worker", "node").unwrap();
        operation_handle(&path, codex_handle()).unwrap();
        assert!(flush_operations(workspace));
        assert_eq!(
            durable_node_result(workspace, "journal", "nd-old")["state"],
            "unknown"
        );
    }

    #[test]
    fn test_ordinary_send_tracks_engine_result_without_runner_poll() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::new();
        env.set("HIVE_HOME", tmp.path().join("home"));
        let workspace = tmp.path().to_str().unwrap();
        crate::bus::init_workspace(workspace).unwrap();
        let mut hook = hooks(workspace);
        hook.agent_dispatch_turn = Some(Arc::new(|_, _| Ok(codex_handle())));
        hook.cas_turn_result = Some(Arc::new(|_| {
            Some(TurnResult {
                thread_id: "thread".into(),
                status: None,
                error: None,
                messages: vec![],
            })
        }));
        let _guard = testhook::install(hook);
        let answer = send_payload(
            workspace,
            "journal",
            SendOrigin::Member("orch"),
            "worker",
            "hi",
            "",
        )
        .unwrap();
        let id = answer["dispatchId"].as_str().unwrap();
        let path = record_path(workspace, "123", id);
        assert!(active().lock().unwrap().contains_key(&path));
        assert!(!hooked_run_dir(workspace).join("operations").exists());
        assert!(drain_ready(workspace));
        reopen_admission();
        testhook::update(|h| h.cas_turn_result = hooks(workspace).cas_turn_result);
        assert!(drain_ready(workspace));
        assert!(!active().lock().unwrap().contains_key(&path));
        assert!(!hooked_run_dir(workspace).join("operations").exists());
    }

    #[test]
    fn test_only_definite_refusal_allows_resubmission() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::new();
        env.set("HIVE_HOME", tmp.path().join("home"));
        let workspace = tmp.path().to_str().unwrap();
        crate::bus::init_workspace(workspace).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let called = Arc::clone(&calls);
        let mut hook = hooks(workspace);
        hook.agent_dispatch_turn = Some(Arc::new(move |_, _| {
            if called.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Err(crate::agent::DeliveryError(
                    "refused before acceptance".into(),
                ))
            } else {
                Ok(codex_handle())
            }
        }));
        let _guard = testhook::install(hook);
        let submit = || {
            send_payload(
                workspace,
                "journal",
                SendOrigin::Node {
                    dispatch_id: "nd-retry",
                },
                "worker",
                "task",
                "",
            )
            .unwrap()
        };
        assert_eq!(submit()["ok"], false);
        assert_eq!(submit()["ok"], true);
        assert!(submit().contains_key("dispatchUnknown"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(crate::bus::read_all_events(workspace).unwrap().len(), 2);
    }

    #[test]
    fn test_explicit_registry_removal_interrupts_but_unreadable_registry_does_not() {
        for mode in ["member", "team", "unreadable"] {
            let tmp = tempfile::tempdir().unwrap();
            let mut env = EnvGuard::new();
            env.set("HIVE_HOME", tmp.path().join("home"));
            let workspace = tmp.path().to_str().unwrap();
            let member = json!({"name":"worker","cli":"codex","sessionId":"thread"});
            crate::registry::record_team(
                "journal",
                workspace,
                "123",
                &[member.as_object().unwrap().clone()],
                "",
            )
            .unwrap();
            let mut hook = hooks(workspace);
            hook.cas_turn_result = Some(Arc::new(|_| None));
            let _guard = testhook::install(hook);
            let path =
                prepare_operation(workspace, "journal", "123", "nd-killed", "worker", "node")
                    .unwrap();
            operation_handle(&path, codex_handle()).unwrap();
            match mode {
                "member" => {
                    crate::registry::remove_member("journal", "worker", "123").unwrap();
                }
                "team" => {
                    crate::registry::delete_team("journal").unwrap();
                }
                _ => fs::write(
                    crate::registry::entry_path("journal").unwrap(),
                    "broken json",
                )
                .unwrap(),
            }
            if mode == "unreadable" {
                assert!(!flush_operations(workspace));
            } else {
                assert!(flush_operations(workspace));
                let record: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                assert_eq!(record["result"]["status"], "interrupted");
            }
        }
    }

    #[test]
    fn test_result_read_uses_the_observation_that_was_journaled() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::new();
        env.set("HIVE_HOME", tmp.path().join("home"));
        let workspace = tmp.path().to_str().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let called = Arc::clone(&calls);
        let mut hook = hooks(workspace);
        hook.cas_turn_result = Some(Arc::new(move |_| {
            Some(TurnResult {
                thread_id: "thread".into(),
                status: (called.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0)
                    .then(|| "completed".into()),
                error: None,
                messages: vec!["terminal".into()],
            })
        }));
        let _guard = testhook::install(hook);
        let path =
            prepare_operation(workspace, "journal", "123", "nd-observe", "worker", "node").unwrap();
        operation_handle(&path, codex_handle()).unwrap();
        assert_eq!(
            durable_node_result(workspace, "journal", "nd-observe")["state"],
            "running"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let result = durable_node_result(workspace, "journal", "nd-observe");
        assert_eq!(result["state"], "ended");
        let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record["result"], Value::Object(result));
    }

    #[test]
    fn test_journal_paths_are_readable_and_reject_untrusted_components() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::new();
        env.set("HIVE_HOME", tmp.path().join("home"));
        let workspace = tmp.path().to_str().unwrap();
        let _guard = testhook::install(hooks(workspace));
        for id in ["../escape", "/absolute", "a/b", "a\\b", ""] {
            assert!(prepare_operation(workspace, "journal", "123", id, "worker", "node").is_err());
            assert_eq!(durable_node_result(workspace, "journal", id)["ok"], false);
        }
        assert!(!hooked_run_dir(workspace).join("operations").exists());
        let path =
            prepare_operation(workspace, "journal", "123", "nd-visible", "worker", "node").unwrap();
        assert_eq!(
            path,
            hooked_run_dir(workspace).join("operations/123/nd-visible.json")
        );
    }
}
