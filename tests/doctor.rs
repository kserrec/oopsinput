//! Doctor and CLI diagnostic consistency. These paths exercise environment-
//! and argument-derived text that reaches the user's terminal, in addition to
//! pinning doctor's config and mode reporting to the same source of truth.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const ALL_WRAPPED: &str =
    "accept-line,accept-line-and-down-history,accept-and-hold,accept-and-infer-next-history";

fn doctor_command(home: &Path, xdg: Option<&Path>) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oopsinput"));
    cmd.arg("doctor")
        .env("HOME", home)
        .env_remove("OOPSINPUT_MODE")
        .env_remove("OOPSINPUT_STATE_DIR")
        .env_remove("XDG_STATE_HOME")
        .env_remove("OOPSINPUT_PLUGIN_ACTIVE")
        .env_remove("OOPSINPUT_WRAPPED_WIDGETS")
        .env_remove("OOPSINPUT_TEST_OLLAMA_PORT");
    match xdg {
        Some(dir) => cmd.env("XDG_CONFIG_HOME", dir),
        None => cmd.env_remove("XDG_CONFIG_HOME"),
    };
    cmd
}

fn doctor_output(home: &Path, xdg: Option<&Path>) -> String {
    let out = doctor_command(home, xdg).output().expect("run doctor");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn healthy_setup(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "oopsinput-doctor-health-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let home = base.join("home");
    let xdg = base.join("config");
    let state = home.join(".local/state/oopsinput");
    let plugin = home.join(".local/share/oopsinput/oopsinput.zsh");
    std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    std::fs::write(&plugin, "# installed plugin\n").unwrap();
    std::fs::write(
        home.join(".zshrc"),
        "# >>> oopsinput >>>\nsource installed-plugin\n# <<< oopsinput <<<\n",
    )
    .unwrap();
    std::fs::create_dir_all(xdg.join("oopsinput")).unwrap();
    std::fs::write(xdg.join("oopsinput/config"), "mode = suggest\n").unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(state.join("events.jsonl"), "").unwrap();
    std::fs::set_permissions(
        state.join("events.jsonl"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    (base, home, xdg)
}

fn healthy_doctor(home: &Path, xdg: &Path) -> Command {
    let mut cmd = doctor_command(home, Some(xdg));
    cmd.env("OOPSINPUT_PLUGIN_ACTIVE", "1")
        .env("OOPSINPUT_WRAPPED_WIDGETS", ALL_WRAPPED);
    cmd
}

#[test]
fn doctor_config_line_and_mode_line_agree_under_xdg() {
    let base = std::env::temp_dir().join(format!("oopsinput-doctor-{}", std::process::id()));
    let home = base.join("home");
    let xdg = base.join("xdg");
    std::fs::create_dir_all(home.join(".config/oopsinput")).unwrap();
    std::fs::create_dir_all(xdg.join("oopsinput")).unwrap();
    std::fs::write(xdg.join("oopsinput/config"), "mode = suggest\n").unwrap();

    let out = doctor_output(&home, Some(&xdg));
    let config_display = xdg.join("oopsinput/config");
    assert!(
        out.contains(&format!("{} (present)", config_display.display())),
        "config line must show the XDG path doctor's mode actually reads:\n{out}"
    );
    assert!(
        out.contains("mode:       suggest"),
        "mode must come from the displayed config:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_default_path_reports_shadow_when_no_config() {
    let base = std::env::temp_dir().join(format!("oopsinput-doctor2-{}", std::process::id()));
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = doctor_output(&home, None);
    assert!(
        out.contains("(absent — defaults in effect)"),
        "missing config must read as absent:\n{out}"
    );
    assert!(
        out.contains("mode:       shadow"),
        "no config means shadow:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_warn_mode_describes_the_landed_warning_ui() {
    // Regression (README audit 2026-08-08): after the M3 warning UI shipped,
    // doctor still described Warn mode as "pending the M3 UI". The real
    // configured mode worked; the setup diagnostic was what had gone stale.
    let base = std::env::temp_dir().join(format!(
        "oopsinput-doctor-warn-description-{}",
        std::process::id()
    ));
    let home = base.join("home");
    let xdg = base.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(xdg.join("oopsinput")).unwrap();
    std::fs::write(xdg.join("oopsinput/config"), "mode = warn\n").unwrap();

    let out = doctor_output(&home, Some(&xdg));
    assert!(
        out.contains("mode:       warn (L1 prompts and visible warnings)"),
        "doctor did not describe Warn mode's implemented UI:\n{out}"
    );
    assert!(
        !out.contains("pending"),
        "doctor still describes shipped behavior as pending:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_labels_a_symlinked_config_ignored_and_uses_defaults() {
    // Reproduced during the M5 test audit (2026-08-08): config loading ignored
    // the symlink as required, but doctor followed it for its existence check
    // and called the same ignored path "present" next to a shadow-mode line.
    let base = std::env::temp_dir().join(format!(
        "oopsinput-doctor-config-link-{}",
        std::process::id()
    ));
    let home = base.join("home");
    let xdg = base.join("xdg");
    let target = base.join("outside-config");
    std::fs::create_dir_all(xdg.join("oopsinput")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(&target, "mode = confirm\n").unwrap();
    std::os::unix::fs::symlink(&target, xdg.join("oopsinput/config")).unwrap();

    let out = doctor_output(&home, Some(&xdg));
    assert!(
        out.contains("(ignored — not a regular file; defaults in effect)"),
        "doctor did not describe the config loader's real decision:\n{out}"
    );
    assert!(
        out.contains("mode:       shadow"),
        "ignored config affected the mode:\n{out}"
    );
    assert!(
        !out.contains("(present)"),
        "symlink was called present:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_escapes_control_and_bidi_bytes_in_the_config_path() {
    // Reproduced while verifying SECURITY.md (2026-08-08): an ESC byte in
    // XDG_CONFIG_HOME reached stdout through direct `config.display()`
    // interpolation. U+202E is paired with it because both are forbidden by
    // the same SPEC §9 display boundary.
    let base = std::env::temp_dir().join(format!(
        "oopsinput-doctor-config-display-{}",
        std::process::id()
    ));
    let home = base.join("home");
    let xdg = base.join("xdg-\u{1b}[31m-\u{202e}");
    std::fs::create_dir_all(&home).unwrap();

    let out = doctor_output(&home, Some(&xdg));
    assert!(
        !out.contains('\u{1b}') && !out.contains('\u{202e}'),
        "active terminal controls survived in doctor's config path: {out:?}"
    );
    assert!(
        out.contains("^[[31m") && out.contains("\\u{202E}"),
        "doctor did not render both hostile fragments visibly: {out:?}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_escapes_control_and_bidi_bytes_in_the_discovered_zsh_path() {
    // Source audit of the same bug found a second direct interpolation: the
    // first executable `zsh` path discovered from untrusted PATH entries.
    let base = std::env::temp_dir().join(format!(
        "oopsinput-doctor-zsh-display-{}",
        std::process::id()
    ));
    let home = base.join("home");
    let hostile_path = base.join("path-\u{1b}]0;BAD\u{7}-\u{2067}");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&hostile_path).unwrap();
    let zsh = hostile_path.join("zsh");
    std::fs::write(&zsh, "not executed\n").unwrap();
    std::fs::set_permissions(&zsh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("doctor")
        .env("HOME", &home)
        .env("PATH", &hostile_path)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("OOPSINPUT_MODE")
        .output()
        .expect("run doctor");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}') && !stdout.contains('\u{7}') && !stdout.contains('\u{2067}'),
        "active terminal controls survived in doctor's zsh path: {stdout:?}"
    );
    assert!(
        stdout.contains("^[]0;BAD^G") && stdout.contains("\\u{2067}"),
        "doctor did not render both hostile fragments visibly: {stdout:?}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn unknown_command_diagnostic_escapes_control_and_bidi_bytes() {
    // Source audit of the doctor finding found the same direct interpolation
    // in the top-level usage error.
    let out = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("bad\u{1b}[31m-\u{202e}")
        .output()
        .expect("run unknown command");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains('\u{1b}') && !stderr.contains('\u{202e}'),
        "active terminal controls survived in the usage error: {stderr:?}"
    );
    assert!(
        stderr.contains("^[[31m") && stderr.contains("\\u{202E}"),
        "usage error did not render both hostile fragments visibly: {stderr:?}"
    );
}

#[test]
fn doctor_reports_a_complete_healthy_install_ready() {
    let (base, home, xdg) = healthy_setup("ready");
    let out = healthy_doctor(&home, &xdg).output().expect("run doctor");
    assert!(out.status.success(), "doctor failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("plugin:     installed"), "{stdout}");
    assert!(stdout.contains("widgets:    4/4 wrapped"), "{stdout}");
    assert!(stdout.contains("(present) — valid"), "{stdout}");
    assert!(
        stdout.contains("state:") && stdout.contains("0700; 1 owned file(s) present at 0600"),
        "{stdout}"
    );
    assert!(stdout.contains("model:      disabled"), "{stdout}");
    assert!(stdout.contains("result:     ready"), "{stdout}");

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_rejects_an_invalid_config_and_names_each_safe_issue() {
    let (base, home, xdg) = healthy_setup("config-invalid");
    std::fs::write(
        xdg.join("oopsinput/config"),
        "mode = impossible\nhostile\u{1b}]0;KEY\u{7} = value\n",
    )
    .unwrap();

    let out = healthy_doctor(&home, &xdg).output().expect("run doctor");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("INVALID (2 issue(s))"), "{stdout:?}");
    assert!(stdout.contains("line 1: invalid mode"), "{stdout:?}");
    assert!(stdout.contains("line 2: unknown key"), "{stdout:?}");
    assert!(
        !stdout.contains("hostile") && !stdout.contains('\u{1b}'),
        "raw config text reached doctor output: {stdout:?}"
    );
    assert!(stdout.contains("result:     problems found"), "{stdout:?}");

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_rejects_missing_plugin_artifact_despite_a_marker_block() {
    let (base, home, xdg) = healthy_setup("plugin-missing");
    std::fs::remove_file(home.join(".local/share/oopsinput/oopsinput.zsh")).unwrap();

    let out = healthy_doctor(&home, &xdg).output().expect("run doctor");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("plugin:     incomplete")
            && stdout.contains("~/.local/share/oopsinput/oopsinput.zsh is absent"),
        "{stdout}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_rejects_a_partially_wrapped_live_shell() {
    let (base, home, xdg) = healthy_setup("widgets-partial");
    let mut cmd = doctor_command(&home, Some(&xdg));
    cmd.env("OOPSINPUT_PLUGIN_ACTIVE", "1").env(
        "OOPSINPUT_WRAPPED_WIDGETS",
        "accept-line,accept-line-and-down-history,accept-and-hold",
    );

    let out = cmd.output().expect("run doctor");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("widgets:    3/4 wrapped"), "{stdout}");

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_rejects_nonprivate_state_modes_without_repairing_them() {
    let (base, home, xdg) = healthy_setup("state-modes");
    let state = home.join(".local/state/oopsinput");
    let events = state.join("events.jsonl");
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&events, std::fs::Permissions::from_mode(0o644)).unwrap();

    let out = healthy_doctor(&home, &xdg).output().expect("run doctor");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("directory mode is 755; required 700"),
        "{stdout}"
    );
    assert!(
        stdout.contains("events.jsonl mode is 644; required 600"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o755,
        "doctor must diagnose, not repair"
    );
    assert_eq!(
        std::fs::metadata(&events).unwrap().permissions().mode() & 0o777,
        0o644,
        "doctor must diagnose, not repair"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn configured_unreachable_model_makes_doctor_fail() {
    let (base, home, xdg) = healthy_setup("model-unreachable");
    std::fs::write(
        xdg.join("oopsinput/config"),
        "mode = suggest\nmodel = test-model\nmodel_timeout_ms = 100\n",
    )
    .unwrap();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut cmd = healthy_doctor(&home, &xdg);
    cmd.env("OOPSINPUT_TEST_OLLAMA_PORT", port.to_string());
    let out = cmd.output().expect("run doctor");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Ollama not reachable"), "{stdout}");
    assert!(stdout.contains("result:     problems found"), "{stdout}");

    let _ = std::fs::remove_dir_all(&base);
}
