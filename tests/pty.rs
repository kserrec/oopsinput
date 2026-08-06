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

    /// Interactive zsh under a PTY (util-linux `script`) with this session's
    /// isolated ZDOTDIR.
    fn spawn_zsh(&self) -> std::process::Child {
        Command::new("script")
            .args(["-qec", "zsh -i", "/dev/null"])
            .env("ZDOTDIR", &self.dir)
            .env("TERM", "xterm")
            .env_remove("OOPSINPUT_BIN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn script+zsh")
    }

    /// The session's event log; panics if no event was written.
    fn events_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("state/events.jsonl")).expect("event log written")
    }

    /// Feed keystroke lines to the interactive shell, return everything the
    /// terminal displayed. `exit` is appended automatically.
    fn run(&self, lines: &[&str]) -> String {
        let mut child = self.spawn_zsh();

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
    ///
    /// A missing marker panics with the transcript instead of hanging the
    /// suite: reads run on a helper thread and the wait is bounded (probed
    /// 2026-08-06 — a marker typo hung `cargo test` for its full timeout).
    fn run_staged(&self, stages: &[(&str, u64, &str)]) -> String {
        use std::io::Read;
        use std::sync::mpsc;
        let mut child = self.spawn_zsh();

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

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut seen: Vec<u8> = Vec::new();
        for (wait_for, delay_ms, send) in stages {
            while !String::from_utf8_lossy(&seen).contains(wait_for) {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(
                    !left.is_zero(),
                    "marker {wait_for:?} never appeared; transcript so far:\n{}",
                    String::from_utf8_lossy(&seen)
                );
                match rx.recv_timeout(left) {
                    Ok(bytes) => seen.extend_from_slice(&bytes),
                    Err(_) => {
                        let _ = child.kill();
                        panic!(
                            "marker {wait_for:?} never appeared; transcript so far:\n{}",
                            String::from_utf8_lossy(&seen)
                        );
                    }
                }
            }
            if *delay_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
            }
            let _ = stdin.write_all(send.as_bytes());
        }
        let _ = stdin.write_all(b"exit\r");
        drop(stdin);
        // Drain whatever the session prints on its way out. A silent-but-
        // alive session is killed rather than waited on — otherwise the
        // child.wait() below would block forever on a shell that wedged
        // after `exit` (bughunt 2026-08-06).
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
        let _ = child.wait();
        let _ = reader.join();
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

    let log = s.events_log();
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

    let log = s.events_log();
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

    let log = s.events_log();
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

    let log = s.events_log();
    assert!(
        log.contains("\"decision\":\"replace\"") && log.contains("typo.accepted"),
        "accepted outcome not recorded:\n{log}"
    );
}

#[test]
fn suggest_mode_resolving_words_never_prompt() {
    // M2 acceptance: zero tolerance for L1 false positives — every resolution
    // kind that can actually run must pass through silently even with prompts
    // enabled.
    let s = Session::new(None, &[("OOPSINPUT_MODE", "suggest")]);
    let out = s.run(&[
        "alias myok='echo ALIASRAN-$((6+7))'",
        "myok",                                        // alias
        "echo builtin-ran",                            // builtin
        "/bin/ls /tmp >/dev/null && echo command-ran", // command + chain
        "true && echo chain-ran",
    ]);
    assert!(
        !out.contains("[y/n]"),
        "a resolving command word prompted:\n{out}"
    );
    assert!(out.contains("ALIASRAN-13"), "alias did not run:\n{out}");
    assert!(out.contains("builtin-ran"), "builtin did not run:\n{out}");
    assert!(out.contains("command-ran"), "command did not run:\n{out}");
    assert!(out.contains("chain-ran"), "chain did not run:\n{out}");
}

#[test]
fn config_file_mode_suggest_enables_prompts() {
    // The exact artifact install.zsh writes must turn prompts on through the
    // real config path (no env override involved).
    let s = Session::new(None, &[]);
    let confdir = s.dir.join("config/oopsinput");
    std::fs::create_dir_all(&confdir).unwrap();
    std::fs::write(
        confdir.join("config"),
        "# oopsinput config (see SPEC.md §15 for the full surface)\nmode = suggest   # shadow | suggest | warn | confirm\n",
    )
    .unwrap();

    let out = s.run_staged(&[
        (
            "PTYTEST%",
            0,
            "alias oopspecialf='echo CONFIGRAN-$((5+6))'\recho marker-f1\r",
        ),
        ("marker-f1", 0, "oopspecialfq\r"),
        ("[y/n]", 0, "yecho marker-f2\r"),
    ]);
    assert!(
        out.contains("CONFIGRAN-11"),
        "config-file suggest mode did not prompt/correct:\n{out}"
    );
    assert!(
        out.contains("marker-f2"),
        "session did not continue:\n{out}"
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

    let log = s.events_log();
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

    let log = s.events_log();
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

// ---- M3 warning UI: the flagship pair, end-to-end ----

/// A repo with one committed file that has uncommitted modifications —
/// exactly the context in which `git reset --hard` destroys work.
fn make_repo(s: &Session, dirty: bool) -> PathBuf {
    let repo = s.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run git")
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    std::fs::write(repo.join("tracked.txt"), "clean\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "-m",
        "c",
    ]);
    if dirty {
        std::fs::write(repo.join("tracked.txt"), "DIRTY\n").unwrap();
    }
    repo
}

/// The shared opening of every warning-flow test: a warn-mode session with
/// a fixture repo, plus the first stage's send-string (enter the repo, print
/// `marker`).
fn warn_session(
    dirty: bool,
    marker: &str,
    extra_env: &[(&str, &str)],
) -> (Session, PathBuf, String) {
    let mut env = vec![("OOPSINPUT_MODE", "warn")];
    env.extend_from_slice(extra_env);
    let s = Session::new(None, &env);
    let repo = make_repo(&s, dirty);
    let cd = format!("cd {repo:?}\recho {marker}\r");
    (s, repo, cd)
}

#[test]
fn warn_mode_dirty_reset_cancel_has_zero_side_effects() {
    // M3 acceptance, flagship half 1: dirty `git reset --hard` warns with
    // the facts named; cancel runs nothing and the dirty work survives.
    // The 300 ms pause proves the watchdog retires for warning prompts too.
    let (s, repo, cd) = warn_session(true, "marker-w1", &[]);
    let out = s.run_staged(&[
        ("PTYTEST%", 0, &cd),
        ("marker-w1", 0, "git reset --hard\r"),
        ("[e]dit", 300, "cecho marker-w2\r"),
    ]);
    assert!(
        out.contains("discard uncommitted changes"),
        "warning anatomy line missing:\n{out}"
    );
    assert!(
        out.contains("1 modified tracked file"),
        "context facts not named:\n{out}"
    );
    assert!(
        out.contains("marker-w2"),
        "session did not continue:\n{out}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "DIRTY\n",
        "cancel must leave the dirty work untouched"
    );
    let log = s.events_log();
    assert!(
        log.contains("\"decision\":\"warn\"")
            && log.contains("policy.dirty_work_at_risk")
            && log.contains("\"outcome\":\"cancelled\""),
        "cancelled warning not recorded:\n{log}"
    );
}

#[test]
fn warn_mode_dirty_reset_run_once_executes_unchanged() {
    let (s, repo, cd) = warn_session(true, "marker-r1", &[]);
    let out = s.run_staged(&[
        ("PTYTEST%", 0, &cd),
        ("marker-r1", 0, "git reset --hard\r"),
        ("[e]dit", 0, "recho marker-r2\r"),
    ]);
    assert!(
        out.contains("marker-r2"),
        "session did not continue:\n{out}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "clean\n",
        "run-once must execute the original command unchanged"
    );
    let log = s.events_log();
    assert!(
        log.contains("\"outcome\":\"ran_unchanged\""),
        "run-once outcome not recorded:\n{log}"
    );
}

#[test]
fn warn_mode_edit_restores_the_exact_buffer_to_zle() {
    let (s, repo, cd) = warn_session(true, "marker-e1", &[]);
    // e → the original buffer must come back editable; Ctrl-U kills it and a
    // replacement line proves ZLE was live for editing.
    let out = s.run_staged(&[
        ("PTYTEST%", 0, &cd),
        ("marker-e1", 0, "git reset --hard\r"),
        ("[e]dit", 0, "e"),
        ("git reset --hard", 200, "\u{15}echo edited-ok\r"),
    ]);
    assert!(out.contains("edited-ok"), "editing did not work:\n{out}");
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "DIRTY\n",
        "edit must not run the command"
    );
    // The command text appears at least twice: once as typed, once redrawn
    // in ZLE after exit 11 restored the buffer.
    assert!(
        out.matches("git reset --hard").count() >= 2,
        "restored buffer not redrawn:\n{out}"
    );
    let log = s.events_log();
    assert!(
        log.contains("\"outcome\":\"edited\""),
        "edited outcome not recorded:\n{log}"
    );
}

#[test]
fn warn_mode_clean_reset_is_silently_allowed() {
    // M3 acceptance, flagship half 2 (the counterfactual): the identical
    // command on a clean tree passes without a word.
    let (s, _repo, cd) = warn_session(false, "marker-s1", &[]);
    let out = s.run_staged(&[
        ("PTYTEST%", 0, &cd),
        ("marker-s1", 0, "git reset --hard\recho marker-s2\r"),
    ]);
    assert!(
        out.contains("marker-s2"),
        "session did not continue:\n{out}"
    );
    assert!(
        !out.contains("[e]dit"),
        "clean-tree reset must not warn:\n{out}"
    );
    let log = s.events_log();
    assert!(
        log.contains("policy.context_clear"),
        "silent allow not recorded with its reason:\n{log}"
    );
}

#[test]
fn warning_names_the_previous_command_from_recency() {
    // SPEC §5-L3 flagship phrasing: "git reset --hard … right after git
    // diff". The recency relation is computed in the plugin (no raw history
    // text crosses) and surfaces as a "previous command:" line.
    let (s, _repo, cd) = warn_session(true, "marker-p1", &[("GIT_PAGER", "cat")]);
    let out = s.run_staged(&[
        ("PTYTEST%", 0, &cd),
        // `git diff` output proves completion without adding another history
        // entry in between. Wait on the deletion line: color rendering keeps
        // "-clean" in one ANSI span, while "+DIRTY" gets split by the
        // added-whitespace highlight (probed: the first marker hung).
        ("marker-p1", 0, "git diff\r"),
        ("-clean", 0, "git reset --hard\r"),
        ("[e]dit", 0, "cecho marker-p2\r"),
    ]);
    assert!(
        out.contains("previous command: git diff"),
        "recency line missing from warning:\n{out}"
    );
    assert!(
        out.contains("marker-p2"),
        "session did not continue:\n{out}"
    );
}

#[test]
fn warning_outranks_the_typo_prompt_on_compound_buffers() {
    // Regression (bughunt 2026-08-06): `oopspecialvq; git reset --hard` has
    // an unresolvable first word AND a dangerous second segment. `;` does
    // not short-circuit, so the reset runs whichever way a typo question is
    // answered — the warning must win the prompt, not the typo chat.
    let (s, repo, cd) = warn_session(true, "marker-x1", &[]);
    let setup = format!("alias oopspecialv='echo NOPE'\r{cd}");
    let out = s.run_staged(&[
        ("PTYTEST%", 0, &setup),
        ("marker-x1", 0, "oopspecialvq; git reset --hard\r"),
        ("[e]dit", 300, "cecho marker-x2\r"),
    ]);
    assert!(
        !out.contains("[y/n]"),
        "typo prompt shown instead of the warning:\n{out}"
    );
    assert!(
        out.contains("discard uncommitted changes"),
        "warning missing:\n{out}"
    );
    assert!(
        out.contains("marker-x2"),
        "session did not continue:\n{out}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "DIRTY\n",
        "cancel must run nothing"
    );
}

#[test]
fn recency_overlap_counts_shared_targets_not_shared_flags() {
    // Regression (bughunt 2026-08-06): `ls -f /tmp` then `rm -f x` scored
    // recency.target_overlap from the shared "-f" — flag words are not
    // "you referenced this target moments ago".
    let (s, _repo, cd) = warn_session(false, "marker-o1", &[]);
    let out = s.run_staged(&[
        ("PTYTEST%", 0, &cd),
        // pair 1: only the flag "-f" is shared with the previous command
        (
            "marker-o1",
            0,
            "ls -f /tmp >/dev/null\rrm -f zzq-nonexistent\recho marker-o2\r",
        ),
        // pair 2: the target word itself is shared with the previous command
        (
            "marker-o2",
            0,
            "touch zz-shared\rrm -f zz-shared\recho marker-o3\r",
        ),
    ]);
    assert!(
        out.contains("marker-o3"),
        "session did not continue:\n{out}"
    );
    let log = s.events_log();
    let rm_events: Vec<&str> = log.lines().filter(|l| l.contains("fs.rm_force")).collect();
    assert_eq!(rm_events.len(), 2, "expected two rm events:\n{log}");
    assert!(
        !rm_events[0].contains("recency.target_overlap"),
        "shared flag scored as target overlap:\n{}",
        rm_events[0]
    );
    assert!(
        rm_events[1].contains("recency.target_overlap"),
        "genuine shared target not scored:\n{}",
        rm_events[1]
    );
}

#[test]
fn warn_mode_benign_commands_stay_silent() {
    let s = Session::new(None, &[("OOPSINPUT_MODE", "warn")]);
    let out = s.run(&["echo warn-benign-ok"]);
    assert!(out.contains("warn-benign-ok"));
    assert!(
        !out.contains("oopsinput:"),
        "benign command provoked output:\n{out}"
    );
}

#[test]
fn arrow_keys_at_a_prompt_leave_no_stray_bytes() {
    // Regression (bughunt 2026-08-06, fixed in the M3 key-reader rebuild):
    // an arrow key at the prompt was read as ESC + leftover bytes, and the
    // leftovers ("[A") leaked into the next ZLE buffer as stray characters.
    let s = Session::new(None, &[("OOPSINPUT_MODE", "suggest")]);
    let out = s.run_staged(&[
        (
            "PTYTEST%",
            0,
            "alias oopspeciala='echo WRONG-$((8+9))'\recho marker-a1\r",
        ),
        ("marker-a1", 0, "oopspecialaq\r"),
        // Up arrow first — must be consumed whole and ignored — then n.
        ("[y/n]", 200, "\u{1b}[Anecho marker-a2\r"),
    ]);
    assert!(
        out.contains("command not found: oopspecialaq"),
        "n after arrow did not run the original:\n{out}"
    );
    assert!(
        out.contains("marker-a2"),
        "session did not continue:\n{out}"
    );
    assert!(
        !out.contains("[Aecho") && !out.contains("command not found: [A"),
        "arrow-key bytes leaked into the next buffer:\n{out}"
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
