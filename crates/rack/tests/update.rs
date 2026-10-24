//! `rack publish` and `rack update` end to end against an in-process server,
//! on a copy of `rack` in a temp directory. One test function on purpose: the
//! test writes executables and runs them, and a parallel test forking at that
//! moment could hold the file open for writing (ETXTBSY on exec).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LINUX: &str = "linux-token-0123456789abcdef";
const WINDOWS: &str = "windows-token-0123456789abcdef";
const TARGET: &str = env!("RACK_TARGET");

struct Env {
    root: PathBuf,
    url: String,
    server: Option<racking_server::Running>,
}

impl Env {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("rack-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("bin")).expect("bin dir");
        let config = racking_server::Config::parse(&format!(
            "bind = \"127.0.0.1:0\"\ndata_dir = {:?}\n[agents.linux]\ntoken = \"{LINUX}\"\nadmin = true\n\
             [agents.windows]\ntoken = \"{WINDOWS}\"\n",
            root.join("data").display().to_string()
        ))
        .expect("config");
        let server = racking_server::start(&config).expect("start");
        let url = format!("http://{}", server.addr());
        Self {
            root,
            url,
            server: Some(server),
        }
    }

    fn run(&self, exe: &Path, token: &str, args: &[&str]) -> Output {
        Command::new(exe)
            .args(args)
            .env("RACK_URL", &self.url)
            .env("RACK_TOKEN", token)
            .env_remove("RACK_CONFIG")
            .output()
            .expect("run rack")
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(out: &Output) -> String {
    format!(
        "status {}\nstdout: {}stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("dir")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    names
}

#[test]
fn publish_then_update_replaces_and_failures_keep_the_old_binary() {
    let env = Env::new();
    let real = Path::new(env!("CARGO_BIN_EXE_rack"));
    let installed = env.root.join("bin/rack");
    fs::copy(real, &installed).expect("copy rack");
    let original = fs::read(&installed).expect("read rack");

    let out = env.run(&installed, LINUX, &["--version"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("rack {}\n", env!("CARGO_PKG_VERSION"))
    );

    let out = env.run(&installed, LINUX, &["update", "--check"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("none published"),
        "{}",
        text(&out)
    );

    // Only an admin agent publishes; the version comes from the file's own --version.
    let newer = env.script("rack-new", "echo rack 99.0.0");
    let out = env.run(
        real,
        WINDOWS,
        &["publish", "--target", TARGET, &newer.display().to_string()],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not_admin"),
        "{}",
        text(&out)
    );
    let out = env.run(
        real,
        LINUX,
        &["publish", "--target", TARGET, &newer.display().to_string()],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with(&format!("published rack 99.0.0 for {TARGET}")));

    let out = env.run(&installed, WINDOWS, &["update", "--check"]);
    assert!(out.status.success(), "{}", text(&out));
    let line = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(line.lines().count(), 1, "{line}");
    assert!(line.contains("available 99.0.0: update available"), "{line}");

    let out = env.run(&installed, WINDOWS, &["update"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(fs::read(&installed).expect("read"), fs::read(&newer).expect("read new"));
    let out = Command::new(&installed).arg("--version").output().expect("run new");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "rack 99.0.0\n");
    assert_eq!(files_in(&env.root.join("bin")), ["rack"], "no temp file left");

    // A replacement that does not run is refused and the old binary stays.
    fs::copy(real, &installed).expect("reinstall rack");
    let broken = env.script("rack-broken", "exit 1");
    let out = env.run(
        real,
        LINUX,
        &[
            "publish",
            "--target",
            TARGET,
            "--version",
            "99.0.1",
            &broken.display().to_string(),
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = env.run(&installed, WINDOWS, &["update"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(err.contains("update failed") && err.contains("hint:"), "{err}");
    assert_eq!(fs::read(&installed).expect("read"), original, "old binary kept");
    assert_eq!(files_in(&env.root.join("bin")), ["rack"], "no temp file left");

    // So is one that reports another version than the manifest.
    let liar = env.script("rack-liar", "echo rack 98.0.0");
    let out = env.run(
        real,
        LINUX,
        &[
            "publish",
            "--target",
            TARGET,
            "--version",
            "99.0.2",
            &liar.display().to_string(),
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = env.run(&installed, WINDOWS, &["update"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("reports 98.0.0"),
        "{}",
        text(&out)
    );
    assert_eq!(fs::read(&installed).expect("read"), original, "old binary kept");

    // An older release than the running one is never installed.
    let out = env.run(
        real,
        LINUX,
        &[
            "publish",
            "--target",
            TARGET,
            "--version",
            "0.0.1",
            &liar.display().to_string(),
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = env.run(&installed, WINDOWS, &["update"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("up to date"),
        "{}",
        text(&out)
    );
    assert_eq!(fs::read(&installed).expect("read"), original);

    // A test build goes to its own slot, read from its own --version, and leaves the stable release.
    let test_build = env.script("rack-test", "echo rack 99.1.0-1");
    let out = env.run(
        real,
        LINUX,
        &["publish", "--target", TARGET, &test_build.display().to_string()],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .starts_with(&format!("published rack 99.1.0-1 for {TARGET} in the pre slot")),
        "{}",
        text(&out)
    );

    // The default channel does not see it.
    let out = env.run(&installed, WINDOWS, &["update", "--check"]);
    let line = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(line.lines().count(), 1, "{line}");
    assert!(line.contains("available 0.0.1: up to date"), "{line}");
    let out = env.run(&installed, WINDOWS, &["update"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        fs::read(&installed).expect("read"),
        original,
        "stable channel ignores the test build"
    );

    // --pre shows both slots and installs the test build, downloaded from /dist/pre/.
    let out = env.run(&installed, WINDOWS, &["update", "--check", "--pre"]);
    let lines = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(lines.lines().count(), 2, "{lines}");
    assert!(lines.contains("stable 0.0.1: up to date"), "{lines}");
    assert!(
        lines.contains("pre 99.1.0-1: update available, run `rack update --pre`"),
        "{lines}"
    );
    let out = env.run(&installed, WINDOWS, &["update", "--pre"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        fs::read(&installed).expect("read"),
        fs::read(&test_build).expect("read test")
    );

    // Once the stable 99.1.0 is out it outranks 99.1.0-1, still in the pre slot, even on --pre.
    fs::copy(real, &installed).expect("reinstall rack");
    let stable = env.script("rack-stable", "echo rack 99.1.0");
    let out = env.run(
        real,
        LINUX,
        &["publish", "--target", TARGET, &stable.display().to_string()],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("in the stable slot"),
        "{}",
        text(&out)
    );
    let out = env.run(&installed, WINDOWS, &["update", "--check", "--pre"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("pre 99.1.0-1: not newer than stable 99.1.0, ignored"),
        "{}",
        text(&out)
    );
    let out = env.run(&installed, WINDOWS, &["update", "--pre"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        fs::read(&installed).expect("read"),
        fs::read(&stable).expect("read stable")
    );
    assert_eq!(files_in(&env.root.join("bin")), ["rack"], "no temp file left");
}
