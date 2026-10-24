//! `rack guide` and the per-command help must work on a machine with no server, token or config.

use std::process::{Command, Output};

fn rack(args: &[&str]) -> Output {
    let home = std::env::temp_dir().join("rack-guide-test-home");
    std::fs::create_dir_all(&home).expect("home");
    Command::new(env!("CARGO_BIN_EXE_rack"))
        .args(args)
        .env_clear()
        .env("HOME", &home)
        .env("APPDATA", &home)
        .output()
        .expect("run rack")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("utf-8")
}

#[test]
fn guide_needs_no_server_token_or_config() {
    let out = rack(&["guide"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    assert!(text.starts_with(&format!("rack {} guide (", env!("CARGO_PKG_VERSION"))));
    assert!(text.contains("## Commands"));

    let changes = stdout(&rack(&["guide", "--changes"]));
    assert!(changes.starts_with(&format!("rack {}: what changed", env!("CARGO_PKG_VERSION"))));
}

#[test]
fn every_form_of_command_help_prints_the_same_text() {
    let list = stdout(&rack(&["guide", "--list"]));
    assert!(list.lines().count() >= 20);
    for name in list.lines() {
        let guide = rack(&["guide", name]);
        assert!(guide.status.success(), "rack guide {name}");
        let text = stdout(&guide);
        assert!(text.starts_with(&format!("# rack {name}\n")), "{name}");
        assert_eq!(stdout(&rack(&["help", name])), text, "rack help {name}");
        assert_eq!(stdout(&rack(&[name, "--help"])), text, "rack {name} --help");
    }
}

#[test]
fn unknown_command_is_a_usage_error_listing_the_valid_ones() {
    for args in [["guide", "nope"], ["help", "nope"]] {
        let out = rack(&args);
        assert_eq!(out.status.code(), Some(2));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("unknown command `nope`") && err.contains("get-attachment"),
            "{err}"
        );
    }
    assert_eq!(rack(&["guide", "--list", "--changes"]).status.code(), Some(2));
}
