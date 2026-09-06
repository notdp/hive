use std::path::Path;

use serde_json::{Map, Value};

use super::*;

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

pub(crate) fn parse_config_value(raw: &str) -> Value {
    let lowered = raw.trim().to_lowercase();
    if lowered == "true" {
        return Value::Bool(true);
    }
    if lowered == "false" {
        return Value::Bool(false);
    }
    if let Ok(int_value) = raw.trim().parse::<i64>() {
        return Value::Number(int_value.into());
    }
    if let Ok(float_value) = raw.trim().parse::<f64>() {
        if let Some(number) = serde_json::Number::from_f64(float_value) {
            return Value::Number(number);
        }
    }
    Value::String(raw.to_string())
}

pub fn config_get(key: &str) {
    let value = match crate::settings::get_setting(key) {
        Some(value) => value,
        None => std::process::exit(1),
    };
    match &value {
        Value::Object(_) | Value::Array(_) => {
            println!("{}", py_dumps(&value, true, Some(2), true));
        }
        _ => println!("{}", py_dumps(&value, true, None, false)),
    }
}

pub fn config_set(key: &str, value: &str) {
    let parsed = parse_config_value(value);
    ok_or_fail(crate::settings::set_setting(key, parsed.clone()));
    println!("{}", py_dumps(&parsed, true, None, false));
}

pub fn config_unset(key: &str) {
    if !ok_or_fail(crate::settings::unset_setting(key)) {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// notify
// ---------------------------------------------------------------------------

pub fn notify_cmd(message: &str) {
    let target_pane = resolve_target_pane();
    let payload = ok_or_fail(crate::notify_ui::notify(message, &target_pane, ""));
    let value = serde_json::to_value(&payload).unwrap_or(Value::Null);
    println!("{}", py_dumps(&value, true, None, false));
}

// ---------------------------------------------------------------------------
// plugin
// ---------------------------------------------------------------------------

fn render_plugin_mutation_result(action: &str, payload: &Map<String, Value>) -> String {
    let name = map_str(payload, "name");
    let mut lines = vec![format!("Plugin '{name}' {action}.")];
    let install_root = map_str(payload, "installRoot");
    let commands: Vec<String> = payload
        .get("commands")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut command_names: Vec<String> = Vec::new();
    for item in &commands {
        let path = Path::new(item);
        let label = if path.extension().and_then(|e| e.to_str()) == Some("md") {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            path.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        if !command_names.contains(&label) {
            command_names.push(label);
        }
    }

    if !install_root.is_empty() {
        lines.push(format!("  install root: {install_root}"));
    }
    if !command_names.is_empty() {
        lines.push(format!("  commands: {}", command_names.join(", ")));
    }
    lines.push(
        "  note: existing Codex panes may not reload plugin settings dynamically; \
         restart them if old commands still run."
            .to_string(),
    );
    lines.join("\n")
}

pub fn plugin_list(plain: bool) {
    let rows = ok_or_fail(crate::plugin_manager::list_plugins());
    if !plain {
        println!("{}", py_dumps(&Value::Array(rows), false, None, false));
        return;
    }
    let enabled_count = rows.iter().filter(|row| truthy(row.get("enabled"))).count();
    println!("Plugins ({enabled_count}/{} enabled)", rows.len());
    if rows.is_empty() {
        return;
    }
    let name_of = |row: &Value| {
        row.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let name_width = rows.iter().map(|row| name_of(row).len()).max().unwrap_or(0);
    for row in &rows {
        let status = if truthy(row.get("enabled")) {
            "enabled"
        } else {
            "disabled"
        };
        let description = row.get("description").and_then(Value::as_str).unwrap_or("");
        println!(
            "  {:<name_width$}  {status:<8}  {description}",
            name_of(row)
        );
    }
}

pub fn plugin_ls(plain: bool) {
    plugin_list(plain);
}

pub fn plugin_enable(name: &str, plain: bool) {
    match crate::plugin_manager::enable_plugin(name) {
        Ok(payload) => {
            if !plain {
                println!("{}", py_dumps(&payload, false, None, false));
                return;
            }
            let empty = Map::new();
            let map = payload.as_object().unwrap_or(&empty);
            println!("{}", render_plugin_mutation_result("enabled", map));
        }
        Err(e) => fail(&e.to_string()),
    }
}

pub fn plugin_sync() {
    match crate::plugin_manager::materialize_marketplace() {
        Ok(payload) => println!("{}", payload.display()),
        Err(e) => fail(&e.to_string()),
    }
}

// `hive plugin setup` — the one human-run install step. Registers the
// materialized marketplace and installs the plugin for claude and codex on
// PATH; each sub-step tolerates "already done" failures so re-running is
// safe, and re-running is also how an install is repaired.
fn setup_step(label: &str, argv: &[&str]) {
    let out = match std::process::Command::new(argv[0])
        .args(&argv[1..])
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            println!("setup: {label}: failed to run ({e})");
            return;
        }
    };
    if out.status.success() {
        println!("setup: {label}: ok");
    } else {
        let text = String::from_utf8_lossy(if out.stderr.is_empty() {
            &out.stdout
        } else {
            &out.stderr
        })
        .trim()
        .lines()
        .last()
        .unwrap_or("")
        .to_string();
        println!("setup: {label}: {text}");
    }
}

pub fn plugin_setup() {
    let root = ok_or_fail(crate::plugin_manager::materialize_marketplace());
    let marketplace = root
        .ancestors()
        .nth(3)
        .expect("payload sits three levels under the marketplace root")
        .to_path_buf();
    println!("setup: marketplace synced at {}", marketplace.display());

    if which_on_path("claude") {
        let dir = marketplace.join("claude");
        setup_step(
            "claude marketplace",
            &[
                "claude",
                "plugin",
                "marketplace",
                "add",
                &dir.to_string_lossy(),
            ],
        );
        setup_step(
            "claude plugin",
            &["claude", "plugin", "install", "hive@hive", "--yes"],
        );
        setup_step(
            "claude plugin refresh",
            &["claude", "plugin", "update", "hive@hive", "--yes"],
        );
    } else {
        println!("setup: claude: not on PATH, skipped");
    }

    if which_on_path("codex") {
        let dir = marketplace.join("codex");
        setup_step(
            "codex marketplace",
            &[
                "codex",
                "plugin",
                "marketplace",
                "add",
                &dir.to_string_lossy(),
            ],
        );
        setup_step("codex plugin", &["codex", "plugin", "add", "hive@hive"]);
    } else {
        println!("setup: codex: not on PATH, skipped");
    }
}

fn which_on_path(name: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn plugin_disable(name: &str, plain: bool) {
    match crate::plugin_manager::disable_plugin(name, false) {
        Ok(payload) => {
            if !plain {
                println!("{}", py_dumps(&payload, false, None, false));
                return;
            }
            let empty = Map::new();
            let map = payload.as_object().unwrap_or(&empty);
            println!("{}", render_plugin_mutation_result("disabled", map));
        }
        Err(e) => fail(&e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// shell-init
// ---------------------------------------------------------------------------

const _SHELL_INIT_POSIX: &str = r#"# hive launchers — `hcodex` / `hclaude` / `hgrok` start a hive-connected codex /
# claude / grok in the current tmux pane (shared app-server daemon for codex,
# pane-keyed leader for grok, supervisor-hosted bg job for claude) and print a
# cd-ready resume hint when it exits. Outside tmux, and for management subcommands / non-interactive flags,
# they run the plain binary. Plain `codex` / `claude` / `grok` are never touched.
function hcodex {
  if ! command -v hive >/dev/null 2>&1; then
    echo "hcodex: hive is not on PATH" >&2; return 127
  fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is codex's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive codex "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # print a cd-ready resume hint for the session that just ended.
  hive resume-hint codex 2>/dev/null || true
  return $_hive_rc
}

function hclaude {
  if ! command -v hive >/dev/null 2>&1; then
    echo "hclaude: hive is not on PATH" >&2; return 127
  fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is claude's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive claude "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # claude's own resume hint omits the directory; print a cd-ready one.
  hive resume-hint claude 2>/dev/null || true
  return $_hive_rc
}

function hgrok {
  if ! command -v hive >/dev/null 2>&1; then
    echo "hgrok: hive is not on PATH" >&2; return 127
  fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is grok's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive grok "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # print a cd-ready resume hint for the session that just ended.
  hive resume-hint grok 2>/dev/null || true
  return $_hive_rc
}
"#;

const _SHELL_INIT_FISH: &str = r#"# hive launchers — `hcodex` / `hclaude` / `hgrok` start a hive-connected codex /
# claude / grok in the current tmux pane (shared app-server daemon for codex,
# pane-keyed leader for grok, supervisor-hosted bg job for claude) and print a
# cd-ready resume hint when it exits. Outside tmux, and for management subcommands / non-interactive flags,
# they run the plain binary. Plain `codex` / `claude` / `grok` are never touched.
function hcodex
    if not type -q hive
        echo "hcodex: hive is not on PATH" >&2
        return 127
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # codex's own, never a fallback signal
    hive codex $argv
    set -l _hive_rc $status
    # print a cd-ready resume hint for the session that just ended.
    hive resume-hint codex 2>/dev/null
    return $_hive_rc
end

function hclaude
    if not type -q hive
        echo "hclaude: hive is not on PATH" >&2
        return 127
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # claude's own, never a fallback signal
    hive claude $argv
    set -l _hive_rc $status
    # claude's own resume hint omits the directory; print a cd-ready one.
    hive resume-hint claude 2>/dev/null
    return $_hive_rc
end

function hgrok
    if not type -q hive
        echo "hgrok: hive is not on PATH" >&2
        return 127
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # grok's own, never a fallback signal
    hive grok $argv
    set -l _hive_rc $status
    # print a cd-ready resume hint for the session that just ended.
    hive resume-hint grok 2>/dev/null
    return $_hive_rc
end
"#;

pub fn shell_init_cmd(shell: &str) {
    print!("{}", shell_init_script(shell));
}

/// The launcher script for *shell* (`$SHELL`'s basename when empty, zsh
/// when that is unset too): fish gets its own dialect, everything else the
/// zsh/bash one.
pub(crate) fn shell_init_script(shell: &str) -> &'static str {
    let resolved = if shell.is_empty() {
        let env_shell = env_string("SHELL");
        if env_shell.is_empty() {
            "zsh".to_string()
        } else {
            Path::new(&env_shell)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        }
    } else {
        shell.to_string()
    };
    if resolved.trim() == "fish" {
        _SHELL_INIT_FISH
    } else {
        // zsh and bash share this syntax. The ksh-style `function name {` form
        // bypasses alias expansion of the name in BOTH shells, so a stray
        // alias cannot break the parse.
        _SHELL_INIT_POSIX
    }
}
