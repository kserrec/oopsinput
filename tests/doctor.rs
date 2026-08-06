//! Doctor output consistency. Regression (bughunt 2026-08-06): the config
//! line hardcoded ~/.config while the mode line honored XDG_CONFIG_HOME, so
//! doctor could report the config "absent — defaults in effect" while
//! simultaneously reporting a mode read from that very config.

use std::process::Command;

fn doctor_output(home: &std::path::Path, xdg: Option<&std::path::Path>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oopsinput"));
    cmd.arg("doctor").env("HOME", home);
    cmd.env_remove("OOPSINPUT_MODE");
    match xdg {
        Some(dir) => cmd.env("XDG_CONFIG_HOME", dir),
        None => cmd.env_remove("XDG_CONFIG_HOME"),
    };
    let out = cmd.output().expect("run doctor");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn doctor_config_line_and_mode_line_agree_under_xdg() {
    let base = std::env::temp_dir().join(format!("oopsinput-doctor-{}", std::process::id()));
    let home = base.join("home");
    let xdg = base.join("xdg");
    std::fs::create_dir_all(home.join(".config/oopsinput")).unwrap();
    std::fs::create_dir_all(xdg.join("oopsinput")).unwrap();
    std::fs::write(xdg.join("oopsinput/config"), "mode = suggest\n").unwrap();

    let out = doctor_output(&home, Some(&xdg));
    let config_display = xdg.join("oopsinput/config");
    assert!(
        out.contains(&format!("{} (present)", config_display.display())),
        "config line must show the XDG path doctor's mode actually reads:\n{out}"
    );
    assert!(
        out.contains("mode:       suggest"),
        "mode must come from the displayed config:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_default_path_reports_shadow_when_no_config() {
    let base = std::env::temp_dir().join(format!("oopsinput-doctor2-{}", std::process::id()));
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = doctor_output(&home, None);
    assert!(
        out.contains("(absent — defaults in effect)"),
        "missing config must read as absent:\n{out}"
    );
    assert!(
        out.contains("mode:       shadow"),
        "no config means shadow:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&base);
}
