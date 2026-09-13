//! A launch leader's way into a team: `hgrok` at a terminal outside tmux
//! raises a leader on a launch key (`l-<id>`) and its TUI is that leader's
//! client; a create or join binds the leader to the member it becomes.
//!
//! The leader binds its socket path itself, so binding never moves a file:
//! it writes the member's alias (`m-<team>.<member>.alias` -> launch key),
//! and from then on every lookup for the member — socket, session record,
//! kill, the hived's reap — follows the alias (`keys::canonical_key`). The
//! launcher's own stop (`stop_launch`) steps back the moment an alias names
//! its key: the member owns the leader now.
//!
//! Called from the terminal handoff in the window between the old TUI's
//! stop and the team pane's TUI start, on either side (launcher or client),
//! so every step is idempotent. Every write to a launch's record and to
//! the aliases naming it — a bind's record write and alias publish, its
//! restore when the alias is refused, a rollback, a kill's alias removal
//! — runs under the launch's lock (`launch_lock`), so a bind that fails
//! after another bind of the same launch succeeded restores what it read
//! under the lock, never what it read before.

use std::fs;
use std::os::unix::io::AsRawFd;

use anyhow::{bail, Result};

use super::{
    alias_path_for_key, alias_target, bind_session_key, is_launch_key, kill_daemon_key, member_key,
    probe_socket, read_session_key, write_session_key, RecordBinding,
};

/// A fresh launch key for a leader a launcher is about to raise.
pub fn mint_launch_key() -> String {
    super::launch_key(&crate::agent::uuid4().replace('-', "")[..12])
}

fn check_launch(key: &str, session: &str) -> Result<()> {
    if !is_launch_key(key) {
        bail!("{key} is not a launch key");
    }
    if session.is_empty() {
        bail!("launch {key} has no session id");
    }
    Ok(())
}

/// Bind the launch leader on *key*, serving *session*, to *team*.*member*
/// of the team instance *created_at* (a `team::created_at_key`).
///
/// The leader must be listening and its session record must name
/// *session* (a record missing after a launcher restart is rewritten from
/// the arguments, never guessed). The record then takes the member's
/// binding — the same fields a mint writes, so a revival after the leader
/// has exited checks this member against the registry the way it checks a
/// spawned one — and only then is the alias published; an alias refused
/// puts back the binding the record held when this bind read it, under
/// the same lock, so a failed bind leaves no half-bound record and
/// overwrites no bind of the launch that landed since. A member already
/// aliased to another launch, or with a leader of its own, is refused:
/// nothing here replaces an engine. *pane* is display and takes no part.
pub fn bind_launch(
    key: &str,
    session: &str,
    cwd: &str,
    team: &str,
    created_at: &str,
    member: &str,
    _pane: &str,
) -> Result<()> {
    check_launch(key, session)?;
    let launch_sock = super::grok_home().join("hive").join(format!("{key}.sock"));
    if !probe_socket(&launch_sock) {
        bail!("launch leader {key} is not listening");
    }
    let binding = RecordBinding {
        team: team.to_string(),
        created_at: created_at.to_string(),
        member: member.to_string(),
    };
    let member_key = member_key(team, member);
    let _lock = launch_lock(key)?;
    let prior = match read_session_key(key) {
        Some(record) if record.session_id != session => bail!(
            "launch {key} serves session {}, not {session}",
            record.session_id
        ),
        Some(record) => {
            bind_session_key(key, Some(&binding))?;
            record.binding
        }
        None => {
            write_session_key(key, session, cwd, Some(&binding))?;
            None
        }
    };
    let published = (|| {
        let own_sock = super::grok_home()
            .join("hive")
            .join(format!("{member_key}.sock"));
        if own_sock.exists() {
            bail!("{member_key} already has a leader of its own");
        }
        publish_alias(&member_key, key)
    })();
    if published.is_err() {
        let _ = bind_session_key(key, prior.as_ref());
    }
    published
}

/// The launch's lock, held across every read-then-write of its record and
/// of the aliases naming it: a bind and a rollback from the launcher's and
/// the client's side can interleave, and two binds of one launch to two
/// members write the same record. Two launches racing for one member name
/// need none of it — the alias is published by a hard link that at most
/// one of them makes.
pub(crate) fn launch_lock(key: &str) -> Result<fs::File> {
    let path = super::grok_home()
        .join("hive")
        .join(format!("{key}.bind-lock"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(file)
}

/// Publish `member_key.alias` -> *key* without ever overwriting: the
/// alias is written whole into a private file and hard-linked into place,
/// which fails when an alias already exists. Two joins racing for one
/// member name therefore bind at most one launch; the loser sees whose.
/// The same key already published is idempotent; anything else there —
/// another launch, or garbage — is refused.
fn publish_alias(member_key: &str, key: &str) -> Result<()> {
    let alias = alias_path_for_key(member_key);
    let Some(parent) = alias.parent() else {
        bail!("alias path has no directory");
    };
    fs::create_dir_all(parent)?;
    let staged = parent.join(format!(
        ".{member_key}.{}.{}.alias-tmp",
        std::process::id(),
        crate::agent::uuid4()
    ));
    fs::write(&staged, key)?;
    let linked = fs::hard_link(&staged, &alias);
    let _ = fs::remove_file(&staged);
    match linked {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match alias_target(member_key) {
            Some(existing) if existing == key => Ok(()),
            Some(existing) => bail!("{member_key} is already bound to launch {existing}"),
            None => bail!("{member_key} carries an alias that names no launch"),
        },
        Err(e) => Err(e.into()),
    }
}

/// Undo one `bind_launch`: the one of *team*.*member* at the team instance
/// *created_at* on the launch *key* serving *session*.
///
/// The alias and the record's binding are that bind's to undo only while
/// nothing later owns them: a record on *key* bound since to the same
/// member name of another team instance, or the launch re-minted onto
/// another session and bound to that name again, is a later bind of the
/// same name, and this rollback — the same name at an instance and
/// session that are no longer the record's — undoes nothing. Otherwise
/// the alias goes when it names *key* (a record bound since to another
/// member, or unbound, or gone, leaves an alias no bind of this member
/// still stands behind), and the record's binding goes only when it is
/// exactly this bind's: the session and all three binding fields. The
/// leader and its session record stay the launcher's, untouched
/// otherwise.
pub fn rollback_launch(
    key: &str,
    session: &str,
    team: &str,
    created_at: &str,
    member: &str,
    _pane: &str,
) -> Result<()> {
    check_launch(key, session)?;
    let binding = RecordBinding {
        team: team.to_string(),
        created_at: created_at.to_string(),
        member: member.to_string(),
    };
    let member_key = member_key(team, member);
    let _lock = launch_lock(key)?;
    let record = read_session_key(key);
    let later_bind_of_this_name = record.as_ref().is_some_and(|record| {
        record.binding.as_ref().is_some_and(|bound| {
            bound.team == team
                && bound.member == member
                && (record.session_id != session || *bound != binding)
        })
    });
    if later_bind_of_this_name {
        return Ok(());
    }
    if alias_target(&member_key).as_deref() == Some(key) {
        fs::remove_file(alias_path_for_key(&member_key))?;
    }
    let bound_here = record.is_some_and(|record| {
        record.session_id == session && record.binding.as_ref() == Some(&binding)
    });
    if bound_here {
        bind_session_key(key, None)?;
    }
    Ok(())
}

/// Whether *team*.*member* at the team instance *created_at* is bound to
/// the launch on *key* serving *session*: the alias names the launch, and
/// the launch's record names the session and carries that binding — the
/// same name at another instance is another bind.
pub fn launch_is_bound(
    key: &str,
    session: &str,
    team: &str,
    created_at: &str,
    member: &str,
    _pane: &str,
) -> bool {
    alias_target(&member_key(team, member)).as_deref() == Some(key)
        && read_session_key(key).is_some_and(|record| {
            record.session_id == session
                && record.binding.is_some_and(|bound| {
                    bound.team == team && bound.created_at == created_at && bound.member == member
                })
        })
}

/// The launcher's own stop: the leader goes with its files unless a member
/// owns it by now — then it is the member's lifecycle (kill, delete, the
/// hived's reap) that ends it, and the launcher steps back.
pub fn stop_launch(key: &str, _session: &str) {
    if !is_launch_key(key) || bound_member(key).is_some() {
        return;
    }
    kill_daemon_key(key);
}

/// The member whose alias names *key*, if any.
pub fn bound_member(key: &str) -> Option<String> {
    let root = super::grok_home().join("hive");
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(member) = super::key_from_alias_name(name.to_str()?) else {
            continue;
        };
        if alias_target(&member).as_deref() == Some(key) {
            return Some(member);
        }
    }
    None
}
