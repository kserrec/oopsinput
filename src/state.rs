//! User-local state coordination, retention, and explicit deletion.
//!
//! Every successful mutating path takes the same advisory file lock. Analysis-
//! time writes are bounded by the process watchdog; writes after a prompt wait
//! at most 25 ms and append only, deferring retention so neither contention nor
//! a large sweep can override the user's choice. The lock anchor is stable
//! across atomic log replacement, so pruning cannot race an append from
//! another shell and silently lose its record. `purge` unlinks the anchor only
//! while holding it; a waiter verifies the inode after acquiring the lock and
//! retries on the new anchor instead of writing under a stale lock.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const LOCK_FILE: &str = ".oopsinput.lock";
const EVENT_RETENTION_FILE: &str = ".events-retention";
const POLICY_RETENTION_FILE: &str = ".policy-retention";
const TEMP_PREFIX: &str = ".oopsinput-tmp-";
const OWNED_FILES: &[&str] = &[
    "events.jsonl",
    "policy.jsonl",
    "key",
    "config_warned",
    EVENT_RETENTION_FILE,
    POLICY_RETENTION_FILE,
];
const RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const PRUNE_INTERVAL_MS: u64 = 24 * 60 * 60 * 1_000;
const POST_PROMPT_LOCK_TIMEOUT: Duration = Duration::from_millis(25);
/// Structural M5 records are well under 1 KiB. This cap is intentionally
/// generous while preventing a corrupted file from making one line allocate
/// without bound during report or retention.
pub(crate) const JSONL_RECORD_CAP: usize = 64 * 1024;

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// State dir: absolute $OOPSINPUT_STATE_DIR override (tests, custom setups)
/// else absolute $XDG_STATE_HOME/oopsinput else absolute
/// ~/.local/state/oopsinput. An explicit relative override disables state;
/// a relative XDG root is invalid and falls back to HOME.
pub(crate) fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OOPSINPUT_STATE_DIR")
        && !dir.is_empty()
    {
        let dir = PathBuf::from(dir);
        return dir.is_absolute().then_some(dir);
    }
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME")
        && !xdg.is_empty()
    {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg.join("oopsinput"));
        }
    }
    let home = std::env::var_os("HOME")?;
    let home = PathBuf::from(home);
    home.is_absolute()
        .then(|| home.join(".local/state/oopsinput"))
}

#[derive(Debug)]
pub(crate) struct StateInspection {
    pub dir: Option<PathBuf>,
    pub present: bool,
    pub checked_files: usize,
    pub issues: Vec<StateIssue>,
}

#[derive(Debug)]
pub(crate) enum StateIssue {
    DirectoryUnavailable,
    DirectoryNotReal,
    DirectoryUnreadable,
    DirectoryMode(u32),
    EntryUnavailable(&'static str),
    EntryNotRegular(&'static str),
    EntryMode(&'static str, u32),
}

/// Read-only setup inspection for `doctor`. It neither creates the state
/// directory nor repairs modes: diagnostics must describe the current state,
/// while the normal write path remains the sole owner of creation and repair.
pub(crate) fn inspect_state() -> StateInspection {
    let Some(dir) = state_dir() else {
        return StateInspection {
            dir: None,
            present: false,
            checked_files: 0,
            issues: vec![StateIssue::DirectoryUnavailable],
        };
    };
    let mut inspection = StateInspection {
        dir: Some(dir.clone()),
        present: false,
        checked_files: 0,
        issues: Vec::new(),
    };
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return inspection,
        Err(_) => {
            inspection.issues.push(StateIssue::DirectoryUnavailable);
            return inspection;
        }
    };
    inspection.present = true;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        inspection.issues.push(StateIssue::DirectoryNotReal);
        return inspection;
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o700 {
        inspection.issues.push(StateIssue::DirectoryMode(mode));
    }

    inspect_state_entry(&dir.join(LOCK_FILE), LOCK_FILE, &mut inspection);
    for name in OWNED_FILES {
        inspect_state_entry(&dir.join(name), name, &mut inspection);
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => {
            inspection.issues.push(StateIssue::DirectoryUnreadable);
            return inspection;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                inspection.issues.push(StateIssue::DirectoryUnreadable);
                break;
            }
        };
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            inspect_state_entry(&entry.path(), "temporary state file", &mut inspection);
        }
    }
    inspection
}

fn inspect_state_entry(path: &Path, label: &'static str, inspection: &mut StateInspection) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            inspection.checked_files += 1;
            if meta.file_type().is_symlink() || !meta.is_file() {
                inspection.issues.push(StateIssue::EntryNotRegular(label));
                return;
            }
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                inspection.issues.push(StateIssue::EntryMode(label, mode));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => inspection.issues.push(StateIssue::EntryUnavailable(label)),
    }
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
/// choice even if another shell is holding the state lock for compaction, and
/// never start a retention sweep after the watchdog has retired.
pub(crate) fn append_jsonl_after_prompt(
    dir: &Path,
    log_name: &'static str,
    line: &[u8],
) -> std::io::Result<()> {
    let _lock = StateLock::acquire_after_prompt(dir)?;
    append_private_file(&dir.join(log_name), line)
}

/// Atomically replace one small, product-owned state file under the shared
/// lock. This never truncates or writes through a symlink.
#[cfg(test)]
pub(crate) fn replace_small_file(
    dir: &Path,
    name: &'static str,
    bytes: &[u8],
) -> std::io::Result<()> {
    match begin_small_file_update(dir, name, bytes)? {
        Some(update) => update.commit(),
        None => Ok(()),
    }
}

/// A small-file update whose comparison and eventual replacement share the
/// state lock. The caller may perform one small side effect (the config
/// warning display) before `commit`; concurrent processes cannot all pass the
/// comparison while the old marker is still present.
pub(crate) struct SmallFileUpdate {
    _lock: StateLock,
    dir: PathBuf,
    name: &'static str,
    bytes: Vec<u8>,
}

impl SmallFileUpdate {
    pub(crate) fn commit(self) -> std::io::Result<()> {
        replace_private_file(&self.dir, self.name, &self.bytes)
    }
}

pub(crate) fn begin_small_file_update(
    dir: &Path,
    name: &'static str,
    bytes: &[u8],
) -> std::io::Result<Option<SmallFileUpdate>> {
    let lock = StateLock::acquire(dir)?;
    if small_file_matches(&dir.join(name), bytes)? {
        return Ok(None);
    }
    Ok(Some(SmallFileUpdate {
        _lock: lock,
        dir: dir.to_path_buf(),
        name,
        bytes: bytes.to_vec(),
    }))
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

    fn acquire_after_prompt(dir: &Path) -> std::io::Result<Self> {
        Self::acquire_until(dir, Some(Instant::now() + POST_PROMPT_LOCK_TIMEOUT))
    }

    fn acquire_until(dir: &Path, deadline: Option<Instant>) -> std::io::Result<Self> {
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(state_lock_timeout());
            }
            match ensure_state_dir(dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            let path = dir.join(LOCK_FILE);
            reject_non_regular(&path, "state lock")?;
            let file = match open_lock_anchor(&path) {
                Ok(file) => file,
                // `purge` can remove the directory between validation and
                // open. Recreate and retry rather than turning that benign
                // race into a permanent logging failure.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                // Another first writer atomically created the anchor between
                // our existing-file open and `create_new`; join it next loop.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let opened = match opened_regular_file_metadata(&path, &file, "state lock") {
                Ok(opened) => opened,
                // Purge can unlink the anchor in the gap after open. Join the
                // replacement anchor rather than dropping this event.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if opened.permissions().mode() & 0o077 != 0 {
                return Err(std::io::Error::other(
                    "state lock is not a user-only regular file",
                ));
            }
            lock_file(&file, deadline)?;

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

fn lock_file(file: &File, deadline: Option<Instant>) -> std::io::Result<()> {
    let Some(deadline) = deadline else {
        return file.lock();
    };
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_micros(250));
            }
            Err(std::fs::TryLockError::WouldBlock) => return Err(state_lock_timeout()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

fn state_lock_timeout() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "state lock stayed busy past the write deadline",
    )
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Verify that an opened regular file is still the nonsymlink path inspected
/// by the caller. This closes every race after `open`; the remaining pre-open
/// FIFO swap requires a process already able to mutate the private directory.
pub(crate) fn opened_regular_file_metadata(
    path: &Path,
    file: &File,
    label: &str,
) -> std::io::Result<std::fs::Metadata> {
    let opened = file.metadata()?;
    let current = std::fs::symlink_metadata(path)?;
    if !opened.is_file() || current.file_type().is_symlink() || !same_file(&opened, &current) {
        return Err(std::io::Error::other(format!(
            "{label} changed while it was opened"
        )));
    }
    Ok(opened)
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

/// Open the stable lock anchor without ever combining create + follow. The
/// existing-file branch is verified immediately after open; the creation
/// branch uses `create_new`, whose atomic existence check refuses symlinks.
fn open_lock_anchor(path: &Path) -> std::io::Result<File> {
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path),
        Err(error) => Err(error),
    }
}

fn open_append_file(path: &Path) -> std::io::Result<File> {
    match OpenOptions::new().read(true).append(true).open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(path),
        Err(error) => Err(error),
    }
}

fn append_private_file(path: &Path, line: &[u8]) -> std::io::Result<()> {
    reject_non_regular(path, "JSONL state path")?;
    let mut file = open_append_file(path)?;
    let meta = opened_regular_file_metadata(path, &file, "JSONL state path")?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::other(
            "JSONL state path is not a user-only regular file",
        ));
    }
    // A process can be killed midway through its one-line append. Retention
    // normally removes that torn tail, but post-prompt writes deliberately
    // defer retention; add a separator so the new record is still parseable.
    if meta.len() > 0 {
        file.seek(std::io::SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
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
    let mut file = match File::open(marker) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    let meta = opened_regular_file_metadata(marker, &file, "retention marker")?;
    if meta.len() > 32 {
        return Ok(true);
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let last = match text.trim().parse::<u64>() {
        Ok(last) => last,
        Err(_) => return Ok(true),
    };
    // Small wall-clock corrections must not turn every command into a
    // full-log compaction until the clock catches up. A marker more than
    // one sweep interval in the future is implausible, so rebase it once.
    Ok(now_ms.abs_diff(last) >= PRUNE_INTERVAL_MS)
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
    let source_meta = opened_regular_file_metadata(path, &source, "JSONL state path")?;
    if source_meta.permissions().mode() & 0o077 != 0 {
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
        let had_newline = match read_jsonl_record(&mut reader, &mut line)? {
            JsonlRead::Eof => break,
            JsonlRead::Oversized => {
                changed = true;
                continue;
            }
            JsonlRead::Record { had_newline } => had_newline,
        };
        match serde_json::from_slice::<Timestamped>(&line) {
            Ok(stored) if stored.ts_ms >= cutoff_ms => {
                temp.write_all(&line)?;
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JsonlRead {
    Eof,
    Record { had_newline: bool },
    Oversized,
}

/// Read one JSONL record without retaining its trailing newline. Oversized
/// records are drained through the delimiter and reported once, while `line`
/// never grows beyond `JSONL_RECORD_CAP`.
pub(crate) fn read_jsonl_record(
    reader: &mut dyn BufRead,
    line: &mut Vec<u8>,
) -> std::io::Result<JsonlRead> {
    line.clear();
    let mut saw_input = false;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if !saw_input {
                Ok(JsonlRead::Eof)
            } else if oversized {
                Ok(JsonlRead::Oversized)
            } else {
                Ok(JsonlRead::Record { had_newline: false })
            };
        }
        saw_input = true;

        let newline = available.iter().position(|byte| *byte == b'\n');
        let record_bytes = newline.unwrap_or(available.len());
        if !oversized {
            if record_bytes > JSONL_RECORD_CAP.saturating_sub(line.len()) {
                line.clear();
                oversized = true;
            } else {
                line.extend_from_slice(&available[..record_bytes]);
            }
        }
        let consumed = record_bytes + usize::from(newline.is_some());
        reader.consume(consumed);

        if newline.is_some() {
            return if oversized {
                Ok(JsonlRead::Oversized)
            } else {
                Ok(JsonlRead::Record { had_newline: true })
            };
        }
    }
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

fn small_file_matches(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    reject_non_regular(path, "state path")?;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let opened = opened_regular_file_metadata(path, &file, "state path")?;
    if opened.permissions().mode() & 0o077 != 0 || opened.len() != bytes.len() as u64 {
        return Ok(false);
    }
    let mut current_bytes = Vec::with_capacity(bytes.len());
    file.read_to_end(&mut current_bytes)?;
    Ok(current_bytes == bytes)
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
                write!(
                    f,
                    "no absolute state directory can be resolved from the environment"
                )
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
    validate_owned_entries(&owned_entries(dir)?)?;

    prepare_lock_anchor_for_purge(dir)?;
    let lock = StateLock::acquire(dir)?;
    let owned = owned_entries(dir)?;
    // Repeat under the lock to close races with another oopsinput process.
    validate_owned_entries(&owned)?;

    for path in &owned {
        std::fs::remove_file(path)?;
    }
    lock.unlink_anchor()?;
    drop(lock);

    let directory_retained = remove_state_dir_if_empty(dir)?;
    Ok(PurgeResult {
        removed_files: owned.len(),
        directory_retained,
    })
}

/// Recover the one product-owned path that must be usable before purge can
/// take its coordination lock. A symbolic link is unlinked, never followed;
/// a regular anchor has its required private mode restored. Other inode types
/// remain outside our ownership boundary and make purge refuse recursively.
fn prepare_lock_anchor_for_purge(dir: &Path) -> Result<(), PurgeError> {
    let path = dir.join(LOCK_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(path)?;
        }
        Ok(meta) if meta.is_file() => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(_) => return Err(PurgeError::UnsafeOwnedEntry),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(PurgeError::Io(error)),
    }
    Ok(())
}

fn validate_owned_entries(paths: &[PathBuf]) -> Result<(), PurgeError> {
    for path in paths {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_symlink() && !meta.is_file() {
            return Err(PurgeError::UnsafeOwnedEntry);
        }
    }
    Ok(())
}

fn remove_state_dir_if_empty(dir: &Path) -> Result<bool, PurgeError> {
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
    Ok(directory_retained)
}

fn owned_entries(dir: &Path) -> Result<Vec<PathBuf>, PurgeError> {
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
    fn atomic_create_fallback_never_follows_a_dangling_symlink() {
        // M5 audit hardening (2026-08-08): this is the exact state after a
        // path changes between the pre-open inspection and creation. Both
        // create paths must refuse the link without creating its target.
        let dir =
            std::env::temp_dir().join(format!("oopsinput-create-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let append_target = dir.join("append-target");
        let append_link = dir.join("events.jsonl");
        std::os::unix::fs::symlink(&append_target, &append_link).unwrap();
        assert!(open_append_file(&append_link).is_err());
        assert!(!append_target.exists());

        let lock_target = dir.join("lock-target");
        let lock_link = dir.join(LOCK_FILE);
        std::os::unix::fs::symlink(&lock_target, &lock_link).unwrap();
        assert!(open_lock_anchor(&lock_link).is_err());
        assert!(!lock_target.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opened_file_identity_check_rejects_a_replaced_path() {
        let dir =
            std::env::temp_dir().join(format!("oopsinput-open-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        std::fs::write(&path, "first").unwrap();
        let opened = File::open(&path).unwrap();
        std::fs::rename(&path, dir.join("old-events.jsonl")).unwrap();
        std::fs::write(&path, "replacement").unwrap();

        assert!(opened_regular_file_metadata(&path, &opened, "event log").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_jsonl_record_is_drained_with_bounded_memory_and_next_record_survives() {
        // M5 audit hardening (2026-08-08): BufRead::read_until previously let
        // one corrupted line allocate without bound. Drain it without keeping
        // more than the record cap, then resume exactly at the next record.
        let mut bytes = vec![b'x'; JSONL_RECORD_CAP + 1];
        bytes.extend_from_slice(b"\n{\"ts_ms\":7}\n");
        let mut reader = std::io::Cursor::new(bytes);
        let mut line = Vec::new();

        assert_eq!(
            read_jsonl_record(&mut reader, &mut line).unwrap(),
            JsonlRead::Oversized
        );
        assert!(line.len() <= JSONL_RECORD_CAP);
        assert_eq!(
            read_jsonl_record(&mut reader, &mut line).unwrap(),
            JsonlRead::Record { had_newline: true }
        );
        assert_eq!(line, br#"{"ts_ms":7}"#);
        assert_eq!(
            read_jsonl_record(&mut reader, &mut line).unwrap(),
            JsonlRead::Eof
        );
    }

    #[test]
    fn retention_drops_only_expired_and_unusable_records_from_both_logs() {
        // Probed 2026-08-07 through `append_jsonl` after implementing the M5
        // path: each log contained one record just outside the 30-day window,
        // one exactly on its boundary, one recent record, one valid JSON
        // record over the cap, and a torn record. The concrete risks are
        // retaining data past the promised cutoff, deleting the boundary
        // counterfactual, accepting a structurally valid oversized record, or
        // letting one unusable record discard neighboring valid state.
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
            let mut stored = format!(
                "{{\"ts_ms\":{},\"tag\":\"expired\"}}\n\
                 {{\"ts_ms\":{cutoff},\"tag\":\"boundary\"}}\n",
                cutoff - 1,
            )
            .into_bytes();
            let mut oversized = format!(
                "{{\"ts_ms\":{},\"tag\":\"oversized\",\"padding\":\"",
                now - 1
            )
            .into_bytes();
            oversized.resize(JSONL_RECORD_CAP, b'x');
            oversized.extend_from_slice(b"\"}\n");
            stored.extend_from_slice(&oversized);
            stored.extend_from_slice(
                format!("{{\"ts_ms\":{},\"tag\":\"recent\"}}\n{{\"ts_ms\":", now - 1,).as_bytes(),
            );
            std::fs::write(dir.join(log_name), stored).unwrap();
            std::fs::set_permissions(dir.join(log_name), std::fs::Permissions::from_mode(0o600))
                .unwrap();

            let appended = format!("{{\"ts_ms\":{now},\"tag\":\"appended\"}}\n");
            append_jsonl(&dir, log_name, appended.as_bytes(), now).unwrap();

            let retained = std::fs::read_to_string(dir.join(log_name)).unwrap();
            assert!(!retained.contains("expired"), "{retained}");
            assert!(!retained.contains("oversized"), "{retained}");
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
    fn retention_sweeps_at_most_once_per_day() {
        // The sweep exists on the command path. Ignoring its marker would scan
        // a large long-lived log on every Enter; never revisiting it would
        // break the 30-day promise. An expired record inserted after a sweep
        // must survive inside the interval and disappear at the next one.
        let dir =
            std::env::temp_dir().join(format!("oopsinput-retention-rate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = RETENTION_MS + 10_000;
        std::fs::write(
            dir.join("events.jsonl"),
            format!(
                "{{\"ts_ms\":1,\"tag\":\"expired\"}}\n\
                 {{\"ts_ms\":{now},\"tag\":\"recent\"}}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join(EVENT_RETENTION_FILE), now.to_string()).unwrap();
        for path in [dir.join("events.jsonl"), dir.join(EVENT_RETENTION_FILE)] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let inside_interval = now + 1;
        let appended = format!("{{\"ts_ms\":{inside_interval},\"tag\":\"inside_interval\"}}\n");
        append_jsonl(&dir, "events.jsonl", appended.as_bytes(), inside_interval).unwrap();
        let deferred = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert!(deferred.contains("expired"), "swept too early: {deferred}");

        let next_sweep = now + PRUNE_INTERVAL_MS;
        let appended = format!("{{\"ts_ms\":{next_sweep},\"tag\":\"next_sweep\"}}\n");
        append_jsonl(&dir, "events.jsonl", appended.as_bytes(), next_sweep).unwrap();
        let retained = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert!(
            !retained.contains("expired"),
            "sweep never resumed: {retained}"
        );
        assert!(retained.contains("recent"), "{retained}");
        assert!(retained.contains("inside_interval"), "{retained}");
        assert!(retained.contains("next_sweep"), "{retained}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_waiter_rejoins_the_replacement_anchor_after_purge_unlinks_the_old_lock() {
        // The stable anchor exists so a purge cannot split concurrent writers
        // between a deleted locked inode and its replacement. Observe the
        // waiter's open descriptors directly: it first waits on the old inode,
        // then must close it and wait on the replacement before returning.
        let dir =
            std::env::temp_dir().join(format!("oopsinput-lock-rejoin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let held = StateLock::acquire(&dir).unwrap();
        let old_meta = held.file.metadata().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let waiter_dir = dir.clone();
        let waiter = std::thread::spawn(move || {
            let acquired = StateLock::acquire(&waiter_dir).unwrap();
            tx.send(acquired.file.metadata().unwrap()).unwrap();
        });

        fn open_handle_count(wanted: &std::fs::Metadata) -> usize {
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::metadata(entry.path()).ok())
                .filter(|meta| same_file(meta, wanted))
                .count()
        }

        fn wait_for_second_handle(wanted: &std::fs::Metadata) {
            let deadline = Instant::now() + Duration::from_secs(1);
            while open_handle_count(wanted) < 2 {
                assert!(
                    Instant::now() < deadline,
                    "waiter never opened the expected lock inode"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        wait_for_second_handle(&old_meta);
        held.unlink_anchor().unwrap();
        let replacement = StateLock::acquire(&dir).unwrap();
        let replacement_meta = replacement.file.metadata().unwrap();
        assert!(!same_file(&old_meta, &replacement_meta));

        drop(held);
        wait_for_second_handle(&replacement_meta);
        assert!(
            matches!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
            "waiter returned under the stale lock instead of joining the replacement"
        );

        drop(replacement);
        let acquired_meta = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let current_meta = std::fs::symlink_metadata(dir.join(LOCK_FILE)).unwrap();
        assert!(same_file(&acquired_meta, &current_meta));
        waiter.join().unwrap();
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
        let result = append_jsonl_after_prompt(&dir, "events.jsonl", b"{\"ts_ms\":1}\n");
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

    #[test]
    fn post_prompt_append_defers_retention_and_preserves_its_own_record() {
        // Regression (M5 bughunt 2026-08-08): after the watchdog retired, a
        // due retention sweep scanned a 128 MiB log and held the user's shell
        // for more than a second after they had already answered. This path
        // may append only; the next ordinary write performs the sweep.
        let dir = std::env::temp_dir().join(format!(
            "oopsinput-post-prompt-retention-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = RETENTION_MS + 100;
        std::fs::write(
            dir.join("events.jsonl"),
            "{\"ts_ms\":1,\"tag\":\"expired\"}\n{\"ts_ms\":",
        )
        .unwrap();
        std::fs::set_permissions(
            dir.join("events.jsonl"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        let after_prompt = format!("{{\"ts_ms\":{now},\"tag\":\"after_prompt\"}}\n");
        append_jsonl_after_prompt(&dir, "events.jsonl", after_prompt.as_bytes()).unwrap();

        let deferred = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert!(
            deferred.contains("expired"),
            "retention ran after prompt: {deferred}"
        );
        assert!(
            deferred.contains("{\"ts_ms\":\n"),
            "torn tail was rewritten: {deferred}"
        );
        assert!(deferred.lines().last().unwrap().contains("after_prompt"));
        assert!(!dir.join(EVENT_RETENTION_FILE).exists());

        let ordinary = format!("{{\"ts_ms\":{},\"tag\":\"ordinary\"}}\n", now + 1);
        append_jsonl(&dir, "events.jsonl", ordinary.as_bytes(), now + 1).unwrap();
        let retained = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert!(!retained.contains("expired"), "{retained}");
        assert!(!retained.contains("{\"ts_ms\":\n"), "{retained}");
        assert!(retained.contains("after_prompt"), "{retained}");
        assert!(retained.contains("ordinary"), "{retained}");
        assert_eq!(retained.lines().count(), 2, "{retained}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_file_comparison_and_commit_are_one_locked_transaction() {
        // Regression (M5 bughunt 2026-08-08): config-warning processes used
        // to compare the marker before taking the state lock, so two could
        // both decide to print. Hold the same lock from comparison through
        // commit; exactly one concurrent caller gets the side-effect token.
        let dir = std::env::temp_dir().join(format!(
            "oopsinput-small-update-race-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let emissions = std::sync::atomic::AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..16 {
                let dir = dir.clone();
                let barrier = barrier.clone();
                let emissions = &emissions;
                scope.spawn(move || {
                    barrier.wait();
                    if let Some(update) =
                        begin_small_file_update(&dir, "config_warned", b"fingerprint").unwrap()
                    {
                        emissions.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(2));
                        update.commit().unwrap();
                    }
                });
            }
        });

        assert_eq!(emissions.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(dir.join("config_warned")).unwrap(),
            b"fingerprint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
