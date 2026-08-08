//! M5 purge command through the shipped binary: dispatch, exact ownership
//! boundary, symlink refusal, and honest output.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn purge_removes_owned_state_without_following_links_or_deleting_unknowns() {
    // Probed 2026-08-07 with the real command before pinning this: the known
    // symlink itself disappeared, its external target stayed byte-exact, and
    // an unknown state-directory entry survived. Recursive deletion would
    // violate that ownership boundary even though it is reversible in theory.
    let base = std::env::temp_dir().join(format!("oopsinput-purge-cli-{}", std::process::id()));
    let dir = base.join("state");
    let config = base.join("config/oopsinput/config");
    let victim = base.join("victim.txt");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("events.jsonl"), "event\n").unwrap();
    std::fs::write(dir.join("policy.jsonl"), "policy\n").unwrap();
    std::fs::write(dir.join("key"), "key\n").unwrap();
    std::fs::write(dir.join(".events-retention"), "1").unwrap();
    std::fs::write(dir.join(".policy-retention"), "1").unwrap();
    std::fs::write(dir.join(".oopsinput-tmp-abandoned"), "partial").unwrap();
    std::fs::write(dir.join("unknown.txt"), "KEEP\n").unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "mode = suggest\n").unwrap();
    std::fs::write(&victim, "PRECIOUS\n").unwrap();
    std::os::unix::fs::symlink(&victim, dir.join("config_warned")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("purge")
        .env("OOPSINPUT_STATE_DIR", &dir)
        .env("XDG_CONFIG_HOME", base.join("config"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("removed 7 state files"), "{stdout}");
    assert!(stdout.contains("kept the state directory"), "{stdout}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!dir.join("events.jsonl").exists());
    assert!(!dir.join("policy.jsonl").exists());
    assert!(!dir.join("key").exists());
    assert!(!dir.join("config_warned").exists());
    assert!(!dir.join(".events-retention").exists());
    assert!(!dir.join(".policy-retention").exists());
    assert!(!dir.join(".oopsinput-tmp-abandoned").exists());
    assert!(!dir.join(".oopsinput.lock").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("unknown.txt")).unwrap(),
        "KEEP\n"
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "PRECIOUS\n");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "mode = suggest\n",
        "purge must keep configuration"
    );

    let help = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("help")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("purge    delete all recorded state"),
        "purge command missing from help"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn purge_refuses_to_enter_a_symlinked_state_directory() {
    // Probed 2026-08-07 through the real CLI: an override pointing at a
    // directory symlink must fail before inspecting or deleting the target.
    let base = std::env::temp_dir().join(format!("oopsinput-purge-dirlink-{}", std::process::id()));
    let target = base.join("target");
    let link = base.join("state-link");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("events.jsonl"), "PRECIOUS\n").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("purge")
        .env("OOPSINPUT_STATE_DIR", &link)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("state directory path is a symlink"),
        "{output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(target.join("events.jsonl")).unwrap(),
        "PRECIOUS\n"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn purge_of_absent_state_is_an_empty_success() {
    // A fresh install has no state directory; explicit cleanup is still a
    // successful, comprehensible operation rather than an internal error.
    let dir = std::env::temp_dir().join(format!("oopsinput-purge-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("purge")
        .env("OOPSINPUT_STATE_DIR", &dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "oopsinput purge\n  nothing to purge\n"
    );
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!dir.exists());
}

#[test]
fn purge_with_any_trailing_argument_is_non_destructive_usage_error() {
    // Failure-hunt 2026-08-07: command dispatch matched only argv[0], so the
    // first implementation treated `oopsinput purge --help` as authorization
    // to delete. A destructive command must require the exact invocation.
    let dir = std::env::temp_dir().join(format!("oopsinput-purge-args-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("events.jsonl"), "PRECIOUS\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .args(["purge", "--help"])
        .env("OOPSINPUT_STATE_DIR", &dir)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("takes no arguments"),
        "{output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("events.jsonl")).unwrap(),
        "PRECIOUS\n"
    );
    assert!(!dir.join(LOCK_FILE_FOR_ASSERTION).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

const LOCK_FILE_FOR_ASSERTION: &str = ".oopsinput.lock";

#[test]
fn purge_recovers_a_symlinked_lock_without_following_its_target() {
    // Regression (M5 bughunt 2026-08-08): purge promised to remove every
    // coordination marker, but normal lock acquisition rejected a corrupted
    // lock symlink before purge could unlink it.
    let base =
        std::env::temp_dir().join(format!("oopsinput-purge-locklink-{}", std::process::id()));
    let dir = base.join("state");
    let victim = base.join("victim.txt");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("events.jsonl"), "event\n").unwrap();
    std::fs::write(&victim, "PRECIOUS\n").unwrap();
    std::os::unix::fs::symlink(&victim, dir.join(LOCK_FILE_FOR_ASSERTION)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("purge")
        .env("OOPSINPUT_STATE_DIR", &dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "PRECIOUS\n");
    assert!(!dir.exists(), "state directory survived: {output:?}");

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn purge_recovers_a_regular_lock_with_non_private_permissions() {
    let dir = std::env::temp_dir().join(format!("oopsinput-purge-lockmode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("events.jsonl"), "event\n").unwrap();
    let lock = dir.join(LOCK_FILE_FOR_ASSERTION);
    std::fs::write(&lock, "").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("purge")
        .env("OOPSINPUT_STATE_DIR", &dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!dir.exists(), "state directory survived: {output:?}");
}

#[test]
fn purge_refuses_a_directory_at_the_lock_path_without_deleting_inside_it() {
    let dir = std::env::temp_dir().join(format!("oopsinput-purge-lockdir-{}", std::process::id()));
    let lock_dir = dir.join(LOCK_FILE_FOR_ASSERTION);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&lock_dir).unwrap();
    std::fs::write(dir.join("events.jsonl"), "PRECIOUS\n").unwrap();
    std::fs::write(lock_dir.join("precious.txt"), "PRECIOUS\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("purge")
        .env("OOPSINPUT_STATE_DIR", &dir)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing recursive deletion"),
        "{output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("events.jsonl")).unwrap(),
        "PRECIOUS\n"
    );
    assert_eq!(
        std::fs::read_to_string(lock_dir.join("precious.txt")).unwrap(),
        "PRECIOUS\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn purge_refuses_recursive_deletion_without_mutating_the_directory() {
    // Probed 2026-08-07 through the real CLI with an `events.jsonl` directory:
    // purge refused, preserved its child, and did not even leave a lock file.
    // A product-owned filename is not authority to recursively delete an
    // object whose type the product never creates.
    let dir =
        std::env::temp_dir().join(format!("oopsinput-purge-recursive-{}", std::process::id()));
    let event_dir = dir.join("events.jsonl");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&event_dir).unwrap();
    std::fs::write(event_dir.join("precious.txt"), "PRECIOUS\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oopsinput"))
        .arg("purge")
        .env("OOPSINPUT_STATE_DIR", &dir)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing recursive deletion"),
        "{output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(event_dir.join("precious.txt")).unwrap(),
        "PRECIOUS\n"
    );
    assert!(!dir.join(LOCK_FILE_FOR_ASSERTION).exists());
    let _ = std::fs::remove_dir_all(&dir);
}
