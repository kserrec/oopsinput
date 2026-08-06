//! Tests for zsh/install.zsh: the post-install default is `mode = suggest`
//! (SPEC §8), written with user-only permissions (SPEC §9-4), and an existing
//! config is never touched.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("zsh/install.zsh")
}

struct FakeHome {
    dir: PathBuf,
}

impl FakeHome {
    fn new() -> FakeHome {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oopsinput-inst-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A stand-in "release binary" for the installer to copy.
        let fake_bin = dir.join("fake-oopsinput");
        std::fs::write(&fake_bin, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        FakeHome { dir }
    }

    /// Run install.zsh against this fake HOME. XDG_CONFIG_HOME is cleared so
    /// the default ~/.config path is what gets exercised.
    fn run_install(&self) -> bool {
        Command::new("zsh")
            .arg(script())
            .env("HOME", &self.dir)
            .env("OOPSINPUT_BIN_SRC", self.dir.join("fake-oopsinput"))
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .expect("run install.zsh")
            .status
            .success()
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join(".config/oopsinput/config")
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

    // The rest of the install happened too: binary copied, block added.
    assert!(h.dir.join(".local/bin/oopsinput").exists());
    let zshrc = std::fs::read_to_string(h.dir.join(".zshrc")).unwrap();
    assert!(zshrc.contains("# >>> oopsinput >>>"));
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
