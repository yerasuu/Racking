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

pub fn check(c: &Client) -> Result<()> {
    let current = current()?;
    let line = match origin::latest(c, TARGET)? {
        None => format!("rack {current} ({TARGET}), available: none published, no update"),
        Some(r) if r.version > current => format!(
            "rack {current} ({TARGET}), available {}: update available, run `rack update`",
            r.version
        ),
        Some(r) => format!("rack {current} ({TARGET}), available {}: up to date", r.version),
    };
    println!("{line}");
    Ok(())
}

pub fn update(c: &Client) -> Result<()> {
    let current = current()?;
    let release = origin::latest(c, TARGET)
        .map_err(|e| {
            failed(
                &current,
                Failure {
                    why: format!("cannot read the manifest: {}", one_line(&e)),
                    hint: "check RACK_URL and RACK_TOKEN with `rack whoami`, then retry",
                },
            )
        })?
        .ok_or_else(|| {
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
    let result = install(c, &release, &exe, &tmp);
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

fn install(c: &Client, release: &origin::Release, exe: &Path, tmp: &Path) -> std::result::Result<(), Failure> {
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
    let v = version.to_string();
    let published: Value = c.put_file(&format!("/dist/{target}/rack"), file, &[("X-Version", &v)])?;
    println!(
        "published rack {v} for {target}: {} bytes, sha256 {}",
        published["size"],
        published["sha256"].as_str().unwrap_or("?")
    );
    Ok(())
}

pub fn print_version() {
    println!("rack {CURRENT}");
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
}
