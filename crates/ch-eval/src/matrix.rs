//! The matrix: `--vary` dimensions, the cells they make, and the order of runs.
//!
//! - `--vary KEY=V1,V2` adds a dimension. Each value is a TOML literal passed to `ch` as
//!   `--set KEY=V`, so a value that isn't valid TOML is a string, as with `ch --set`.
//!   Values are split at commas outside quotes and brackets.
//! - A key without a dot names a plugin, and its values are `on` and `off`. `off` adds
//!   the plugin to `plugins.disable`; `on` adds it to `plugins.enable` (and takes it out
//!   of a `plugins.disable` from the base config), so it also turns on plugins that are
//!   off by default. The lists start from the base config's, since `--set` replaces a
//!   list, and are merged across dimensions.
//! - A key with a dot must start with a section `ch` reads: `model`, `agent`,
//!   `plugins` or `select`. A misspelled key would otherwise vary nothing.
//! - Cells are the cartesian product of the dimensions, the first varying slowest. With
//!   no dimension there is one cell, `base`. Names must differ in more than case, since
//!   they name directories on case-insensitive file systems too.

use anyhow::{Context as _, bail};
use toml::Value;

/// The cell's name when there are no dimensions.
pub const BASE_CELL: &str = "base";

/// The config sections a `--vary` key may start with.
const SECTIONS: &[&str] = &["model", "agent", "plugins", "select"];

/// One `--vary` dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct Dimension {
    /// The config key, or a plugin id.
    pub key: String,
    /// The values as written.
    pub values: Vec<String>,
}

impl Dimension {
    /// A key without a dot names a plugin, switched `on` or `off`.
    pub fn is_plugin(&self) -> bool {
        !self.key.contains('.')
    }
}

/// Parses `KEY=V1,V2,...`.
pub fn parse_vary(arg: &str) -> anyhow::Result<Dimension> {
    let Some((key, raw)) = arg.split_once('=') else {
        bail!("--vary {arg}: expected KEY=V1,V2, e.g. planning=on,off");
    };
    let key = key.trim();
    if key.is_empty() || key.split('.').any(|p| p.trim().is_empty()) {
        bail!("--vary {arg}: `{key}` is not a config key");
    }
    let values = split_values(raw).with_context(|| format!("--vary {arg}"))?;
    if values.is_empty() {
        bail!("--vary {arg}: no values");
    }
    for (i, v) in values.iter().enumerate() {
        if v.is_empty() {
            bail!("--vary {arg}: empty value");
        }
        if values[..i].contains(v) {
            bail!("--vary {arg}: `{v}` is given twice");
        }
    }
    let dim = Dimension {
        key: key.to_string(),
        values,
    };
    if !dim.is_plugin() {
        let section = dim.key.split('.').next().unwrap_or_default();
        if !SECTIONS.contains(&section) {
            bail!(
                "--vary {arg}: `{}` isn't a setting ch reads; keys start with {}",
                dim.key,
                SECTIONS.join(", ")
            );
        }
    }
    if dim.is_plugin() {
        let plugin_id = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if !dim.key.chars().all(plugin_id) {
            bail!("--vary {arg}: `{}` is not a plugin id", dim.key);
        }
        if let Some(v) = dim.values.iter().find(|v| *v != "on" && *v != "off") {
            bail!(
                "--vary {arg}: `{}` names a plugin, so its values are `on` and `off`, not `{v}`",
                dim.key
            );
        }
    }
    Ok(dim)
}

/// Splits at commas that are outside quotes and brackets, trimming each value.
fn split_values(raw: &str) -> anyhow::Result<Vec<String>> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    let mut escaped = false;
    for c in raw.chars() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    values.push(current.trim().to_string());
                    current.clear();
                    continue;
                }
                _ => {}
            },
        }
        current.push(c);
    }
    if quote.is_some() {
        bail!("unclosed quote");
    }
    values.push(current.trim().to_string());
    Ok(values)
}

/// `plugins.enable` and `plugins.disable` from the base config, after `--profile`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginLists {
    pub enable: Vec<String>,
    pub disable: Vec<String>,
}

impl PluginLists {
    /// The lists in `config`, with `[profiles.<profile>]` applied over them.
    pub fn from_config(config: &toml::Table, profile: Option<&str>) -> anyhow::Result<Self> {
        let profile_table = match profile {
            Some(name) => Some(
                config
                    .get("profiles")
                    .and_then(|p| p.get(name))
                    .and_then(Value::as_table)
                    .with_context(|| format!("the base config has no [profiles.{name}]"))?,
            ),
            None => None,
        };
        let list = |key: &str| -> anyhow::Result<Vec<String>> {
            let pick = |t: &toml::Table| t.get("plugins").and_then(|p| p.get(key)).cloned();
            let value = profile_table.and_then(pick).or_else(|| pick(config));
            match value {
                None => Ok(Vec::new()),
                Some(v) => v
                    .try_into::<Vec<String>>()
                    .with_context(|| format!("`plugins.{key}` must be a list of plugin ids")),
            }
        };
        Ok(Self {
            enable: list("enable")?,
            disable: list("disable")?,
        })
    }
}

/// One combination of dimension values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cell {
    /// Its settings, such as `planning=off,context.strategy=none`, made safe for a
    /// directory name.
    pub name: String,
    /// `KEY=VALUE` assignments, each passed to `ch` as `--set`.
    pub settings: Vec<String>,
    /// Plugins switched `on` (true) or `off` (false).
    pub switches: Vec<(String, bool)>,
    /// Plugins with a `plugins.<id>.*` setting in this cell.
    pub configured: Vec<String>,
}

impl Cell {
    /// Checks a run's enabled plugins (from `session_start`) against the cell: what it
    /// switched on is on, what it switched off is off, and what it configured is on.
    /// Otherwise the run didn't measure the cell.
    pub fn check_plugins(&self, enabled: &[String]) -> Result<(), String> {
        let on = |id: &str| enabled.iter().any(|p| p == id);
        for (id, want) in &self.switches {
            if on(id) != *want {
                let (want, got) = if *want { ("on", "off") } else { ("off", "on") };
                return Err(format!(
                    "cell {} switches plugin `{id}` {want}, but the run had it {got}",
                    self.name
                ));
            }
        }
        if let Some(id) = self.configured.iter().find(|id| !on(id)) {
            return Err(format!(
                "cell {} sets plugins.{id}.*, but plugin `{id}` wasn't enabled in the run, so \
                 the setting did nothing",
                self.name
            ));
        }
        Ok(())
    }
}

/// The cells for `dims`, in order. `base` holds the plugin lists the cells start from.
pub fn cells(dims: &[Dimension], base: &PluginLists) -> anyhow::Result<Vec<Cell>> {
    for (i, d) in dims.iter().enumerate() {
        if dims[..i].iter().any(|e| e.key == d.key) {
            bail!("--vary {} is given twice", d.key);
        }
    }
    let lists = ["plugins.enable", "plugins.disable"];
    if dims.iter().any(Dimension::is_plugin) && dims.iter().any(|d| lists.contains(&&*d.key)) {
        bail!(
            "--vary with a plugin name changes plugins.enable and plugins.disable, so it \
             can't be combined with --vary on those lists"
        );
    }
    if dims.is_empty() {
        return Ok(vec![Cell {
            name: BASE_CELL.into(),
            ..Default::default()
        }]);
    }

    let mut out: Vec<Cell> = Vec::new();
    let count: usize = dims.iter().map(|d| d.values.len()).product();
    for index in 0..count {
        // Mixed radix, the last dimension varying fastest.
        let mut rest = index;
        let mut picks = vec![0; dims.len()];
        for (i, d) in dims.iter().enumerate().rev() {
            picks[i] = rest % d.values.len();
            rest /= d.values.len();
        }
        let chosen: Vec<(&Dimension, &str)> = dims
            .iter()
            .zip(&picks)
            .map(|(d, &p)| (d, d.values[p].as_str()))
            .collect();
        let cell = make_cell(&chosen, base);
        let same = |c: &&Cell| c.name.eq_ignore_ascii_case(&cell.name);
        if let Some(other) = out.iter().find(same) {
            bail!(
                "two cells get the directory name `{}` ({:?} and {:?}); use values that \
                 differ in more than case or punctuation",
                cell.name,
                other.settings,
                cell.settings
            );
        }
        out.push(cell);
    }
    Ok(out)
}

fn make_cell(chosen: &[(&Dimension, &str)], base: &PluginLists) -> Cell {
    let mut settings = Vec::new();
    let mut switches = Vec::new();
    let mut configured = Vec::new();
    let mut lists = base.clone();
    let mut has_plugins = false;
    for (dim, value) in chosen {
        if let Some(id) = dim
            .key
            .strip_prefix("plugins.")
            .and_then(|r| r.split_once('.'))
        {
            configured.push(id.0.to_string());
        }
        if dim.is_plugin() {
            has_plugins = true;
            let id = dim.key.clone();
            switches.push((id.clone(), *value == "on"));
            if *value == "on" {
                lists.disable.retain(|p| *p != id);
                if !lists.enable.contains(&id) {
                    lists.enable.push(id);
                }
            } else if !lists.disable.contains(&id) {
                lists.disable.push(id);
            }
        } else {
            settings.push(format!("{}={value}", dim.key));
        }
    }
    if has_plugins {
        if lists.enable != base.enable {
            settings.push(format!("plugins.enable={}", toml_list(&lists.enable)));
        }
        if lists.disable != base.disable {
            settings.push(format!("plugins.disable={}", toml_list(&lists.disable)));
        }
    }
    let label: Vec<String> = chosen
        .iter()
        .map(|(dim, value)| {
            let key = dim.key.strip_prefix("plugins.").unwrap_or(&dim.key);
            format!("{key}={}", short_value(value))
        })
        .collect();
    configured.retain(|id| !switches.iter().any(|(s, on)| s == id && !on));
    configured.sort();
    configured.dedup();
    Cell {
        name: safe_name(&label.join(",")),
        settings,
        switches,
        configured,
    }
}

/// `["a", "b"]`.
fn toml_list(items: &[String]) -> String {
    Value::Array(items.iter().cloned().map(Value::String).collect()).to_string()
}

/// A string value without its quotes; anything else as written.
fn short_value(raw: &str) -> String {
    match format!("v = {raw}").parse::<toml::Table>() {
        Ok(t) => match t.get("v") {
            Some(Value::String(s)) => s.clone(),
            _ => raw.to_string(),
        },
        Err(_) => raw.to_string(),
    }
}

/// Keeps letters, digits and `._-=,+`; anything else becomes `_`.
fn safe_name(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '=' | ',' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// One planned run, by index into the task and cell lists. Repeats count from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Planned {
    pub task: usize,
    pub cell: usize,
    pub repeat: u32,
}

/// Every run, interleaved: each repeat goes through the tasks, and each task through
/// the cells, so the cells of a task run close together in time and a slow hour of the
/// API doesn't fall on one cell. The cells start one further along for each task and
/// repeat, so the same cell isn't always first, warming the prompt cache for the rest.
pub fn plan(tasks: usize, cells: usize, repeats: u32) -> Vec<Planned> {
    let mut out = Vec::with_capacity(tasks * cells * repeats as usize);
    for repeat in 1..=repeats {
        for task in 0..tasks {
            for k in 0..cells {
                let cell = (k + task + repeat as usize - 1) % cells;
                out.push(Planned { task, cell, repeat });
            }
        }
    }
    out
}

/// The `ch` arguments for one run, after the program name: the instruction comes on
/// stdin, events go to stdout, and every tool call is allowed but for the built-in deny
/// list (the container is the boundary). The cell's settings follow as `--set` flags,
/// then the task's step limit.
pub fn ch_args(cell: &Cell, max_steps: Option<u32>, profile: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "-",
        "--output-format",
        "stream-json",
        "--permission-mode",
        "bypass",
    ]
    .map(String::from)
    .to_vec();
    if let Some(p) = profile {
        args.extend(["--profile".into(), p.into()]);
    }
    for s in &cell.settings {
        args.extend(["--set".into(), s.clone()]);
    }
    if let Some(n) = max_steps {
        args.extend(["--set".into(), format!("agent.max_steps={n}")]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(args: &[&str]) -> Vec<Dimension> {
        args.iter().map(|a| parse_vary(a).unwrap()).collect()
    }

    #[test]
    fn splits_values_outside_quotes_and_brackets() {
        let d = parse_vary(r#"plugins.context.strategy="none","staged""#).unwrap();
        assert_eq!(d.values, [r#""none""#, r#""staged""#]);
        let d = parse_vary(r#"agent.k=[1, 2],"a,b",'c,d',{x = 1, y = 2}, 3"#).unwrap();
        assert_eq!(
            d.values,
            ["[1, 2]", r#""a,b""#, "'c,d'", "{x = 1, y = 2}", "3"]
        );
        let d = parse_vary(r#"agent.k="say \"hi, there\"",2"#).unwrap();
        assert_eq!(d.values, [r#""say \"hi, there\"""#, "2"]);
    }

    #[test]
    fn rejects_bad_dimensions() {
        for arg in [
            "context.strategy=none",
            "agnet.max_steps=5",
            "planning",
            "=on,off",
            "a..b=1",
            "planning=on,maybe",
            "plan ning=on",
            "a.k=1,,2",
            "a.k=1,1",
            "a.k=\"open",
        ] {
            assert!(parse_vary(arg).is_err(), "{arg}");
        }
    }

    #[test]
    fn no_dimension_is_one_base_cell() {
        let cells = cells(&[], &PluginLists::default()).unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].name, "base");
        assert!(cells[0].settings.is_empty());
    }

    #[test]
    fn product_with_the_first_dimension_slowest() {
        let d = dims(&[
            "planning=on,off",
            r#"plugins.context.strategy="none","staged""#,
        ]);
        let cells = cells(&d, &PluginLists::default()).unwrap();
        let names: Vec<&str> = cells.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "planning=on,context.strategy=none",
                "planning=on,context.strategy=staged",
                "planning=off,context.strategy=none",
                "planning=off,context.strategy=staged",
            ]
        );
        assert_eq!(
            cells[2].settings,
            [
                r#"plugins.context.strategy="none""#,
                r#"plugins.disable=["planning"]"#,
            ]
        );
        assert_eq!(
            cells[0].settings,
            [
                r#"plugins.context.strategy="none""#,
                r#"plugins.enable=["planning"]"#
            ]
        );
    }

    #[test]
    fn plugin_switches_merge_with_the_base_lists() {
        let base = PluginLists {
            enable: vec!["provider-scripted".into()],
            disable: vec!["spill".into(), "planning".into()],
        };
        let d = dims(&["planning=on,off", "tools-fs=off", "stuck=off"]);
        let cells = cells(&d, &base).unwrap();
        assert_eq!(cells[0].name, "planning=on,tools-fs=off,stuck=off");
        assert_eq!(
            cells[0].settings,
            [
                r#"plugins.enable=["provider-scripted", "planning"]"#,
                r#"plugins.disable=["spill", "tools-fs", "stuck"]"#,
            ]
        );
        // `off` for a plugin the base already disables leaves enable alone.
        assert_eq!(
            cells[1].settings,
            [r#"plugins.disable=["spill", "planning", "tools-fs", "stuck"]"#]
        );
    }

    #[test]
    fn names_are_safe_and_distinct() {
        let d = dims(&[r#"agent.max_steps=10,20"#, r#"plugins.x.mode="a/b","c d""#]);
        let made = cells(&d, &PluginLists::default()).unwrap();
        assert_eq!(made[0].name, "agent.max_steps=10,x.mode=a_b");
        assert_eq!(made[1].name, "agent.max_steps=10,x.mode=c_d");
        // Two values that differ only in characters the name drops collide.
        let d = dims(&[r#"plugins.x.mode="a/b","a b""#]);
        let err = cells(&d, &PluginLists::default()).unwrap_err();
        assert!(err.to_string().contains("x.mode=a_b"), "{err}");
        // So do names that differ only in case.
        let d = dims(&[r#"plugins.x.mode="Fast","fast""#]);
        assert!(cells(&d, &PluginLists::default()).is_err());
    }

    #[test]
    fn conflicting_dimensions_are_errors() {
        let base = PluginLists::default();
        assert!(cells(&dims(&["agent.k=1", "agent.k=2"]), &base).is_err());
        assert!(cells(&dims(&["planning=off", r#"plugins.disable=["x"]"#]), &base).is_err());
    }

    #[test]
    fn reads_base_lists_through_a_profile() {
        let config: toml::Table = r#"
            [plugins]
            enable = ["provider-scripted"]
            disable = ["spill"]
            [profiles.lean.plugins]
            disable = ["spill", "planning"]
        "#
        .parse()
        .unwrap();
        let lists = PluginLists::from_config(&config, None).unwrap();
        assert_eq!(lists.disable, ["spill"]);
        let lists = PluginLists::from_config(&config, Some("lean")).unwrap();
        assert_eq!(lists.enable, ["provider-scripted"]);
        assert_eq!(lists.disable, ["spill", "planning"]);
        assert!(PluginLists::from_config(&config, Some("missing")).is_err());
        let bad: toml::Table = "plugins.disable = \"spill\"".parse().unwrap();
        assert!(PluginLists::from_config(&bad, None).is_err());
    }

    #[test]
    fn runs_are_interleaved_by_task_with_rotating_cells() {
        let p = plan(2, 3, 2);
        let triples: Vec<(usize, usize, u32)> =
            p.iter().map(|r| (r.task, r.cell, r.repeat)).collect();
        assert_eq!(
            triples,
            [
                (0, 0, 1),
                (0, 1, 1),
                (0, 2, 1),
                (1, 1, 1),
                (1, 2, 1),
                (1, 0, 1),
                (0, 1, 2),
                (0, 2, 2),
                (0, 0, 2),
                (1, 2, 2),
                (1, 0, 2),
                (1, 1, 2),
            ]
        );
    }

    #[test]
    fn checks_a_runs_plugins_against_the_cell() {
        let d = dims(&[
            "planning=on,off",
            r#"plugins.context.strategy="none""#,
            "stuck=off",
            "plugins.stuck.limit=3",
        ]);
        let made = cells(&d, &PluginLists::default()).unwrap();
        let (on, off) = (&made[0], &made[1]);
        assert_eq!(
            on.switches,
            [("planning".to_string(), true), ("stuck".to_string(), false)]
        );
        // Configuring a plugin the cell switches off doesn't require it.
        assert_eq!(on.configured, ["context"]);
        let enabled = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(on.check_plugins(&enabled(&["planning", "context"])).is_ok());
        assert!(off.check_plugins(&enabled(&["context"])).is_ok());
        let err = off
            .check_plugins(&enabled(&["planning", "context"]))
            .unwrap_err();
        assert!(err.contains("`planning` off"), "{err}");
        let err = on.check_plugins(&enabled(&["planning"])).unwrap_err();
        assert!(err.contains("plugins.context.*"), "{err}");
        assert!(
            on.check_plugins(&enabled(&["planning", "context", "stuck"]))
                .is_err()
        );
    }

    #[test]
    fn builds_the_ch_command_line() {
        let cell = Cell {
            name: "planning=off".into(),
            settings: vec![r#"plugins.disable=["planning"]"#.into()],
            ..Default::default()
        };
        assert_eq!(
            ch_args(&cell, Some(80), Some("lean")),
            [
                "-p",
                "-",
                "--output-format",
                "stream-json",
                "--permission-mode",
                "bypass",
                "--profile",
                "lean",
                "--set",
                r#"plugins.disable=["planning"]"#,
                "--set",
                "agent.max_steps=80",
            ]
        );
        assert_eq!(ch_args(&cell, None, None).len(), 8);
    }
}
