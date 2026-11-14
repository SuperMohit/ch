//! What a server entry runs: the expanded environment and headers, the resolved
//! program, the working directory, and the hash its trust record holds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{Cwd, Secret, ServerConfig, Transport};

/// The only variables a server inherits from ch's environment. Everything else,
/// including `ANTHROPIC_API_KEY` and other credentials ch holds, is left out.
pub const INHERITED_ENV: &[&str] = &["PATH", "HOME", "USER", "LANG", "TERM", "TMPDIR", "SHELL"];

/// Where things are, for resolving an entry.
#[derive(Debug, Clone)]
pub struct Places {
    pub workspace: PathBuf,
    pub home: PathBuf,
}

impl Places {
    /// `workspace`, and `$HOME` (the workspace without one). The CLI and the plugin both
    /// build it this way, so they hash an entry the same way.
    pub fn new(workspace: &Path) -> Self {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace.to_path_buf());
        Self {
            workspace: workspace.to_path_buf(),
            home,
        }
    }
}

/// `${VAR}` names in `text`, in order: exactly the variables [`expand`] would read,
/// found by running it, so the two can't disagree (`${${KEY}}` reads `KEY`).
pub fn variables_in(text: &str) -> Vec<String> {
    let read = std::cell::RefCell::new(Vec::new());
    let _ = expand(text, &|name| {
        read.borrow_mut().push(name.to_string());
        Some(String::new())
    });
    read.into_inner()
}

fn is_var_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Replaces each `${VAR}` in `text` with its value from `lookup`. A variable that isn't
/// set is an error, so a server doesn't start with an empty token. Other `$` text is
/// kept as written.
pub fn expand(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) if is_var_name(&after[..end]) => {
                let name = &after[..end];
                let value =
                    lookup(name).with_context(|| format!("the variable `{name}` isn't set"))?;
                out.push_str(&value);
                rest = &after[end + 1..];
            }
            _ => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// ch's own environment.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Expanded `(name, value)` pairs, and the values of the variables they read.
type Expanded = (Vec<(String, String)>, Vec<String>);

/// Expands every value, and returns the values of the variables read too: a server can
/// echo a token without the `Bearer ` around it, so each is redacted on its own.
fn expand_all(
    map: &BTreeMap<String, Secret>,
    lookup: &dyn Fn(&str) -> Option<String>,
    what: &str,
) -> anyhow::Result<Expanded> {
    let mut read = Vec::new();
    let expanded = map
        .iter()
        .map(|(k, v)| {
            for name in variables_in(v.expose()) {
                read.extend(lookup(&name));
            }
            let value = expand(v.expose(), lookup).with_context(|| format!("{what} `{k}`"))?;
            Ok((k.clone(), value))
        })
        .collect::<anyhow::Result<_>>()?;
    Ok((expanded, read))
}

/// How to spawn a stdio server.
#[derive(Clone)]
pub struct StdioLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The whole environment: nothing else is inherited.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// Values of the variables the entry read.
    pub read: Vec<String>,
}

/// How to reach an HTTP server.
#[derive(Clone)]
pub struct HttpLaunch {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Values of the variables the entry read.
    pub read: Vec<String>,
}

#[derive(Clone)]
pub enum Launch {
    Stdio(StdioLaunch),
    Http(HttpLaunch),
}

impl Launch {
    /// The configured values that are secrets once expanded: env and header values, and
    /// the variables they read. The redactor strips the ones long enough to recognise.
    pub fn secret_values(&self) -> Vec<String> {
        match self {
            Launch::Stdio(s) => s
                .env
                .iter()
                .filter(|(k, _)| !INHERITED_ENV.contains(&k.as_str()) && k != "CH_WORKSPACE")
                .map(|(_, v)| v.clone())
                .chain(s.read.iter().cloned())
                .collect(),
            Launch::Http(h) => (h.headers.iter().map(|(_, v)| v.clone()))
                .chain(h.read.iter().cloned())
                .collect(),
        }
    }
}

/// The working directory for `cfg`.
pub fn working_dir(cfg: &ServerConfig, places: &Places) -> PathBuf {
    match &cfg.transport {
        Transport::Stdio { cwd: Cwd::Home, .. } => places.home.clone(),
        _ => places.workspace.clone(),
    }
}

/// Finds `command`: a path with a `/` is taken relative to `cwd`; a bare name is
/// looked up in `path` (a `PATH` value). The result is absolute and canonical.
pub fn resolve_program(command: &str, cwd: &Path, path: Option<&str>) -> anyhow::Result<PathBuf> {
    let candidate = if command.contains('/') {
        let p = Path::new(command);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    } else {
        let dirs = path.unwrap_or_default();
        let found = std::env::split_paths(dirs)
            .filter(|d| !d.as_os_str().is_empty())
            .map(|d| if d.is_absolute() { d } else { cwd.join(d) })
            .map(|d| d.join(command))
            .find(|p| is_executable(p));
        match found {
            Some(p) => p,
            None => bail!("`{}` isn't on PATH", ch_kernel::util::printable(command)),
        }
    };
    if !is_executable(&candidate) {
        bail!(
            "`{}` isn't an executable file",
            ch_kernel::util::printable(&candidate.display().to_string())
        );
    }
    candidate
        .canonicalize()
        .with_context(|| format!("resolving {}", candidate.display()))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Resolves `cfg` against `places` and the variables `lookup` gives.
pub fn resolve(
    cfg: &ServerConfig,
    places: &Places,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Launch> {
    match &cfg.transport {
        Transport::Stdio {
            command, args, env, ..
        } => {
            let cwd = working_dir(cfg, places);
            let mut full: Vec<(String, String)> = INHERITED_ENV
                .iter()
                .filter_map(|k| lookup(k).map(|v| (k.to_string(), v)))
                .collect();
            full.push((
                "CH_WORKSPACE".into(),
                places.workspace.display().to_string(),
            ));
            let (expanded, read) = expand_all(env, lookup, "env")?;
            for (k, v) in expanded {
                full.retain(|(existing, _)| *existing != k);
                full.push((k, v));
            }
            // ch's own PATH finds the program, never the entry's `env.PATH`: that is
            // what the trust prompt showed and the trust hash covers.
            let program = resolve_program(command, &cwd, lookup("PATH").as_deref())?;
            Ok(Launch::Stdio(StdioLaunch {
                program,
                args: args.clone(),
                env: full,
                cwd,
                read,
            }))
        }
        Transport::Http { url, headers } => {
            let (headers, read) = expand_all(headers, lookup, "header")?;
            Ok(Launch::Http(HttpLaunch {
                url: url.clone(),
                headers,
                read,
            }))
        }
    }
}

/// SHA-256 of a file's content, as hex.
fn file_hash(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(hex(&Sha256::digest(&bytes)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash a trust record holds for `cfg`: the whole entry as written (env and header
/// values before expansion, so no secret is stored), the resolved program, and the
/// content of the program and of every file inside the workspace that an argument names,
/// whole or after `=` (`--require=./x.js`). A change to any of them asks again. Files
/// reached in other ways (imports, a directory argument) aren't covered.
pub fn trust_hash(cfg: &ServerConfig, places: &Places) -> String {
    let mut files = BTreeMap::new();
    let mut program = Value::Null;
    if let Transport::Stdio { command, args, .. } = &cfg.transport {
        let cwd = working_dir(cfg, places);
        let resolved = resolve_program(command, &cwd, process_env("PATH").as_deref()).ok();
        let root = places
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| places.workspace.clone());
        let mut inside = |p: &Path| {
            if let Ok(real) = p.canonicalize()
                && real.starts_with(&root)
                && real.is_file()
                && let Some(h) = file_hash(&real)
            {
                files.insert(real.display().to_string(), h);
            }
        };
        if let Some(p) = &resolved {
            inside(p);
            program = json!(p.display().to_string());
        }
        for arg in args {
            inside(&cwd.join(arg));
            if let Some((_, value)) = arg.split_once('=') {
                inside(&cwd.join(value));
            }
        }
    }
    let entry = serde_json::to_value(cfg).unwrap_or(Value::Null);
    let all = json!({ "entry": entry, "program": program, "files": files });
    hex(&Sha256::digest(all.to_string().as_bytes()))
}

/// What the user is asked to trust: one `(label, value)` per line, already safe to
/// print. Header values aren't shown, only their names and the variables they read. Env
/// values of a project entry are shown, except references to ch's variables
/// (`${TOKEN}`): `NODE_OPTIONS` or `PYTHONPATH` can change what runs. A user entry's
/// values, which may be literal secrets, show only as names.
pub fn describe(cfg: &ServerConfig, places: &Places) -> Vec<(String, String)> {
    use ch_kernel::util::printable;
    let mut out = vec![(
        "from".to_string(),
        printable(&cfg.origin.describe(&places.workspace)),
    )];
    match &cfg.transport {
        Transport::Stdio {
            command, args, env, ..
        } => {
            let cwd = working_dir(cfg, places);
            let shown = match resolve_program(command, &cwd, process_env("PATH").as_deref()) {
                Ok(p) => p.display().to_string(),
                Err(e) => format!("{command} ({e:#})"),
            };
            out.push(("runs".into(), printable(&shown)));
            if !args.is_empty() {
                let args = args
                    .iter()
                    .map(|a| format!("{a:?}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                out.push(("args".into(), printable(&args)));
            }
            out.push(("in".into(), printable(&cwd.display().to_string())));
            if !env.is_empty() {
                let shown: Vec<String> = env
                    .iter()
                    .map(|(k, v)| {
                        let v = v.expose();
                        let only_vars =
                            expand(v, &|_| Some(String::new())).is_ok_and(|rest| rest.is_empty());
                        if cfg.origin.is_project() && !only_vars {
                            format!("{k}={v:?}")
                        } else {
                            k.clone()
                        }
                    })
                    .collect();
                out.push(("env".into(), printable(&shown.join(", "))));
            }
        }
        Transport::Http { url, headers } => {
            out.push((
                "url".into(),
                printable(&crate::redact::url_without_credentials(url)),
            ));
            if !headers.is_empty() {
                let names: Vec<&str> = headers.keys().map(String::as_str).collect();
                out.push(("headers".into(), printable(&names.join(", "))));
            }
        }
    }
    let vars = cfg.variables();
    if !vars.is_empty() {
        out.push(("reads".into(), printable(&vars.join(", "))));
    }
    if !cfg.read_only_tools.is_empty() && !cfg.origin.is_project() {
        out.push((
            "read-only".into(),
            printable(&cfg.read_only_tools.join(", ")),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Origin, OriginKind};

    fn lookup(name: &str) -> Option<String> {
        match name {
            "TOKEN" => Some("s3cr3t-value".into()),
            "PATH" => Some("/usr/bin:/bin".into()),
            "HOME" => Some("/home/u".into()),
            "ANTHROPIC_API_KEY" => Some("sk-ant".into()),
            _ => None,
        }
    }

    fn stdio(command: &str, env: &[(&str, &str)], origin: Origin) -> ServerConfig {
        ServerConfig {
            name: "t".into(),
            origin: origin.clone(),
            transport: Transport::Stdio {
                command: command.into(),
                args: vec![],
                env: env
                    .iter()
                    .map(|(k, v)| (k.to_string(), Secret::new(*v)))
                    .collect(),
                cwd: if origin.is_project() {
                    Cwd::Workspace
                } else {
                    Cwd::Home
                },
            },
            read_only_tools: vec![],
            startup_timeout_secs: None,
            call_timeout_secs: None,
        }
    }

    #[test]
    fn expansion() {
        let f = |t: &str| expand(t, &lookup);
        assert_eq!(f("Bearer ${TOKEN}").unwrap(), "Bearer s3cr3t-value");
        assert_eq!(
            f("$TOKEN ${} ${1X} $${TOKEN}").unwrap(),
            "$TOKEN ${} ${1X} $s3cr3t-value"
        );
        assert!(f("${MISSING}").unwrap_err().to_string().contains("MISSING"));
        assert_eq!(variables_in("a ${A} ${B_1} ${} ${9}"), ["A", "B_1"]);
        // What `expand` reads, even nested.
        assert_eq!(
            variables_in("${${ANTHROPIC_API_KEY}}"),
            ["ANTHROPIC_API_KEY"]
        );
        assert_eq!(f("${${TOKEN}}").unwrap(), "${s3cr3t-value}");
    }

    #[test]
    fn the_environment_is_cleared() {
        let places = Places {
            workspace: PathBuf::from("/w"),
            home: PathBuf::from("/"),
        };
        let cfg = stdio("sh", &[("API", "${TOKEN}")], Origin::user());
        let Launch::Stdio(l) = resolve(&cfg, &places, &lookup).unwrap() else {
            panic!()
        };
        let names: Vec<&str> = l.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["PATH", "HOME", "CH_WORKSPACE", "API"]);
        assert!(l.program.is_absolute());
        assert_eq!(l.cwd, PathBuf::from("/"));
        assert!(
            Launch::Stdio(l)
                .secret_values()
                .iter()
                .all(|v| v == "s3cr3t-value")
        );
    }

    #[test]
    fn the_entry_cant_choose_the_program_through_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        std::fs::create_dir(ws.join("bin")).unwrap();
        let fake = ws.join("bin/sh");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let places = Places {
            workspace: ws.clone(),
            home: ws.clone(),
        };
        let origin = Origin {
            kind: OriginKind::McpJson,
            file: ws.join(".mcp.json"),
        };
        let cfg = stdio(
            "sh",
            &[
                ("PATH", "bin:/usr/bin:/bin"),
                ("NODE_OPTIONS", "--require ./x.js"),
            ],
            origin,
        );
        let Launch::Stdio(l) = resolve(&cfg, &places, &lookup).unwrap() else {
            panic!()
        };
        assert!(!l.program.starts_with(&ws), "{}", l.program.display());
        let shown = format!("{:?}", describe(&cfg, &places));
        assert!(
            shown.contains(r#"NODE_OPTIONS=\"--require ./x.js\""#),
            "{shown}"
        );
        assert!(shown.contains(r#"PATH=\"bin:/usr/bin:/bin\""#), "{shown}");
    }

    #[test]
    fn trust_hash_follows_the_entry_and_workspace_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let script = ws.join("server.sh");
        std::fs::write(&script, "#!/bin/sh\necho a\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let places = Places {
            workspace: ws.clone(),
            home: ws.clone(),
        };
        let origin = Origin {
            kind: OriginKind::McpJson,
            file: ws.join(".mcp.json"),
        };
        let cfg = stdio("./server.sh", &[("A", "${TOKEN}")], origin.clone());
        let h1 = trust_hash(&cfg, &places);
        assert_eq!(h1, trust_hash(&cfg, &places));
        std::fs::write(&script, "#!/bin/sh\necho b\n").unwrap();
        let h2 = trust_hash(&cfg, &places);
        assert_ne!(h1, h2, "the script's content counts");
        let other = stdio("./server.sh", &[("A", "${OTHER}")], origin.clone());
        assert_ne!(h2, trust_hash(&other, &places), "env counts");

        std::fs::write(ws.join("x.js"), "a").unwrap();
        let mut node = stdio("./server.sh", &[], origin);
        if let Transport::Stdio { args, .. } = &mut node.transport {
            args.push("--require=./x.js".into());
        }
        let before = trust_hash(&node, &places);
        std::fs::write(ws.join("x.js"), "b").unwrap();
        assert_ne!(
            before,
            trust_hash(&node, &places),
            "a file after `=` counts"
        );

        let shown = describe(&cfg, &places);
        let text = format!("{shown:?}");
        assert!(
            text.contains("server.sh") && text.contains("TOKEN"),
            "{text}"
        );
    }
}
