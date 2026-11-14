//! Server entries: `[[mcp_servers]]` in a `config.toml`, and `.mcp.json`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

/// A configured value that may hold a credential (an env value, a header value). Its
/// `Debug` never shows it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value as configured, before `${VAR}` expansion.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Which file an entry came from. User-level entries are trusted; project ones start
/// only once the user trusts them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    /// `~/.ch/config.toml`, a profile or `--set`.
    User,
    /// The project's `.ch/config.toml`.
    ProjectConfig,
    /// The project's `.mcp.json`.
    McpJson,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    pub kind: OriginKind,
    /// The file, for the trust record and messages. Empty for user-level entries.
    pub file: PathBuf,
}

impl Origin {
    pub fn user() -> Self {
        Self {
            kind: OriginKind::User,
            file: PathBuf::new(),
        }
    }

    pub fn is_project(&self) -> bool {
        self.kind != OriginKind::User
    }

    /// For messages: `~/.ch/config.toml`, `.mcp.json`, ...
    pub fn describe(&self, workspace: &Path) -> String {
        match self.kind {
            OriginKind::User => "your config".into(),
            _ => self
                .file
                .strip_prefix(workspace)
                .unwrap_or(&self.file)
                .display()
                .to_string(),
        }
    }
}

/// Where a stdio server runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cwd {
    /// The user's home directory: the default for user-level servers, so a repo's
    /// `.npmrc` or `uv.toml` can't steer a trusted `npx` or `uvx` command.
    Home,
    /// The workspace: always for project servers, and for user-level ones that set
    /// `cwd = "workspace"`.
    Workspace,
}

#[derive(Clone, PartialEq, Serialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, Secret>,
        cwd: Cwd,
    },
    Http {
        url: String,
        headers: BTreeMap<String, Secret>,
    },
}

/// Never shows env or header values, URL credentials or secret query parameters.
impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Transport::Stdio {
                command,
                args,
                env,
                cwd,
            } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", env)
                .field("cwd", cwd)
                .finish(),
            Transport::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", &crate::redact::url_without_credentials(url))
                .field("headers", headers)
                .finish(),
        }
    }
}

/// One MCP server entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerConfig {
    /// `[a-z0-9-]{1,24}`; its tools are named `mcp__<name>__<tool>`.
    pub name: String,
    #[serde(skip)]
    pub origin: Origin,
    #[serde(flatten)]
    pub transport: Transport,
    /// Tools the user says are read-only, by exact name (`mcp__github__list_issues`, or
    /// the server's own `list_issues`). The server's own hints are ignored.
    pub read_only_tools: Vec<String>,
    pub startup_timeout_secs: Option<u64>,
    pub call_timeout_secs: Option<u64>,
}

impl ServerConfig {
    /// The kernel's source id: `mcp:<name>`.
    pub fn source_id(&self) -> String {
        format!("mcp:{}", self.name)
    }

    /// `mcp__<name>__`.
    pub fn prefix(&self) -> String {
        format!("mcp__{}__", self.name)
    }

    /// `${VAR}` names this entry reads from ch's environment, sorted.
    pub fn variables(&self) -> Vec<String> {
        let values: Vec<&str> = match &self.transport {
            Transport::Stdio { env, .. } => env.values().map(Secret::expose).collect(),
            Transport::Http { headers, .. } => headers.values().map(Secret::expose).collect(),
        };
        let mut vars: Vec<String> = values
            .into_iter()
            .flat_map(crate::launch::variables_in)
            .collect();
        vars.sort();
        vars.dedup();
        vars
    }
}

/// Whether `name` is a valid server name.
pub fn valid_name(name: &str) -> bool {
    (1..=24).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// An entry as written in TOML or `.mcp.json`. No `Debug`: it holds raw values.
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawEntry {
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
    read_only_tools: Vec<String>,
    startup_timeout_secs: Option<u64>,
    call_timeout_secs: Option<u64>,
}

/// The TOML fields; unknown ones are an error, so a typo doesn't silently drop a
/// setting.
const TOML_FIELDS: &[&str] = &[
    "name",
    "type",
    "command",
    "args",
    "env",
    "cwd",
    "url",
    "headers",
    "read_only_tools",
    "startup_timeout_secs",
    "call_timeout_secs",
];

impl RawEntry {
    fn into_config(self, name: String, origin: &Origin) -> anyhow::Result<ServerConfig> {
        if !valid_name(&name) {
            bail!(
                "the MCP server name `{}` must be 1 to 24 lowercase letters, digits or `-`",
                ch_kernel::util::printable(&name)
            );
        }
        let kind = self.kind.as_deref().map(str::to_ascii_lowercase);
        let transport = match (kind.as_deref(), self.command, self.url) {
            (Some("sse"), ..) => bail!(
                "MCP server `{name}`: the old SSE transport isn't supported; use the server's \
                 streamable HTTP endpoint (`type = \"http\"`)"
            ),
            (None | Some("stdio"), Some(command), None) => {
                if command.trim().is_empty() {
                    bail!("MCP server `{name}`: `command` is empty");
                }
                if !self.headers.is_empty() {
                    bail!("MCP server `{name}`: `headers` only applies to HTTP servers");
                }
                let cwd = match (self.cwd.as_deref(), origin.is_project()) {
                    (None, false) => Cwd::Home,
                    (None | Some("workspace"), _) => Cwd::Workspace,
                    (Some(other), _) => bail!(
                        "MCP server `{name}`: `cwd` may only be \"workspace\", not `{}`",
                        ch_kernel::util::printable(other)
                    ),
                };
                Transport::Stdio {
                    command,
                    args: self.args,
                    env: secrets(self.env),
                    cwd,
                }
            }
            (None | Some("http" | "streamable-http" | "streamable_http"), None, Some(url)) => {
                let parsed = url::Url::parse(&url)
                    .with_context(|| format!("MCP server `{name}`: invalid `url`"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("MCP server `{name}`: `url` must be http or https");
                }
                if !self.args.is_empty() || !self.env.is_empty() || self.cwd.is_some() {
                    bail!(
                        "MCP server `{name}`: `args`, `env` and `cwd` only apply to stdio servers"
                    );
                }
                Transport::Http {
                    url,
                    headers: secrets(self.headers),
                }
            }
            (_, Some(_), Some(_)) => {
                bail!("MCP server `{name}`: set either `command` or `url`, not both")
            }
            (Some(other), ..) if !matches!(other, "stdio" | "http") => bail!(
                "MCP server `{name}`: unknown `type` `{}` (expected stdio or http)",
                ch_kernel::util::printable(other)
            ),
            _ => bail!("MCP server `{name}`: set `command` (stdio) or `url` (HTTP)"),
        };
        Ok(ServerConfig {
            name,
            origin: origin.clone(),
            transport,
            read_only_tools: self.read_only_tools,
            startup_timeout_secs: self.startup_timeout_secs,
            call_timeout_secs: self.call_timeout_secs,
        })
    }
}

fn secrets(map: BTreeMap<String, String>) -> BTreeMap<String, Secret> {
    map.into_iter().map(|(k, v)| (k, Secret(v))).collect()
}

/// Parses the `mcp_servers` list of a `config.toml`. Each entry parses on its own: the
/// result has one item per entry, in order.
pub fn parse_toml(value: &toml::Value, origin: &Origin) -> Vec<anyhow::Result<ServerConfig>> {
    let Some(list) = value.as_array() else {
        return vec![Err(anyhow::anyhow!(
            "`mcp_servers` must be a list of tables ([[mcp_servers]])"
        ))];
    };
    list.iter()
        .enumerate()
        .map(|(i, entry)| {
            let table = entry
                .as_table()
                .with_context(|| format!("mcp_servers[{i}] must be a table"))?;
            if let Some(key) = table.keys().find(|k| !TOML_FIELDS.contains(&k.as_str())) {
                bail!(
                    "mcp_servers[{i}]: unknown key `{}`",
                    ch_kernel::util::printable(key)
                );
            }
            let raw: RawEntry = entry
                .clone()
                .try_into()
                .with_context(|| format!("mcp_servers[{i}]"))?;
            let name = raw
                .name
                .clone()
                .with_context(|| format!("mcp_servers[{i}] has no `name`"))?;
            raw.into_config(name, origin)
        })
        .collect()
}

/// Parses a `.mcp.json` (`{"mcpServers": {"<name>": {...}}}`), sorted by name (JSON
/// objects have no order ch keeps). Fields other tools add are ignored; `cwd` is not read (project servers always run in
/// the workspace).
pub fn parse_mcp_json(
    text: &str,
    origin: &Origin,
) -> anyhow::Result<Vec<anyhow::Result<ServerConfig>>> {
    let value: serde_json::Value = serde_json::from_str(text).context("parsing .mcp.json")?;
    let Some(servers) = value.get("mcpServers") else {
        return Ok(Vec::new());
    };
    let Some(servers) = servers.as_object() else {
        bail!("`mcpServers` in .mcp.json must be an object");
    };
    Ok(servers
        .iter()
        .map(|(name, entry)| {
            let mut raw: RawEntry = serde_json::from_value(entry.clone()).with_context(|| {
                format!(
                    "MCP server `{}` in .mcp.json",
                    ch_kernel::util::printable(name)
                )
            })?;
            raw.cwd = None;
            raw.into_config(name.clone(), origin)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Origin {
        Origin {
            kind: OriginKind::McpJson,
            file: PathBuf::from("/w/.mcp.json"),
        }
    }

    fn toml_one(text: &str, origin: &Origin) -> anyhow::Result<ServerConfig> {
        let table: toml::Table = text.parse().unwrap();
        parse_toml(&table["mcp_servers"], origin).pop().unwrap()
    }

    #[test]
    fn stdio_and_http_entries() {
        let s = toml_one(
            r#"
            [[mcp_servers]]
            name = "github"
            command = "github-mcp"
            args = ["--stdio"]
            env = { TOKEN = "${GITHUB_TOKEN}" }
            read_only_tools = ["list_issues"]
            "#,
            &Origin::user(),
        )
        .unwrap();
        assert_eq!(s.prefix(), "mcp__github__");
        assert_eq!(s.variables(), ["GITHUB_TOKEN"]);
        let Transport::Stdio { cwd, env, .. } = &s.transport else {
            panic!("{s:?}");
        };
        assert_eq!(*cwd, Cwd::Home);
        assert_eq!(format!("{:?}", env["TOKEN"]), "[redacted]");
        assert!(!format!("{s:?}").contains("GITHUB_TOKEN}"), "{s:?}");
        let http = toml_one(
            "[[mcp_servers]]\nname = \"d\"\nurl = \"https://al:pw12345@x.example/m?token=abc\"",
            &Origin::user(),
        )
        .unwrap();
        let shown = format!("{http:?}");
        assert!(
            !shown.contains("pw12345") && !shown.contains("abc"),
            "{shown}"
        );

        let s = toml_one(
            "[[mcp_servers]]\nname = \"docs\"\nurl = \"https://x.example/mcp\"\nheaders = { Authorization = \"Bearer ${T}\" }",
            &Origin::user(),
        )
        .unwrap();
        assert!(matches!(s.transport, Transport::Http { .. }));
    }

    #[test]
    fn bad_entries_are_refused() {
        for (text, needle) in [
            ("name = \"GitHub\"\ncommand = \"x\"", "lowercase"),
            ("name = \"a\"", "set `command`"),
            (
                "name = \"a\"\ncommand = \"x\"\nurl = \"http://x\"",
                "not both",
            ),
            ("name = \"a\"\nurl = \"ftp://x\"", "http or https"),
            ("name = \"a\"\ncommand = \"x\"\ncwd = \"/tmp\"", "workspace"),
            (
                "name = \"a\"\ncommand = \"x\"\ncomand = \"y\"",
                "unknown key",
            ),
            (
                "name = \"a\"\nurl = \"http://x\"\nenv = { A = \"b\" }",
                "stdio",
            ),
            ("name = \"a\"\ntype = \"sse\"\nurl = \"http://x\"", "SSE"),
        ] {
            let err = toml_one(&format!("[[mcp_servers]]\n{text}"), &Origin::user())
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn mcp_json_entries_run_in_the_workspace() {
        let list = parse_mcp_json(
            r#"{"mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "fs-server"], "cwd": "/etc", "extra": 1},
                "Bad_Name": {"command": "x"},
                "web": {"type": "http", "url": "https://x.example/mcp"}
            }}"#,
            &project(),
        )
        .unwrap();
        assert_eq!(list.len(), 3);
        // Sorted by name: `Bad_Name`, `fs`, `web`.
        assert!(list[0].is_err());
        let fs = list[1].as_ref().unwrap();
        assert!(matches!(
            fs.transport,
            Transport::Stdio {
                cwd: Cwd::Workspace,
                ..
            }
        ));
        assert!(list[2].is_ok());
        assert!(parse_mcp_json("{}", &project()).unwrap().is_empty());
    }
}
