use std::path::Path;

use serde_json::{Map, Value};

use super::*;

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

pub(crate) fn _parse_config_value(raw: &str) -> Value {
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
    let parsed = _parse_config_value(value);
    ok_or_fail(crate::settings::set_setting(key, parsed.clone()));
    println!("{}", py_dumps(&parsed, true, None, false));
}

pub fn config_unset(key: &str) {
    if !ok_or_fail(crate::settings::unset_setting(key)) {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------------

// The plugin SessionStart hook's converge step. The hook's shell wrapper
// proves phase 1 (a `hive` exists; a pre-bootstrap binary rejects this
// subcommand and the wrapper maps that to the upgrade hint); this side is
// phase 2: the Claude-side marketplace autoUpdate entry, failing closed on
// any foreign shape with zero mutation.

const _MARKETPLACE_REPO: &str = "notdp/hive";

fn _bootstrap_settings_path() -> std::path::PathBuf {
    let root = match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude"),
    };
    root.join("settings.json")
}

fn _canonical_marketplace_source() -> Value {
    let mut source = Map::new();
    source.insert("source".into(), Value::from("github"));
    source.insert("repo".into(), Value::from(_MARKETPLACE_REPO));
    Value::Object(source)
}

pub(crate) fn _ensure_marketplace_settings(path: &Path) -> Result<String, String> {
    let mut data = Map::new();
    let mut mode: Option<u32> = None;
    if path.exists() {
        use std::os::unix::fs::PermissionsExt;
        mode = std::fs::metadata(path).ok().map(|m| m.permissions().mode());
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|_| format!("{} is not valid JSON; fix it manually", path.display()))?;
        data = match parsed {
            Value::Object(map) => map,
            _ => return Err(format!("{} top level is not an object", path.display())),
        };
    }

    let markets = data
        .entry("extraKnownMarketplaces")
        .or_insert_with(|| Value::Object(Map::new()));
    let markets = match markets {
        Value::Object(map) => map,
        _ => {
            return Err(format!(
                "{}: extraKnownMarketplaces is not an object",
                path.display()
            ))
        }
    };
    match markets.get_mut("hive") {
        Some(Value::Object(entry)) => {
            if entry.get("source") != Some(&_canonical_marketplace_source()) {
                return Err(format!(
                    "{}: extraKnownMarketplaces.hive has a foreign source ({}); refusing to touch it",
                    path.display(),
                    entry.get("source").map_or("None".to_string(), |v| v.to_string()),
                ));
            }
            if entry.get("autoUpdate") == Some(&Value::Bool(true)) {
                return Ok("settings already converged".to_string());
            }
            entry.insert("autoUpdate".into(), Value::Bool(true));
        }
        Some(_) => {
            return Err(format!(
                "{}: extraKnownMarketplaces.hive is not an object",
                path.display()
            ))
        }
        None => {
            let mut entry = Map::new();
            entry.insert("source".into(), _canonical_marketplace_source());
            entry.insert("autoUpdate".into(), Value::Bool(true));
            markets.insert("hive".into(), Value::Object(entry));
        }
    }

    let payload = serde_json::to_string_pretty(&Value::Object(data))
        .map_err(|e| format!("cannot serialize settings: {e}"))?
        + "\n";
    let parent = path
        .parent()
        .ok_or_else(|| "settings path has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let tmp = parent.join(format!(".settings-{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        std::fs::write(&tmp, &payload)?;
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", path.display()));
    }
    Ok("settings updated: extraKnownMarketplaces.hive autoUpdate enabled".to_string())
}

pub fn bootstrap_cmd() {
    println!("bootstrap: hive {}", env!("CARGO_PKG_VERSION"));
    let disabled = std::env::var("DISABLE_AUTOUPDATER").is_ok_and(|v| !v.is_empty());
    let forced = std::env::var("FORCE_AUTOUPDATE_PLUGINS").is_ok_and(|v| !v.is_empty());
    if disabled && !forced {
        println!(
            "bootstrap: skipped: DISABLE_AUTOUPDATER is set without FORCE_AUTOUPDATE_PLUGINS, \
             so Claude will not auto-update any plugin. To receive hive updates automatically, \
             also set FORCE_AUTOUPDATE_PLUGINS=1; until then run `claude plugin update hive@hive` \
             manually"
        );
        return;
    }
    match _ensure_marketplace_settings(&_bootstrap_settings_path()) {
        Ok(summary) => println!("bootstrap: {summary}"),
        Err(message) => {
            eprintln!("bootstrap: {message}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// notify
// ---------------------------------------------------------------------------

pub fn notify_cmd(message: &str) {
    let target_pane = _resolve_target_pane();
    let payload = ok_or_fail(crate::notify_ui::notify(message, &target_pane, ""));
    let value = serde_json::to_value(&payload).unwrap_or(Value::Null);
    println!("{}", py_dumps(&value, true, None, false));
}

// ---------------------------------------------------------------------------
// plugin
// ---------------------------------------------------------------------------

fn _render_plugin_mutation_result(action: &str, payload: &Map<String, Value>) -> String {
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
         restart them if old hooks or commands still run."
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
            println!("{}", _render_plugin_mutation_result("enabled", map));
        }
        Err(e) => fail(&e.to_string()),
    }
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
            println!("{}", _render_plugin_mutation_result("disabled", map));
        }
        Err(e) => fail(&e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// shell-init
// ---------------------------------------------------------------------------

const _SHELL_INIT_POSIX: &str = r#"# hive launchers — `hcodex` / `hclaude` / `hgrok` start a hive-connected codex /
# claude / grok in the current tmux pane (shared app-server daemon for codex,
# per-pane leader for grok, supervisor-hosted bg job for claude) and print a
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
# per-pane leader for grok, supervisor-hosted bg job for claude) and print a
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
        print!("{_SHELL_INIT_FISH}");
    } else {
        // zsh and bash share this syntax. The ksh-style `function name {` form
        // bypasses alias expansion of the name in BOTH shells, so a stray
        // alias cannot break the parse.
        print!("{_SHELL_INIT_POSIX}");
    }
}
