//! Saving an "always allow" rule to the project config.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use toml::{Table, Value};

/// Where "always" rules are saved, relative to the workspace.
pub const PROJECT_CONFIG: &str = ".ch/config.toml";

pub fn project_config_path(workspace: &Path) -> PathBuf {
    workspace.join(PROJECT_CONFIG)
}

/// Adds `rule` to `plugins.safety.allow` in the TOML document `text`. Returns the new
/// document, or `None` when the rule is already there.
///
/// Comments and formatting in the file are not preserved.
pub fn add_allow_rule(text: &str, rule: &str) -> anyhow::Result<Option<String>> {
    let mut root: Table = text
        .parse()
        .context("the config file is not valid TOML; fix it or add the rule by hand")?;
    let plugins = child_table(&mut root, "plugins")?;
    let safety = child_table(plugins, "safety")?;
    let allow = safety
        .entry("allow")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(list) = allow else {
        bail!("`plugins.safety.allow` is not a list; fix the config file or add the rule by hand");
    };
    if list.iter().any(|v| v.as_str() == Some(rule)) {
        return Ok(None);
    }
    list.push(Value::String(rule.to_string()));
    Ok(Some(toml::to_string(&root)?))
}

fn child_table<'a>(table: &'a mut Table, key: &str) -> anyhow::Result<&'a mut Table> {
    match table
        .entry(key)
        .or_insert_with(|| Value::Table(Table::new()))
    {
        Value::Table(t) => Ok(t),
        _ => bail!("`{key}` in the config file is not a table; fix it or add the rule by hand"),
    }
}

/// Adds `rule` to the allow list in the config file at `path`, creating the file (and
/// its directory) if needed. Returns `false` if the rule was already there.
pub fn save_allow_rule(path: &Path, rule: &str) -> anyhow::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let Some(updated) =
        add_allow_rule(&text, rule).with_context(|| format!("updating {}", path.display()))?
    else {
        return Ok(false);
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_rule_to_empty_and_existing_config() {
        let out = add_allow_rule("", "bash:git status").unwrap().unwrap();
        let t: Table = out.parse().unwrap();
        assert_eq!(
            t["plugins"]["safety"]["allow"].as_array().unwrap()[0].as_str(),
            Some("bash:git status")
        );

        let existing =
            "[model]\nid = \"m\"\n\n[plugins.safety]\nmode = \"ask\"\nallow = [\"read_file\"]\n";
        let out = add_allow_rule(existing, "edit_file").unwrap().unwrap();
        let t: Table = out.parse().unwrap();
        assert_eq!(t["model"]["id"].as_str(), Some("m"));
        assert_eq!(t["plugins"]["safety"]["mode"].as_str(), Some("ask"));
        let allow: Vec<&str> = t["plugins"]["safety"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(allow, vec!["read_file", "edit_file"]);

        assert!(add_allow_rule(existing, "read_file").unwrap().is_none());
    }

    #[test]
    fn rejects_bad_shapes() {
        assert!(add_allow_rule("plugins = 3", "x").is_err());
        assert!(add_allow_rule("[plugins.safety]\nallow = \"x\"", "x").is_err());
        assert!(add_allow_rule("not toml [", "x").is_err());
    }

    #[test]
    fn saves_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".ch/config.toml");
        assert!(save_allow_rule(&path, "bash:ls *").unwrap());
        assert!(!save_allow_rule(&path, "bash:ls *").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("bash:ls *"));
    }
}
