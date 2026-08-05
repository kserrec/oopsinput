//! PTY integration tests: a real interactive zsh (ZLE active) inside a
//! pseudo-terminal provided by util-linux `script`, with the plugin loaded
//! from an isolated ZDOTDIR. These tests are the product: buffer exactness
//! and fail-open behavior under every failure mode we can simulate.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Session {
    dir: PathBuf,
}

impl Session {
    /// Create an isolated ZDOTDIR whose .zshrc loads the plugin with the
    /// freshly built test binary (or an override path for failure tests).
    fn new(bin_override: Option<&str>, extra_env: &[(&str, &str)]) -> Session {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oopsinput-pty-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let bin = bin_override
            .map(String::from)
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_oopsinput").to_string());
        let plugin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("zsh/oopsinput.zsh");

        let mut zshrc = String::new();
        zshrc.push_str("PS1='PTYTEST%% '\n");
        zshrc.push_str(&format!("export OOPSINPUT_BIN={bin:?}\n"));
        zshrc.push_str(&format!(
            "export OOPSINPUT_STATE_DIR={:?}\n",
            dir.join("state")
        ));
        for (k, v) in extra_env {
            zshrc.push_str(&format!("export {k}={v}\n"));
        }
        zshrc.push_str(&format!("source {plugin:?}\n"));
        std::fs::write(dir.join(".zshrc"), zshrc).unwrap();

        Session { dir }
    }

    /// Feed keystroke lines to the interactive shell, return everything the
    /// terminal displayed. `exit` is appended automatically.
    fn run(&self, lines: &[&str]) -> String {
        let mut child = Command::new("script")
            .args(["-qec", "zsh -i", "/dev/null"])
            .env("ZDOTDIR", &self.dir)
            .env("TERM", "xterm")
            .env_remove("OOPSINPUT_BIN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn script+zsh");

        {
            let stdin = child.stdin.as_mut().unwrap();
            for line in lines {
                stdin.write_all(line.as_bytes()).unwrap();
                stdin.write_all(b"\r").unwrap();
            }
            stdin.write_all(b"exit\r").unwrap();
        }

        let out = child.wait_with_output().expect("collect output");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn ordinary_commands_pass_through_unchanged() {
    let s = Session::new(None, &[]);
    let out = s.run(&[
        "echo pty-a-$((20+3))",
        "echo pty-b-$(echo nested)",
        "true && echo pty-c-ok",
    ]);
    assert!(
        out.contains("pty-a-23"),
        "arithmetic output missing:\n{out}"
    );
    assert!(
        out.contains("pty-b-nested"),
        "substitution output missing:\n{out}"
    );
    assert!(out.contains("pty-c-ok"), "&& chain output missing:\n{out}");
}

#[test]
fn unicode_and_quoting_survive() {
    let s = Session::new(None, &[]);
    let out = s.run(&[
        "echo 'pty-q \"double inside\" $notexpanded'",
        "echo pty-u-dziękuję-嗯",
    ]);
    assert!(
        out.contains("pty-q \"double inside\" $notexpanded"),
        "quoted literal altered:\n{out}"
    );
    assert!(out.contains("pty-u-dziękuję-嗯"), "unicode altered:\n{out}");
}

#[test]
fn multiline_continuation_passes() {
    let s = Session::new(None, &[]);
    // Unclosed quote forces a PS2 continuation line; plugin must pass it through.
    let out = s.run(&["echo 'pty-m-one", "pty-m-two'"]);
    assert!(out.contains("pty-m-one"), "line 1 missing:\n{out}");
    assert!(out.contains("pty-m-two"), "line 2 missing:\n{out}");
}

#[test]
fn missing_binary_fails_open() {
    let s = Session::new(Some("/nonexistent/oopsinput"), &[]);
    let out = s.run(&["echo pty-nofail-ok"]);
    assert!(
        out.contains("pty-nofail-ok"),
        "command lost with missing binary:\n{out}"
    );
}

#[test]
fn hanging_binary_fails_open_within_deadline() {
    // Binary sleeps 30s; watchdog must fail open at the (shortened) deadline.
    let s = Session::new(
        None,
        &[
            ("OOPSINPUT_TEST_HANG", "1"),
            ("OOPSINPUT_TEST_DEADLINE_MS", "100"),
        ],
    );
    let started = std::time::Instant::now();
    let out = s.run(&["echo pty-hang-ok"]);
    assert!(
        out.contains("pty-hang-ok"),
        "command lost with hanging binary:\n{out}"
    );
    assert!(
        started.elapsed().as_secs() < 10,
        "watchdog did not fire; session took {:?}",
        started.elapsed()
    );
}

#[test]
fn events_are_recorded_and_structural_only() {
    let s = Session::new(None, &[]);
    let secret = "export API_KEY=sk-super-secret-value-12345";
    let out = s.run(&[secret, "echo pty-ev-ok"]);
    assert!(out.contains("pty-ev-ok"), "commands did not run:\n{out}");

    let log = std::fs::read_to_string(s.dir.join("state/events.jsonl")).expect("event log written");
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines.len() >= 2, "expected >=2 events, got:\n{log}");
    assert!(
        !log.contains("sk-super-secret-value"),
        "raw secret leaked into event log:\n{log}"
    );
    assert!(
        !log.contains("API_KEY"),
        "raw command text leaked into event log:\n{log}"
    );
    assert!(log.contains("\"decision\":\"allow\""));
    assert!(log.contains("\"reason_code\":\"shadow.observed\""));
}

#[test]
fn resolution_kinds_are_correctly_extracted() {
    // Regression: the plugin once passed the whole "word: kind" whence output,
    // collapsing every res_kind to "other" (and leaking the word into argv).
    let s = Session::new(None, &[]);
    let out = s.run(&[
        "alias myls='ls -l'",
        "myls /tmp", // alias, multi-word buffer
        "myls",      // alias, single-word buffer (regression:
        //                            single-word split string-indexed to "m")
        "echo pty-res-ok",         // builtin
        "/bin/ls /tmp",            // command
        "definitely-not-real-xyz", // none
    ]);
    assert!(out.contains("pty-res-ok"), "commands did not run:\n{out}");

    let log = std::fs::read_to_string(s.dir.join("state/events.jsonl")).expect("event log written");
    assert_eq!(
        log.matches("\"res_kind\":\"alias\"").count(),
        2,
        "expected alias kind from both multi-word and single-word buffers:\n{log}"
    );
    assert!(
        log.contains("\"res_kind\":\"builtin\""),
        "builtin kind missing:\n{log}"
    );
    assert!(
        log.contains("\"res_kind\":\"command\""),
        "command kind missing:\n{log}"
    );
    assert!(
        log.contains("\"res_kind\":\"none\""),
        "none kind missing:\n{log}"
    );
    assert!(
        !log.contains("\"res_kind\":\"other\""),
        "vocabulary leak — 'other' should not appear from the real plugin:\n{log}"
    );
}

#[test]
fn double_source_is_harmless() {
    let s = Session::new(None, &[]);
    let plugin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("zsh/oopsinput.zsh");
    let out = s.run(&[
        &format!("source {plugin:?}"),
        &format!("source {plugin:?}"),
        "echo pty-double-ok",
    ]);
    assert!(
        out.contains("pty-double-ok"),
        "command lost after double source:\n{out}"
    );
}

#[test]
fn vi_mode_accepts_commands() {
    let s = Session::new(None, &[]);
    let out = s.run(&["bindkey -v", "echo pty-vi-ok"]);
    assert!(out.contains("pty-vi-ok"), "vi keymap broke accept:\n{out}");
}
