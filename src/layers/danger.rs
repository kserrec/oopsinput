//! L2 — danger layer (SPEC §5-L2). Deterministic recognition of
//! high-consequence operations from curated, code-reviewed rule tables —
//! plain Rust data and matches, never a scripting language, never execution.
//!
//! Danger alone does not intervene: a finding marks the command a *candidate*
//! and hands off to L3/policy (M3). The one exception is the
//! direct-catastrophic subset (recursive delete aimed at `/` or the home
//! directory), reported here as a flag; acting on it is still policy's call.
//!
//! Honesty rules, applied throughout: a word the shell would expand at
//! runtime (`$VAR`, substitutions) is unknowable here, so it never matches a
//! table — except the curated `$HOME`/`${HOME}` forms, whose meaning is
//! exactly the point. Quoting is the shell's business, not the command's:
//! `'rm' -rf /` runs rm, so quoted text still matches; `'~'` stays a literal
//! file, so it doesn't.

use crate::lexer::{Lexed, Token, Word, is_assignment};

pub struct Analysis {
    /// Stable evidence codes, deduplicated, first-seen order. Static strings
    /// only — raw command text never leaves the analysis (SPEC §9).
    pub codes: Vec<&'static str>,
    /// Direct-catastrophic subset (SPEC §5-L2): recursive delete of / or ~.
    pub catastrophic: bool,
    /// Literal target words of fired rules, for L3 to stat. Only words whose
    /// runtime value is knowable (no expansion) are collected. These carry
    /// raw text and exist for in-process analysis only — they never reach a
    /// log or a display without the escaper (SPEC §9).
    pub targets: Vec<String>,
}

/// Targets are for L3 stat calls; a handful bounds that syscall cost.
const MAX_TARGETS: usize = 8;

impl Analysis {
    fn note(&mut self, code: &'static str) {
        if !self.codes.contains(&code) {
            self.codes.push(code);
        }
    }

    /// Record a literal target for L3. Only called from rules that fired, so
    /// a benign command contributes no targets.
    fn note_target(&mut self, w: &Word) {
        self.note_target_text(&w.text, w.expands);
    }

    /// Same, for a target that is a slice of a word (dd's `of=…` value).
    fn note_target_text(&mut self, text: &str, expands: bool) {
        if !expands
            && !text.is_empty()
            && self.targets.len() < MAX_TARGETS
            && !self.targets.iter().any(|t| t == text)
        {
            self.targets.push(text.to_string());
        }
    }
}

pub fn analyze(lexed: &Lexed) -> Analysis {
    let home = std::env::var("HOME").ok();
    analyze_with_home(lexed, home.as_deref())
}

/// The whole layer with the home directory explicit — the seam the hermetic
/// golden corpus runs through (fixtures must not depend on the machine's
/// real $HOME).
pub fn analyze_with_home(lexed: &Lexed, home: Option<&str>) -> Analysis {
    let mut out = Analysis {
        codes: Vec::new(),
        catastrophic: false,
        targets: Vec::new(),
    };
    // Split the token stream into simple-command segments (any control
    // operator ends one), collecting each segment's words and redirects.
    // Leading assignments are environment, not the command word.
    let mut words: Vec<&Word> = Vec::new();
    let mut redirects: Vec<(&str, Option<&Word>)> = Vec::new();
    let mut awaiting_target = false;
    for t in &lexed.tokens {
        match t {
            Token::Op(_) => {
                analyze_segment(&words, &redirects, home, &mut out);
                words.clear();
                redirects.clear();
                awaiting_target = false;
            }
            Token::Redirect(op) => {
                redirects.push((op.as_str(), None));
                awaiting_target = true;
            }
            Token::Word(w) => {
                if awaiting_target {
                    if let Some(last) = redirects.last_mut() {
                        last.1 = Some(w);
                    }
                    awaiting_target = false;
                } else if !(words.is_empty() && is_assignment(w)) {
                    words.push(w);
                }
            }
        }
    }
    analyze_segment(&words, &redirects, home, &mut out);
    out
}

fn analyze_segment(
    words: &[&Word],
    redirects: &[(&str, Option<&Word>)],
    home: Option<&str>,
    out: &mut Analysis,
) {
    analyze_command(words, home, out);
    rule_redirects(redirects, out);
}

/// Flags of sudo/doas that consume the following word, so the real command
/// can be found past them (`sudo -u root rm …`). Curated, not exhaustive:
/// an unknown arg-taking long flag costs a false negative, never a false
/// positive.
const WRAPPER_ARG_FLAGS: [&str; 15] = [
    "-u", "-g", "-p", "-h", "-C", "-D", "-R", "-T", "-U", "-r", "-t", "-a", "--user", "--group",
    "--prompt",
];

/// git's global flags that consume the following word (`git -C path reset`).
const GIT_ARG_FLAGS: [&str; 5] = ["-C", "-c", "--git-dir", "--work-tree", "--namespace"];

fn analyze_command(words: &[&Word], home: Option<&str>, out: &mut Analysis) {
    let Some((cmd, args)) = words.split_first() else {
        return;
    };
    // An expanding command word ($CC, $(which x)) is unknowable — no match.
    if cmd.expands {
        return;
    }
    let Some(name) = command_name(&cmd.text) else {
        return;
    };
    match name {
        // Privilege escalates severity (SPEC §5-L2) — but only of "the
        // above": priv.sudo is noted only when the wrapped command itself
        // tripped a rule, so plain `sudo apt update` stays evidence-free.
        "sudo" | "doas" => {
            let before = out.codes.len();
            analyze_command(strip_wrapper_flags(args), home, out);
            if out.codes.len() > before {
                out.note("priv.sudo");
            }
        }
        "rm" => rule_rm(args, home, out),
        "chmod" => rule_recursive_perm(args, "fs.chmod_recursive", home, out),
        "chown" => rule_recursive_perm(args, "fs.chown_recursive", home, out),
        "cp" | "mv" => rule_copy_move(args, out),
        "git" => rule_git(args, out),
        "dd" => rule_dd(args, out),
        "kill" => rule_kill(args, out),
        "pkill" => rule_pkill(args, out),
        "systemctl" => rule_systemctl(args, out),
        "service" => rule_service(args, out),
        "apt" | "apt-get" | "dnf" | "yum" => rule_pkg_subcommand(args, out),
        "pacman" => rule_pacman(args, out),
        n if n == "mkfs" || n.starts_with("mkfs.") => rule_mkfs(args, out),
        _ => {}
    }
}

/// The name rules match on. An absolute path resolves to its basename
/// (`/bin/rm` is rm); a relative path with a slash (`./rm`) is some local
/// file we know nothing about — no match.
fn command_name(text: &str) -> Option<&str> {
    if !text.contains('/') {
        return Some(text);
    }
    if text.starts_with('/') {
        text.rsplit('/').next().filter(|s| !s.is_empty())
    } else {
        None
    }
}

fn strip_wrapper_flags<'a, 'b>(args: &'b [&'a Word]) -> &'b [&'a Word] {
    let mut i = 0;
    while i < args.len() {
        let w = args[i];
        if w.expands || !w.text.starts_with('-') || w.text.len() < 2 {
            break;
        }
        i += 1;
        if WRAPPER_ARG_FLAGS.contains(&w.text.as_str()) {
            i += 1; // the flag's argument
        }
    }
    &args[i..]
}

/// First non-flag word and what follows it, skipping over flags — with
/// `arg_flags` naming the flags that consume the next word.
fn subcommand_split<'a, 'b>(
    args: &'b [&'a Word],
    arg_flags: &[&str],
) -> Option<(&'a Word, &'b [&'a Word])> {
    let mut i = 0;
    while i < args.len() {
        let w = args[i];
        if !w.expands && w.text.starts_with('-') && w.text.len() > 1 {
            i += 1;
            if arg_flags.contains(&w.text.as_str()) {
                i += 1;
            }
            continue;
        }
        return Some((w, &args[i + 1..]));
    }
    None
}

/// Split args into flag words and operands, honoring `--` end-of-options.
/// Quoted flags still count (`rm '-rf' x` is `rm -rf x` to rm — quoting is
/// consumed by the shell); expanding words are never flags we can read.
fn split_flags<'a>(args: &[&'a Word]) -> (Vec<&'a Word>, Vec<&'a Word>) {
    let mut flags = Vec::new();
    let mut operands = Vec::new();
    let mut options_done = false;
    for w in args {
        if !options_done && !w.expands && w.text == "--" {
            options_done = true;
            continue;
        }
        if !options_done && !w.expands && w.text.starts_with('-') && w.text.len() > 1 {
            flags.push(*w);
        } else {
            operands.push(*w);
        }
    }
    (flags, operands)
}

/// True when `w` is a single-dash short-flag cluster containing `flag`
/// (`'f'` in `-rf`). Long flags never match a cluster letter.
fn cluster_has(w: &Word, flag: char) -> bool {
    w.text.starts_with('-') && !w.text.starts_with("--") && w.text[1..].contains(flag)
}

fn any_flag(flags: &[&Word], long: &str, short: Option<char>) -> bool {
    flags
        .iter()
        .any(|w| w.text == long || short.is_some_and(|c| cluster_has(w, c)))
}

// ---- filesystem rules -----------------------------------------------------

fn rule_rm(args: &[&Word], home: Option<&str>, out: &mut Analysis) {
    let (flags, operands) = split_flags(args);
    let recursive = any_flag(&flags, "--recursive", Some('r')) || any_flag(&flags, "", Some('R'));
    let force = any_flag(&flags, "--force", Some('f'));
    if !recursive && !force {
        return; // plain rm is not a danger rule (SPEC lists recursive/force)
    }
    if recursive {
        out.note("fs.rm_recursive");
    }
    if force {
        out.note("fs.rm_force");
    }
    let mut catastrophic_target = false;
    for w in &operands {
        out.note_target(w);
        if let Some(code) = classify_target(&w.text, w.expands, home) {
            out.note(code);
            catastrophic_target |= matches!(code, "fs.target_root" | "fs.target_home");
        }
    }
    if recursive && catastrophic_target {
        out.catastrophic = true;
    }
}

fn rule_recursive_perm(args: &[&Word], code: &'static str, home: Option<&str>, out: &mut Analysis) {
    let (flags, operands) = split_flags(args);
    if !(any_flag(&flags, "--recursive", Some('R'))) {
        return;
    }
    out.note(code);
    // First operand is the mode/owner — it classifies to nothing and is not
    // a stat target; real targets follow it.
    for (i, w) in operands.iter().enumerate() {
        if i > 0 {
            out.note_target(w);
        }
        if let Some(code) = classify_target(&w.text, w.expands, home) {
            out.note(code);
        }
    }
}

fn rule_copy_move(args: &[&Word], out: &mut Analysis) {
    let (flags, operands) = split_flags(args);
    let force = any_flag(&flags, "--force", Some('f'));
    if force {
        out.note("fs.force_overwrite");
    }
    // The destination is the last operand; only a block device is danger
    // evidence by itself (cwd/home destinations are everyday copies).
    if let Some(dest) = operands.last() {
        if let Some(code) = blockdev_target(&dest.text) {
            out.note(code);
            out.note_target(dest);
        } else if force {
            out.note_target(dest);
        }
    }
}

fn rule_redirects(redirects: &[(&str, Option<&Word>)], out: &mut Analysis) {
    for (op, target) in redirects {
        if !is_truncating(op) {
            continue;
        }
        let Some(w) = target else { continue };
        let t = w.text.as_str();
        // Unknowable targets and the /dev/null idiom are not evidence.
        if t.contains('$') || t.contains('`') || t == "/dev/null" {
            continue;
        }
        // `>&N` / `2>&1` duplicate a file descriptor — no file is written.
        if op.ends_with('&') && t.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // Whether the target exists (truncation of *existing* data) is L3's
        // stat to make; L2 marks the truncating shape and hands the target on.
        out.note("fs.redirect_truncate");
        out.note_target(w);
        if let Some(code) = blockdev_target(t) {
            out.note(code);
        }
    }
}

/// Truncating redirect shapes: `>` `>|` `>&file` `&>file`, any fd prefix.
/// Appends (`>>`) and input redirects are not.
fn is_truncating(op: &str) -> bool {
    matches!(
        op.trim_start_matches(|c: char| c.is_ascii_digit()),
        ">" | ">|" | ">&" | "&>"
    )
}

// ---- git rules ------------------------------------------------------------

fn rule_git(args: &[&Word], out: &mut Analysis) {
    let Some((sub, rest)) = subcommand_split(args, &GIT_ARG_FLAGS) else {
        return;
    };
    if sub.expands {
        return;
    }
    let (flags, operands) = split_flags(rest);
    match sub.text.as_str() {
        "reset" => {
            if any_flag(&flags, "--hard", None) {
                out.note("git.reset_hard");
            }
        }
        "clean" => {
            if any_flag(&flags, "--force", Some('f')) {
                out.note("git.clean_force");
            }
        }
        "push" => {
            if any_flag(&flags, "--force", Some('f'))
                || flags
                    .iter()
                    .any(|w| w.text.starts_with("--force-with-lease"))
            {
                out.note("git.push_force");
            }
            // Remote deletion: --delete/-d, or the `:ref` push-nothing refspec.
            if any_flag(&flags, "--delete", Some('d'))
                || operands
                    .iter()
                    .any(|w| !w.expands && w.text.len() > 1 && w.text.starts_with(':'))
            {
                out.note("git.push_delete");
            }
        }
        "branch" => {
            // -D, or -d/--delete combined with -f/--force.
            let delete = any_flag(&flags, "--delete", Some('d')) || any_flag(&flags, "", Some('D'));
            let force = any_flag(&flags, "--force", Some('f')) || any_flag(&flags, "", Some('D'));
            if delete && force {
                out.note("git.branch_delete_force");
            }
        }
        // History rewrite. Plain `git rebase` is deliberately absent: it is
        // a routine, intentional operation — flagging it would burn the
        // intervention budget on noise.
        "filter-branch" => out.note("git.filter_branch"),
        _ => {}
    }
}

// ---- system rules ---------------------------------------------------------

fn rule_dd(args: &[&Word], out: &mut Analysis) {
    for w in args {
        if let Some(target) = w.text.strip_prefix("of=") {
            out.note("system.dd_of");
            out.note_target_text(target, w.expands);
            if let Some(code) = blockdev_target(target) {
                out.note(code);
            }
        }
    }
}

fn rule_mkfs(args: &[&Word], out: &mut Analysis) {
    out.note("system.mkfs");
    let (_, operands) = split_flags(args);
    if let Some(w) = operands.first() {
        out.note_target(w);
        if let Some(code) = blockdev_target(&w.text) {
            out.note(code);
        }
    }
}

/// `kill -9 -1` (SIGKILL to every process the user can signal). The `-1`
/// must follow a KILL-signal flag: before one it would itself be parsed as
/// a signal (SIGHUP), not a pid.
fn rule_kill(args: &[&Word], out: &mut Analysis) {
    let mut kill_signal_seen = false;
    let mut i = 0;
    while i < args.len() {
        let t = args[i].text.as_str();
        if kill_signal_seen && t == "-1" {
            out.note("system.kill_all");
            return;
        }
        match t {
            "-9" | "-KILL" | "-SIGKILL" => kill_signal_seen = true,
            "-s" | "--signal" => {
                if matches!(
                    args.get(i + 1).map(|w| w.text.as_str()),
                    Some("9") | Some("KILL") | Some("SIGKILL")
                ) {
                    kill_signal_seen = true;
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Broad pkill: a pattern of ≤3 characters matches far more than one
/// process family (`pkill sh` kills every -sh descendant; `pkill .` kills
/// everything — pkill patterns are regexes). Arg-taking pkill flags are not
/// modeled; their argument reads as the pattern, which can only widen what
/// counts as broad, never narrow it (the misread argument would need to be
/// ≤3 chars itself).
fn rule_pkill(args: &[&Word], out: &mut Analysis) {
    let Some((pattern, _)) = subcommand_split(args, &[]) else {
        return;
    };
    if !pattern.expands && pattern.text.chars().count() <= 3 {
        out.note("system.pkill_broad");
    }
}

fn rule_systemctl(args: &[&Word], out: &mut Analysis) {
    if let Some((sub, _)) = subcommand_split(args, &[])
        && !sub.expands
        && matches!(sub.text.as_str(), "stop" | "disable" | "mask")
    {
        out.note("system.service_stop");
    }
}

fn rule_service(args: &[&Word], out: &mut Analysis) {
    if args.len() >= 2 && args[1].text == "stop" {
        out.note("system.service_stop");
    }
}

fn rule_pkg_subcommand(args: &[&Word], out: &mut Analysis) {
    if let Some((sub, _)) = subcommand_split(args, &[])
        && !sub.expands
        && matches!(
            sub.text.as_str(),
            "remove" | "purge" | "autoremove" | "erase"
        )
    {
        out.note("system.pkg_remove");
    }
}

fn rule_pacman(args: &[&Word], out: &mut Analysis) {
    if args.iter().any(|w| !w.expands && w.text.starts_with("-R")) {
        out.note("system.pkg_remove");
    }
}

// ---- target classification ------------------------------------------------

/// What a target word points at, when that is knowable without evaluation.
/// Expanding words are unknowable except the curated `$HOME` forms; a glob's
/// raw text is classifiable exactly when it is one of the curated shapes.
/// Quoting needs no parameter here: a single-quoted `'$HOME'` or escaped
/// `\~` arrives with `expands == false`, which already reads as literal.
fn classify_target(text: &str, expands: bool, home: Option<&str>) -> Option<&'static str> {
    if text.contains('$') || text.contains('`') {
        return (expands && matches!(text, "$HOME" | "${HOME}")).then_some("fs.target_home");
    }
    if expands {
        return match text {
            "~" => Some("fs.target_home"),
            "/*" => Some("fs.target_root"),
            "*" | "./*" => Some("fs.target_cwd"),
            "../*" => Some("fs.target_parent"),
            _ => blockdev_target(text), // e.g. /dev/sd*
        };
    }
    match text {
        "/" => Some("fs.target_root"),
        "." | "./" => Some("fs.target_cwd"),
        ".." | "../" => Some("fs.target_parent"),
        _ => {
            if let Some(h) = home {
                let h = h.trim_end_matches('/');
                if !h.is_empty() && h != "/" && text.trim_end_matches('/') == h {
                    return Some("fs.target_home");
                }
            }
            blockdev_target(text)
        }
    }
}

/// Lexical block-device recognition (writes to these are a SPEC §5-L2 rule).
/// Whether the path is truly a block device is L3's stat; this is the
/// curated name-shape table.
const BLOCKDEV_PREFIXES: [&str; 11] = [
    "/dev/sd",
    "/dev/hd",
    "/dev/vd",
    "/dev/xvd",
    "/dev/nvme",
    "/dev/mmcblk",
    "/dev/loop",
    "/dev/md",
    "/dev/dm-",
    "/dev/mapper/",
    "/dev/disk",
];

fn blockdev_target(text: &str) -> Option<&'static str> {
    if text.contains('$') || text.contains('`') {
        return None;
    }
    BLOCKDEV_PREFIXES
        .iter()
        .any(|p| text.starts_with(p))
        .then_some("fs.target_blockdev")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    /// All tests run through the hermetic home seam; "/home/u" stands in for
    /// the real $HOME.
    fn run(buffer: &str) -> Analysis {
        analyze_with_home(&lex(buffer), Some("/home/u"))
    }

    fn codes(buffer: &str) -> Vec<&'static str> {
        run(buffer).codes
    }

    #[test]
    fn rm_flag_clusters_and_long_flags() {
        assert_eq!(codes("rm -rf x"), ["fs.rm_recursive", "fs.rm_force"]);
        assert_eq!(codes("rm -fr x"), ["fs.rm_recursive", "fs.rm_force"]);
        assert_eq!(codes("rm -R x"), ["fs.rm_recursive"]);
        assert_eq!(
            codes("rm --recursive --force x"),
            ["fs.rm_recursive", "fs.rm_force"]
        );
        assert_eq!(codes("rm x"), Vec::<&str>::new()); // plain rm: no rule
    }

    #[test]
    fn double_dash_ends_options() {
        // flags after `--` are operands: no recursive/force read
        assert_eq!(codes("rm -- -rf"), Vec::<&str>::new());
        // ...but flags before it still count, and the target still classifies
        let a = run("rm -r -- /");
        assert_eq!(a.codes, ["fs.rm_recursive", "fs.target_root"]);
        assert!(a.catastrophic);
    }

    #[test]
    fn catastrophic_subset_is_recursive_root_or_home() {
        assert!(run("rm -rf /").catastrophic);
        assert!(run("rm -rf /*").catastrophic);
        assert!(run("rm -rf ~").catastrophic);
        assert!(run("rm -rf \"$HOME\"").catastrophic);
        assert!(run("rm -rf ${HOME}").catastrophic);
        assert!(run("rm -rf /home/u").catastrophic); // literal home path
        assert!(run("rm -rf /home/u/").catastrophic); // trailing slash
        assert!(run("sudo rm -rf /").catastrophic);
        // force without recursion cannot delete a directory tree
        assert!(!run("rm -f ~").catastrophic);
        // subpaths of home are not home
        assert!(!run("rm -rf ~/old").catastrophic);
        assert!(!run("rm -rf /home/u/proj").catastrophic);
        assert!(!run("rm -rf ./build").catastrophic);
    }

    #[test]
    fn quoting_matches_the_shells_meaning() {
        // the shell strips quotes before rm sees them: still the rm rule
        assert_eq!(codes("'rm' -rf /tmp/x"), ["fs.rm_recursive", "fs.rm_force"]);
        assert_eq!(codes("\\rm -rf /tmp/x"), ["fs.rm_recursive", "fs.rm_force"]);
        // a quoted tilde is a literal file named ~, not home
        let a = run("rm -rf '~'");
        assert_eq!(a.codes, ["fs.rm_recursive", "fs.rm_force"]);
        assert!(!a.catastrophic);
        // a single-quoted $HOME is a literal file named $HOME
        assert!(!run("rm -rf '$HOME'").catastrophic);
    }

    #[test]
    fn expanding_command_word_never_matches() {
        assert_eq!(codes("$RM -rf /"), Vec::<&str>::new());
        assert_eq!(codes("$(which rm) -rf /"), Vec::<&str>::new());
    }

    #[test]
    fn absolute_path_resolves_relative_path_does_not() {
        assert_eq!(
            codes("/bin/rm -rf /tmp/x"),
            ["fs.rm_recursive", "fs.rm_force"]
        );
        // ./rm is some local file, not necessarily rm
        assert_eq!(codes("./rm -rf /"), Vec::<&str>::new());
    }

    #[test]
    fn assignment_prefix_is_skipped() {
        let a = run("FOO=1 rm -rf /");
        assert_eq!(
            a.codes,
            ["fs.rm_recursive", "fs.rm_force", "fs.target_root"]
        );
        assert!(a.catastrophic);
    }

    #[test]
    fn sudo_wraps_and_escalates_only_real_findings() {
        assert_eq!(
            codes("sudo rm -rf /tmp/x"),
            ["fs.rm_recursive", "fs.rm_force", "priv.sudo"]
        );
        // arg-taking wrapper flags are stepped over to find the command
        assert!(run("sudo -u root rm -rf ~").catastrophic);
        assert_eq!(
            codes("doas rm -rf /tmp/x"),
            ["fs.rm_recursive", "fs.rm_force", "priv.sudo"]
        );
        // a benign command under sudo is not evidence
        assert_eq!(codes("sudo apt update"), Vec::<&str>::new());
        assert_eq!(codes("sudo ls /root"), Vec::<&str>::new());
    }

    #[test]
    fn git_rules_fire_on_the_exact_shapes() {
        assert_eq!(codes("git reset --hard"), ["git.reset_hard"]);
        assert_eq!(codes("git reset --hard HEAD~2"), ["git.reset_hard"]);
        // global arg-flags are stepped over to find the subcommand
        assert_eq!(codes("git -C /tmp/repo reset --hard"), ["git.reset_hard"]);
        assert_eq!(codes("git reset --soft HEAD~1"), Vec::<&str>::new());
        assert_eq!(codes("git clean -fdx"), ["git.clean_force"]);
        assert_eq!(codes("git clean -n"), Vec::<&str>::new());
        assert_eq!(codes("git push --force origin main"), ["git.push_force"]);
        assert_eq!(
            codes("git push --force-with-lease origin main"),
            ["git.push_force"]
        );
        assert_eq!(codes("git push origin main"), Vec::<&str>::new());
        assert_eq!(codes("git push origin :feature"), ["git.push_delete"]);
        assert_eq!(codes("git push -d origin feature"), ["git.push_delete"]);
        assert_eq!(codes("git branch -D feature"), ["git.branch_delete_force"]);
        assert_eq!(
            codes("git branch -d -f feature"),
            ["git.branch_delete_force"]
        );
        assert_eq!(codes("git branch -d feature"), Vec::<&str>::new());
        assert_eq!(
            codes("git filter-branch --tree-filter 'rm -f secrets' HEAD"),
            ["git.filter_branch"]
        );
        assert_eq!(codes("git status"), Vec::<&str>::new());
    }

    #[test]
    fn system_rules() {
        assert_eq!(
            codes("dd if=arch.iso of=/dev/sdb bs=4M"),
            ["system.dd_of", "fs.target_blockdev"]
        );
        assert_eq!(codes("dd if=/dev/sda of=backup.img"), ["system.dd_of"]);
        assert_eq!(
            codes("mkfs.ext4 /dev/nvme0n1p2"),
            ["system.mkfs", "fs.target_blockdev"]
        );
        assert_eq!(codes("kill -9 -1"), ["system.kill_all"]);
        assert_eq!(codes("kill -KILL -1"), ["system.kill_all"]);
        assert_eq!(codes("kill -s KILL -1"), ["system.kill_all"]);
        assert_eq!(codes("kill -9 43210"), Vec::<&str>::new());
        // before a signal flag, -1 is itself a signal (SIGHUP), not "all pids"
        assert_eq!(codes("kill -1 4321"), Vec::<&str>::new());
        assert_eq!(codes("pkill -9 sh"), ["system.pkill_broad"]);
        assert_eq!(codes("pkill my-worker-daemon"), Vec::<&str>::new());
        assert_eq!(codes("systemctl stop nginx"), ["system.service_stop"]);
        assert_eq!(codes("systemctl status nginx"), Vec::<&str>::new());
        assert_eq!(codes("service ssh stop"), ["system.service_stop"]);
        assert_eq!(codes("apt remove golang"), ["system.pkg_remove"]);
        assert_eq!(codes("apt-get purge golang"), ["system.pkg_remove"]);
        assert_eq!(codes("apt install golang"), Vec::<&str>::new());
        assert_eq!(codes("pacman -Rns pkg"), ["system.pkg_remove"]);
        assert_eq!(codes("pacman -S pkg"), Vec::<&str>::new());
    }

    #[test]
    fn copy_move_rules() {
        assert_eq!(codes("mv -f a b"), ["fs.force_overwrite"]);
        assert_eq!(codes("cp -f a b"), ["fs.force_overwrite"]);
        assert_eq!(codes("mv a b"), Vec::<&str>::new());
        // writing to a block device is evidence even without -f
        assert_eq!(codes("cp image.img /dev/sdb"), ["fs.target_blockdev"]);
    }

    #[test]
    fn chmod_chown_recursive() {
        assert_eq!(
            codes("chmod -R 777 /"),
            ["fs.chmod_recursive", "fs.target_root"]
        );
        assert_eq!(codes("chmod 644 notes.txt"), Vec::<&str>::new());
        assert_eq!(
            codes("chown -R u:g ."),
            ["fs.chown_recursive", "fs.target_cwd"]
        );
        // recursive perm change is a candidate, never catastrophic
        assert!(!run("chmod -R 000 /").catastrophic);
    }

    #[test]
    fn redirect_truncation() {
        assert_eq!(codes("echo data > results.csv"), ["fs.redirect_truncate"]);
        assert_eq!(codes("echo data >| results.csv"), ["fs.redirect_truncate"]);
        assert_eq!(codes("cmd 2> err.log"), ["fs.redirect_truncate"]);
        assert_eq!(codes("cmd &> all.log"), ["fs.redirect_truncate"]);
        assert_eq!(
            codes("echo x > /dev/sda1"),
            ["fs.redirect_truncate", "fs.target_blockdev"]
        );
        // appends, /dev/null, fd dups, unknowable targets: not evidence
        assert_eq!(codes("echo data >> results.csv"), Vec::<&str>::new());
        assert_eq!(codes("make 2>/dev/null"), Vec::<&str>::new());
        assert_eq!(codes("cmd 2>&1"), Vec::<&str>::new());
        assert_eq!(codes("cmd >&2"), Vec::<&str>::new());
        assert_eq!(codes("echo x > $OUT"), Vec::<&str>::new());
    }

    #[test]
    fn segments_are_analyzed_independently_and_deduped() {
        let a = run("make && rm -rf ./build");
        assert_eq!(a.codes, ["fs.rm_recursive", "fs.rm_force"]);
        assert!(!a.catastrophic);
        // same code from two segments appears once
        assert_eq!(
            codes("rm -rf a; rm -rf b"),
            ["fs.rm_recursive", "fs.rm_force"]
        );
        // target facts stay attached to the dangerous segment: the `/` here
        // belongs to ls, not rm
        let a = run("rm -rf ./x && ls /");
        assert_eq!(a.codes, ["fs.rm_recursive", "fs.rm_force"]);
        assert!(!a.catastrophic);
    }

    #[test]
    fn expanding_targets_are_unknowable() {
        assert_eq!(codes("rm -rf $DIR"), ["fs.rm_recursive", "fs.rm_force"]);
        assert_eq!(
            codes("rm -rf $(find-stale)"),
            ["fs.rm_recursive", "fs.rm_force"]
        );
        assert!(!run("rm -rf $DIR").catastrophic);
    }

    #[test]
    fn targets_are_collected_only_from_fired_rules_and_only_literals() {
        // The failure modes this pins: a benign command contributing stat
        // targets, an expanding word being stat'ed as its literal spelling,
        // and a mode/owner operand being mistaken for a path.
        assert_eq!(run("rm -rf ./build ./dist").targets, ["./build", "./dist"]);
        assert!(run("rm x").targets.is_empty()); // no rule fired
        assert!(run("rm -rf $DIR").targets.is_empty()); // unknowable
        assert!(run("rm -rf ~").targets.is_empty()); // tilde expands too
        assert_eq!(run("chmod -R 777 /srv/data").targets, ["/srv/data"]); // mode skipped
        assert_eq!(run("echo x > out.txt").targets, ["out.txt"]);
        assert_eq!(run("dd if=a of=disk.img").targets, ["disk.img"]);
        assert_eq!(run("mkfs.ext4 /dev/sdb1").targets, ["/dev/sdb1"]);
        assert_eq!(run("mv -f a b").targets, ["b"]); // destination only
        assert!(run("mv a b").targets.is_empty()); // no rule fired
        // capped and deduplicated
        assert_eq!(run("rm -rf a a b c d e f g h i j").targets.len(), 8);
    }
}
