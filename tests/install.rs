//! Tests for zsh/install.zsh: install/update artifacts are stable outside the
//! checkout, the post-install default is `mode = suggest` (SPEC §8), and no
//! existing config or symlink target is overwritten.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("zsh/install.zsh")
}

struct FakeHome {
    dir: PathBuf,
    bin_src: PathBuf,
    plugin_src: PathBuf,
}

impl FakeHome {
    fn new() -> FakeHome {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oopsinput-inst-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A stand-in "release binary" for the installer to copy.
        let bin_src = dir.join("fake-oopsinput");
        let plugin_src = dir.join("fake-oopsinput.zsh");
        std::fs::write(&bin_src, "#!/bin/sh\n# version 1\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin_src, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&plugin_src, "# fake plugin version 1\n").unwrap();
        FakeHome {
            dir,
            bin_src,
            plugin_src,
        }
    }

    /// Run install.zsh against this fake HOME. XDG_CONFIG_HOME is cleared so
    /// the default ~/.config path is what gets exercised.
    fn run_install(&self) -> bool {
        self.run_install_output(None).status.success()
    }

    fn run_install_output(&self, xdg: Option<&Path>) -> Output {
        let mut cmd = Command::new("zsh");
        cmd.arg(script())
            .env("LC_ALL", "C")
            .env("HOME", &self.dir)
            .env("OOPSINPUT_BIN_SRC", &self.bin_src)
            .env("OOPSINPUT_PLUGIN_SRC", &self.plugin_src);
        match xdg {
            Some(dir) => cmd.env("XDG_CONFIG_HOME", dir),
            None => cmd.env_remove("XDG_CONFIG_HOME"),
        };
        cmd.output().expect("run install.zsh")
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

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}

impl Drop for FakeHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn fresh_install_defaults_to_suggest_with_tight_perms() {
    let h = FakeHome::new();
    assert!(h.run_install(), "install must succeed on a fresh home");

    let config = std::fs::read_to_string(h.config_path()).expect("config written");
    assert!(
        config.contains("mode = suggest"),
        "post-install default must be suggest (SPEC §8):\n{config}"
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
    assert_eq!(FakeHome::mode_of(&h.installed_bin()), 0o755);
    assert_eq!(FakeHome::mode_of(&h.installed_plugin()), 0o600);
    assert_eq!(
        FakeHome::mode_of(h.installed_plugin().parent().unwrap()),
        0o700
    );
    let zshrc = std::fs::read_to_string(h.dir.join(".zshrc")).unwrap();
    assert!(zshrc.contains("# >>> oopsinput >>>"));
    assert!(zshrc.contains(&format!("source {}", h.installed_plugin().display())));
}

#[test]
fn existing_config_is_never_touched() {
    let h = FakeHome::new();
    std::fs::create_dir_all(h.config_path().parent().unwrap()).unwrap();
    let user_config = "# my config\nmode = shadow\n";
    std::fs::write(h.config_path(), user_config).unwrap();

    assert!(h.run_install(), "install must succeed over existing config");
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

    assert!(h.run_install(), "install must still succeed");
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
    assert!(h.run_install(), "second install must succeed");
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
    assert!(h.run_install(), "update install must succeed");

    assert_eq!(
        std::fs::read_to_string(h.installed_bin()).unwrap(),
        "#!/bin/sh\n# version 2\nexit 0\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.installed_plugin()).unwrap(),
        "# fake plugin version 2\n"
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
