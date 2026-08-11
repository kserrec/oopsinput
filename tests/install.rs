//! Tests for zsh/install.zsh: a fresh mode is explicit, install/update
//! artifacts are stable outside the checkout, rollback restores one complete
//! prior state, and no existing config or symlink target is overwritten.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("zsh/install.zsh")
}

struct FakeHome {
    dir: PathBuf,
    bin_src: PathBuf,
    plugin_src: PathBuf,
    uninstall_src: PathBuf,
}

impl FakeHome {
    fn new() -> FakeHome {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oopsinput-inst-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A stand-in "release binary" for the installer to copy.
        let bin_src = dir.join("fake-oopsinput");
        let plugin_src = dir.join("fake-oopsinput.zsh");
        let uninstall_src = dir.join("fake-uninstall.zsh");
        std::fs::write(&bin_src, "#!/bin/sh\n# version 1\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin_src, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&plugin_src, "# fake plugin version 1\n").unwrap();
        std::fs::write(&uninstall_src, "#!/usr/bin/env zsh\n# version 1\n").unwrap();
        FakeHome {
            dir,
            bin_src,
            plugin_src,
            uninstall_src,
        }
    }

    /// Run install.zsh against this fake HOME. XDG_CONFIG_HOME is cleared so
    /// the default ~/.config path is what gets exercised.
    fn run_install(&self) -> bool {
        self.run_install_output(None).status.success()
    }

    fn run_install_output(&self, xdg: Option<&Path>) -> Output {
        self.run_install_with(&["--mode", "suggest"], xdg, None)
    }

    fn run_update(&self) -> bool {
        self.run_install_with(&[], None, None).status.success()
    }

    fn run_install_with(
        &self,
        args: &[&str],
        xdg: Option<&Path>,
        fail_after: Option<&str>,
    ) -> Output {
        let mut cmd = Command::new("zsh");
        cmd.arg(script())
            .args(args)
            .env("LC_ALL", "C")
            .env("HOME", &self.dir)
            .env("OOPSINPUT_BIN_SRC", &self.bin_src)
            .env("OOPSINPUT_PLUGIN_SRC", &self.plugin_src)
            .env("OOPSINPUT_UNINSTALL_SRC", &self.uninstall_src);
        match xdg {
            Some(dir) => cmd.env("XDG_CONFIG_HOME", dir),
            None => cmd.env_remove("XDG_CONFIG_HOME"),
        };
        match fail_after {
            Some(point) => cmd.env("OOPSINPUT_TEST_FAIL_AFTER", point),
            None => cmd.env_remove("OOPSINPUT_TEST_FAIL_AFTER"),
        };
        cmd.output().expect("run install.zsh")
    }

    fn run_install_pty(&self, keys: &[u8]) -> Output {
        use std::io::Read;
        use std::sync::mpsc;

        let command = format!(
            "env -u XDG_CONFIG_HOME HOME={} OOPSINPUT_BIN_SRC={} \
             OOPSINPUT_PLUGIN_SRC={} OOPSINPUT_UNINSTALL_SRC={} zsh {}",
            shell_quote(&self.dir),
            shell_quote(&self.bin_src),
            shell_quote(&self.plugin_src),
            shell_quote(&self.uninstall_src),
            shell_quote(&script()),
        );
        let mut child = Command::new("script")
            .args(["-qec", &command, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start installer PTY");
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(chunk[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Sending Ctrl-C before the installer has armed its /dev/tty read can
        // signal the wrapper process instead of exercising installer cancel.
        // Wait for the visible chooser marker, exactly as the product PTY
        // harness stages timing-sensitive prompt keys.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = Vec::new();
        while !String::from_utf8_lossy(&seen).contains("Focus: none") {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !left.is_zero(),
                "installer chooser never appeared:\n{}",
                String::from_utf8_lossy(&seen)
            );
            match rx.recv_timeout(left) {
                Ok(bytes) => seen.extend_from_slice(&bytes),
                Err(_) => {
                    let _ = child.kill();
                    panic!(
                        "installer chooser never appeared:\n{}",
                        String::from_utf8_lossy(&seen)
                    );
                }
            }
        }
        stdin.write_all(keys).expect("send installer keys");
        drop(stdin);

        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(bytes) => seen.extend_from_slice(&bytes),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    break;
                }
            }
        }
        let status = child.wait().expect("wait for installer PTY");
        let _ = reader.join();
        Output {
            status,
            stdout: seen,
            stderr: Vec::new(),
        }
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join(".config/oopsinput/config")
    }

    fn installed_bin(&self) -> PathBuf {
        self.dir.join(".local/bin/oopsinput")
    }

    fn installed_plugin(&self) -> PathBuf {
        self.dir.join(".local/share/oopsinput/oopsinput.zsh")
    }

    fn installed_uninstaller(&self) -> PathBuf {
        self.dir.join(".local/share/oopsinput/uninstall.zsh")
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

impl Drop for FakeHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn fresh_install_uses_explicit_suggest_with_tight_perms() {
    let h = FakeHome::new();
    assert!(h.run_install(), "install must succeed on a fresh home");

    let config = std::fs::read_to_string(h.config_path()).expect("config written");
    assert!(
        config.contains("mode = suggest"),
        "the explicit Suggest choice must be written (SPEC §8):\n{config}"
    );
    assert_eq!(
        FakeHome::mode_of(&h.config_path()),
        0o600,
        "config file must be user-only (SPEC §9-4)"
    );
    assert_eq!(
        FakeHome::mode_of(h.config_path().parent().unwrap()),
        0o700,
        "config dir must be user-only (SPEC §9-4)"
    );

    // The rest of the install happened too: both runtime artifacts were
    // copied to stable paths and the shell block points at that plugin.
    assert!(h.installed_bin().exists());
    assert!(h.installed_plugin().exists());
    assert!(h.installed_uninstaller().exists());
    assert_eq!(FakeHome::mode_of(&h.installed_bin()), 0o755);
    assert_eq!(FakeHome::mode_of(&h.installed_plugin()), 0o600);
    assert_eq!(FakeHome::mode_of(&h.installed_uninstaller()), 0o700);
    assert_eq!(
        FakeHome::mode_of(h.installed_plugin().parent().unwrap()),
        0o700
    );
    let zshrc = std::fs::read_to_string(h.dir.join(".zshrc")).unwrap();
    assert!(zshrc.contains("# >>> oopsinput >>>"));
    assert!(zshrc.contains(&format!("source {}", h.installed_plugin().display())));
}

#[test]
fn every_promptless_mode_writes_only_the_explicit_choice() {
    for mode in ["shadow", "suggest", "warn", "confirm"] {
        let h = FakeHome::new();
        let out = h.run_install_with(&["--mode", mode], None, None);
        assert!(
            out.status.success(),
            "{mode} install failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let config = std::fs::read_to_string(h.config_path()).unwrap();
        assert!(
            config
                .lines()
                .any(|line| line.starts_with(&format!("mode = {mode} "))),
            "{mode} was not the sole written choice:\n{config}"
        );
    }
}

#[test]
fn missing_or_invalid_promptless_mode_fails_before_writing() {
    for args in [vec![], vec!["--mode"], vec!["--mode", "automatic"]] {
        let h = FakeHome::new();
        let out = h.run_install_with(&args, None, None);
        assert!(
            !out.status.success(),
            "invalid invocation unexpectedly passed"
        );
        assert!(!h.installed_bin().exists());
        assert!(!h.installed_plugin().exists());
        assert!(!h.installed_uninstaller().exists());
        assert!(!h.config_path().exists());
        assert!(!h.dir.join(".zshrc").exists());
    }
}

#[test]
fn mode_argument_cannot_overwrite_an_existing_config() {
    let h = FakeHome::new();
    std::fs::create_dir_all(h.config_path().parent().unwrap()).unwrap();
    let original = "# user-owned\nmode = shadow\nmodel = custom\n";
    std::fs::write(h.config_path(), original).unwrap();

    let out = h.run_install_with(&["--mode", "confirm"], None, None);
    assert!(
        !out.status.success(),
        "--mode must not claim existing config"
    );
    assert_eq!(std::fs::read_to_string(h.config_path()).unwrap(), original);
    assert!(!h.installed_bin().exists());
    assert!(!h.dir.join(".zshrc").exists());
}

#[test]
fn interactive_tab_then_enter_selects_shadow_from_no_initial_focus() {
    let h = FakeHome::new();
    let out = h.run_install_pty(b"\t\n");
    assert!(
        out.status.success(),
        "interactive install failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(transcript.contains("Focus: none"), "{transcript:?}");
    assert!(
        transcript.contains("\u{1b}[7mShadow\u{1b}[0m"),
        "{transcript:?}"
    );
    assert!(
        std::fs::read_to_string(h.config_path())
            .unwrap()
            .contains("mode = shadow")
    );
}

#[test]
fn interactive_bare_enter_is_not_a_choice_but_digit_is() {
    let h = FakeHome::new();
    let out = h.run_install_pty(b"\n3");
    assert!(
        out.status.success(),
        "interactive install failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        std::fs::read_to_string(h.config_path())
            .unwrap()
            .contains("mode = warn")
    );
}

#[test]
fn interactive_ctrl_c_cancels_without_any_installation() {
    let h = FakeHome::new();
    let out = h.run_install_pty(b"\x03");
    assert!(
        !out.status.success(),
        "Ctrl-C unexpectedly installed oopsinput"
    );
    assert!(!h.installed_bin().exists());
    assert!(!h.installed_plugin().exists());
    assert!(!h.installed_uninstaller().exists());
    assert!(!h.config_path().exists());
    assert!(!h.dir.join(".zshrc").exists());
}

#[test]
fn existing_config_is_never_touched() {
    let h = FakeHome::new();
    std::fs::create_dir_all(h.config_path().parent().unwrap()).unwrap();
    let user_config = "# my config\nmode = shadow\n";
    std::fs::write(h.config_path(), user_config).unwrap();

    assert!(h.run_update(), "install must succeed over existing config");
    let after = std::fs::read_to_string(h.config_path()).unwrap();
    assert_eq!(
        after, user_config,
        "an existing config must be byte-identical after install"
    );
}

#[test]
fn dangling_config_symlink_is_left_alone() {
    // Regression (audit 2026-08-06): the guard used -f, which is false for a
    // dangling symlink, so `>` followed the link and created the config's
    // content at the link's target — a file the installer never inspected.
    let h = FakeHome::new();
    std::fs::create_dir_all(h.config_path().parent().unwrap()).unwrap();
    let elsewhere = h.dir.join("elsewhere.txt");
    std::os::unix::fs::symlink(&elsewhere, h.config_path()).unwrap();

    assert!(h.run_update(), "install must still succeed");
    assert!(
        !elsewhere.exists(),
        "installer wrote through a dangling symlink to {}",
        elsewhere.display()
    );
}

#[test]
fn install_is_idempotent() {
    let h = FakeHome::new();
    assert!(h.run_install());
    assert!(h.run_update(), "second install must succeed");
    let zshrc = std::fs::read_to_string(h.dir.join(".zshrc")).unwrap();
    assert_eq!(
        zshrc.matches("# >>> oopsinput >>>").count(),
        1,
        "plugin block must not be duplicated:\n{zshrc}"
    );
}

#[test]
fn installed_plugin_does_not_depend_on_the_checkout() {
    // Regression target from M6: the old block sourced zsh/oopsinput.zsh in
    // the checkout, so moving or deleting the clone broke every new shell.
    let h = FakeHome::new();
    assert!(h.run_install());

    let installed = std::fs::read_to_string(h.installed_plugin()).unwrap();
    let source = std::fs::read_to_string(&h.plugin_src).unwrap();
    assert_eq!(installed, source, "the complete plugin must be installed");

    let zshrc = std::fs::read_to_string(h.dir.join(".zshrc")).unwrap();
    assert_eq!(
        zshrc,
        format!(
            "# >>> oopsinput >>>\nsource {}\n# <<< oopsinput <<<\n",
            h.installed_plugin().display()
        )
    );
    assert!(
        !zshrc.contains(env!("CARGO_MANIFEST_DIR")),
        "installed hook must not point into the repository:\n{zshrc}"
    );
}

#[test]
fn repeat_install_updates_assets_and_migrates_the_old_block_in_place() {
    let h = FakeHome::new();
    let old_zshrc = "before\n# >>> oopsinput >>>\nsource /checkout/that/moved/zsh/oopsinput.zsh\n# <<< oopsinput <<<\nafter\n";
    std::fs::write(h.dir.join(".zshrc"), old_zshrc).unwrap();

    assert!(h.run_install(), "migration install must succeed");
    std::fs::write(&h.bin_src, "#!/bin/sh\n# version 2\nexit 0\n").unwrap();
    std::fs::write(&h.plugin_src, "# fake plugin version 2\n").unwrap();
    std::fs::write(&h.uninstall_src, "#!/usr/bin/env zsh\n# version 2\n").unwrap();
    assert!(h.run_update(), "update install must succeed");

    assert_eq!(
        std::fs::read_to_string(h.installed_bin()).unwrap(),
        "#!/bin/sh\n# version 2\nexit 0\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.installed_plugin()).unwrap(),
        "# fake plugin version 2\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.installed_uninstaller()).unwrap(),
        "#!/usr/bin/env zsh\n# version 2\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.dir.join(".zshrc")).unwrap(),
        format!(
            "before\n# >>> oopsinput >>>\nsource {}\n# <<< oopsinput <<<\nafter\n",
            h.installed_plugin().display()
        ),
        "only the validated marker block may change"
    );
}

#[test]
fn plugin_destination_symlink_is_refused_without_touching_its_target() {
    // Probe before the fix: `cp plugin destination-symlink` replaced the
    // target with all 7,789 plugin bytes. This guard prevents that write.
    let h = FakeHome::new();
    let victim = h.dir.join("not-owned-by-oopsinput");
    std::fs::write(&victim, "keep me\n").unwrap();
    std::fs::create_dir_all(h.installed_plugin().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&victim, h.installed_plugin()).unwrap();

    assert!(!h.run_install(), "installer must refuse the symlink");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me\n");
    assert!(h.installed_plugin().is_symlink());
    assert!(!h.installed_bin().exists(), "validation must happen first");
}

#[test]
fn binary_destination_symlink_is_refused_without_touching_its_target() {
    let h = FakeHome::new();
    let victim = h.dir.join("not-the-installed-binary");
    std::fs::write(&victim, "keep me too\n").unwrap();
    std::fs::create_dir_all(h.installed_bin().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&victim, h.installed_bin()).unwrap();

    assert!(!h.run_install(), "installer must refuse the symlink");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me too\n");
    assert!(h.installed_bin().is_symlink());
}

#[test]
fn damaged_marker_block_refuses_before_installing_assets() {
    let h = FakeHome::new();
    let damaged = "before\n# >>> oopsinput >>>\nsource old-plugin\nafter\n";
    std::fs::write(h.dir.join(".zshrc"), damaged).unwrap();

    assert!(!h.run_install(), "ambiguous edit boundary must be refused");
    assert_eq!(
        std::fs::read_to_string(h.dir.join(".zshrc")).unwrap(),
        damaged
    );
    assert!(!h.installed_bin().exists());
    assert!(!h.installed_plugin().exists());
}

#[test]
fn marker_text_joined_to_a_user_line_is_refused_before_installing_assets() {
    // Upgrade regression for the no-final-newline bug: an older broken
    // install could join the begin marker to the user's last line. It is not
    // an ownership receipt and must never authorize deleting that line.
    let h = FakeHome::new();
    let damaged = "export KEEP_ME=1# >>> oopsinput >>>\nsource old-plugin\n# <<< oopsinput <<<\n";
    std::fs::write(h.dir.join(".zshrc"), damaged).unwrap();

    assert!(!h.run_install(), "embedded marker must be refused");
    assert_eq!(
        std::fs::read_to_string(h.dir.join(".zshrc")).unwrap(),
        damaged
    );
    assert!(!h.installed_bin().exists());
    assert!(!h.installed_plugin().exists());
}

#[test]
fn fresh_install_does_not_claim_a_preexisting_binary() {
    let h = FakeHome::new();
    std::fs::create_dir_all(h.installed_bin().parent().unwrap()).unwrap();
    std::fs::write(h.installed_bin(), "someone else's file\n").unwrap();

    assert!(
        !h.run_install(),
        "a fresh install must not overwrite the file"
    );
    assert_eq!(
        std::fs::read_to_string(h.installed_bin()).unwrap(),
        "someone else's file\n"
    );
}

#[test]
fn fresh_install_does_not_claim_a_preexisting_plugin() {
    let h = FakeHome::new();
    std::fs::create_dir_all(h.installed_plugin().parent().unwrap()).unwrap();
    std::fs::write(h.installed_plugin(), "someone else's plugin\n").unwrap();

    assert!(
        !h.run_install(),
        "a fresh install must not overwrite the file"
    );
    assert_eq!(
        std::fs::read_to_string(h.installed_plugin()).unwrap(),
        "someone else's plugin\n"
    );
}

#[test]
fn handled_fresh_failure_restores_shell_and_preexisting_backup() {
    // The transaction deliberately fails after all three runtime files have
    // been replaced. Before rollback existed, this exact point stranded an
    // unowned partial installation with no marker receipt.
    let h = FakeHome::new();
    let zshrc = h.dir.join(".zshrc");
    let backup = h.dir.join(".zshrc.oopsinput-backup");
    std::fs::write(&zshrc, "# original shell\n").unwrap();
    std::fs::write(&backup, "# older user backup\n").unwrap();

    let out = h.run_install_with(&["--mode", "confirm"], None, Some("uninstaller"));
    assert!(
        !out.status.success(),
        "injected failure unexpectedly passed"
    );
    assert_eq!(
        std::fs::read_to_string(&zshrc).unwrap(),
        "# original shell\n"
    );
    assert_eq!(
        std::fs::read_to_string(&backup).unwrap(),
        "# older user backup\n"
    );
    assert!(!h.installed_bin().exists());
    assert!(!h.installed_plugin().exists());
    assert!(!h.installed_uninstaller().exists());
    assert!(!h.config_path().exists());
}

#[test]
fn handled_update_failure_restores_one_complete_old_runtime_set() {
    let h = FakeHome::new();
    assert!(h.run_install());
    let old_zshrc = std::fs::read(h.dir.join(".zshrc")).unwrap();
    let old_config = std::fs::read(h.config_path()).unwrap();

    std::fs::write(&h.bin_src, "#!/bin/sh\n# version 2\nexit 0\n").unwrap();
    std::fs::write(&h.plugin_src, "# fake plugin version 2\n").unwrap();
    std::fs::write(&h.uninstall_src, "#!/usr/bin/env zsh\n# version 2\n").unwrap();
    let out = h.run_install_with(&[], None, Some("plugin"));
    assert!(
        !out.status.success(),
        "injected update failure unexpectedly passed"
    );

    assert_eq!(
        std::fs::read_to_string(h.installed_bin()).unwrap(),
        "#!/bin/sh\n# version 1\nexit 0\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.installed_plugin()).unwrap(),
        "# fake plugin version 1\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.installed_uninstaller()).unwrap(),
        "#!/usr/bin/env zsh\n# version 1\n"
    );
    assert_eq!(std::fs::read(h.dir.join(".zshrc")).unwrap(), old_zshrc);
    assert_eq!(std::fs::read(h.config_path()).unwrap(), old_config);
}

#[test]
fn backup_failure_happens_before_assets_and_retry_remains_possible() {
    // Reproduced on 2026-08-08: runtime assets were installed before this
    // backup copy failed. The absent marker then made both retry and uninstall
    // refuse the stranded files.
    let h = FakeHome::new();
    let zshrc = h.dir.join(".zshrc");
    let backup = h.dir.join(".zshrc.oopsinput-backup");
    std::fs::write(&zshrc, "# original\n").unwrap();
    std::fs::write(&backup, "# existing backup\n").unwrap();
    std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o400)).unwrap();

    let failed = h.run_install_output(None);
    assert!(!failed.status.success(), "unwritable backup must fail");
    assert!(!h.installed_bin().exists(), "binary was stranded");
    assert!(!h.installed_plugin().exists(), "plugin was stranded");
    assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), "# original\n");

    std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        h.run_install(),
        "fixing the backup permission must make an ordinary retry succeed"
    );
}

#[test]
fn installer_escapes_control_and_bidi_bytes_in_displayed_paths() {
    // Reproduced while verifying SECURITY.md (2026-08-08): installer-authored
    // path messages emitted OSC bytes and U+202E from environment overrides.
    let h = FakeHome::new();
    let xdg = h.dir.join("xdg-\u{1b}]0;INSTALL\u{7}-\u{202e}");
    let out = h.run_install_output(Some(&xdg));
    assert!(
        out.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}') && !stdout.contains('\u{7}') && !stdout.contains('\u{202e}'),
        "active terminal controls survived in installer output: {stdout:?}"
    );
    assert!(
        stdout.contains("^[]0;INSTALL^G") && stdout.contains("\\u{202E}"),
        "installer did not render both hostile fragments visibly: {stdout:?}"
    );
}
