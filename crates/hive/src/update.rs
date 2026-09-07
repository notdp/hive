//! `hive update`: replace the binary this process is running with the
//! latest GitHub release.
//!
//! Hive downloads, verifies and commits the release archive itself rather
//! than re-running the published installer: the installer's checksum step
//! is skipped when `sha256sum` is missing, its install directory follows
//! env vars that have nothing to do with the running binary, and it writes
//! a receipt and PATH lines this command has no business touching. Four
//! release triples and the `sha2` crate are all it takes to do the same
//! work honestly.
//!
//! The target is `current_exe()` — the file this process is running, not a
//! search of PATH or of cargo's bin dir. `symlink_metadata` must call it a
//! regular file: a symlink, a directory or a missing path is refused with
//! the path printed, and no attempt is made to recognize every packaging
//! layout that could put a link there.
//!
//! The commit is `rename` inside the target's own directory, so the swap
//! is atomic against readers. It is not atomic against another *writer*:
//! a `cargo install` landing on the same path while this download runs is
//! detected, not prevented. The target's sha256, length and mtime are
//! taken after the lock and compared again immediately before the rename;
//! any difference refuses the update rather than overwriting a newer
//! install with an older observation. The lock (`$HIVE_HOME/state/locks/
//! update-<16 hex>.lock`, named by the canonical target path) keeps two
//! `hive update` runs off one target; two runs under different
//! `$HIVE_HOME`s fall back to that re-check.
//!
//! Nothing else moves: no receipt, no PATH, no plugin sync, no hived or
//! member restart. An already-running hived keeps its own image until it
//! notices the new bytes on its own terms.
//!
//! [`Io`] is the whole outside world — curl, tar, the candidate's
//! `--version`, `current_exe` — so the tests drive every failure branch
//! without a network or a process-global.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

/// Where the "latest stable release" redirect lives.
pub const LATEST_URL: &str = "https://github.com/notdp/hive/releases/latest";
/// The only prefix a resolved latest URL may have.
const TAG_PREFIX: &str = "https://github.com/notdp/hive/releases/tag/";
/// The only prefix a download URL is built from.
const DOWNLOAD_PREFIX: &str = "https://github.com/notdp/hive/releases/download/";

const CONNECT_TIMEOUT: &str = "10";
const LATEST_MAX_TIME: &str = "30";
const DOWNLOAD_MAX_TIME: &str = "300";
/// How long the candidate gets to print its version before it is killed.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Query or parse of the latest release failed (`--check` exit 2).
pub const EXIT_QUERY_FAILED: i32 = 2;
/// `--check` found a newer release.
pub const EXIT_UPDATE_AVAILABLE: i32 = 1;

// ---------------------------------------------------------------------------
// versions
// ---------------------------------------------------------------------------

/// A release version: three numbers, compared numerically (0.18.10 is
/// newer than 0.18.9). Nothing else is a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// `1.2.3` or `v1.2.3`. Leading zeros, extra segments, pre-release and
/// build metadata are all "not a release tag".
pub fn parse_version(raw: &str) -> Option<Version> {
    let body = raw.strip_prefix('v').unwrap_or(raw);
    let mut parts = body.split('.');
    let mut numbers = [0u64; 3];
    for slot in numbers.iter_mut() {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(Version(numbers[0], numbers[1], numbers[2]))
}

/// This binary's own version, as compiled in.
pub fn current_version() -> Version {
    parse_version(env!("CARGO_PKG_VERSION")).expect("crate version is a release triple")
}

/// The version named by curl's `%{url_effective}` after following the
/// `releases/latest` redirect. Anything but the exact tag page of this
/// repository — another host, a login redirect, a query string, an extra
/// path segment, an illegal tag — is a query failure.
pub fn version_from_effective_url(url: &str) -> Result<Version, String> {
    let refused = || format!("unexpected release URL: {url}");
    let tag = url.trim().strip_prefix(TAG_PREFIX).ok_or_else(refused)?;
    if tag.contains('/') || tag.contains('?') || tag.contains('#') {
        return Err(refused());
    }
    if !tag.starts_with('v') {
        return Err(format!("release tag is not a v-prefixed release: {tag}"));
    }
    parse_version(tag).ok_or_else(|| format!("release tag is not a release triple: {tag}"))
}

// ---------------------------------------------------------------------------
// release assets
// ---------------------------------------------------------------------------

/// The release triple for the running build, or `None` where no release
/// asset is published. Linux is GNU-only: a musl build cannot assume the
/// glibc archive runs.
pub fn target_triple() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            return Some("aarch64-apple-darwin");
        }
        if cfg!(target_arch = "x86_64") {
            return Some("x86_64-apple-darwin");
        }
        return None;
    }
    if cfg!(all(target_os = "linux", target_env = "gnu")) {
        if cfg!(target_arch = "aarch64") {
            return Some("aarch64-unknown-linux-gnu");
        }
        if cfg!(target_arch = "x86_64") {
            return Some("x86_64-unknown-linux-gnu");
        }
    }
    None
}

pub fn archive_name(triple: &str) -> String {
    format!("hive-{triple}.tar.xz")
}

fn archive_url(version: Version, triple: &str) -> String {
    format!("{DOWNLOAD_PREFIX}v{version}/{}", archive_name(triple))
}

fn checksum_url(version: Version, triple: &str) -> String {
    format!("{}.sha256", archive_url(version, triple))
}

/// The one digest in a `.sha256` sidecar: `<64 hex><whitespace>[*]<name>`,
/// where the name must be the archive this run downloaded. More than one
/// digest line, a short or non-hex digest, a missing or foreign name, or
/// trailing fields all fail — the file is never taken as a bare digest.
pub fn parse_sha256_file(text: &str, expected_name: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() != 1 {
        return Err(format!(
            "checksum file holds {} digest lines, expected 1",
            lines.len()
        ));
    }
    let mut fields = lines[0].split_whitespace();
    let digest = fields.next().unwrap_or_default();
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("checksum file has no sha256 digest: {}", lines[0]));
    }
    let name = fields
        .next()
        .ok_or_else(|| format!("checksum file names no file: {}", lines[0]))?;
    if fields.next().is_some() {
        return Err(format!("checksum file has trailing fields: {}", lines[0]));
    }
    let name = name.strip_prefix('*').unwrap_or(name);
    if name != expected_name {
        return Err(format!(
            "checksum file is for {name}, expected {expected_name}"
        ));
    }
    Ok(digest.to_ascii_lowercase())
}

/// What `tar -tf` must show before anything is unpacked: every entry under
/// the single `hive-<triple>/` directory the release ships, the binary
/// among them, no absolute path and no `..` component anywhere. The
/// extract then runs with `--strip-components 1`.
pub fn validate_archive_entries(entries: &[String], triple: &str) -> Result<(), String> {
    let root = format!("hive-{triple}");
    let binary = format!("{root}/hive");
    let mut seen_binary = false;
    let mut seen_any = false;
    for raw in entries {
        let entry = raw.trim_end_matches('\n').trim_end_matches('\r');
        if entry.is_empty() {
            continue;
        }
        seen_any = true;
        if entry.starts_with('/') {
            return Err(format!("archive holds an absolute path: {entry}"));
        }
        if entry.split('/').any(|part| part == "..") {
            return Err(format!("archive holds a parent path: {entry}"));
        }
        let rest = entry
            .strip_prefix(&root)
            .ok_or_else(|| format!("archive entry outside {root}/: {entry}"))?;
        if !(rest.is_empty() || rest.starts_with('/')) {
            return Err(format!("archive entry outside {root}/: {entry}"));
        }
        if entry.trim_end_matches('/') == binary {
            seen_binary = true;
        }
    }
    if !seen_any {
        return Err("archive is empty".to_string());
    }
    if !seen_binary {
        return Err(format!("archive holds no {binary}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the outside world
// ---------------------------------------------------------------------------

/// Every side effect `update` has on the world outside its own staging
/// directory. The real implementation shells out to curl and tar; tests
/// inject fakes and keep the sha256, the lock and the rename real.
pub trait Io {
    /// The file this process is running.
    fn current_exe(&self) -> Result<PathBuf, String>;
    /// Follow *url* and report where it landed (curl's `%{url_effective}`).
    fn fetch_effective_url(&self, url: &str) -> Result<String, String>;
    fn download(&self, url: &str, dest: &Path) -> Result<(), String>;
    /// `tar -tf`, one entry per element.
    fn tar_list(&self, archive: &Path) -> Result<Vec<String>, String>;
    /// `tar -xf … --strip-components 1` into *dest*.
    fn tar_extract(&self, archive: &Path, dest: &Path) -> Result<(), String>;
    /// The candidate's own `--version` line, trimmed.
    fn run_version(&self, binary: &Path) -> Result<String, String>;
}

/// The real outside world. The candidate's `--version` timeout is a field
/// rather than a constant read inside the method, so a test can drive
/// [`Io::run_version`] itself against a candidate that never answers
/// without waiting [`VERSION_TIMEOUT`] out.
pub struct RealIo {
    version_timeout: Duration,
}

impl Default for RealIo {
    fn default() -> Self {
        Self {
            version_timeout: VERSION_TIMEOUT,
        }
    }
}

fn curl_base(max_time: &str) -> Command {
    let mut cmd = Command::new("curl");
    // --disable first: a user's .curlrc must not rewrite any of this.
    cmd.args([
        "--disable",
        "-sSfL",
        "--proto",
        "=https",
        "--max-redirs",
        "5",
        "--connect-timeout",
        CONNECT_TIMEOUT,
        "--max-time",
        max_time,
    ]);
    cmd
}

fn run_capture(mut cmd: Command, what: &str) -> Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{what} failed: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Io for RealIo {
    fn current_exe(&self) -> Result<PathBuf, String> {
        std::env::current_exe().map_err(|e| format!("cannot resolve the running binary: {e}"))
    }

    fn fetch_effective_url(&self, url: &str) -> Result<String, String> {
        let mut cmd = curl_base(LATEST_MAX_TIME);
        cmd.args(["-o", "/dev/null", "-w", "%{url_effective}", url]);
        Ok(run_capture(cmd, "release lookup")?.trim().to_string())
    }

    fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
        let mut cmd = curl_base(DOWNLOAD_MAX_TIME);
        cmd.arg("-o").arg(dest).arg(url);
        run_capture(cmd, &format!("download of {url}")).map(|_| ())
    }

    fn tar_list(&self, archive: &Path) -> Result<Vec<String>, String> {
        let mut cmd = Command::new("tar");
        cmd.arg("-tf").arg(archive);
        let listing = run_capture(cmd, "archive listing")?;
        Ok(listing.lines().map(str::to_string).collect())
    }

    fn tar_extract(&self, archive: &Path, dest: &Path) -> Result<(), String> {
        let mut cmd = Command::new("tar");
        cmd.arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .args(["--strip-components", "1"]);
        run_capture(cmd, "archive extract").map(|_| ())
    }

    fn run_version(&self, binary: &Path) -> Result<String, String> {
        let child = Command::new(binary)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("candidate could not run: {e}"))?;
        let out = wait_with_timeout(child, self.version_timeout)?;
        if !out.status.success() {
            return Err(format!(
                "candidate --version failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// Wait for *child*, killing it once *timeout* has passed. A candidate
/// that hangs must not hang the update.
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|e| format!("candidate --version: {e}"))
            }
            Ok(None) => {}
            Err(e) => return Err(format!("candidate --version: {e}")),
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "candidate --version did not answer within {timeout:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// target identity and the lock
// ---------------------------------------------------------------------------

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// What the target looked like when the lock was taken. Re-read before the
/// rename: a different digest, length or mtime means somebody else
/// installed over the target while this run was downloading.
#[derive(Debug, PartialEq, Eq)]
struct Fingerprint {
    digest: String,
    len: u64,
    mtime: Option<SystemTime>,
}

fn fingerprint(path: &Path) -> Result<Fingerprint, String> {
    let meta =
        fs::symlink_metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    Ok(Fingerprint {
        digest: sha256_file(path)?,
        len: meta.len(),
        mtime: meta.modified().ok(),
    })
}

/// The exclusive lock on one target path, released on drop.
struct UpdateLock(fs::File);

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Lock file stem for *target*: the first 16 hex of the sha256 of its
/// canonical path, so every spelling of one binary takes one lock.
fn lock_key(target: &Path) -> String {
    let canonical = fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(std::os::unix::ffi::OsStrExt::as_bytes(
        canonical.as_os_str(),
    ));
    format!("{:x}", hasher.finalize())[..16].to_string()
}

fn take_lock(target: &Path) -> Result<UpdateLock, String> {
    let dir = crate::paths::locks_dir().map_err(|e| e.to_string())?;
    let path = dir.join(format!("update-{}.lock", lock_key(target)));
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(UpdateLock(file));
    }
    Err(format!(
        "another hive update is already running on {}",
        target.display()
    ))
}

/// The staging directory, removed on every exit path.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// the command
// ---------------------------------------------------------------------------

/// What the caller prints, and the exit code that goes with it.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub lines: Vec<String>,
    pub code: i32,
}

/// A refusal: the message on stderr, and the exit code.
#[derive(Debug, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub code: i32,
}

fn fail(code: i32, message: impl Into<String>) -> Failure {
    Failure {
        message: message.into(),
        code,
    }
}

fn latest_version(io: &dyn Io) -> Result<Version, String> {
    let effective = io.fetch_effective_url(LATEST_URL)?;
    version_from_effective_url(&effective)
}

/// `hive update [--check|--force]`.
///
/// `--check` never touches the disk: it resolves the latest release and
/// reports, 0 when there is nothing to install (including a local build
/// ahead of the release), 1 when there is, 2 when the query or the tag
/// could not be parsed. Without it, an equal version installs only under
/// `--force` and a newer local version never downgrades.
pub fn run(io: &dyn Io, check: bool, force: bool) -> Result<Outcome, Failure> {
    let current = current_version();
    let query_exit = if check { EXIT_QUERY_FAILED } else { 1 };
    let latest = latest_version(io).map_err(|e| fail(query_exit, e))?;

    if check {
        let status = if latest > current {
            "update available"
        } else if latest == current {
            "up to date"
        } else {
            "ahead of the latest release"
        };
        let code = if latest > current {
            EXIT_UPDATE_AVAILABLE
        } else {
            0
        };
        return Ok(Outcome {
            lines: vec![
                format!("current {current}"),
                format!("latest  {latest}"),
                format!("status  {status}"),
            ],
            code,
        });
    }

    if latest < current {
        return Ok(Outcome {
            lines: vec![format!(
                "hive {current} is ahead of the latest release {latest}; nothing to install"
            )],
            code: 0,
        });
    }
    if latest == current && !force {
        return Ok(Outcome {
            lines: vec![format!("hive {current} is the latest release")],
            code: 0,
        });
    }
    install(io, current, latest, target_triple()).map_err(|e| fail(1, e))
}

/// Download, verify and commit *latest* over the running binary. The
/// running build's release triple is a parameter, not a call to
/// [`target_triple`]: the `None` lane belongs to this function and a test
/// cannot recompile itself for an unpublished target.
fn install(
    io: &dyn Io,
    current: Version,
    latest: Version,
    triple: Option<&str>,
) -> Result<Outcome, String> {
    let triple = triple.ok_or_else(|| {
        format!(
            "no release binary is published for this target ({} {}); \
             build from source instead",
            std::env::consts::ARCH,
            std::env::consts::OS
        )
    })?;
    let target = io.current_exe()?;
    let meta = fs::symlink_metadata(&target)
        .map_err(|e| format!("cannot stat the running binary {}: {e}", target.display()))?;
    if !meta.file_type().is_file() {
        return Err(format!(
            "the running binary {} is not a regular file; update it the way it was installed",
            target.display()
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?
        .to_path_buf();

    let _lock = take_lock(&target)?;
    let before = fingerprint(&target)?;

    let staging = Staging(
        crate::paths::mkdtemp_in(&parent, "hive-update-").map_err(|e| {
            format!(
                "cannot create a staging directory in {}: {e}",
                parent.display()
            )
        })?,
    );

    let name = archive_name(triple);
    let archive = staging.0.join(&name);
    let checksum = staging.0.join(format!("{name}.sha256"));
    io.download(&archive_url(latest, triple), &archive)?;
    io.download(&checksum_url(latest, triple), &checksum)?;

    let expected = parse_sha256_file(
        &fs::read_to_string(&checksum)
            .map_err(|e| format!("cannot read the checksum file: {e}"))?,
        &name,
    )?;
    let actual = sha256_file(&archive)?;
    if actual != expected {
        return Err(format!(
            "checksum mismatch for {name}: expected {expected}, got {actual}"
        ));
    }

    validate_archive_entries(&io.tar_list(&archive)?, triple)?;
    io.tar_extract(&archive, &staging.0)?;

    let candidate = staging.0.join("hive");
    let candidate_meta = fs::symlink_metadata(&candidate)
        .map_err(|e| format!("the archive unpacked no hive binary: {e}"))?;
    if !candidate_meta.file_type().is_file() {
        return Err("the unpacked hive is not a regular file".to_string());
    }
    let reported = io.run_version(&candidate)?;
    let wanted = format!("hive, version {latest}");
    if reported != wanted {
        return Err(format!(
            "the downloaded binary reports {reported:?}, expected {wanted:?}"
        ));
    }

    if fingerprint(&target)? != before {
        return Err(format!(
            "{} changed while this update was downloading; nothing was installed, run hive update again",
            target.display()
        ));
    }
    fs::rename(&candidate, &target)
        .map_err(|e| format!("cannot install over {}: {e}", target.display()))?;

    match fs::symlink_metadata(&target) {
        Ok(m) if m.file_type().is_file() && m.len() == candidate_meta.len() => {}
        _ => {
            return Err(format!(
                "hive {latest} was written to {} but could not be verified afterwards",
                target.display()
            ))
        }
    }
    Ok(Outcome {
        lines: vec![format!("hive {current} -> {latest} ({})", target.display())],
        code: 0,
    })
}

/// Print an outcome and exit with its code.
pub fn print_and_exit(outcome: Outcome) -> ! {
    let mut out = std::io::stdout();
    for line in &outcome.lines {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
    std::process::exit(outcome.code)
}

// ---------------------------------------------------------------------------
// Tests — offline: no network, no real cargo bin, no live team.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testenv::EnvGuard;

    const OLD: &[u8] = b"old hive binary\n";

    fn v(raw: &str) -> Version {
        parse_version(raw).expect(raw)
    }

    // --- pure planning ---

    #[test]
    fn test_parse_version_orders_releases_numerically() {
        assert!(v("0.18.10") > v("0.18.9"));
        assert!(v("v1.0.0") > v("0.99.99"));
        assert_eq!(v("v0.18.2"), v("0.18.2"));
        assert_eq!(v("0.18.2").to_string(), "0.18.2");
        for illegal in [
            "",
            "v",
            "0.18",
            "0.18.2.1",
            "0.18.2-rc1",
            "0.18.2+meta",
            "01.2.3",
            "0.x.2",
            "vv0.1.2",
            " 0.1.2",
            "0.1.2 ",
            "-1.2.3",
        ] {
            assert!(parse_version(illegal).is_none(), "{illegal:?} parsed");
        }
        // the crate's own version is one, or `current_version` panics
        current_version();
        // and the fixtures the decision tests hang off straddle it, at
        // every crate version — including a `.0` patch
        assert!(older() < current_version(), "{} is not older", older());
        assert!(newer() > current_version(), "{} is not newer", newer());
    }

    #[test]
    fn test_version_from_effective_url_accepts_only_this_repo_tag_page() {
        assert_eq!(
            version_from_effective_url("https://github.com/notdp/hive/releases/tag/v0.18.3")
                .unwrap(),
            v("0.18.3")
        );
        for refused in [
            // another host, another repo, http, the unresolved latest URL
            "https://example.com/notdp/hive/releases/tag/v0.18.3",
            "https://github.com/someone/hive/releases/tag/v0.18.3",
            "http://github.com/notdp/hive/releases/tag/v0.18.3",
            "https://github.com/notdp/hive/releases/latest",
            // a login or consent redirect, an extra segment, a query string
            "https://github.com/login?return_to=%2Fnotdp%2Fhive",
            "https://github.com/notdp/hive/releases/tag/v0.18.3/assets",
            "https://github.com/notdp/hive/releases/tag/v0.18.3?x=1",
            "https://github.com/notdp/hive/releases/tag/v0.18.3#notes",
            // tags that are not releases
            "https://github.com/notdp/hive/releases/tag/0.18.3",
            "https://github.com/notdp/hive/releases/tag/v0.18.3-rc1",
            "https://github.com/notdp/hive/releases/tag/nightly",
            "https://github.com/notdp/hive/releases/tag/",
        ] {
            assert!(
                version_from_effective_url(refused).is_err(),
                "{refused} accepted"
            );
        }
    }

    #[test]
    fn test_target_triple_is_a_published_release_triple_or_none() {
        const PUBLISHED: [&str; 4] = [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        ];
        if let Some(triple) = target_triple() {
            assert!(PUBLISHED.contains(&triple), "{triple} is not published");
            assert_eq!(archive_name(triple), format!("hive-{triple}.tar.xz"));
            assert_eq!(
                archive_url(v("0.18.3"), triple),
                format!(
                    "https://github.com/notdp/hive/releases/download/v0.18.3/hive-{triple}.tar.xz"
                )
            );
            assert_eq!(
                checksum_url(v("0.18.3"), triple),
                format!("{}.sha256", archive_url(v("0.18.3"), triple))
            );
        }
    }

    #[test]
    fn test_parse_sha256_file_takes_one_digest_for_the_expected_name() {
        let digest = "a".repeat(64);
        let name = "hive-aarch64-apple-darwin.tar.xz";
        // the shape cargo-dist publishes, plus the plain coreutils one
        for text in [
            format!("{digest}  *{name}\n"),
            format!("{digest}  {name}\n"),
            format!("{digest} {name}"),
            format!("{digest}\t*{name}\n\n"),
        ] {
            assert_eq!(parse_sha256_file(&text, name).unwrap(), digest, "{text:?}");
        }
        assert_eq!(
            parse_sha256_file(&format!("{} *{name}\n", digest.to_uppercase()), name).unwrap(),
            digest
        );
        for refused in [
            String::new(),
            format!("{digest}\n"),                        // no file name
            format!("{digest}  *{name}  extra\n"),        // trailing fields
            format!("{digest}  *{name}\n{digest}  *b\n"), // two digests
            format!("{digest}  *hive-other.tar.xz\n"),    // another file
            format!("{}  *{name}\n", "a".repeat(63)),     // short digest
            format!("{}  *{name}\n", "z".repeat(64)),     // not hex
            format!("*{name}  {digest}\n"),               // reversed
        ] {
            assert!(
                parse_sha256_file(&refused, name).is_err(),
                "{refused:?} accepted"
            );
        }
    }

    fn entries(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_validate_archive_entries_demands_one_release_root_holding_hive() {
        let triple = "aarch64-apple-darwin";
        // the real v0.18.2 listing
        validate_archive_entries(
            &entries(&[
                "hive-aarch64-apple-darwin/",
                "hive-aarch64-apple-darwin/hive",
                "hive-aarch64-apple-darwin/README.md",
                "hive-aarch64-apple-darwin/CHANGELOG.md",
                "hive-aarch64-apple-darwin/LICENSE",
                "",
            ]),
            triple,
        )
        .unwrap();
        for refused in [
            entries(&[]),                                     // empty
            entries(&["hive-aarch64-apple-darwin/"]),         // no binary
            entries(&["hive-aarch64-apple-darwin/bin/hive"]), // binary one level down
            entries(&["hive"]),                               // root-level binary
            entries(&["hive-x86_64-apple-darwin/hive"]),      // another triple
            entries(&["hive-aarch64-apple-darwinx/hive"]),    // prefix look-alike
            entries(&["hive-aarch64-apple-darwin/hive", "other/thing"]),
            entries(&["hive-aarch64-apple-darwin/hive", "/etc/passwd"]),
            entries(&["hive-aarch64-apple-darwin/hive", "../escape"]),
            entries(&[
                "hive-aarch64-apple-darwin/hive",
                "hive-aarch64-apple-darwin/../../x",
            ]),
            entries(&[
                "hive-aarch64-apple-darwin/hive",
                "./hive-aarch64-apple-darwin/x",
            ]),
        ] {
            assert!(
                validate_archive_entries(&refused, triple).is_err(),
                "{refused:?} accepted"
            );
        }
    }

    #[test]
    fn test_check_and_force_are_mutually_exclusive_in_the_command_tree() {
        let cli = crate::cli::build_cli();
        cli.clone()
            .try_get_matches_from(["hive", "update", "--check"])
            .unwrap();
        cli.clone()
            .try_get_matches_from(["hive", "update", "--force"])
            .unwrap();
        assert!(cli
            .try_get_matches_from(["hive", "update", "--check", "--force"])
            .is_err());
    }

    // --- the injected world ---

    enum Extract {
        /// the archive unpacks a plain `hive` file
        File,
        /// … a directory named `hive`
        Dir,
        /// … nothing at all
        Nothing,
        Fail(String),
    }

    struct Fake {
        exe: Result<PathBuf, String>,
        target: PathBuf,
        effective: Result<String, String>,
        download_err: Option<String>,
        archive: Vec<u8>,
        checksum: String,
        list: Result<Vec<String>, String>,
        extract: Extract,
        version: Result<String, String>,
        /// The last moment before the fingerprint re-check and the rename.
        on_version: Option<VersionHook>,
        /// Every URL asked for, in order.
        urls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn urls(&self) -> Vec<String> {
            self.urls.borrow().clone()
        }
    }

    /// Runs where the real `--version` call sits, on (candidate, target).
    type VersionHook = Box<dyn Fn(&Path, &Path)>;
    /// One knob of a [`Fake`], turned to break one step.
    type Breakage = Box<dyn Fn(&mut Fake)>;

    const NEW_BINARY: &[u8] = b"new hive binary\n";

    /// A fake whose every step succeeds, publishing *latest*.
    fn fake(target: &Path, latest: Version) -> Fake {
        let triple = target_triple().unwrap_or("aarch64-apple-darwin");
        let archive = b"pretend this is a tar.xz".to_vec();
        let mut hasher = Sha256::new();
        hasher.update(&archive);
        let digest = format!("{:x}", hasher.finalize());
        Fake {
            exe: Ok(target.to_path_buf()),
            target: target.to_path_buf(),
            effective: Ok(format!("{TAG_PREFIX}v{latest}")),
            download_err: None,
            checksum: format!("{digest}  *{}\n", archive_name(triple)),
            archive,
            list: Ok(entries(&[])
                .into_iter()
                .chain([
                    format!("hive-{triple}/"),
                    format!("hive-{triple}/hive"),
                    format!("hive-{triple}/README.md"),
                ])
                .collect()),
            extract: Extract::File,
            version: Ok(format!("hive, version {latest}")),
            on_version: None,
            urls: RefCell::new(Vec::new()),
        }
    }

    impl Io for Fake {
        fn current_exe(&self) -> Result<PathBuf, String> {
            self.exe.clone()
        }
        fn fetch_effective_url(&self, url: &str) -> Result<String, String> {
            self.urls.borrow_mut().push(url.to_string());
            self.effective.clone()
        }
        fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
            self.urls.borrow_mut().push(url.to_string());
            if let Some(err) = &self.download_err {
                return Err(err.clone());
            }
            let body: Vec<u8> = if url.ends_with(".sha256") {
                self.checksum.as_bytes().to_vec()
            } else {
                self.archive.clone()
            };
            fs::write(dest, body).map_err(|e| e.to_string())
        }
        fn tar_list(&self, _archive: &Path) -> Result<Vec<String>, String> {
            self.list.clone()
        }
        fn tar_extract(&self, _archive: &Path, dest: &Path) -> Result<(), String> {
            match &self.extract {
                Extract::File => {
                    fs::write(dest.join("hive"), NEW_BINARY).map_err(|e| e.to_string())
                }
                Extract::Dir => fs::create_dir(dest.join("hive")).map_err(|e| e.to_string()),
                Extract::Nothing => Ok(()),
                Extract::Fail(err) => Err(err.clone()),
            }
        }
        fn run_version(&self, binary: &Path) -> Result<String, String> {
            if let Some(hook) = &self.on_version {
                hook(binary, &self.target);
            }
            self.version.clone()
        }
    }

    /// A throwaway install directory holding one `hive` target, with
    /// `$HIVE_HOME` (the lock's home) elsewhere under the same temp root.
    struct Bed {
        _tmp: tempfile::TempDir,
        _env: EnvGuard,
        target: PathBuf,
    }

    fn bed() -> Bed {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::new();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        let dir = tmp.path().join("bin");
        fs::create_dir(&dir).unwrap();
        let target = dir.join("hive");
        fs::write(&target, OLD).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        Bed {
            _tmp: tmp,
            _env: env,
            target,
        }
    }

    /// The target still holds *bytes*, and no staging directory survives
    /// beside it.
    fn assert_intact(target: &Path, bytes: &[u8]) {
        assert_eq!(fs::read(target).unwrap(), bytes, "target bytes changed");
        let left: Vec<String> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["hive".to_string()], "staging left behind");
    }

    fn newer() -> Version {
        let Version(major, minor, patch) = current_version();
        Version(major, minor, patch + 1)
    }

    /// A release strictly older than this build. Decrementing the patch
    /// alone would land on the current version whenever it ends in `.0`.
    fn older() -> Version {
        let Version(major, minor, patch) = current_version();
        if patch > 0 {
            Version(major, minor, patch - 1)
        } else if minor > 0 {
            Version(major, minor - 1, 999)
        } else {
            Version(
                major.checked_sub(1).expect("the crate is past 0.0.0"),
                999,
                999,
            )
        }
    }

    // --- decisions ---

    #[test]
    fn test_check_reports_three_states_without_touching_the_disk() {
        let bed = bed();
        let current = current_version();

        let mut io = fake(&bed.target, newer());
        let out = run(&io, true, false).unwrap();
        assert_eq!(out.code, EXIT_UPDATE_AVAILABLE);
        assert_eq!(out.lines[0], format!("current {current}"));
        assert_eq!(out.lines[2], "status  update available");

        io.effective = Ok(format!("{TAG_PREFIX}v{current}"));
        let out = run(&io, true, false).unwrap();
        assert_eq!(out.code, 0);
        assert_eq!(out.lines[2], "status  up to date");

        io.effective = Ok(format!("{TAG_PREFIX}v{}", older()));
        let out = run(&io, true, false).unwrap();
        assert_eq!(out.code, 0);
        assert_eq!(out.lines[1], format!("latest  {}", older()));
        assert_eq!(out.lines[2], "status  ahead of the latest release");

        // nothing was created: no lock, no staging
        assert_intact(&bed.target, OLD);
        assert!(!crate::paths::hive_home().exists());
    }

    #[test]
    fn test_a_local_build_ahead_of_the_release_is_never_downgraded() {
        let bed = bed();
        let io = fake(&bed.target, older());
        let out = run(&io, false, false).unwrap();
        assert_eq!(out.code, 0);
        assert!(out.lines[0].contains("is ahead of the latest release"));
        assert_intact(&bed.target, OLD);
    }

    #[test]
    fn test_the_same_version_installs_only_under_force() {
        let bed = bed();
        let current = current_version();
        let io = fake(&bed.target, current);
        let out = run(&io, false, false).unwrap();
        assert_eq!(
            out.lines[0],
            format!("hive {current} is the latest release")
        );
        assert_intact(&bed.target, OLD);

        let out = run(&io, false, true).unwrap();
        assert_eq!(out.code, 0);
        assert_eq!(
            out.lines[0],
            format!("hive {current} -> {current} ({})", bed.target.display())
        );
        assert_intact(&bed.target, NEW_BINARY);
    }

    #[test]
    fn test_a_newer_release_replaces_the_running_binary() {
        let bed = bed();
        let latest = newer();
        let triple = target_triple().expect("this build has a release triple");
        let mut io = fake(&bed.target, latest);
        // the candidate is staged beside the target, so the commit rename
        // stays inside one filesystem
        io.on_version = Some(Box::new(|candidate: &Path, target: &Path| {
            let staging = candidate
                .parent()
                .expect("the candidate has a staging directory");
            assert_eq!(
                staging.parent(),
                target.parent(),
                "{} is not beside {}",
                staging.display(),
                target.display()
            );
            assert!(
                staging
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("hive-update-"),
                "{}",
                staging.display()
            );
        }));

        let out = run(&io, false, false).unwrap();
        assert_eq!(out.code, 0);
        assert_eq!(
            out.lines[0],
            format!(
                "hive {} -> {latest} ({})",
                current_version(),
                bed.target.display()
            )
        );
        assert_intact(&bed.target, NEW_BINARY);

        // the release it went and got: the latest tag, this triple, spelled
        // out rather than rebuilt from the same helpers the code uses
        assert_eq!(
            io.urls(),
            vec![
                "https://github.com/notdp/hive/releases/latest".to_string(),
                format!(
                    "https://github.com/notdp/hive/releases/download/v{latest}/hive-{triple}.tar.xz"
                ),
                format!(
                    "https://github.com/notdp/hive/releases/download/v{latest}/hive-{triple}.tar.xz.sha256"
                ),
            ]
        );
    }

    // --- the failure matrix: every branch leaves the target's bytes alone ---

    #[test]
    fn test_every_failure_leaves_the_target_untouched() {
        let latest = newer();
        let triple = target_triple().unwrap_or("aarch64-apple-darwin");
        let cases: Vec<(&str, Breakage)> = vec![
            (
                "curl fails",
                Box::new(|f: &mut Fake| f.effective = Err("curl: (6) could not resolve".into())),
            ),
            (
                "the redirect lands somewhere else",
                Box::new(|f: &mut Fake| f.effective = Ok("https://example.com/x".into())),
            ),
            (
                "the download fails",
                Box::new(|f: &mut Fake| f.download_err = Some("curl: (22) 404".into())),
            ),
            (
                "the checksum does not match",
                Box::new(move |f: &mut Fake| {
                    f.checksum = format!("{}  *{}\n", "b".repeat(64), archive_name(triple))
                }),
            ),
            (
                "the checksum file is malformed",
                Box::new(|f: &mut Fake| f.checksum = "not a checksum file\n".into()),
            ),
            (
                "tar cannot list the archive",
                Box::new(|f: &mut Fake| f.list = Err("tar: unrecognized archive format".into())),
            ),
            (
                "the archive holds no hive",
                Box::new(move |f: &mut Fake| f.list = Ok(vec![format!("hive-{triple}/README.md")])),
            ),
            (
                "the archive holds a parent path",
                Box::new(move |f: &mut Fake| {
                    f.list = Ok(vec![
                        format!("hive-{triple}/hive"),
                        format!("hive-{triple}/../../evil"),
                    ])
                }),
            ),
            (
                "tar cannot extract",
                Box::new(|f: &mut Fake| f.extract = Extract::Fail("tar: broken pipe".into())),
            ),
            (
                "the candidate is missing",
                Box::new(|f: &mut Fake| f.extract = Extract::Nothing),
            ),
            (
                "the candidate is not a regular file",
                Box::new(|f: &mut Fake| f.extract = Extract::Dir),
            ),
            (
                "the candidate cannot run",
                Box::new(|f: &mut Fake| f.version = Err("exec format error".into())),
            ),
            (
                "the candidate reports another version",
                Box::new(|f: &mut Fake| f.version = Ok("hive, version 0.0.1".into())),
            ),
            (
                "the candidate never answers",
                Box::new(|f: &mut Fake| {
                    f.version = Err("candidate --version did not answer within 10s".into())
                }),
            ),
            (
                "the rename fails",
                Box::new(|f: &mut Fake| {
                    // the verified candidate is gone by commit time
                    f.on_version = Some(Box::new(|candidate: &Path, _target: &Path| {
                        fs::remove_file(candidate).unwrap();
                    }))
                }),
            ),
        ];
        for (name, break_it) in cases {
            let bed = bed();
            let mut io = fake(&bed.target, latest);
            break_it(&mut io);
            let failure = run(&io, false, false)
                .err()
                .unwrap_or_else(|| panic!("{name}: the update went through"));
            assert_eq!(failure.code, 1, "{name}");
            assert!(!failure.message.is_empty(), "{name}");
            assert_intact(&bed.target, OLD);
        }
    }

    #[test]
    fn test_a_query_failure_exits_2_under_check_and_1_under_update() {
        let bed = bed();
        let mut io = fake(&bed.target, newer());
        io.effective = Err("curl: (28) operation timed out".into());
        assert_eq!(run(&io, true, false).unwrap_err().code, EXIT_QUERY_FAILED);
        assert_eq!(run(&io, false, false).unwrap_err().code, 1);
        assert_intact(&bed.target, OLD);
    }

    #[test]
    fn test_a_target_replaced_during_the_download_is_refused() {
        let bed = bed();
        let mut io = fake(&bed.target, newer());
        // another installer lands on the same path mid-run
        io.on_version = Some(Box::new(|_candidate: &Path, target: &Path| {
            fs::write(target, b"a newer install\n").unwrap();
        }));
        let failure = run(&io, false, false).unwrap_err();
        assert!(
            failure
                .message
                .contains("changed while this update was downloading"),
            "{}",
            failure.message
        );
        // the other install's bytes survive; the staging is gone
        assert_intact(&bed.target, b"a newer install\n");
    }

    #[test]
    fn test_a_second_update_on_the_same_target_is_refused_while_one_runs() {
        let bed = bed();
        let dir = crate::paths::locks_dir().unwrap();
        let path = dir.join(format!("update-{}.lock", lock_key(&bed.target)));
        let held = fs::File::create(&path).unwrap();
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        let io = fake(&bed.target, newer());
        let failure = run(&io, false, false).unwrap_err();
        assert!(
            failure
                .message
                .contains("another hive update is already running"),
            "{}",
            failure.message
        );
        assert_intact(&bed.target, OLD);

        // released, the same run goes through
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        run(&io, false, false).unwrap();
        assert_intact(&bed.target, NEW_BINARY);
        // and the lock is free again afterwards
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[test]
    fn test_a_symlinked_target_is_refused_with_its_path() {
        let bed = bed();
        let link = bed.target.parent().unwrap().join("hive-link");
        std::os::unix::fs::symlink(&bed.target, &link).unwrap();
        let mut io = fake(&bed.target, newer());
        io.exe = Ok(link.clone());
        let failure = run(&io, false, false).unwrap_err();
        assert!(
            failure.message.contains("not a regular file")
                && failure.message.contains(&link.display().to_string()),
            "{}",
            failure.message
        );
        assert_eq!(fs::read(&bed.target).unwrap(), OLD);
    }

    #[test]
    fn test_check_needs_no_target_it_can_resolve() {
        // `current_exe` is read in the install lane alone, so a target that
        // cannot be resolved leaves `--check` working.
        let bed = bed();
        let mut io = fake(&bed.target, newer());
        io.exe = Err("no current_exe on this platform".into());
        assert_eq!(run(&io, true, false).unwrap().code, EXIT_UPDATE_AVAILABLE);
        assert_eq!(run(&io, false, false).unwrap_err().code, 1);
        assert_intact(&bed.target, OLD);
    }

    #[test]
    fn test_a_target_with_no_published_archive_refuses_before_anything_else() {
        // `target_triple()` is decided at compile time, so the unpublished
        // lane is driven through `install`'s parameter: a machine running
        // this suite always has a triple of its own.
        let bed = bed();
        let latest = newer();
        let current = current_version();
        let mut io = fake(&bed.target, latest);
        io.exe = Err("no current_exe on this platform".into());

        let err = install(&io, current, latest, None).unwrap_err();
        assert!(
            err.contains("no release binary is published for this target")
                && err.contains(std::env::consts::ARCH)
                && err.contains(std::env::consts::OS),
            "{err}"
        );
        // before the running binary is even looked up, and before any fetch
        assert!(!err.contains("current_exe"), "{err}");
        assert!(io.urls().is_empty(), "an unsupported target still fetched");
        assert_intact(&bed.target, OLD);

        // given a triple, the same run gets past that check
        let err = install(&io, current, latest, Some("aarch64-apple-darwin")).unwrap_err();
        assert!(err.contains("no current_exe"), "{err}");
        assert_intact(&bed.target, OLD);

        // and `--check`, which never enters `install`, still answers
        assert_eq!(run(&io, true, false).unwrap().code, EXIT_UPDATE_AVAILABLE);
    }

    // --- the real thing, offline: real tar, real sha2, real rename ---

    /// tar is a hard requirement of the flow test the way tmux is of the
    /// display tests: it builds and unpacks a real release archive.
    fn require_tar() {
        if let Err(err) = Command::new("tar").arg("--version").output() {
            panic!("tar is required by the update flow test ({err})");
        }
    }

    /// Everything but the network: `current_exe` and the two downloads are
    /// fakes, tar and the candidate's `--version` are the real ones.
    struct FlowIo {
        target: PathBuf,
        latest: Version,
        archive: PathBuf,
        checksum: PathBuf,
    }

    impl Io for FlowIo {
        fn current_exe(&self) -> Result<PathBuf, String> {
            Ok(self.target.clone())
        }
        fn fetch_effective_url(&self, _url: &str) -> Result<String, String> {
            Ok(format!("{TAG_PREFIX}v{}", self.latest))
        }
        fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
            let from = if url.ends_with(".sha256") {
                &self.checksum
            } else {
                &self.archive
            };
            fs::copy(from, dest).map(|_| ()).map_err(|e| e.to_string())
        }
        fn tar_list(&self, archive: &Path) -> Result<Vec<String>, String> {
            RealIo::default().tar_list(archive)
        }
        fn tar_extract(&self, archive: &Path, dest: &Path) -> Result<(), String> {
            RealIo::default().tar_extract(archive, dest)
        }
        fn run_version(&self, binary: &Path) -> Result<String, String> {
            // real tar unpacked it beside the target, on one filesystem
            assert_eq!(
                binary.parent().and_then(Path::parent),
                self.target.parent(),
                "{} is not staged beside {}",
                binary.display(),
                self.target.display()
            );
            RealIo::default().run_version(binary)
        }
    }

    #[test]
    fn test_the_whole_flow_over_a_real_archive_replaces_the_binary() {
        require_tar();
        let bed = bed();
        let latest = newer();
        let triple = target_triple().expect("this build has a release triple");

        // the release layout: hive-<triple>/hive plus the docs beside it
        let build = tempfile::tempdir().unwrap();
        let root = build.path().join(format!("hive-{triple}"));
        fs::create_dir(&root).unwrap();
        let script = format!("#!/bin/sh\necho \"hive, version {latest}\"\n");
        fs::write(root.join("hive"), &script).unwrap();
        fs::set_permissions(root.join("hive"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("README.md"), "# hive\n").unwrap();
        let archive = build.path().join(archive_name(triple));
        let made = Command::new("tar")
            .arg("-cJf")
            .arg(&archive)
            .arg("-C")
            .arg(build.path())
            .arg(format!("hive-{triple}"))
            .output()
            .expect("tar runs");
        assert!(
            made.status.success(),
            "tar -cJf: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        let checksum = build
            .path()
            .join(format!("{}.sha256", archive_name(triple)));
        fs::write(
            &checksum,
            format!(
                "{}  *{}\n",
                sha256_file(&archive).unwrap(),
                archive_name(triple)
            ),
        )
        .unwrap();

        let io = FlowIo {
            target: bed.target.clone(),
            latest,
            archive,
            checksum,
        };
        let out = run(&io, false, false).unwrap();
        assert_eq!(out.code, 0);
        assert_eq!(
            out.lines,
            vec![format!(
                "hive {} -> {latest} ({})",
                current_version(),
                bed.target.display()
            )]
        );

        // the target is the archived binary, still executable, and the
        // staging directory beside it is gone
        assert_intact(&bed.target, script.as_bytes());
        assert_eq!(
            fs::metadata(&bed.target).unwrap().permissions().mode() & 0o111,
            0o111
        );
        assert_eq!(
            RealIo::default().run_version(&bed.target).unwrap(),
            format!("hive, version {latest}")
        );

        // the lock is released
        let path = crate::paths::locks_dir()
            .unwrap()
            .join(format!("update-{}.lock", lock_key(&bed.target)));
        let file = fs::File::open(&path).unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[test]
    fn test_run_version_kills_a_candidate_that_never_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("hive");
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        // the method the command itself calls, with only the wait shortened
        let io = RealIo {
            version_timeout: Duration::from_millis(200),
        };
        let started = Instant::now();
        let err = io.run_version(&script).unwrap_err();
        assert!(err.contains("did not answer within 200ms"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited too long"
        );
        // and what it waits in production
        assert_eq!(RealIo::default().version_timeout, VERSION_TIMEOUT);
    }

    #[test]
    fn test_lock_key_is_one_key_per_binary_whatever_the_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        fs::create_dir(&dir).unwrap();
        let target = dir.join("hive");
        fs::write(&target, OLD).unwrap();
        let key = lock_key(&target);
        assert_eq!(key.len(), 16);
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(key, lock_key(&dir.join(".").join("hive")));
        assert_ne!(key, lock_key(&dir.join("other")));
    }
}
