//! Pinned tests for zsh/uninstall.zsh (audit finding #1): a damaged marker
//! block must cause refusal, never a delete-to-end-of-file.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("zsh/uninstall.zsh")
}

/// Run uninstall.zsh against a fake HOME containing the given .zshrc.
fn run_uninstall(zshrc: &str) -> (bool, String) {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let home = std::env::temp_dir().join(format!("oopsinput-uninst-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".zshrc"), zshrc).unwrap();

    let out = Command::new("zsh")
        .arg(script())
        .env("HOME", &home)
        .output()
        .expect("run uninstall.zsh");

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
fn no_block_is_a_clean_noop() {
    let original = "just a normal zshrc\nwith lines\n";
    let (ok, result) = run_uninstall(original);
    assert!(ok, "uninstall without a block should succeed as a no-op");
    assert_eq!(result, original);
}
