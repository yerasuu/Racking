//! `rack update` (and `--check`) installs the newest `rack` the origin
//! publishes for this build's target; `rack publish` puts one there.
//!
//! Integrity rests on the SHA-256 in the origin's manifest, with no signature
//! (decision #21, option B): whoever controls the origin controls the binary.
//! What this module does guarantee is that a failed update never leaves the
//! machine without a working `rack`: the new file is checked (size, hash, and
//! that it runs and reports the expected version) before it replaces anything.

mod origin;
mod replace;
mod version;

use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::client::{self, Client, RackError, Result};
pub use origin::Channel;
use origin::{Available, Release};
use version::Version;

const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Set by build.rs from Cargo's `TARGET`.
pub const TARGET: &str = env!("RACK_TARGET");
/// A healthy `rack --version` answers at once; this only bounds a broken binary.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Why an update stopped, and what the user can do about it.
struct Failure {
    why: String,
    hint: &'static str,
}

pub fn check(c: &Client, channel: Channel) -> Result<()> {
    let current = current()?;
    let available = origin::latest(c, TARGET, channel)?;
    print!("{}", check_lines(&current, &available, channel));
    Ok(())
}

/// One line on the stable channel; with `--pre`, one per slot, so the test
/// build shows even when it is not the one `rack update --pre` would take.
fn check_lines(current: &Version, available: &Available, channel: Channel) -> String {
    let head = format!("rack {current} ({TARGET})");
    let Channel::Pre = channel else {
        return match &available.stable {
            None => format!("{head}, available: none published, no update\n"),
            Some(r) if r.version > *current => {
                format!("{head}, available {}: update available, run `rack update`\n", r.version)
            }
            Some(r) => format!("{head}, available {}: up to date\n", r.version),
        };
    };
    let stable = match &available.stable {
        None => format!("{head}, stable: none published"),
        Some(r) if r.version > *current => {
            format!("{head}, stable {}: update available, run `rack update`", r.version)
        }
        Some(r) => format!("{head}, stable {}: up to date", r.version),
    };
    let pre = match (&available.pre, &available.stable) {
        (None, _) => format!("{head}, pre: none published"),
        (Some(p), Some(s)) if p.version <= s.version => format!(
            "{head}, pre {}: not newer than stable {}, ignored",
            p.version, s.version
        ),
        (Some(p), _) if p.version > *current => {
            format!("{head}, pre {}: update available, run `rack update --pre`", p.version)
        }
        (Some(p), _) => format!("{head}, pre {}: up to date", p.version),
    };
    format!("{stable}\n{pre}\n")
}

/// The release `rack update` installs on `channel`, if one is newer than `current`.
/// On `Pre` the higher of both slots wins, the stable one on a tie, so a test
/// build `X.Y.Z-N` moves on to its stable `X.Y.Z` once that is published.
fn pick(available: &Available, channel: Channel) -> Option<&Release> {
    let stable = available.stable.as_ref();
    match (channel, stable, available.pre.as_ref()) {
        (Channel::Pre, Some(s), Some(p)) if p.version > s.version => Some(p),
        (Channel::Pre, None, pre) => pre,
        _ => stable,
    }
}

pub fn update(c: &Client, channel: Channel) -> Result<()> {
    let current = current()?;
    let available = origin::latest(c, TARGET, channel).map_err(|e| {
        failed(
            &current,
            Failure {
                why: format!("cannot read the manifest: {}", one_line(&e)),
                hint: "check RACK_URL and RACK_TOKEN with `rack whoami`, then retry",
            },
        )
    })?;
    let release = pick(&available, channel).ok_or_else(|| {
        failed(
            &current,
            Failure {
                why: format!("no rack published for {TARGET}"),
                hint: "an admin agent publishes one with `rack publish`",
            },
        )
    })?;
    if release.version <= current {
        println!("rack {current} is up to date (available {})", release.version);
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| {
        failed(
            &current,
            Failure {
                why: format!("cannot locate the running rack: {e}"),
                hint: "download it by hand from GET /dist/manifest",
            },
        )
    })?;
    let Some(dir) = exe.parent() else {
        return Err(failed(
            &current,
            Failure {
                why: format!("{} has no directory", exe.display()),
                hint: "run rack from its install path",
            },
        ));
    };
    // Same directory as the executable, so the final rename never crosses file systems.
    let tmp = dir.join(format!(
        ".rack-update-{}{}",
        std::process::id(),
        std::env::consts::EXE_SUFFIX
    ));
    let result = install(c, release, &exe, &tmp);
    if result.is_err()
        && let Err(e) = fs::remove_file(&tmp)
        && e.kind() != io::ErrorKind::NotFound
    {
        eprintln!("rack: could not remove {}: {e}", tmp.display());
    }
    result.map_err(|f| failed(&current, f))?;
    println!("rack updated {current} -> {}", release.version);
    Ok(())
}

fn install(c: &Client, release: &Release, exe: &Path, tmp: &Path) -> std::result::Result<(), Failure> {
    const REPUBLISH: &str = "retry; if it repeats, ask the owner to publish the binary again";
    origin::fetch(c, TARGET, release, tmp).map_err(|e| Failure {
        why: format!("download: {}", one_line(&e)),
        hint: REPUBLISH,
    })?;
    let size = fs::metadata(tmp).map(|m| m.len()).map_err(|e| Failure {
        why: format!("cannot read {}: {e}", tmp.display()),
        hint: REPUBLISH,
    })?;
    if size != release.size {
        return Err(Failure {
            why: format!("downloaded {size} bytes, the manifest says {}", release.size),
            hint: REPUBLISH,
        });
    }
    let sha = client::sha256_file(tmp).map_err(|e| Failure {
        why: one_line(&e),
        hint: REPUBLISH,
    })?;
    if sha != release.sha256 {
        return Err(Failure {
            why: format!("sha256 {sha} differs from the manifest's {}", release.sha256),
            hint: "do not run it; ask the owner to check the service and publish again",
        });
    }
    // The running binary's permissions are the ones that work here (executable on Unix).
    let perms = fs::metadata(exe).map(|m| m.permissions()).map_err(|e| Failure {
        why: format!("cannot read {}: {e}", exe.display()),
        hint: "check the permissions of the install directory",
    })?;
    fs::set_permissions(tmp, perms).map_err(|e| Failure {
        why: format!("cannot make {} executable: {e}", tmp.display()),
        hint: "check the permissions of the install directory",
    })?;
    let reported = reported_version(tmp).map_err(|why| Failure {
        why: format!("the new binary does not run: {why}"),
        hint: "it may be built for another platform; ask the owner to publish the right target",
    })?;
    if reported != release.version {
        return Err(Failure {
            why: format!(
                "the new binary reports {reported}, the manifest says {}",
                release.version
            ),
            hint: "ask the owner to publish the binary with its own version",
        });
    }
    let replaced = if cfg!(windows) {
        let old = exe.with_file_name("rack.old");
        replace::replace_windows(tmp, exe, &old)
    } else {
        replace::replace_unix(tmp, exe)
    };
    replaced.map_err(|why| Failure {
        why,
        hint: "check the permissions of the install directory",
    })
}

/// `rack publish`: uploads `file` as the release for `target`. Without
/// `version`, the file's own `--version` output names it.
pub fn publish(c: &Client, target: &str, version: Option<&str>, file: &Path) -> Result<()> {
    // The target goes into the URL path; the service checks it against its list of targets.
    if target.is_empty()
        || !target
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(RackError::Usage(format!("--target `{target}` must be a target triple")));
    }
    let version: Version = match version {
        Some(v) => v.parse().map_err(|e| RackError::Usage(format!("--version: {e}")))?,
        None => reported_version(file).map_err(|why| {
            RackError::Usage(format!(
                "pass --version: {} does not run here to tell its own ({why})",
                file.display()
            ))
        })?,
    };
    // A service from before the test slot stores every upload as the release, so a
    // test build sent there would reach everyone running a plain `rack update`.
    if version.is_prerelease() && !origin::has_test_slot(c)? {
        return Err(RackError::Local(format!(
            "the server keeps no test builds apart and would publish {version} as the stable release; \
             update racking-server first, or publish a stable X.Y.Z"
        )));
    }
    let v = version.to_string();
    let published: Value = c.put_file(&format!("/dist/{target}/rack"), file, &[("X-Version", &v)])?;
    println!(
        "published rack {v} for {target} in the {} slot: {} bytes, sha256 {}",
        published["slot"].as_str().unwrap_or("stable"),
        published["size"],
        published["sha256"].as_str().unwrap_or("?")
    );
    Ok(())
}

pub fn print_version() {
    println!("{}", version_line(CURRENT));
}

/// What `rack --version` prints and `reported_version` reads back: the whole
/// version, pre-release part included, since `rack update` checks the new binary by it.
fn version_line(version: &str) -> String {
    format!("rack {version}")
}

/// Runs `exe --version` with a deadline and reads the `rack X.Y.Z` it prints.
fn reported_version(exe: &Path) -> std::result::Result<Version, String> {
    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("--version did not answer in {} s", VERSION_TIMEOUT.as_secs()));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    if !status.success() {
        return Err(format!("--version failed ({status})"));
    }
    let mut out = String::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(4096)
            .read_to_string(&mut out)
            .map_err(|e| format!("--version output: {e}"))?;
    }
    parse_reported(&out)
}

fn parse_reported(out: &str) -> std::result::Result<Version, String> {
    let line = out.lines().next().unwrap_or_default().trim();
    line.strip_prefix("rack ")
        .ok_or_else(|| {
            format!(
                "--version printed `{}`, not `rack X.Y.Z`",
                line.chars().take(60).collect::<String>()
            )
        })?
        .parse()
}

fn current() -> Result<Version> {
    CURRENT
        .parse()
        .map_err(|e| RackError::Local(format!("this build's own version: {e}")))
}

fn failed(current: &Version, f: Failure) -> RackError {
    RackError::Local(format!(
        "update failed: {}; rack {current} left in place (hint: {})",
        f.why, f.hint
    ))
}

fn one_line(e: &RackError) -> String {
    e.to_string().replace('\n', "; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_version_lines() {
        assert_eq!(
            parse_reported("rack 1.2.3\n").map(|v| v.to_string()),
            Ok("1.2.3".to_owned())
        );
        assert!(parse_reported("racking-server 1.2.3\n").is_err());
        assert!(parse_reported("").is_err());
        assert!(parse_reported("rack latest").is_err());
    }

    #[test]
    fn own_version_and_target() {
        assert!(current().is_ok());
        assert!(!TARGET.is_empty(), "build.rs embeds the target triple");
    }

    #[test]
    fn version_line_keeps_the_prerelease_part() {
        // What a binary built with `version = "0.1.4-1"` prints, and reads back when it is the new one.
        assert_eq!(version_line("0.1.4-1"), "rack 0.1.4-1");
        assert_eq!(
            parse_reported(&format!("{}\n", version_line("0.1.4-1"))).map(|v| v.to_string()),
            Ok("0.1.4-1".to_owned())
        );
        assert_eq!(version_line(CURRENT), format!("rack {}", env!("CARGO_PKG_VERSION")));
    }

    fn v(s: &str) -> Version {
        s.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    fn release(version: &str, channel: Channel) -> Release {
        Release {
            version: v(version),
            channel,
            file: "rack".to_owned(),
            size: 1,
            sha256: "0".repeat(64),
        }
    }

    fn offer(stable: Option<&str>, pre: Option<&str>) -> Available {
        Available {
            stable: stable.map(|s| release(s, Channel::Stable)),
            pre: pre.map(|p| release(p, Channel::Pre)),
        }
    }

    fn picked(available: &Available, channel: Channel) -> Option<String> {
        pick(available, channel).map(|r| r.version.to_string())
    }

    #[test]
    fn pick_follows_the_channel() {
        let both = offer(Some("0.1.3"), Some("0.1.4-2"));
        assert_eq!(picked(&both, Channel::Stable).as_deref(), Some("0.1.3"));
        assert_eq!(picked(&both, Channel::Pre).as_deref(), Some("0.1.4-2"));
        assert!(pick(&both, Channel::Pre).is_some_and(|r| r.channel == Channel::Pre));

        // Once 0.1.4 is out, the stale 0.1.4-2 left in the pre slot is passed over.
        let released = offer(Some("0.1.4"), Some("0.1.4-2"));
        assert_eq!(picked(&released, Channel::Pre).as_deref(), Some("0.1.4"));
        assert!(pick(&released, Channel::Pre).is_some_and(|r| r.channel == Channel::Stable));
        let tie = offer(Some("0.1.4"), Some("0.1.4"));
        assert!(pick(&tie, Channel::Pre).is_some_and(|r| r.channel == Channel::Stable));

        assert_eq!(
            picked(&offer(None, Some("0.1.4-1")), Channel::Pre).as_deref(),
            Some("0.1.4-1")
        );
        assert_eq!(picked(&offer(None, Some("0.1.4-1")), Channel::Stable), None);
        assert_eq!(picked(&offer(None, None), Channel::Pre), None);
    }

    #[test]
    fn a_test_build_updates_to_its_stable_release() {
        // Running 0.1.4-1, the default channel installs 0.1.4 because 0.1.4 > 0.1.4-1.
        let running = v("0.1.4-1");
        let released = offer(Some("0.1.4"), Some("0.1.4-1"));
        for channel in [Channel::Stable, Channel::Pre] {
            assert!(pick(&released, channel).is_some_and(|r| r.version > running));
        }
        // And never steps back to an older stable one.
        let older = offer(Some("0.1.3"), Some("0.1.4-1"));
        assert!(pick(&older, Channel::Stable).is_some_and(|r| r.version < running));
    }

    #[test]
    fn check_prints_one_line_per_channel() {
        let head = format!("rack 0.1.3 ({TARGET})");
        let both = offer(Some("0.1.3"), Some("0.1.4-2"));
        assert_eq!(
            check_lines(&v("0.1.3"), &both, Channel::Stable),
            format!("{head}, available 0.1.3: up to date\n")
        );
        assert_eq!(
            check_lines(&v("0.1.3"), &both, Channel::Pre),
            format!(
                "{head}, stable 0.1.3: up to date\n\
                 {head}, pre 0.1.4-2: update available, run `rack update --pre`\n"
            )
        );
        assert_eq!(
            check_lines(&v("0.1.3"), &offer(Some("0.1.4"), Some("0.1.4-2")), Channel::Pre),
            format!(
                "{head}, stable 0.1.4: update available, run `rack update`\n\
                 {head}, pre 0.1.4-2: not newer than stable 0.1.4, ignored\n"
            )
        );
        assert_eq!(
            check_lines(&v("0.1.3"), &offer(None, None), Channel::Pre),
            format!("{head}, stable: none published\n{head}, pre: none published\n")
        );
    }
}
