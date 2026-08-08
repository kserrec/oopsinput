//! M5 state-directory resolution through the shipped binary.

use std::io::Write;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn base(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("oopsinput-state-path-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn run_check(dir: &Path, configure: impl FnOnce(&mut Command)) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oopsinput"));
    command
        .args(["check", "--res", "command"])
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env_remove("OOPSINPUT_MODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure(&mut command);

    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"echo state-path-probe")
        .unwrap();
    child.wait_with_output().unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn relative_explicit_override_disables_state_without_claiming_the_working_directory() {
    // Reproduced before the M5 audit fix (2026-08-08): with
    // OOPSINPUT_STATE_DIR=., one ordinary check changed this directory from
    // 0755 to 0700 and created the lock, retention marker, and event log here.
    let dir = base("override");
    let output = run_check(&dir, |command| {
        command
            .env("OOPSINPUT_STATE_DIR", ".")
            .env("XDG_STATE_HOME", dir.join("xdg-state"))
            .env("HOME", dir.join("home"));
    });

    assert!(output.status.success(), "{output:?}");
    assert_eq!(mode(&dir), 0o755, "check claimed the working directory");
    assert!(!dir.join(".oopsinput.lock").exists());
    assert!(!dir.join("events.jsonl").exists());
    assert!(!dir.join("home/.local/state/oopsinput").exists());
    assert!(!dir.join("xdg-state/oopsinput").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn relative_xdg_state_home_falls_back_to_absolute_home() {
    let dir = base("xdg");
    let output = run_check(&dir, |command| {
        command
            .env_remove("OOPSINPUT_STATE_DIR")
            .env("XDG_STATE_HOME", "relative-state")
            .env("HOME", dir.join("home"));
    });

    assert!(output.status.success(), "{output:?}");
    assert_eq!(mode(&dir), 0o755);
    assert!(!dir.join("relative-state/oopsinput").exists());
    assert!(
        dir.join("home/.local/state/oopsinput/events.jsonl")
            .is_file()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn relative_home_disables_state_without_creating_a_working_directory_path() {
    let dir = base("home");
    let output = run_check(&dir, |command| {
        command
            .env_remove("OOPSINPUT_STATE_DIR")
            .env_remove("XDG_STATE_HOME")
            .env("HOME", "relative-home");
    });

    assert!(output.status.success(), "{output:?}");
    assert_eq!(mode(&dir), 0o755);
    assert!(!dir.join("relative-home").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_fifo_log_is_rejected_before_retention_can_block_opening_it() {
    // The old socket fixture could pass even without the pre-open file-type
    // check because opening a Unix socket as a file already fails quickly.
    // A FIFO is the real failure mode: a read-only open blocks until a writer
    // appears. The process watchdog makes this regression fail instead of
    // hanging the test suite.
    let dir = base("fifo");
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let fifo = state.join("events.jsonl");
    let made = Command::new("/usr/bin/mkfifo")
        .arg(&fifo)
        .status()
        .expect("run /usr/bin/mkfifo");
    assert!(made.success(), "mkfifo failed");
    assert!(
        std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo()
    );

    let started = Instant::now();
    let output = run_check(&dir, |command| {
        command
            .env("OOPSINPUT_STATE_DIR", &state)
            .env("HOME", dir.join("home"))
            .env("OOPSINPUT_TEST_DEADLINE_MS", "200");
    });
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "FIFO open reached the watchdog instead of failing open: {output:?}"
    );
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert!(!state.join(".events-retention").exists());
    assert!(
        std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn analysis_time_lock_contention_is_bounded_by_the_process_watchdog() {
    // M5 deliberately lets ordinary evidence writes wait for the shared lock,
    // relying on the existing process watchdog to preserve fail-open latency.
    // Exercise that exact wiring through the binary rather than testing the
    // lock and watchdog in isolation.
    let dir = base("analysis-lock");
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(state.join(".oopsinput.lock"))
        .unwrap();
    lock.lock().unwrap();

    let started = Instant::now();
    let output = run_check(&dir, |command| {
        command
            .env("OOPSINPUT_STATE_DIR", &state)
            .env("HOME", dir.join("home"))
            .env("OOPSINPUT_TEST_DEADLINE_MS", "100");
    });
    let elapsed = started.elapsed();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(elapsed >= Duration::from_millis(50), "{elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(1),
        "state lock outlived the fail-open watchdog: {elapsed:?}"
    );
    assert!(!state.join("events.jsonl").exists());

    drop(lock);
    let _ = std::fs::remove_dir_all(&dir);
}
