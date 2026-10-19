//! User settings (`scn config`): a small JSON map in `$XDG_CONFIG_HOME/scion/config.json`
//! (default `~/.config/scion/config.json`; `SCION_CONFIG_FILE` overrides the path).
//!
//! The commit author comes from, in order: `--author` / `SCION_AUTHOR`, `user.name` and
//! `user.email` here, then the same keys in git's configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::output::{CliError, Render};

/// Keys `scn config` accepts.
pub const KEYS: [&str; 2] = ["user.name", "user.email"];

/// Where the settings file lives.
pub fn path(env: impl Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(f) = env("SCION_CONFIG_FILE").filter(|f| !f.is_empty()) {
        return PathBuf::from(f);
    }
    env("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env("HOME").unwrap_or_else(|| ".".into())).join(".config"))
        .join("scion")
        .join("config.json")
}

pub fn load(path: &Path) -> Result<BTreeMap<String, String>, CliError> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)
            .map_err(|e| CliError::Usage(format!("{} is not valid JSON: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(CliError::Usage(format!("read {}: {e}", path.display()))),
    }
}

fn save(path: &Path, map: &BTreeMap<String, String>) -> Result<(), CliError> {
    let fail = |e: std::io::Error| CliError::Usage(format!("write {}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec_pretty(map).map_err(|e| fail(std::io::Error::other(e)))?;
    std::fs::write(&tmp, body).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(fail)
}

fn check_key(key: &str) -> Result<(), CliError> {
    if KEYS.contains(&key) {
        Ok(())
    } else {
        Err(CliError::Usage(format!(
            "unknown key {key:?}; known keys: {}",
            KEYS.join(", ")
        )))
    }
}

/// Values git and scion both reject in an identity: angle brackets and line breaks.
fn check_value(key: &str, value: &str) -> Result<(), CliError> {
    if value.trim().is_empty() || value.contains(['<', '>', '\n', '\r']) {
        return Err(CliError::Usage(format!(
            "invalid {key} {value:?}: must be non-empty without <, > or line breaks"
        )));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct ConfigOut {
    pub schema: &'static str,
    pub path: String,
    /// The keys shown (all set keys for `list`, the one key for `get`/`set`/`unset`).
    pub values: BTreeMap<String, Option<String>>,
}

impl Render for ConfigOut {
    fn text(&self) -> String {
        if self.values.len() == 1 {
            return self.values.values().next().cloned().flatten().unwrap_or_default();
        }
        self.values
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_deref().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn out(path: &Path, values: BTreeMap<String, Option<String>>) -> ConfigOut {
    ConfigOut {
        schema: "scion.cli.config/v1",
        path: path.display().to_string(),
        values,
    }
}

pub fn list(path: &Path) -> Result<ConfigOut, CliError> {
    let map = load(path)?;
    Ok(out(path, map.into_iter().map(|(k, v)| (k, Some(v))).collect()))
}

pub fn get(path: &Path, key: &str) -> Result<ConfigOut, CliError> {
    check_key(key)?;
    let v = load(path)?.remove(key);
    Ok(out(path, BTreeMap::from([(key.to_owned(), v)])))
}

pub fn set(path: &Path, key: &str, value: &str) -> Result<ConfigOut, CliError> {
    check_key(key)?;
    check_value(key, value)?;
    let mut map = load(path)?;
    map.insert(key.to_owned(), value.trim().to_owned());
    save(path, &map)?;
    Ok(out(
        path,
        BTreeMap::from([(key.to_owned(), Some(value.trim().to_owned()))]),
    ))
}

pub fn unset(path: &Path, key: &str) -> Result<ConfigOut, CliError> {
    check_key(key)?;
    let mut map = load(path)?;
    let old = map.remove(key);
    if old.is_some() {
        save(path, &map)?;
    }
    Ok(out(path, BTreeMap::from([(key.to_owned(), None)])))
}

/// `git config --get <key>`, or None when git or the key is missing.
pub fn git_config(key: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["config", "--get", key])
        .output()
        .ok()?;
    let v = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !v.is_empty()).then_some(v)
}

/// The default commit author: each of name and email from scion's settings, else from git's.
/// None when either is missing from both.
pub fn author(settings: &BTreeMap<String, String>, git: impl Fn(&str) -> Option<String>) -> Option<(String, String)> {
    let pick = |k: &str| {
        settings
            .get(k)
            .cloned()
            .or_else(|| git(k))
            .filter(|v| check_value(k, v).is_ok())
    };
    Some((pick("user.name")?, pick("user.email")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn path_precedence() {
        assert_eq!(
            path(env(&[("SCION_CONFIG_FILE", "/x/c.json")])),
            PathBuf::from("/x/c.json")
        );
        assert_eq!(
            path(env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/h")])),
            PathBuf::from("/xdg/scion/config.json")
        );
        assert_eq!(
            path(env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/h")])),
            PathBuf::from("/h/.config/scion/config.json")
        );
        assert_eq!(path(env(&[])), PathBuf::from("./.config/scion/config.json"));
    }

    #[test]
    fn set_get_list_unset() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("nested/config.json");
        assert_eq!(get(&p, "user.name").expect("get").text(), "");
        assert!(list(&p).expect("list").values.is_empty());
        set(&p, "user.name", " Ada Lovelace ").expect("set");
        set(&p, "user.email", "ada@example.com").expect("set");
        assert_eq!(get(&p, "user.name").expect("get").text(), "Ada Lovelace");
        assert_eq!(
            list(&p).expect("list").text(),
            "user.email=ada@example.com\nuser.name=Ada Lovelace"
        );
        unset(&p, "user.email").expect("unset");
        unset(&p, "user.email").expect("unset again is fine");
        assert_eq!(list(&p).expect("list").values.len(), 1);
        for (k, v) in [
            ("user.nme", "x"),
            ("user.name", "A <b>"),
            ("user.email", " "),
            ("user.name", "a\nb"),
        ] {
            assert!(matches!(set(&p, k, v), Err(CliError::Usage(_))), "{k}={v:?}");
        }
        assert!(matches!(get(&p, "core.editor"), Err(CliError::Usage(_))));
        assert!(matches!(unset(&p, "nope"), Err(CliError::Usage(_))));
        std::fs::write(&p, "not json").expect("corrupt");
        assert!(matches!(list(&p), Err(CliError::Usage(_))));
        assert!(matches!(load(d.path()), Err(CliError::Usage(_))), "a directory");
    }

    #[test]
    fn author_falls_back_to_git_per_field() {
        let mine = BTreeMap::from([("user.name".to_owned(), "Ada".to_owned())]);
        let git = |k: &str| (k == "user.email").then(|| "ada@git".to_owned());
        assert_eq!(author(&mine, git), Some(("Ada".into(), "ada@git".into())));
        assert_eq!(author(&BTreeMap::new(), git), None, "no name anywhere");
        let bad_git = |k: &str| Some(if k == "user.name" { "<x>".into() } else { "e@x".into() });
        assert_eq!(author(&BTreeMap::new(), bad_git), None, "invalid values are ignored");
        assert_eq!(author(&mine, |_| None), None);
    }
}
