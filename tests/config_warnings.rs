//! Config-warning delivery through concurrent real processes.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

fn check_command(base: &Path, state_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oopsinput"));
    command
        .args(["check", "--res", "command"])
        .env("HOME", base.join("home"))
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("OOPSINPUT_STATE_DIR", state_dir)
        .env_remove("OOPSINPUT_MODE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

#[test]
fn concurrent_shells_emit_once_and_a_changed_warning_set_rearms_delivery() {
    // Regression (M5 bughunt 2026-08-08): every process compared the marker
    // before acquiring the shared lock. Holding that lock let all processes
    // observe a missing marker and print before any could record it. The
    // comparison, display, and commit must now be one locked transaction.
    let base = std::env::temp_dir().join(format!(
        "oopsinput-config-warning-race-{}",
        std::process::id()
    ));
    let config_dir = base.join("config/oopsinput");
    let state_dir = base.join("state");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        config_dir.join("config"),
        "det_timeout_ms = 5000\nunknown_key = ignored\n",
    )
    .unwrap();

    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(state_dir.join(".oopsinput.lock"))
        .unwrap();
    lock.lock().unwrap();

    let mut children = Vec::new();
    for _ in 0..16 {
        children.push(check_command(&base, &state_dir).spawn().unwrap());
    }
    std::thread::sleep(Duration::from_millis(100));
    drop(lock);

    let mut warning_count = 0;
    for child in children {
        let output = child.wait_with_output().unwrap();
        warning_count += String::from_utf8_lossy(&output.stderr)
            .matches("oopsinput: config:")
            .count();
    }
    assert_eq!(warning_count, 1, "duplicate warning sets were displayed");
    assert!(state_dir.join("config_warned").is_file());
    let first_marker = std::fs::read(state_dir.join("config_warned")).unwrap();

    let repeated = check_command(&base, &state_dir).output().unwrap();
    assert!(repeated.status.success(), "{repeated:?}");
    assert_eq!(
        String::from_utf8_lossy(&repeated.stderr)
            .matches("oopsinput: config:")
            .count(),
        0,
        "an unchanged warning set was shown again"
    );

    std::fs::write(config_dir.join("config"), "mode = invalid\n").unwrap();
    let changed = check_command(&base, &state_dir).output().unwrap();
    assert!(changed.status.success(), "{changed:?}");
    let changed_stderr = String::from_utf8_lossy(&changed.stderr);
    assert_eq!(
        changed_stderr.matches("oopsinput: config:").count(),
        1,
        "a changed warning set did not re-arm exactly once: {changed_stderr}"
    );
    assert!(changed_stderr.contains("invalid mode"), "{changed_stderr}");
    assert_ne!(
        std::fs::read(state_dir.join("config_warned")).unwrap(),
        first_marker,
        "the shown marker did not advance to the changed warning set"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn unavailable_terminal_does_not_mark_a_config_warning_as_shown() {
    // Regression companion (M5 audit 2026-08-08): the old plugin discarded
    // stderr while policy still committed `config_warned`. A failed display
    // must leave the marker absent so a later command can retry.
    let base = std::env::temp_dir().join(format!(
        "oopsinput-config-warning-hidden-{}",
        std::process::id()
    ));
    let config_dir = base.join("config/oopsinput");
    let state_dir = base.join("state");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config"), "unknown_key = ignored\n").unwrap();

    let output = check_command(&base, &state_dir)
        .env("OOPSINPUT_DIAGNOSTICS_TTY", "1")
        .env("OOPSINPUT_TEST_NO_TTY", "1")
        .stderr(Stdio::null())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(!state_dir.join("config_warned").exists());

    let _ = std::fs::remove_dir_all(&base);
}
