//! Where `rack` and `racking-mcp` find the server URL and token, and the config
//! directory shared with the state file. One implementation, so the two binaries
//! cannot disagree on precedence.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::client::{RackError, Result, local};

/// `~/.config/rack` on Unix, `%APPDATA%\rack` on Windows.
pub fn default_dir() -> Result<PathBuf> {
    default_dir_with(&|name| std::env::var_os(name))
}

fn default_dir_with(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let base = if cfg!(windows) {
        env("APPDATA").map(PathBuf::from)
    } else {
        env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("rack"))
        .ok_or_else(|| RackError::Usage("cannot find a config directory; pass --state FILE".to_owned()))
}

/// Server URL and token. Per key, the environment wins over the file in
/// `RACK_CONFIG`, which wins over the default `rack.conf`.
pub fn credentials() -> Result<(String, String)> {
    credentials_with(&|name| std::env::var_os(name))
}

fn credentials_with(env: &dyn Fn(&str) -> Option<OsString>) -> Result<(String, String)> {
    let var = |name: &str| env(name).filter(|v| !v.is_empty());
    let mut url = var("RACK_URL").map(|v| v.to_string_lossy().into_owned());
    let mut token = var("RACK_TOKEN").map(|v| v.to_string_lossy().into_owned());
    let default_file = default_dir_with(env).ok().map(|d| d.join("rack.conf"));
    if let Some(path) = var("RACK_CONFIG").map(PathBuf::from) {
        let text = std::fs::read_to_string(&path).map_err(|e| local(&format!("read {}", path.display()), e))?;
        fill(&text, &mut url, &mut token);
    }
    // Skipped once both keys are set, so an unreadable default file cannot break an explicit setup.
    if let (Some(path), true) = (&default_file, url.is_none() || token.is_none()) {
        read_default(path, &mut url, &mut token)?;
    }
    let at = default_file.map_or_else(String::new, |p| format!(", or put it in {}", p.display()));
    let url = url.ok_or_else(|| {
        RackError::Usage(format!(
            "set RACK_URL (e.g. http://127.0.0.1:7878) or `url = \"...\"` in RACK_CONFIG{at}"
        ))
    })?;
    let token = token.ok_or_else(|| {
        RackError::Usage(format!(
            "set RACK_TOKEN (ask the server owner) or `token = \"...\"` in RACK_CONFIG{at}"
        ))
    })?;
    Ok((url, token))
}

fn read_default(path: &Path, url: &mut Option<String>, token: &mut Option<String>) -> Result<()> {
    match std::fs::read_to_string(path) {
        Ok(text) => fill(&text, url, token),
        // Absent is the normal case for anyone configuring through the environment.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(local(&format!("read {}", path.display()), e)),
    }
    Ok(())
}

/// Sets only the keys still empty, so earlier sources keep priority.
fn fill(text: &str, url: &mut Option<String>, token: &mut Option<String>) {
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "url" if url.is_none() => *url = Some(value.to_owned()),
            "token" if token.is_none() => *token = Some(value.to_owned()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use super::*;

    struct Home(PathBuf);

    impl Home {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rack-config-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("rack")).expect("create");
            Self(dir)
        }

        fn write_default(&self, text: &str) {
            fs::write(self.0.join("rack/rack.conf"), text).expect("write");
        }

        fn resolve(&self, vars: &[(&str, &str)]) -> Result<(String, String)> {
            let mut map: HashMap<String, OsString> = vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).into())).collect();
            // Both keys, so the test follows whichever one the platform reads.
            map.insert("APPDATA".to_owned(), self.0.clone().into());
            map.insert("XDG_CONFIG_HOME".to_owned(), self.0.clone().into());
            credentials_with(&|n| map.get(n).cloned())
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pair(url: &str, token: &str) -> (String, String) {
        (url.to_owned(), token.to_owned())
    }

    #[test]
    fn default_file_is_used_when_nothing_is_set() {
        let home = Home::new("default");
        home.write_default("url = \"http://h:1\"\ntoken = \"t0\"\n");
        assert_eq!(home.resolve(&[]).expect("resolve"), pair("http://h:1", "t0"));
    }

    #[test]
    fn env_beats_rack_config_beats_default_per_key() {
        let home = Home::new("order");
        home.write_default("url = \"http://default\"\ntoken = \"default\"\n");
        let custom = home.0.join("custom.conf");
        fs::write(&custom, "url = \"http://custom\"\ntoken = \"custom\"\n").expect("write");
        let custom = custom.to_str().expect("utf8");
        assert_eq!(
            home.resolve(&[("RACK_URL", "http://env"), ("RACK_CONFIG", custom)])
                .expect("resolve"),
            pair("http://env", "custom")
        );
        assert_eq!(
            home.resolve(&[("RACK_TOKEN", "env")]).expect("resolve"),
            pair("http://default", "env")
        );
    }

    #[test]
    fn missing_default_file_falls_through_to_the_usage_error() {
        let home = Home::new("missing");
        let err = home.resolve(&[]).expect_err("nothing configured").to_string();
        assert!(err.contains("RACK_URL") && err.contains("rack.conf"), "{err}");
        let err = home
            .resolve(&[("RACK_URL", "http://x")])
            .expect_err("no token")
            .to_string();
        assert!(err.contains("RACK_TOKEN"), "{err}");
    }

    #[test]
    fn unreadable_default_file_is_a_local_error() {
        let home = Home::new("unreadable");
        // A directory named rack.conf cannot be read as a file, on any platform.
        fs::create_dir(home.0.join("rack/rack.conf")).expect("mkdir");
        let err = home.resolve(&[]).expect_err("unreadable");
        assert!(matches!(err, RackError::Local(_)), "{err}");
    }

    #[test]
    fn complete_env_ignores_the_default_file() {
        let home = Home::new("env-only");
        fs::create_dir(home.0.join("rack/rack.conf")).expect("mkdir");
        let got = home
            .resolve(&[("RACK_URL", "http://e"), ("RACK_TOKEN", "t")])
            .expect("resolve");
        assert_eq!(got, pair("http://e", "t"));
    }
}
