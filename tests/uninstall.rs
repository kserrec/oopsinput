//! Pinned tests for zsh/uninstall.zsh: a damaged marker block must cause
//! refusal, and uninstall removes only the runtime artifacts it owns.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("zsh/uninstall.zsh")
}

fn install_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("zsh/install.zsh")
}

fn uninstall(home: &Path) -> Output {
    Command::new("zsh")
        .arg(script())
        .env("LC_ALL", "C")
        .env("HOME", home)
        .output()
        .expect("run uninstall.zsh")
}

/// Run uninstall.zsh against a fake HOME containing the given .zshrc.
fn run_uninstall(zshrc: &str) -> (bool, String) {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let home = std::env::temp_dir().join(format!("oopsinput-uninst-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".zshrc"), zshrc).unwrap();

    let out = uninstall(&home);

    let result = std::fs::read_to_string(home.join(".zshrc")).unwrap();
    cleanup(&home);
    (out.status.success(), result)
}

fn cleanup(home: &Path) {
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn healthy_block_is_removed_exactly() {
    let (ok, result) = run_uninstall(
        "before-1\n# >>> oopsinput >>>\nsource plugin\n# <<< oopsinput <<<\nafter-1\nafter-2\n",
    );
    assert!(ok, "uninstall should succeed on a healthy block");
    assert_eq!(result, "before-1\nafter-1\nafter-2\n");
}

#[test]
fn missing_end_marker_refuses_and_leaves_file_untouched() {
    let original = "before-1\n# >>> oopsinput >>>\nsource plugin\nafter-1\nafter-2\n";
    let (ok, result) = run_uninstall(original);
    assert!(!ok, "uninstall must refuse when the end marker is missing");
    assert_eq!(
        result, original,
        "file must be byte-identical after refusal"
    );
}

#[test]
fn end_marker_before_begin_refuses_and_leaves_file_untouched() {
    let original = "# <<< oopsinput <<<\nbefore-1\n# >>> oopsinput >>>\nsource plugin\nafter-1\n";
    let (ok, result) = run_uninstall(original);
    assert!(!ok, "uninstall must refuse when markers are out of order");
    assert_eq!(
        result, original,
        "file must be byte-identical after refusal"
    );
}

#[test]
fn marker_text_joined_to_a_user_line_is_not_an_ownership_receipt() {
    // This is the exact corrupted shape produced by the pre-fix installer
    // when `.zshrc` had no final newline. Treating the substring as a marker
    // would delete the user's preceding bytes during uninstall.
    let original = "export KEEP_ME=1# >>> oopsinput >>>\nsource plugin\n# <<< oopsinput <<<\n";
    let (ok, result) = run_uninstall(original);
    assert!(!ok, "an embedded marker must be refused as damaged");
    assert_eq!(result, original, "refusal changed the user's shell file");
}

#[test]
fn newline_receipt_does_not_merge_a_later_user_suffix() {
    let installed = "export FIRST=1\n# >>> oopsinput >>>\n\
                     # oopsinput: restore preceding no-final-newline\n\
                     source plugin\n# <<< oopsinput <<<\nexport SECOND=2\n";
    let (ok, result) = run_uninstall(installed);
    assert!(ok, "healthy block with a later user line must uninstall");
    assert_eq!(
        result, "export FIRST=1\nexport SECOND=2\n",
        "restoring the old final-byte shape merged two user commands"
    );
}

#[test]
fn no_block_is_a_clean_noop() {
    let original = "just a normal zshrc\nwith lines\n";
    let (ok, result) = run_uninstall(original);
    assert!(ok, "uninstall without a block should succeed as a no-op");
    assert_eq!(result, original);
}

#[test]
fn no_block_does_not_authorize_removing_same_named_files() {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let home = std::env::temp_dir().join(format!(
        "oopsinput-uninst-no-receipt-{}-{id}",
        std::process::id()
    ));
    let binary = home.join(".local/bin/oopsinput");
    let plugin = home.join(".local/share/oopsinput/oopsinput.zsh");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    std::fs::write(&binary, "not ours\n").unwrap();
    std::fs::write(&plugin, "also not ours\n").unwrap();

    let out = uninstall(&home);
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(&binary).unwrap(), "not ours\n");
    assert_eq!(std::fs::read_to_string(&plugin).unwrap(), "also not ours\n");

    cleanup(&home);
}

#[test]
fn multiple_marker_blocks_refuse_and_leave_file_untouched() {
    // The old first-begin/last-end range would delete user lines between two
    // blocks. More than one block has no safe automatic edit boundary.
    let original = "# >>> oopsinput >>>\nsource one\n# <<< oopsinput <<<\nkeep this\n# >>> oopsinput >>>\nsource two\n# <<< oopsinput <<<\n";
    let (ok, result) = run_uninstall(original);
    assert!(!ok, "multiple blocks must be refused");
    assert_eq!(result, original);
}

#[test]
fn fresh_install_uninstalls_runtime_artifacts_but_keeps_config_and_state() {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let home =
        std::env::temp_dir().join(format!("oopsinput-uninst-full-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let fake_bin = home.join("fake-oopsinput");
    let fake_plugin = home.join("fake-oopsinput.zsh");
    std::fs::write(&fake_bin, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&fake_plugin, "# fake plugin\n").unwrap();

    let installed = Command::new("zsh")
        .arg(install_script())
        .env("HOME", &home)
        .env("OOPSINPUT_BIN_SRC", &fake_bin)
        .env("OOPSINPUT_PLUGIN_SRC", &fake_plugin)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run install.zsh");
    assert!(
        installed.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&installed.stderr)
    );

    let state = home.join(".local/state/oopsinput/events.jsonl");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    std::fs::write(&state, "recorded state\n").unwrap();
    let config = home.join(".config/oopsinput/config");
    assert!(config.exists(), "install must have written the config");

    let out = uninstall(&home);
    assert!(
        out.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".zshrc")).unwrap(),
        "",
        "the marked block was the whole fresh shell file"
    );
    assert!(!home.join(".local/bin/oopsinput").exists());
    assert!(!home.join(".local/share/oopsinput/oopsinput.zsh").exists());
    assert!(
        !home.join(".local/share/oopsinput").exists(),
        "the empty plugin directory should be removed"
    );
    assert!(
        config.exists(),
        "uninstall deliberately keeps configuration"
    );
    assert!(
        state.exists(),
        "uninstall deliberately keeps recorded state"
    );

    cleanup(&home);
}

#[test]
fn install_update_uninstall_preserves_zshrc_without_final_newline() {
    // Reproduced on 2026-08-08: the marker joined the original final line,
    // making zsh report a parse error; uninstall then removed that user line
    // and overwrote the only good backup with the corrupted file.
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let home = std::env::temp_dir().join(format!(
        "oopsinput-uninst-no-final-newline-{}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let original = b"export OOPSINPUT_PROBE=preserve_me";
    std::fs::write(home.join(".zshrc"), original).unwrap();

    let fake_bin = home.join("fake-oopsinput");
    let fake_plugin = home.join("fake-oopsinput.zsh");
    std::fs::write(&fake_bin, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&fake_plugin, "# fake plugin\n").unwrap();

    let install = || {
        Command::new("zsh")
            .arg(install_script())
            .env("HOME", &home)
            .env("OOPSINPUT_BIN_SRC", &fake_bin)
            .env("OOPSINPUT_PLUGIN_SRC", &fake_plugin)
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .expect("run install.zsh")
    };
    for pass in 1..=2 {
        let out = install();
        assert!(
            out.status.success(),
            "install pass {pass} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let installed_zshrc = home.join(".zshrc");
    let installed = std::fs::read(&installed_zshrc).unwrap();
    assert!(
        installed.starts_with(b"export OOPSINPUT_PROBE=preserve_me\n# >>> oopsinput >>>\n"),
        "marker was not separated from the original line: {installed:?}"
    );
    assert!(
        installed
            .windows(b"# oopsinput: restore preceding no-final-newline".len())
            .any(|window| window == b"# oopsinput: restore preceding no-final-newline"),
        "newline restoration receipt missing"
    );
    let syntax = Command::new("zsh")
        .args(["-n", installed_zshrc.to_str().unwrap()])
        .output()
        .expect("parse installed zshrc");
    assert!(
        syntax.status.success(),
        "installed zshrc is not parseable: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );
    assert_eq!(
        std::fs::read(home.join(".zshrc.oopsinput-backup")).unwrap(),
        original,
        "repeat install overwrote the original recovery copy"
    );

    let out = uninstall(&home);
    assert!(
        out.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(home.join(".zshrc")).unwrap(), original);
    assert_eq!(
        std::fs::read(home.join(".zshrc.oopsinput-backup")).unwrap(),
        original,
        "uninstall overwrote the good install backup"
    );

    cleanup(&home);
}

#[test]
fn unrecognized_file_in_plugin_directory_is_preserved() {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let home = std::env::temp_dir().join(format!(
        "oopsinput-uninst-owned-{}-{id}",
        std::process::id()
    ));
    let plugin_dir = home.join(".local/share/oopsinput");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        home.join(".zshrc"),
        "# >>> oopsinput >>>\nsource installed-plugin\n# <<< oopsinput <<<\n",
    )
    .unwrap();
    std::fs::write(plugin_dir.join("oopsinput.zsh"), "installed plugin\n").unwrap();
    std::fs::write(plugin_dir.join("not-owned"), "preserve this\n").unwrap();

    let out = uninstall(&home);
    assert!(out.status.success());
    assert!(!plugin_dir.join("oopsinput.zsh").exists());
    assert_eq!(
        std::fs::read_to_string(plugin_dir.join("not-owned")).unwrap(),
        "preserve this\n"
    );
    assert!(plugin_dir.exists(), "a non-empty directory must remain");

    cleanup(&home);
}

#[test]
fn uninstaller_escapes_control_and_bidi_bytes_in_displayed_paths() {
    // Reproduced while verifying SECURITY.md (2026-08-08): the no-receipt
    // diagnostic interpolated HOME-derived paths without terminal escaping.
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let home = std::env::temp_dir().join(format!(
        "oopsinput-uninst-display-{}-{id}-\u{1b}]0;UNINSTALL\u{7}-\u{202e}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).unwrap();

    let out = uninstall(&home);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}') && !stdout.contains('\u{7}') && !stdout.contains('\u{202e}'),
        "active terminal controls survived in uninstaller output: {stdout:?}"
    );
    assert!(
        stdout.contains("^[]0;UNINSTALL^G") && stdout.contains("\\u{202E}"),
        "uninstaller did not render both hostile fragments visibly: {stdout:?}"
    );

    cleanup(&home);
}
