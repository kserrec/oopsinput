//! User-local state coordination, retention, and explicit deletion.
//!
//! Every successful mutating path takes the same advisory file lock. Analysis-
//! time writes are bounded by the process watchdog; writes after a prompt wait
//! at most 25 ms so a busy lock cannot override the user's choice. The lock
//! anchor is stable across atomic log replacement, so pruning cannot race an
//! append from another shell and silently lose its record. `purge` unlinks the
//! anchor only while holding it; a waiter verifies the inode after acquiring
//! the lock and retries on the new anchor instead of writing under a stale
//! lock.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const LOCK_FILE: &str = ".oopsinput.lock";
const EVENT_RETENTION_FILE: &str = ".events-retention";
const POLICY_RETENTION_FILE: &str = ".policy-retention";
const TEMP_PREFIX: &str = ".oopsinput-tmp-";
const RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const PRUNE_INTERVAL_MS: u64 = 24 * 60 * 60 * 1_000;
const WRITE_LOCK_TIMEOUT: Duration = Duration::from_millis(25);

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// State dir: $OOPSINPUT_STATE_DIR override (tests, custom setups) else
/// $XDG_STATE_HOME/oopsinput else ~/.local/state/oopsinput.
pub(crate) fn state_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("OOPSINPUT_STATE_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("oopsinput"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/state/oopsinput"))
}

/// Append one already-serialized JSONL record and opportunistically enforce
/// the 30-day retention window. Callers deliberately discard errors: state is
/// evidence, never permission to delay the user's command or prompt choice.
pub(crate) fn append_jsonl(
    dir: &Path,
    log_name: &'static str,
    line: &[u8],
    now_ms: u64,
) -> std::io::Result<()> {
    let _lock = StateLock::acquire(dir)?;
    maybe_prune_log(dir, log_name, now_ms)?;
    append_private_file(&dir.join(log_name), line)
}

/// The post-prompt form of `append_jsonl`: preserve the already-made user
/// choice even if another shell is holding the state lock for compaction.
pub(crate) fn append_jsonl_after_prompt(
    dir: &Path,
    log_name: &'static str,
    line: &[u8],
    now_ms: u64,
) -> std::io::Result<()> {
    let _lock = StateLock::acquire_for_write(dir)?;
    maybe_prune_log(dir, log_name, now_ms)?;
    append_private_file(&dir.join(log_name), line)
}

/// Atomically replace one small, product-owned state file under the shared
/// lock. This never truncates or writes through a symlink.
pub(crate) fn replace_small_file(
    dir: &Path,
    name: &'static str,
    bytes: &[u8],
) -> std::io::Result<()> {
    let _lock = StateLock::acquire(dir)?;
    replace_private_file(dir, name, bytes)
}

fn ensure_state_dir(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(std::io::Error::other(
                "state directory is not a real directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
        }
        Err(error) => return Err(error),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(std::io::Error::other(
            "state directory is not a real directory",
        ));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

struct StateLock {
    file: File,
    path: PathBuf,
}

impl StateLock {
    fn acquire(dir: &Path) -> std::io::Result<Self> {
        Self::acquire_until(dir, None)
    }

    fn acquire_for_write(dir: &Path) -> std::io::Result<Self> {
        Self::acquire_until(dir, Some(Instant::now() + WRITE_LOCK_TIMEOUT))
    }

    fn acquire_until(dir: &Path, deadline: Option<Instant>) -> std::io::Result<Self> {
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "state lock stayed busy past the write deadline",
                ));
            }
            match ensure_state_dir(dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            let path = dir.join(LOCK_FILE);
            reject_non_regular(&path, "state lock")?;
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => file,
                // `purge` can remove the directory between validation and
                // open. Recreate and retry rather than turning that benign
                // race into a permanent logging failure.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let opened = file.metadata()?;
            if !opened.is_file() || opened.permissions().mode() & 0o077 != 0 {
                return Err(std::io::Error::other(
                    "state lock is not a user-only regular file",
                ));
            }
            if let Some(deadline) = deadline {
                loop {
                    match file.try_lock() {
                        Ok(()) => break,
                        Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_micros(250));
                        }
                        Err(std::fs::TryLockError::WouldBlock) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::WouldBlock,
                                "state lock stayed busy past the write deadline",
                            ));
                        }
                        Err(std::fs::TryLockError::Error(error)) => return Err(error),
                    }
                }
            } else {
                file.lock()?;
            }

            match std::fs::symlink_metadata(&path) {
                Ok(current)
                    if !current.file_type().is_symlink() && same_file(&opened, &current) =>
                {
                    return Ok(Self { file, path });
                }
                Ok(current) if current.file_type().is_symlink() => {
                    return Err(std::io::Error::other("state lock is a symlink"));
                }
                // The anchor was purged while this handle waited. Dropping
                // releases the stale inode; the next loop joins the new lock.
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn unlink_anchor(&self) -> std::io::Result<()> {
        let opened = self.file.metadata()?;
        let current = std::fs::symlink_metadata(&self.path)?;
        if current.file_type().is_symlink() || !same_file(&opened, &current) {
            return Err(std::io::Error::other(
                "state lock changed while purge held it",
            ));
        }
        std::fs::remove_file(&self.path)
    }
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Refuse every existing filesystem object except a regular file before an
/// open. In particular, opening a FIFO can block before a post-open metadata
/// check gets a chance to fail open.
fn reject_non_regular(path: &Path, label: &str) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => Err(std::io::Error::other(format!(
            "{label} is not a regular file"
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn append_private_file(path: &Path, line: &[u8]) -> std::io::Result<()> {
    reject_non_regular(path, "JSONL state path")?;
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::other(
            "JSONL state path is not a user-only regular file",
        ));
    }
    file.write_all(line)
}

fn maybe_prune_log(dir: &Path, log_name: &'static str, now_ms: u64) -> std::io::Result<()> {
    let marker_name = match log_name {
        "events.jsonl" => EVENT_RETENTION_FILE,
        "policy.jsonl" => POLICY_RETENTION_FILE,
        _ => return Err(std::io::Error::other("unknown retained log")),
    };
    if !prune_is_due(&dir.join(marker_name), now_ms)? {
        return Ok(());
    }

    prune_jsonl(
        &dir.join(log_name),
        dir,
        now_ms.saturating_sub(RETENTION_MS),
    )?;
    replace_private_file(dir, marker_name, now_ms.to_string().as_bytes())
}

fn prune_is_due(marker: &Path, now_ms: u64) -> std::io::Result<bool> {
    reject_non_regular(marker, "retention marker")?;
    let meta = match std::fs::metadata(marker) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if !meta.is_file() || meta.len() > 32 {
        return Ok(true);
    }
    let last = match std::fs::read_to_string(marker)?.trim().parse::<u64>() {
        Ok(last) => last,
        Err(_) => return Ok(true),
    };
    if now_ms >= last {
        Ok(now_ms - last >= PRUNE_INTERVAL_MS)
    } else {
        // Small wall-clock corrections must not turn every command into a
        // full-log compaction until the clock catches up. A marker more than
        // one sweep interval in the future is implausible, so rebase it once.
        Ok(last - now_ms >= PRUNE_INTERVAL_MS)
    }
}

#[derive(serde::Deserialize)]
struct Timestamped {
    ts_ms: u64,
}

/// Rewrite one log to a private temporary file and atomically replace it.
/// The shared lock prevents concurrent appends; readers see either complete
/// version. Invalid/torn records have no usable timestamp and are discarded.
fn prune_jsonl(path: &Path, dir: &Path, cutoff_ms: u64) -> std::io::Result<()> {
    reject_non_regular(path, "JSONL state path")?;
    let source = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let source_meta = source.metadata()?;
    if !source_meta.is_file() || source_meta.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::other(
            "JSONL state path is not a user-only regular file",
        ));
    }

    let (mut temp, temp_path) = create_temp(dir)?;
    let mut cleanup = TempCleanup(Some(temp_path.clone()));
    let mut reader = std::io::BufReader::new(source);
    let mut line = Vec::new();
    let mut changed = false;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let had_newline = line.last() == Some(&b'\n');
        let record = if had_newline {
            &line[..line.len() - 1]
        } else {
            &line[..]
        };
        match serde_json::from_slice::<Timestamped>(record) {
            Ok(stored) if stored.ts_ms >= cutoff_ms => {
                temp.write_all(record)?;
                temp.write_all(b"\n")?;
                changed |= !had_newline;
            }
            Ok(_) | Err(_) => changed = true,
        }
    }

    if !changed {
        return Ok(());
    }
    temp.sync_all()?;
    let current = std::fs::symlink_metadata(path)?;
    if current.file_type().is_symlink() || !same_file(&source_meta, &current) {
        return Err(std::io::Error::other(
            "JSONL state path changed during retention pruning",
        ));
    }
    std::fs::rename(&temp_path, path)?;
    cleanup.disarm();
    Ok(())
}

fn create_temp(dir: &Path) -> std::io::Result<(File, PathBuf)> {
    loop {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{TEMP_PREFIX}{}-{id}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

struct TempCleanup(Option<PathBuf>);

impl TempCleanup {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn replace_private_file(dir: &Path, name: &'static str, bytes: &[u8]) -> std::io::Result<()> {
    let path = dir.join(name);
    reject_non_regular(&path, "state path")?;
    let (mut temp, temp_path) = create_temp(dir)?;
    let mut cleanup = TempCleanup(Some(temp_path.clone()));
    temp.write_all(bytes)?;
    temp.sync_all()?;
    std::fs::rename(&temp_path, path)?;
    cleanup.disarm();
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PurgeResult {
    pub removed_files: usize,
    pub directory_retained: bool,
}

#[derive(Debug)]
pub(crate) enum PurgeError {
    StateDirUnavailable,
    UnsafeStateDirectory,
    UnsafeOwnedEntry,
    Io(std::io::Error),
}

impl std::fmt::Display for PurgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StateDirUnavailable => {
                write!(f, "HOME is unset, so the state directory cannot be located")
            }
            Self::UnsafeStateDirectory => write!(
                f,
                "the state directory path is a symlink or not a directory; refusing to delete"
            ),
            Self::UnsafeOwnedEntry => write!(
                f,
                "an oopsinput state path is not a file or symlink; refusing recursive deletion"
            ),
            Self::Io(error) => write!(f, "could not delete recorded state: {error}"),
        }
    }
}

impl From<std::io::Error> for PurgeError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn purge() -> Result<PurgeResult, PurgeError> {
    let dir = state_dir().ok_or(PurgeError::StateDirUnavailable)?;
    purge_from_dir(&dir)
}

fn purge_from_dir(dir: &Path) -> Result<PurgeResult, PurgeError> {
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PurgeResult {
                removed_files: 0,
                directory_retained: false,
            });
        }
        Err(error) => return Err(PurgeError::Io(error)),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(PurgeError::UnsafeStateDirectory);
        }
        Ok(_) => {}
    }

    // Preflight everything before deleting anything: a directory or device at
    // one of our names is not ours to recursively remove or unlink. Do the
    // first pass before creating a lock in a manually populated directory, so
    // a refused purge is itself non-mutating.
    for path in owned_entries(dir)? {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_symlink() && !meta.is_file() {
            return Err(PurgeError::UnsafeOwnedEntry);
        }
    }

    let lock = StateLock::acquire(dir)?;
    let owned = owned_entries(dir)?;
    // Repeat under the lock to close races with another oopsinput process.
    for path in &owned {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_symlink() && !meta.is_file() {
            return Err(PurgeError::UnsafeOwnedEntry);
        }
    }

    for path in &owned {
        std::fs::remove_file(path)?;
    }
    lock.unlink_anchor()?;
    drop(lock);

    let directory_retained = match std::fs::read_dir(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(PurgeError::Io(error)),
        Ok(mut entries) => {
            if entries.next().is_some() {
                true
            } else {
                match std::fs::remove_dir(dir) {
                    Ok(()) => false,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(_)
                        if std::fs::read_dir(dir)
                            .is_ok_and(|mut entries| entries.next().is_some()) =>
                    {
                        true
                    }
                    Err(error) => return Err(PurgeError::Io(error)),
                }
            }
        }
    };
    Ok(PurgeResult {
        removed_files: owned.len(),
        directory_retained,
    })
}

fn owned_entries(dir: &Path) -> Result<Vec<PathBuf>, PurgeError> {
    const OWNED_FILES: &[&str] = &[
        "events.jsonl",
        "policy.jsonl",
        "key",
        "config_warned",
        EVENT_RETENTION_FILE,
        POLICY_RETENTION_FILE,
    ];
    let mut paths = Vec::new();
    for name in OWNED_FILES {
        let path = dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => paths.push(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(PurgeError::Io(error)),
        }
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_drops_only_expired_and_unusable_records_from_both_logs() {
        // Probed 2026-08-07 through `append_jsonl` after implementing the M5
        // path: each log contained one record just outside the 30-day window,
        // one exactly on its boundary, one recent record, and a torn record.
        // The concrete risks are retaining data past the promised cutoff,
        // deleting the boundary counterfactual, or letting one torn write make
        // compaction discard neighboring valid state.
        for (log_name, marker_name) in [
            ("events.jsonl", EVENT_RETENTION_FILE),
            ("policy.jsonl", POLICY_RETENTION_FILE),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "oopsinput-retention-{log_name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let now = RETENTION_MS + 10_000;
            let cutoff = now - RETENTION_MS;
            std::fs::write(
                dir.join(log_name),
                format!(
                    "{{\"ts_ms\":{},\"tag\":\"expired\"}}\n\
                     {{\"ts_ms\":{cutoff},\"tag\":\"boundary\"}}\n\
                     {{\"ts_ms\":{},\"tag\":\"recent\"}}\n\
                     {{\"ts_ms\":",
                    cutoff - 1,
                    now - 1,
                ),
            )
            .unwrap();
            std::fs::set_permissions(dir.join(log_name), std::fs::Permissions::from_mode(0o600))
                .unwrap();

            let appended = format!("{{\"ts_ms\":{now},\"tag\":\"appended\"}}\n");
            append_jsonl(&dir, log_name, appended.as_bytes(), now).unwrap();

            let retained = std::fs::read_to_string(dir.join(log_name)).unwrap();
            assert!(!retained.contains("expired"), "{retained}");
            assert!(retained.contains("boundary"), "{retained}");
            assert!(retained.contains("recent"), "{retained}");
            assert!(retained.contains("appended"), "{retained}");
            assert_eq!(
                retained.lines().count(),
                3,
                "torn line survived: {retained}"
            );
            assert!(dir.join(marker_name).is_file());
            assert_eq!(
                std::fs::metadata(dir.join(log_name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            for private in [marker_name, LOCK_FILE] {
                assert_eq!(
                    std::fs::metadata(dir.join(private))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600,
                    "{private} permissions"
                );
            }
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn append_refuses_a_non_regular_log_before_opening_it() {
        // Failure-hunt 2026-08-07: the first retention implementation checked
        // file type only after `open`. A FIFO can block during `open`, before
        // that check, so fail-open requires rejecting every non-regular inode
        // from metadata first. A Unix socket exercises the same type boundary
        // without introducing an external test helper.
        let dir = std::env::temp_dir().join(format!("oopsinput-nonregular-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(dir.join("events.jsonl")).unwrap();

        let result = append_jsonl(&dir, "events.jsonl", b"{\"ts_ms\":1}\n", 1);
        assert!(result.is_err(), "non-regular log was accepted");
        assert!(!dir.join(EVENT_RETENTION_FILE).exists());

        drop(socket);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_busy_state_lock_drops_evidence_quickly_instead_of_holding_the_command() {
        // Failure-hunt 2026-08-07: the analysis watchdog retires while a user
        // prompt is active. A blocking log lock after the answer could then
        // hold the shell indefinitely and prevent an edit/cancel/run choice
        // from taking effect. State is evidence, so contention must lose the
        // record rather than the user's control.
        let dir = std::env::temp_dir().join(format!("oopsinput-busy-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let held = StateLock::acquire(&dir).unwrap();

        let started = Instant::now();
        let result = append_jsonl_after_prompt(&dir, "events.jsonl", b"{\"ts_ms\":1}\n", 1);
        let elapsed = started.elapsed();
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            elapsed < Duration::from_millis(100),
            "busy lock held the write for {elapsed:?}"
        );
        assert!(!dir.join("events.jsonl").exists());

        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
