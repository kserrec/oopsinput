//! PTY integration tests: a real interactive zsh (ZLE active) inside a
//! pseudo-terminal provided by util-linux `script`, with the plugin loaded
//! from an isolated ZDOTDIR. These tests are the product: buffer exactness
//! and fail-open behavior under every failure mode we can simulate.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Render a string as a zsh $'\xNN' literal, byte-exact.
fn zsh_byte_quote(s: &str) -> String {
    let hex: String = s.bytes().map(|b| format!("\\x{b:02x}")).collect();
    format!("$'{hex}'")
}

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
        // $'\xNN' quoting so override paths can carry raw bytes (the escape-
        // injection test needs a real ESC byte to reach the env var).
        zshrc.push_str(&format!("export OOPSINPUT_BIN={}\n", zsh_byte_quote(&bin)));
        zshrc.push_str(&format!(
            "export OOPSINPUT_STATE_DIR={:?}\n",
            dir.join("state")
        ));
        // Isolate config resolution: the developer's real ~/.config must
        // never leak a mode into tests. Tests opt into modes via extra_env.
        zshrc.push_str(&format!(
            "export XDG_CONFIG_HOME={:?}\n",
            dir.join("config")
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

    /// Interactive variant: each stage waits until the cumulative terminal
    /// output contains a marker, optionally pauses, then sends raw bytes.
    /// Needed wherever timing matters — e.g. a 0x03 sent before the binary's
    /// terminal mode switch would become SIGINT instead of a key. `exit` is
    /// appended automatically.
    fn run_staged(&self, stages: &[(&str, u64, &str)]) -> String {
        use std::io::Read;
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

        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut seen: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        for (wait_for, delay_ms, send) in stages {
            while !String::from_utf8_lossy(&seen).contains(wait_for) {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.extend_from_slice(&chunk[..n]),
                }
            }
            if *delay_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
            }
            let _ = stdin.write_all(send.as_bytes());
        }
        let _ = stdin.write_all(b"exit\r");
        drop(stdin);
        let _ = stdout.read_to_end(&mut seen);
        let _ = child.wait();
        String::from_utf8_lossy(&seen).into_owned()
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
fn missing_binary_diagnostic_escapes_hostile_path() {
    // Audit finding #4: the load diagnostic prints the env-supplied binary
    // path; a value carrying terminal escape sequences must not be rendered
    // raw (here: an OSC title-set sequence).
    let hostile = "/nonexistent/\u{1b}]0;EVIL\u{7}x";
    let s = Session::new(Some(hostile), &[]);
    let out = s.run(&["echo pty-esc-ok"]);
    assert!(out.contains("pty-esc-ok"), "commands did not run:\n{out}");
    assert!(
        !out.contains("\u{1b}]0;EVIL"),
        "raw escape sequence from OOPSINPUT_BIN reached the terminal:\n{out:?}"
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
        // alias, single-word buffer — regression: the (z) split of a single
        // word once got string-indexed, resolving "m" instead of "myls"
        "myls",
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
fn typo_near_alias_records_candidate_evidence() {
    // End-to-end L1 shadow path: an unresolvable word one edit away from an
    // alias must record typo-candidate evidence — and neither the typed word
    // nor the candidate name may reach the log (evidence codes only).
    let s = Session::new(None, &[]);
    let out = s.run(&[
        "alias oopspecialx='echo pty-aliased'",
        "oopspecialxq", // none: distance 1 from the alias
        "oopspecialx",  // resolves: must never produce typo evidence
        "echo pty-typo-ok",
    ]);
    assert!(out.contains("pty-typo-ok"), "commands did not run:\n{out}");
    assert!(
        out.contains("pty-aliased"),
        "alias itself did not run:\n{out}"
    );

    let log = std::fs::read_to_string(s.dir.join("state/events.jsonl")).expect("event log written");
    assert_eq!(
        log.matches("typo.candidate_d1").count(),
        1,
        "expected exactly one typo candidate event:\n{log}"
    );
    assert!(
        !log.contains("oopspecial"),
        "typed word or candidate name leaked into the log:\n{log}"
    );
}

#[test]
fn suggest_mode_y_runs_the_correction_exactly() {
    // Full L1 flow: typo near an alias → prompt → y → corrected buffer runs
    // with every other byte (the --flag argument) preserved. The 300 ms pause
    // before answering sits past the 150 ms analysis deadline, proving the
    // watchdog retires while a prompt is on screen.
    // Executed markers use $((...)) so the echoed command line (which shows
    // the source text) can never satisfy an assertion about executed output.
    let s = Session::new(None, &[("OOPSINPUT_MODE", "suggest")]);
    let out = s.run_staged(&[
        (
            "PTYTEST%",
            0,
            "alias oopspecialx='echo CORRECTED-$((2+3))'\recho marker-y1\r",
        ),
        ("marker-y1", 0, "oopspecialxq --flag\r"),
        ("[y/n]", 300, "yecho marker-y2\r"),
    ]);
    assert!(
        out.contains("did you mean 'oopspecialx'?"),
        "prompt missing or wrong candidate:\n{out}"
    );
    assert!(
        out.contains("CORRECTED-5 --flag"),
        "corrected command did not run with args preserved:\n{out}"
    );
    assert!(
        !out.contains("command not found"),
        "original typo ran despite y:\n{out}"
    );
    assert!(
        out.contains("marker-y2"),
        "session did not continue:\n{out}"
    );

    let log = std::fs::read_to_string(s.dir.join("state/events.jsonl")).expect("event log written");
    assert!(
        log.contains("\"decision\":\"replace\"") && log.contains("typo.accepted"),
        "accepted outcome not recorded:\n{log}"
    );
}

#[test]
fn suggest_mode_n_runs_the_original_unchanged() {
    let s = Session::new(None, &[("OOPSINPUT_MODE", "suggest")]);
    let out = s.run_staged(&[
        (
            "PTYTEST%",
            0,
            "alias oopspecialn='echo WRONG-$((3+4))'\recho marker-n1\r",
        ),
        ("marker-n1", 0, "oopspecialnq\r"),
        ("[y/n]", 0, "necho marker-n2\r"),
    ]);
    assert!(
        out.contains("command not found: oopspecialnq"),
        "original did not run after n:\n{out}"
    );
    assert!(!out.contains("WRONG-7"), "correction ran despite n:\n{out}");
    assert!(
        out.contains("marker-n2"),
        "session did not continue:\n{out}"
    );

    let log = std::fs::read_to_string(s.dir.join("state/events.jsonl")).expect("event log written");
    assert!(
        log.contains("typo.declined"),
        "declined outcome not recorded:\n{log}"
    );
}

#[test]
fn suggest_mode_ctrl_c_cancels_running_nothing() {
    let s = Session::new(None, &[("OOPSINPUT_MODE", "suggest")]);
    let out = s.run_staged(&[
        (
            "PTYTEST%",
            0,
            "alias oopspecialc='echo WRONG-$((4+5))'\recho marker-c1\r",
        ),
        ("marker-c1", 0, "oopspecialcq\r"),
        ("[y/n]", 0, "\u{3}echo marker-c2\r"),
    ]);
    assert!(
        !out.contains("command not found"),
        "original ran despite cancel:\n{out}"
    );
    assert!(
        !out.contains("WRONG-9"),
        "correction ran despite cancel:\n{out}"
    );
    assert!(
        out.contains("marker-c2"),
        "session did not continue:\n{out}"
    );

    let log = std::fs::read_to_string(s.dir.join("state/events.jsonl")).expect("event log written");
    assert!(
        log.contains("\"decision\":\"cancel\"") && log.contains("typo.cancelled"),
        "cancelled outcome not recorded:\n{log}"
    );
}

/// Drive the binary's debug-only prompt seam directly under a PTY (no zsh):
/// proves the real /dev/tty open, stty mode switch, and single-key read work.
/// Keys are sent only once the prompt is visible — a real user cannot press
/// earlier, and before the mode switch the pty is still in canonical mode
/// with ISIG on, so an early 0x03 would become SIGINT instead of a key
/// (probed and confirmed 2026-08-06).
fn run_prompt_seam(keys: &[u8]) -> String {
    use std::io::Read;
    let mut child = Command::new("script")
        .args([
            "-qec",
            &format!("{} __prompt-typo-test", env!("CARGO_BIN_EXE_oopsinput")),
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn script+oopsinput");

    let mut stdout = child.stdout.take().unwrap();
    let mut seen = Vec::new();
    let mut chunk = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains("[y/n]") {
        match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break, // EOF before prompt: fall through to asserts
            Ok(n) => seen.extend_from_slice(&chunk[..n]),
        }
    }

    child.stdin.as_mut().unwrap().write_all(keys).unwrap();
    drop(child.stdin.take());
    let _ = stdout.read_to_end(&mut seen);
    let _ = child.wait();
    String::from_utf8_lossy(&seen).into_owned()
}

#[test]
fn prompt_reads_single_key_on_real_tty() {
    // A bare 'y' with no Enter must resolve the prompt (single-key contract).
    let out = run_prompt_seam(b"y");
    assert!(
        out.contains("did you mean 'git'?"),
        "prompt text missing:\n{out}"
    );
    assert!(out.contains("choice=Correct"), "y did not consent:\n{out}");
}

#[test]
fn prompt_ctrl_c_cancels_on_real_tty() {
    // ISIG is disabled during the prompt: 0x03 must arrive as a key meaning
    // cancel, not kill the process (which would read as fail-open).
    let out = run_prompt_seam(&[0x03]);
    assert!(
        out.contains("choice=Cancel"),
        "Ctrl-C did not cancel:\n{out}"
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
