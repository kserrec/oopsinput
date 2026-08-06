//! End-to-end model path through the real binary (M4): the candidate gate,
//! the watchdog's model-deadline extension, and deterministic fallback,
//! driven against mock Ollama servers on loopback. The binary's endpoint
//! port comes from the debug-only OOPSINPUT_TEST_OLLAMA_PORT seam; the
//! address itself stays 127.0.0.1 by construction.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct TestEnv {
    base: PathBuf,
}

impl TestEnv {
    fn new(name: &str, config: &str) -> TestEnv {
        let base = std::env::temp_dir().join(format!("oopsinput-mp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home")).unwrap();
        std::fs::create_dir_all(base.join("xdg/oopsinput")).unwrap();
        std::fs::create_dir_all(base.join("state")).unwrap();
        std::fs::create_dir_all(base.join("cwd")).unwrap();
        std::fs::write(base.join("xdg/oopsinput/config"), config).unwrap();
        TestEnv { base }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Run `oopsinput check` with the buffer on stdin against this env, the mock
/// port, and an optional debug-deadline override. Returns (decision JSON,
/// exit code, elapsed).
fn run_check(
    env: &TestEnv,
    buffer: &str,
    port: u16,
    deadline_ms: Option<u64>,
) -> (Option<serde_json::Value>, i32, Duration) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oopsinput"));
    cmd.args(["check", "--res", "command"])
        .current_dir(env.base.join("cwd"))
        .env("HOME", env.base.join("home"))
        .env("XDG_CONFIG_HOME", env.base.join("xdg"))
        .env("OOPSINPUT_STATE_DIR", env.base.join("state"))
        .env("OOPSINPUT_TEST_OLLAMA_PORT", port.to_string())
        .env_remove("OOPSINPUT_MODE")
        .env_remove("OOPSINPUT_TEST_HANG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match deadline_ms {
        Some(ms) => {
            cmd.env("OOPSINPUT_TEST_DEADLINE_MS", ms.to_string());
        }
        None => {
            cmd.env_remove("OOPSINPUT_TEST_DEADLINE_MS");
        }
    }
    let started = Instant::now();
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(buffer.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let decision = stdout
        .lines()
        .last()
        .and_then(|l| serde_json::from_str(l).ok());
    (decision, out.status.code().unwrap_or(-1), elapsed)
}

fn evidence_codes(decision: &serde_json::Value) -> Vec<String> {
    decision["evidence"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A one-shot mock Ollama: reads the full request, waits `delay`, answers
/// with `content` as the chat message. Returns the port and a
/// was-connected flag.
fn mock_ollama(content: &'static str, delay: Duration) -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connected = Arc::new(AtomicBool::new(false));
    let flag = connected.clone();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            flag.store(true, Ordering::SeqCst);
            read_full_request(&mut s);
            std::thread::sleep(delay);
            let body = serde_json::json!({
                "model": "mock",
                "message": { "role": "assistant", "content": content },
                "done": true,
            })
            .to_string();
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    (port, connected)
}

fn read_full_request(s: &mut TcpStream) {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "client closed before finishing its request");
        got.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&got[..head_end]).into_owned();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    while got.len() < head_end + 4 + len {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
}

const PROBABLE: &str =
    r#"{"assessment":"probable_mismatch","mismatch_kind":"target","reason":"looks off"}"#;
const CONFIG: &str = "model = mock\n";

#[test]
fn ambiguous_candidate_consults_and_records_model_warn_in_shadow() {
    let env = TestEnv::new("consult", CONFIG);
    let (port, connected) = mock_ollama(PROBABLE, Duration::ZERO);
    // git reset --hard outside any repo: candidate + ambiguous → gate opens.
    let (decision, code, _) = run_check(&env, "git reset --hard", port, None);
    let decision = decision.expect("decision JSON");
    assert_eq!(code, 0);
    assert!(connected.load(Ordering::SeqCst), "model was not consulted");
    // Default mode is shadow: the model-driven warn is recorded, not shown —
    // decision observe, the would-be reason preserved (the shadow conversion).
    assert_eq!(decision["decision"], "observe", "{decision}");
    assert_eq!(
        decision["reason_code"], "policy.model_mismatch",
        "{decision}"
    );
    assert!(
        evidence_codes(&decision).contains(&"model.probable_mismatch".to_string()),
        "{decision}"
    );
}

#[test]
fn model_down_falls_back_deterministic_and_fast() {
    let env = TestEnv::new("down", CONFIG);
    // Bind then drop: nothing listens on this port.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let started = Instant::now();
    let (decision, code, _) = run_check(&env, "git reset --hard", port, None);
    let decision = decision.expect("decision JSON");
    assert_eq!(code, 0);
    // Deterministic answer stands; the missing evidence is recorded (SPEC
    // §5-L4: fallback must be distinguishable from success in the log).
    assert_eq!(
        decision["reason_code"], "policy.evidence_unavailable",
        "{decision}"
    );
    assert!(
        evidence_codes(&decision).contains(&"model.unreachable".to_string()),
        "{decision}"
    );
    // A refused loopback connect is immediate — no user-visible stall.
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn slow_model_outlives_deterministic_watchdog() {
    // The model answers 400 ms in — far past the 150 ms deterministic
    // deadline. Only the watchdog's one-shot model extension lets the
    // process live to deliver the verdict. Probed 2026-08-06: with the
    // extension store commented out, the binary was watchdog-killed at the
    // deterministic deadline — no decision JSON, the whole run over in
    // ~160 ms instead of the 400+ the model path needs.
    let env = TestEnv::new("slow", CONFIG);
    let (port, _) = mock_ollama(PROBABLE, Duration::from_millis(400));
    let (decision, code, elapsed) = run_check(&env, "git reset --hard", port, Some(150));
    let decision = decision.expect("decision JSON (watchdog must not kill the model path)");
    assert_eq!(code, 0);
    assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
    assert_eq!(
        decision["reason_code"], "policy.model_mismatch",
        "{decision}"
    );
}

#[test]
fn hanging_model_bounded_by_its_own_timeout() {
    // The server accepts and never answers. consult()'s own deadline
    // (model_timeout_ms) must cut it loose — deterministic fallback, well
    // before the mock's 5 s nap and the watchdog extension's backstop.
    let env = TestEnv::new("hang", "model = mock\nmodel_timeout_ms = 300\n");
    let (port, _) = mock_ollama(PROBABLE, Duration::from_secs(5));
    let started = Instant::now();
    let (decision, code, _) = run_check(&env, "git reset --hard", port, None);
    let decision = decision.expect("decision JSON");
    assert_eq!(code, 0);
    assert!(started.elapsed() < Duration::from_millis(2_000));
    assert_eq!(
        decision["reason_code"], "policy.evidence_unavailable",
        "{decision}"
    );
    assert!(
        evidence_codes(&decision).contains(&"model.timeout".to_string()),
        "{decision}"
    );
}

#[test]
fn benign_and_catastrophic_commands_never_consult() {
    // The two ends of the spectrum stay model-free: no candidate means no
    // gate, and direct-catastrophic is excluded by design (M4 acceptance:
    // the model can never touch that path).
    let env = TestEnv::new("nogate", CONFIG);
    let (port, connected) = mock_ollama(PROBABLE, Duration::ZERO);

    let (decision, code, _) = run_check(&env, "ls -la", port, None);
    let decision = decision.expect("decision JSON");
    assert_eq!(code, 0);
    assert_eq!(decision["decision"], "allow", "{decision}");
    assert!(
        !evidence_codes(&decision)
            .iter()
            .any(|c| c.starts_with("model.")),
        "{decision}"
    );

    let (decision, _, _) = run_check(&env, "rm -rf /", port, None);
    let decision = decision.expect("decision JSON");
    assert_eq!(
        decision["reason_code"], "policy.direct_catastrophic",
        "{decision}"
    );
    assert!(
        !evidence_codes(&decision)
            .iter()
            .any(|c| c.starts_with("model.")),
        "{decision}"
    );

    // Give any stray connect a beat to land, then confirm none ever did.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !connected.load(Ordering::SeqCst),
        "model was consulted without a gate"
    );
}
