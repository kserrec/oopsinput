//! M5 report command through the shipped binary: dispatch, state-directory
//! resolution, and the human-readable output surface.

use std::process::Command;

#[test]
fn report_command_reads_the_selected_state_directory() {
    // Probed 2026-08-07 with the real binary before pinning this: the report
    // module can be correct while a missing dispatch arm or ignored state-dir
    // override still makes the user-facing command useless.
    let dir = std::env::temp_dir().join(format!("oopsinput-report-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("events.jsonl"),
        "{\"decision\":\"allow\",\"reason_code\":\"shadow.observed\",\"evidence\":[],\"duration_us\":42}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("report")
        .env("OOPSINPUT_STATE_DIR", &dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("oopsinput report\n  events: 1"), "{stdout}");
    assert!(
        stdout.contains("deterministic (n=1): p50 42 us"),
        "{stdout}"
    );
    assert!(output.stderr.is_empty(), "{output:?}");

    let help = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("help")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("report   summarize recorded decisions"),
        "report command missing from help"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_home_never_resolves_to_a_relative_state_directory() {
    // Regression (M5 bughunt 2026-08-08): HOME="" became the relative path
    // `.local/state/oopsinput`, so state could be read from or written into
    // whichever project happened to be the current directory.
    let base = std::env::temp_dir().join(format!(
        "oopsinput-report-empty-home-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("report")
        .current_dir(&base)
        .env("HOME", "")
        .env_remove("OOPSINPUT_STATE_DIR")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no absolute state directory"),
        "{output:?}"
    );
    assert!(!base.join(".local").exists(), "relative state was created");

    let _ = std::fs::remove_dir_all(&base);
}
