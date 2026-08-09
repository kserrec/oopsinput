//! L3 — context layer (SPEC §5-L3), deterministic half. Cheap, freshly
//! collected facts that separate "dangerous and intended" from "probably not
//! what you meant". Runs only when L2 marked a candidate, so the common path
//! stays syscall-free.
//!
//! Every collector is hard-capped — bounded walks, bounded reads, bounded
//! child runtime — so a pathological environment degrades to honest
//! "unavailable" facts, never a hang (SPEC §10). Missing evidence is
//! reported as missing, never guessed.
//!
//! The recency relation (structural summaries of recent commands) is the
//! zsh plugin's data to supply and lands with its own plugin + PTY work;
//! this module holds the git and filesystem halves.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::distance::{bounded_osa, max_distance};

pub struct GitFacts {
    pub detached: bool,
    /// Current branch is a conventional primary branch (main/master/trunk).
    pub branch_main_like: bool,
    /// Tracked files with staged or unstaged changes. None: `git status`
    /// evidence unavailable (no binary at a known path, timeout, error).
    pub dirty: Option<u32>,
    pub untracked: Option<bool>,
}

pub struct TargetFact {
    pub exists: bool,
    /// Whether the resolved target is a directory. Policy does not currently
    /// consume this fact, but the inference request includes it as evidence.
    pub is_dir: bool,
    pub is_symlink: bool,
    /// Entry count for a directory target, capped at ENTRY_CAP.
    pub entries: Option<u32>,
    /// Resolves to the working directory itself (catches `../myproject`).
    pub is_cwd: bool,
    pub is_parent: bool,
    /// Doesn't exist, but a sibling name is within typo distance — the
    /// "near-miss target" signal (SPEC §5-L3).
    pub near_miss: bool,
}

pub struct Context {
    /// None: the working directory is not inside a git repository.
    pub git: Option<GitFacts>,
    /// One entry per danger-layer target, same order.
    pub targets: Vec<TargetFact>,
}

/// Ancestor directories examined looking for `.git`.
const WALK_UP_CAP: usize = 64;
/// Directory entries examined per readdir (rough counts degrade, never hang).
const ENTRY_CAP: usize = 1_000;
/// Bytes read from HEAD / gitfile — both are one short line in practice.
const FILE_READ_CAP: u64 = 4_096;
/// Hard deadline for the `git status` child. The whole deterministic path
/// answers to the 150 ms watchdog (SPEC §10); this leaves headroom around it.
const GIT_TIMEOUT_MS: u64 = 80;
/// stdout kept from `git status` (~10k porcelain lines). Beyond it the child
/// is drained but ignored, and the possibly-cut final line is dropped.
const GIT_OUTPUT_CAP: u64 = 512 * 1024;
/// Porcelain lines parsed; the counts saturate there.
const STATUS_LINE_CAP: usize = 10_000;

/// Absolute paths we accept for `git`, in order — never resolved through
/// $PATH (standing rule, audit 2026-08-06: the stty finding; a helper
/// resolved by name executes whatever directory leads PATH).
const GIT_PATHS: [&str; 3] = ["/usr/bin/git", "/usr/local/bin/git", "/bin/git"];

pub fn collect(targets: &[String]) -> Context {
    let cwd = std::env::current_dir().unwrap_or_default();
    collect_at(&cwd, targets, git_path().as_deref())
}

/// The whole layer with the working directory and git binary explicit — the
/// seam hermetic tests run through (process cwd is shared test-wide and
/// must not be chdir'd).
pub fn collect_at(cwd: &Path, targets: &[String], git: Option<&Path>) -> Context {
    Context {
        git: git_facts(cwd, git),
        targets: targets.iter().map(|t| target_fact(cwd, t)).collect(),
    }
}

fn git_path() -> Option<PathBuf> {
    GIT_PATHS
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.metadata().is_ok_and(|m| m.is_file()))
}

// ---- git facts ------------------------------------------------------------

fn git_facts(cwd: &Path, git: Option<&Path>) -> Option<GitFacts> {
    let head_file = repo_head_file(cwd)?;
    let head = read_capped(&head_file, FILE_READ_CAP)?;
    let branch = head.trim().strip_prefix("ref: refs/heads/");
    let (dirty, untracked) = match git.and_then(|g| git_status(g, cwd)) {
        Some((d, u)) => (Some(d), Some(u)),
        None => (None, None),
    };
    Some(GitFacts {
        detached: branch.is_none(),
        branch_main_like: matches!(branch, Some("main" | "master" | "trunk")),
        dirty,
        untracked,
    })
}

/// Walk up from `cwd` to the nearest `.git`, returning the path of its HEAD
/// file. A `.git` *file* (worktree, submodule) is one `gitdir: …` line
/// pointing at the real directory.
fn repo_head_file(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd.to_path_buf();
    for _ in 0..WALK_UP_CAP {
        let dotgit = dir.join(".git");
        if let Ok(meta) = std::fs::metadata(&dotgit) {
            if meta.is_dir() {
                return Some(dotgit.join("HEAD"));
            }
            let text = read_capped(&dotgit, FILE_READ_CAP)?;
            let gitdir = text.trim().strip_prefix("gitdir: ")?;
            let gd = Path::new(gitdir);
            let base = if gd.is_absolute() {
                gd.to_path_buf()
            } else {
                dir.join(gd)
            };
            return Some(base.join("HEAD"));
        }
        if !dir.pop() {
            return None;
        }
    }
    None
}

fn read_capped(path: &Path, cap: u64) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(cap)
        .read_to_end(&mut buf)
        .ok()?;
    String::from_utf8(buf).ok()
}

/// Run `git status` read-only and bounded: fixed argv, no shell, hard
/// timeout (SPEC §9-1). `--no-optional-locks` keeps it from touching the
/// index. Returns (dirty tracked files, untracked present).
///
/// SECURITY (audit 2026-08-06, proven): the repository we run in is not
/// necessarily the user's own work — a directory tree extracted from an
/// archive, or a fixture repo committed inside another project, carries its
/// own `.git/config`. `git status` *executes* `core.fsmonitor` from that
/// config, so merely typing `rm -rf ./build` in such a directory ran the
/// stranger's program: analysis caused execution, which SPEC §9-1 forbids
/// outright. Every known config key that makes git spawn something is
/// neutralized on the command line, where `-c` outranks repo config.
fn git_status(git: &Path, cwd: &Path) -> Option<(u32, bool)> {
    let mut cmd = std::process::Command::new(git);
    cmd.args([
        "-c",
        "core.fsmonitor=", // proven exec vector: repo config runs this program
        "-c",
        "core.hooksPath=/dev/null", // status runs no hooks today; keep it that way
        "-c",
        "core.pager=cat", // never hand our output to a repo-chosen program
        "--no-optional-locks",
        "status",
        "--porcelain",
    ])
    .current_dir(cwd)
    // System/global config can also carry these keys; the repo is the
    // untrusted one, but nothing in `status --porcelain` needs either file.
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_PAGER", "cat")
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null());
    let out = run_capture(cmd, GIT_TIMEOUT_MS, GIT_OUTPUT_CAP)?;
    let capped = out.len() as u64 >= GIT_OUTPUT_CAP;
    let text = String::from_utf8_lossy(&out);
    let mut lines: Vec<&str> = text.lines().take(STATUS_LINE_CAP).collect();
    if capped {
        lines.pop(); // the cap may have cut this line mid-path
    }
    let mut dirty = 0u32;
    let mut untracked = false;
    for line in lines {
        if line.starts_with("??") {
            untracked = true;
        } else if !line.is_empty() {
            dirty += 1;
        }
    }
    Some((dirty, untracked))
}

/// Spawn a child, keep at most `cap` bytes of its stdout, kill it at
/// `timeout_ms`. A dedicated reader thread drains everything past the cap:
/// without it a chatty child would block on the full pipe (~64 KiB) and hit
/// the timeout even when healthy. No path through here outlives the timeout:
/// even after the child is killed, a grandchild it spawned can inherit the
/// pipe's write end and keep the reader blocked (probed: a killed /bin/sh
/// wrapper left its sleeping grandchild holding the pipe for its full
/// runtime) — so the reader is *polled* against the same deadline and
/// abandoned if it never finishes. An abandoned thread is fine here: this is
/// a per-command process that exits immediately after analysis.
fn run_capture(mut cmd: std::process::Command, timeout_ms: u64, cap: u64) -> Option<Vec<u8>> {
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(cap).read_to_end(&mut buf);
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        buf
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    // On failure the reader is left to die with the process.
    if !crate::proc::wait_or_kill(&mut child, deadline) {
        return None;
    }
    while !reader.is_finished() {
        if std::time::Instant::now() >= deadline {
            return None; // orphan grandchild holds the pipe — abandon
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    reader.join().ok()
}

// ---- target facts ---------------------------------------------------------

fn target_fact(cwd: &Path, target: &str) -> TargetFact {
    let path = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        cwd.join(target)
    };
    // symlink_metadata: a dangling symlink still *exists* as a deletable
    // thing, and "is a symlink" must survive the stat.
    let symlink_meta = std::fs::symlink_metadata(&path);
    let exists = symlink_meta.is_ok();
    let is_symlink = symlink_meta.is_ok_and(|m| m.file_type().is_symlink());
    let is_dir = std::fs::metadata(&path).is_ok_and(|m| m.is_dir());
    let entries = if is_dir { count_entries(&path) } else { None };

    let canon = if exists {
        std::fs::canonicalize(&path).ok()
    } else {
        None
    };
    let cwd_canon = std::fs::canonicalize(cwd).ok();
    let is_cwd = matches!((&canon, &cwd_canon), (Some(t), Some(c)) if t == c);
    let is_parent =
        matches!((&canon, &cwd_canon), (Some(t), Some(c)) if c.parent() == Some(t.as_path()));

    TargetFact {
        exists,
        is_dir,
        is_symlink,
        entries,
        is_cwd,
        is_parent,
        near_miss: !exists && has_near_sibling(&path),
    }
}

/// Rough entry count, capped: enough to distinguish "empty-ish" from "a lot
/// of work is about to be destroyed" without walking a huge directory.
fn count_entries(dir: &Path) -> Option<u32> {
    let rd = std::fs::read_dir(dir).ok()?;
    Some(rd.take(ENTRY_CAP).count() as u32)
}

/// A missing target whose parent holds a name within typo distance of it —
/// strong "probably not what you meant" evidence (`rm -rf ./buidl` beside
/// an existing `./build`).
fn has_near_sibling(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let len = name.chars().count();
    if len < 2 {
        return false;
    }
    let max = max_distance(len);
    let target: Vec<char> = name.chars().collect();
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(rd) = std::fs::read_dir(parent) else {
        return false;
    };
    for entry in rd.take(ENTRY_CAP) {
        let Ok(e) = entry else { continue };
        let sibling_os = e.file_name();
        let Some(sibling) = sibling_os.to_str() else {
            continue;
        };
        let sib: Vec<char> = sibling.chars().collect();
        if let Some(d) = bounded_osa(&target, &sib, max)
            && d > 0
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oopsinput-ctx-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The real git binary, for building repo fixtures only (analysis-side
    /// resolution is the fixed-path table; tests may be more relaxed).
    fn real_git() -> PathBuf {
        git_path().expect("tests need git at a standard absolute path")
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let ok = Command::new(real_git())
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run git")
            .success();
        assert!(ok, "git {args:?} failed in {dir:?}");
    }

    fn make_repo(tag: &str) -> PathBuf {
        let dir = tmp(tag);
        git_in(&dir, &["init", "-q"]);
        // Pin the branch name regardless of the machine's init.defaultBranch.
        git_in(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        dir
    }

    #[test]
    fn no_repo_means_no_git_facts() {
        let dir = tmp("norepo");
        // The premise is that no ancestor of the temp dir is a repository.
        // Ask Git rather than treating any `.git` pathname as proof: an empty
        // `/tmp/.git` directory triggered this guard on 2026-08-08 even though
        // Git correctly reported that `/tmp` was not a repository.
        let git = git_path().expect("the context integration tests require git");
        let inside_repo = std::process::Command::new(git)
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("probe repository premise")
            .success();
        assert!(
            !inside_repo,
            "premise broken: {} is inside a git repository, so this test \
             cannot say anything about the no-repo case",
            dir.display()
        );
        let ctx = collect_at(&dir, &[], None);
        assert!(ctx.git.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_repo_on_main() {
        let dir = make_repo("clean");
        let ctx = collect_at(&dir, &[], Some(&real_git()));
        let g = ctx.git.expect("in a repo");
        assert!(!g.detached);
        assert!(g.branch_main_like);
        assert_eq!(g.dirty, Some(0));
        assert_eq!(g.untracked, Some(false));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn untracked_and_staged_files_are_seen() {
        let dir = make_repo("dirty");
        fs::write(dir.join("loose.txt"), "x").unwrap();
        let ctx = collect_at(&dir, &[], Some(&real_git()));
        let g = ctx.git.unwrap();
        assert_eq!(g.untracked, Some(true));
        assert_eq!(g.dirty, Some(0));

        git_in(&dir, &["add", "loose.txt"]);
        let g = collect_at(&dir, &[], Some(&real_git())).git.unwrap();
        assert_eq!(g.dirty, Some(1), "a staged file is dirty work at risk");
        assert_eq!(g.untracked, Some(false));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn feature_branch_and_detached_head() {
        let dir = make_repo("branch");
        git_in(&dir, &["symbolic-ref", "HEAD", "refs/heads/scratch"]);
        let g = collect_at(&dir, &[], None).git.unwrap();
        assert!(!g.branch_main_like);
        assert!(!g.detached);

        // A detached HEAD needs a commit to point at.
        git_in(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        fs::write(dir.join("f"), "x").unwrap();
        git_in(&dir, &["add", "f"]);
        git_in(
            &dir,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "c",
            ],
        );
        git_in(&dir, &["checkout", "-q", "--detach"]);
        let g = collect_at(&dir, &[], None).git.unwrap();
        assert!(g.detached);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn subdirectory_still_finds_the_repo() {
        let dir = make_repo("subdir");
        let sub = dir.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        assert!(collect_at(&sub, &[], None).git.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_git_binary_is_honest_unavailable() {
        let dir = make_repo("nogit");
        let g = collect_at(&dir, &[], None).git.unwrap();
        assert_eq!(g.dirty, None, "no binary → no claim, not zero");
        assert_eq!(g.untracked, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hostile_repo_config_cannot_make_analysis_execute_anything() {
        // Regression (audit 2026-08-06, proven exploitable): `git status`
        // executes `core.fsmonitor` from the repository's own config, so
        // analyzing ANY dangerous-looking command inside a directory whose
        // .git/config came from someone else (an extracted archive, a
        // fixture repo committed inside another project) ran that
        // stranger's program. SPEC §9-1: analysis never executes anything.
        use std::os::unix::fs::PermissionsExt;
        let dir = make_repo("hostile");
        let marker = dir.join("EXECUTED");
        let evil = dir.join("evil.sh");
        fs::write(
            &evil,
            format!("#!/bin/sh\ntouch {}\nexit 1\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&evil, fs::Permissions::from_mode(0o755)).unwrap();
        git_in(&dir, &["config", "core.fsmonitor", evil.to_str().unwrap()]);

        // Sanity: the trap is armed — plain `git status` does run it.
        let _ = Command::new(real_git())
            .args(["status", "--porcelain"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        assert!(
            marker.exists(),
            "fixture is not actually hostile — this test would prove nothing"
        );
        fs::remove_file(&marker).unwrap();

        // Our collector must not.
        let _ = collect_at(&dir, &[], Some(&real_git()));
        assert!(
            !marker.exists(),
            "analysis executed a program from the repository's config"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hung_git_is_killed_at_the_deadline() {
        // Probe-derived: a helper that never exits must cost at most the
        // timeout, and its facts must degrade to unavailable.
        let dir = make_repo("hang");
        let fake = dir.join("fake-git");
        fs::write(&fake, "#!/bin/sh\nsleep 5\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        let g = collect_at(&dir, &[], Some(&fake)).git.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "hung helper must be killed at the deadline, not waited out"
        );
        assert_eq!(g.dirty, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn target_facts_for_files_dirs_and_symlinks() {
        let dir = tmp("targets");
        fs::write(dir.join("file.txt"), "x").unwrap();
        fs::create_dir(dir.join("build")).unwrap();
        fs::write(dir.join("build/a"), "").unwrap();
        fs::write(dir.join("build/b"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("file.txt"), dir.join("link")).unwrap();

        let targets: Vec<String> = ["file.txt", "build", "link", "gone.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ctx = collect_at(&dir, &targets, None);
        let [file, build, link, gone] = ctx.targets.as_slice() else {
            panic!("one fact per target");
        };
        assert!(file.exists && !file.is_dir && !file.is_symlink);
        assert!(build.exists && build.is_dir);
        assert_eq!(build.entries, Some(2));
        assert!(link.exists && link.is_symlink);
        assert!(
            !link.is_dir,
            "facts describe the link, not what it points at"
        );
        assert!(!gone.exists);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_count_is_capped() {
        let dir = tmp("cap");
        let big = dir.join("big");
        fs::create_dir(&big).unwrap();
        for i in 0..(ENTRY_CAP + 50) {
            fs::write(big.join(format!("f{i}")), "").unwrap();
        }
        let ctx = collect_at(&dir, &["big".to_string()], None);
        assert_eq!(ctx.targets[0].entries, Some(ENTRY_CAP as u32));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn near_miss_flags_a_typoed_sibling() {
        let dir = tmp("nearmiss");
        fs::create_dir(dir.join("build")).unwrap();
        let ctx = collect_at(&dir, &["buidl".to_string()], None);
        assert!(ctx.targets[0].near_miss, "buidl sits beside build");
        // no similar sibling → no near-miss
        let ctx = collect_at(&dir, &["zzzzzz".to_string()], None);
        assert!(!ctx.targets[0].near_miss);
        // an existing target is never a near-miss
        let ctx = collect_at(&dir, &["build".to_string()], None);
        assert!(!ctx.targets[0].near_miss);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disguised_cwd_and_parent_are_recognized() {
        let dir = tmp("cwdness");
        let sub = dir.join("proj");
        fs::create_dir(&sub).unwrap();
        // `../proj` from inside proj resolves to cwd itself
        let ctx = collect_at(&sub, &["../proj".to_string()], None);
        assert!(ctx.targets[0].is_cwd);
        assert!(!ctx.targets[0].is_parent);
        let ctx = collect_at(&sub, &["..".to_string()], None);
        assert!(ctx.targets[0].is_parent);
        let _ = fs::remove_dir_all(&dir);
    }
}
