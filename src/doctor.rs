//! Read-only diagnosis of the installed Zsh adapter and its environment.

use std::io::Read;
use std::process::ExitCode;
use std::time::Instant;

use crate::{model, policy, state, ui};

const ZSHRC_READ_CAP: u64 = 1024 * 1024;
const MARK_BEGIN: &[u8] = b"# >>> oopsinput >>>";
const MARK_END: &[u8] = b"# <<< oopsinput <<<";
const ACCEPT_WIDGETS: [&str; 4] = [
    "accept-line",
    "accept-line-and-down-history",
    "accept-and-hold",
    "accept-and-infer-next-history",
];

/// Ollama's /api/show response carries the modelfile, license text, and
/// tensor metadata — legitimately large. Generous cap; this is a reachability
/// check, not model I/O.
const SHOW_RESPONSE_CAP: usize = 4 * 1024 * 1024;

enum PluginInstallStatus {
    Installed,
    HomeUnavailable,
    ZshrcMissing,
    ZshrcUnsafe,
    ZshrcUnreadable,
    ZshrcTooLarge,
    MarkerMissing,
    MarkerDamaged,
    PluginMissing,
    PluginUnsafe,
    PluginUnreadable,
}

enum WidgetStatus {
    Inactive,
    Stale,
    Invalid,
    Wrapped(usize),
}

/// Complete installed-environment diagnosis for the interactive Zsh adapter.
pub(crate) fn run() -> ExitCode {
    println!("oopsinput doctor");
    println!("  version:    {}", env!("CARGO_PKG_VERSION"));

    let mut healthy = true;

    let zsh = find_in_path("zsh");
    let shown_zsh = zsh
        .as_deref()
        .map(ui::escape_for_display)
        .unwrap_or_else(|| "NOT FOUND in PATH".to_string());
    println!("  zsh:        {}", shown_zsh);
    healthy &= zsh.is_some();

    let plugin_ok = print_plugin_line();
    let widgets_ok = print_widgets_line();
    healthy &= plugin_ok && widgets_ok;

    // Regression (bughunt 2026-08-06): this line once hardcoded
    // ~/.config, contradicting the mode line below whenever
    // XDG_CONFIG_HOME pointed elsewhere. Both must resolve identically.
    let config = policy::inspect_config();
    let config_ok = print_config_line(&config);
    println!(
        "  mode:       {}",
        match config.config.mode {
            policy::Mode::Shadow => "shadow",
            policy::Mode::Suggest => "suggest (L1 typo prompts)",
            policy::Mode::Warn => "warn (L1 prompts and visible warnings)",
            policy::Mode::Confirm => {
                "confirm (L1 prompts, warnings, and gated confirmations)"
            }
        }
    );
    let model_ok = print_model_line(&config.config);
    let state_ok = print_state_line();
    healthy &= config_ok && model_ok && state_ok;

    if healthy {
        println!("  result:     ready");
        ExitCode::SUCCESS
    } else {
        println!("  result:     problems found");
        ExitCode::from(1)
    }
}

fn print_config_line(inspection: &policy::ConfigInspection) -> bool {
    let path = inspection
        .path
        .as_deref()
        .map(|path| ui::escape_for_display(&path.to_string_lossy()));
    let file_ok = match (path.as_deref(), inspection.file_state) {
        (Some(path), policy::ConfigFileState::Regular) => {
            if inspection.config.warnings.is_empty() {
                println!("  config:     {path} (present) — valid");
                true
            } else {
                println!(
                    "  config:     {path} (present) — INVALID ({} issue(s))",
                    inspection.config.warnings.len()
                );
                false
            }
        }
        (Some(path), policy::ConfigFileState::Missing) => {
            println!("  config:     {path} (absent — defaults in effect) — valid");
            true
        }
        (Some(path), policy::ConfigFileState::NonRegular) => {
            println!(
                "  config:     {path} (ignored — not a regular file; defaults in effect) — INVALID"
            );
            false
        }
        (Some(path), policy::ConfigFileState::TooLarge) => {
            println!(
                "  config:     {path} (ignored — exceeds 65536-byte limit; defaults in effect) — INVALID"
            );
            false
        }
        (Some(path), policy::ConfigFileState::Unavailable) => {
            println!("  config:     {path} (unavailable — defaults in effect) — INVALID");
            false
        }
        (None, _) => {
            println!("  config:     unavailable — HOME/XDG_CONFIG_HOME did not resolve a path");
            false
        }
    };
    for warning in &inspection.config.warnings {
        println!("              {}", ui::escape_for_display(warning));
    }
    if !inspection.mode_override_valid {
        println!("              OOPSINPUT_MODE is invalid; using shadow");
    }
    file_ok && inspection.mode_override_valid
}

fn print_plugin_line() -> bool {
    let (message, ok) = match inspect_plugin_install() {
        PluginInstallStatus::Installed => (
            "installed (marked ~/.zshrc block + regular installed file)",
            true,
        ),
        PluginInstallStatus::HomeUnavailable => {
            ("unavailable — HOME must be a nonempty absolute path", false)
        }
        PluginInstallStatus::ZshrcMissing => (
            "not installed — ~/.zshrc is absent (run zsh/install.zsh)",
            false,
        ),
        PluginInstallStatus::ZshrcUnsafe => (
            "invalid — ~/.zshrc is a symlink or not a regular file",
            false,
        ),
        PluginInstallStatus::ZshrcUnreadable => {
            ("invalid — ~/.zshrc could not be read safely", false)
        }
        PluginInstallStatus::ZshrcTooLarge => (
            "invalid — ~/.zshrc exceeds the 1 MiB diagnostic read cap",
            false,
        ),
        PluginInstallStatus::MarkerMissing => (
            "not installed — marked block absent from ~/.zshrc (run zsh/install.zsh)",
            false,
        ),
        PluginInstallStatus::MarkerDamaged => (
            "invalid — marked block in ~/.zshrc is duplicated, mismatched, or reversed",
            false,
        ),
        PluginInstallStatus::PluginMissing => (
            "incomplete — ~/.local/share/oopsinput/oopsinput.zsh is absent",
            false,
        ),
        PluginInstallStatus::PluginUnsafe => (
            "invalid — installed plugin path is a symlink or not a regular file",
            false,
        ),
        PluginInstallStatus::PluginUnreadable => {
            ("invalid — installed plugin file is unreadable", false)
        }
    };
    println!("  plugin:     {message}");
    ok
}

fn inspect_plugin_install() -> PluginInstallStatus {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return PluginInstallStatus::HomeUnavailable;
    };
    if !home.is_absolute() || home.as_os_str().is_empty() {
        return PluginInstallStatus::HomeUnavailable;
    }
    let zshrc = home.join(".zshrc");
    match std::fs::symlink_metadata(&zshrc) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PluginInstallStatus::ZshrcMissing;
        }
        Err(_) => return PluginInstallStatus::ZshrcUnreadable,
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return PluginInstallStatus::ZshrcUnsafe;
        }
        Ok(_) => {}
    }
    let file = match std::fs::File::open(&zshrc) {
        Ok(file) => file,
        Err(_) => return PluginInstallStatus::ZshrcUnreadable,
    };
    if state::opened_regular_file_metadata(&zshrc, &file, "~/.zshrc").is_err() {
        return PluginInstallStatus::ZshrcUnreadable;
    }
    let mut text = Vec::new();
    if file
        .take(ZSHRC_READ_CAP + 1)
        .read_to_end(&mut text)
        .is_err()
    {
        return PluginInstallStatus::ZshrcUnreadable;
    }
    if text.len() as u64 > ZSHRC_READ_CAP {
        return PluginInstallStatus::ZshrcTooLarge;
    }
    let mut begins = Vec::new();
    let mut ends = Vec::new();
    for (line_no, line) in text.split(|byte| *byte == b'\n').enumerate() {
        if line == MARK_BEGIN {
            begins.push(line_no);
        }
        if line == MARK_END {
            ends.push(line_no);
        }
    }
    if begins.is_empty() && ends.is_empty() {
        return PluginInstallStatus::MarkerMissing;
    }
    if begins.len() != 1 || ends.len() != 1 || ends[0] < begins[0] {
        return PluginInstallStatus::MarkerDamaged;
    }

    let plugin = home.join(".local/share/oopsinput/oopsinput.zsh");
    match std::fs::symlink_metadata(&plugin) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PluginInstallStatus::PluginMissing;
        }
        Err(_) => return PluginInstallStatus::PluginUnreadable,
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return PluginInstallStatus::PluginUnsafe;
        }
        Ok(_) => {}
    }
    let file = match std::fs::File::open(&plugin) {
        Ok(file) => file,
        Err(_) => return PluginInstallStatus::PluginUnreadable,
    };
    if state::opened_regular_file_metadata(&plugin, &file, "installed plugin").is_err() {
        PluginInstallStatus::PluginUnreadable
    } else {
        PluginInstallStatus::Installed
    }
}

fn print_widgets_line() -> bool {
    match inspect_widgets() {
        WidgetStatus::Wrapped(count) if count == ACCEPT_WIDGETS.len() => {
            println!(
                "  widgets:    {count}/{} wrapped in this shell",
                ACCEPT_WIDGETS.len()
            );
            true
        }
        WidgetStatus::Wrapped(count) => {
            println!(
                "  widgets:    {count}/{} wrapped — reload the plugin in this shell",
                ACCEPT_WIDGETS.len()
            );
            false
        }
        WidgetStatus::Inactive => {
            println!("  widgets:    plugin not active in this shell — open a new terminal");
            false
        }
        WidgetStatus::Stale => {
            println!(
                "  widgets:    live status unavailable — reload the plugin after other shell plugins"
            );
            false
        }
        WidgetStatus::Invalid => {
            println!("  widgets:    plugin status is malformed — reload the plugin");
            false
        }
    }
}

fn inspect_widgets() -> WidgetStatus {
    match std::env::var("OOPSINPUT_PLUGIN_ACTIVE") {
        Ok(value) if value == "1" => {}
        Err(std::env::VarError::NotPresent) => return WidgetStatus::Inactive,
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => return WidgetStatus::Invalid,
    }
    match std::env::var("OOPSINPUT_WIDGET_STATUS_FRESH") {
        Ok(value) if value == "1" => {}
        Ok(value) if value == "0" => return WidgetStatus::Stale,
        Err(std::env::VarError::NotPresent) => return WidgetStatus::Stale,
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => return WidgetStatus::Invalid,
    }
    let value = match std::env::var("OOPSINPUT_WRAPPED_WIDGETS") {
        Ok(value) => value,
        Err(_) => return WidgetStatus::Invalid,
    };
    let mut seen = [false; ACCEPT_WIDGETS.len()];
    if !value.is_empty() {
        for name in value.split(',') {
            let Some(index) = ACCEPT_WIDGETS.iter().position(|expected| *expected == name) else {
                return WidgetStatus::Invalid;
            };
            if seen[index] {
                return WidgetStatus::Invalid;
            }
            seen[index] = true;
        }
    }
    WidgetStatus::Wrapped(seen.into_iter().filter(|wrapped| *wrapped).count())
}

fn print_state_line() -> bool {
    let inspection = state::inspect_state();
    let Some(dir) = inspection.dir.as_deref() else {
        println!("  state:      unavailable — no absolute state directory resolves");
        return false;
    };
    let shown = ui::escape_for_display(&dir.to_string_lossy());
    if inspection.issues.is_empty() {
        if inspection.present {
            println!(
                "  state:      {shown} (0700; {} owned file(s) present at 0600)",
                inspection.checked_files
            );
        } else {
            println!("  state:      {shown} (not created yet — valid)");
        }
        return true;
    }
    println!(
        "  state:      {shown} — INVALID ({} issue(s))",
        inspection.issues.len()
    );
    for issue in inspection.issues {
        match issue {
            state::StateIssue::DirectoryUnavailable => {
                println!("              state directory metadata is unavailable")
            }
            state::StateIssue::DirectoryNotReal => {
                println!("              state path is a symlink or not a directory")
            }
            state::StateIssue::DirectoryUnreadable => {
                println!("              state directory cannot be enumerated")
            }
            state::StateIssue::DirectoryMode(mode) => {
                println!("              state directory mode is {mode:03o}; required 700")
            }
            state::StateIssue::EntryUnavailable(label) => {
                println!("              {label} metadata is unavailable")
            }
            state::StateIssue::EntryNotRegular(label) => {
                println!("              {label} is a symlink or not a regular file")
            }
            state::StateIssue::EntryMode(label, mode) => {
                println!("              {label} mode is {mode:03o}; required 600")
            }
        }
    }
    false
}

/// Doctor's model line: is Ollama up, and is the configured model pulled?
/// POST /api/show answers both without loading the model or running any
/// inference. The model name comes from the config file — untrusted display
/// text, so it goes through the escaper (SPEC §9-4, no exemptions).
fn print_model_line(cfg: &policy::Config) -> bool {
    let Some(name) = &cfg.model else {
        println!("  model:      disabled (deterministic-only)");
        return true;
    };
    let shown = ui::escape_for_display(name);
    let body = serde_json::json!({ "model": name }).to_string();
    let deadline = Instant::now() + std::time::Duration::from_millis(cfg.model_timeout_ms);
    let result = model::post_json(
        model::ollama_addr(),
        "/api/show",
        body.as_bytes(),
        deadline,
        SHOW_RESPONSE_CAP,
    );
    match result {
        Ok(_) => {
            println!("  model:      {shown} (Ollama reachable, model present)");
            true
        }
        Err(model::ModelError::Status(404)) => {
            println!(
                "  model:      {shown} — Ollama is up but this model isn't pulled (ollama pull {shown})"
            );
            false
        }
        Err(model::ModelError::Connect) => {
            println!(
                "  model:      {shown} — Ollama not reachable at 127.0.0.1:11434; runs deterministic-only"
            );
            false
        }
        Err(model::ModelError::UntrustedPeer) => {
            println!(
                "  model:      {shown} — the process on 127.0.0.1:11434 is not owned by you or a \
                 system account; refusing to talk to it (runs deterministic-only)"
            );
            false
        }
        Err(model::ModelError::Timeout) => {
            println!(
                "  model:      {shown} — Ollama didn't answer within {} ms; runs deterministic-only",
                cfg.model_timeout_ms
            );
            false
        }
        Err(_) => {
            println!(
                "  model:      {shown} — unexpected reply from 127.0.0.1:11434; runs deterministic-only"
            );
            false
        }
    }
}

/// PATH lookup via direct metadata checks — never through a shell (SPEC §9).
fn find_in_path(name: &str) -> Option<String> {
    find_in_path_list(&std::env::var("PATH").ok()?, name)
}

fn find_in_path_list(path: &str, name: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = format!("{dir}/{name}");
        if let Ok(meta) = std::fs::metadata(&candidate)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 != 0
        {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_in_path_finds_sh() {
        // Smoke test for the thin env wrapper: `find_in_path` must read $PATH
        // and hand it to the (hermetically tested) lookup below. Its premise
        // is environmental, so it says so when it fails rather than looking
        // like a product bug (test-audit 2026-08-06).
        assert!(
            find_in_path("sh").is_some(),
            "no `sh` on $PATH — this asserts the environment, not the code; \
             the real lookup logic is pinned by find_in_path_requires_executable_bit"
        );
    }

    #[test]
    fn find_in_path_misses_nonsense() {
        assert!(find_in_path("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn find_in_path_requires_executable_bit() {
        // Regression (bughunt #3): a plain file named like the binary was
        // reported as found by doctor.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("oopsinput-xbit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("zsh");
        std::fs::write(&file, "not a binary").unwrap();

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(find_in_path_list(dir.to_str().unwrap(), "zsh").is_none());

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(find_in_path_list(dir.to_str().unwrap(), "zsh").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
