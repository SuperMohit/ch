//! An external plugin for `external-host`'s tests, built with `ch-plugin-sdk`. Not
//! installed or used otherwise.
//!
//! Flags: `--fail-init` (exits 3 before serving), `--hang` (never answers),
//! `--linger` (keeps running two minutes after stdin closes), `--slow-init <ms>` (waits
//! before serving), `--bad-protocol <ms>` (answers `initialize` with protocol 99 after
//! `ms`, then lingers, with a child that ignores SIGTERM, whose pid it writes to
//! `$CH_PLUGIN_DIR/child_pid`). It writes its own pid to `$CH_PLUGIN_DIR/pid`.
//!
//! Tools: `echo`, `guarded_echo`, `env`, `crash`, `slow`, `add_tool`, `notify`,
//! `leak`, `child`, and `extra` after `add_tool`. A `pre_tool` hook on `*guarded*`
//! denies input containing "forbidden"; a `post_tool` hook on `*__echo` adds a note.
//! It contributes a system block, a reminder on turn 1, `/hello`, and writes the
//! events it receives to `$CH_PLUGIN_DIR/events.log`.

#![allow(clippy::zombie_processes)] // `child` leaves a process behind on purpose.

use std::io::Write as _;
use std::time::Duration;

use ch_plugin_sdk::protocol::{CommandRunResult, ToolDef};
use ch_plugin_sdk::{HookDecision, Plugin};
use serde_json::{Value, json};

fn obj(props: Value) -> Value {
    json!({"type": "object", "properties": props})
}

fn text(input: &Value) -> String {
    input["text"].as_str().unwrap_or_default().to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    if flag("--fail-init") {
        eprintln!("cannot start: SECRET_TOKEN=abcd1234efgh is wrong");
        std::process::exit(3);
    }
    if flag("--hang") {
        std::thread::sleep(Duration::from_secs(120));
        return;
    }
    let value = |f: &str| {
        args.iter()
            .position(|a| a == f)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    if let Some(dir) = std::env::var_os("CH_PLUGIN_DIR") {
        let _ = std::fs::write(
            std::path::Path::new(&dir).join("pid"),
            std::process::id().to_string(),
        );
    }
    if let Some(ms) = value("--slow-init") {
        std::thread::sleep(Duration::from_millis(ms));
    }
    if let Some(ms) = value("--bad-protocol") {
        // Answer `initialize` with another protocol version after `ms`, then linger.
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let id = serde_json::from_str::<Value>(&line)
            .map(|v| v["id"].clone())
            .unwrap_or(Value::Null);
        // A child that ignores SIGTERM, so stopping the group takes its full grace.
        if let Ok(child) = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 120"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            && let Some(dir) = std::env::var_os("CH_PLUGIN_DIR")
        {
            let _ = std::fs::write(
                std::path::Path::new(&dir).join("child_pid"),
                child.id().to_string(),
            );
        }
        std::thread::sleep(Duration::from_millis(ms));
        println!(
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": {"protocol": 99}})
        );
        std::thread::sleep(Duration::from_secs(120));
        return;
    }
    let dir = std::env::var("CH_PLUGIN_DIR").unwrap_or_default();
    let plugin = Plugin::new()
        .tool(
            "echo",
            "Echoes text.",
            obj(json!({"text": {"type": "string"}})),
            |i, _| Ok(text(&i)),
        )
        .tool(
            "guarded_echo",
            "Echoes text, checked first.",
            obj(json!({"text": {"type": "string"}})),
            |i, _| Ok(text(&i)),
        )
        .tool("env", "Its environment.", obj(json!({})), |_, _| {
            let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
            let cwd = std::env::current_dir().unwrap_or_default();
            Ok(json!({"env": env, "cwd": cwd, "pid": std::process::id()}).to_string())
        })
        .tool("crash", "Exits.", obj(json!({})), |_, _| {
            eprintln!("crashing on purpose");
            std::process::exit(7);
        })
        .tool(
            "slow",
            "Sleeps.",
            obj(json!({"ms": {"type": "integer"}})),
            |i, _| {
                std::thread::sleep(Duration::from_millis(i["ms"].as_u64().unwrap_or(1000)));
                Ok("slept".into())
            },
        )
        .tool("add_tool", "Adds `extra`.", obj(json!({})), |_, cx| {
            cx.tools_changed(vec![
                ToolDef {
                    name: "echo".into(),
                    description: "Echoes text.".into(),
                    input_schema: obj(json!({"text": {"type": "string"}})),
                },
                ToolDef {
                    name: "extra".into(),
                    description: "Added later.".into(),
                    input_schema: obj(json!({})),
                },
            ]);
            Ok("added".into())
        })
        .tool("extra", "Added later.", obj(json!({})), |_, _| {
            Ok("extra".into())
        })
        .tool("notify", "Notifies the user.", obj(json!({})), |_, cx| {
            cx.notify("hello from the plugin");
            Ok("notified".into())
        })
        .tool("leak", "Returns its secret.", obj(json!({})), |_, _| {
            Ok(format!(
                "secret={}",
                std::env::var("SECRET_TOKEN").unwrap_or_default()
            ))
        })
        .tool(
            "child",
            "Starts a child process.",
            obj(json!({})),
            |_, _| {
                let child = std::process::Command::new("sleep")
                    .arg("120")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| e.to_string())?;
                Ok(child.id().to_string())
            },
        )
        .hook("pre_tool", &["*guarded*"], |event, _| {
            if event["call"]["input"].to_string().contains("forbidden") {
                HookDecision::Deny {
                    reason: "the input is forbidden".into(),
                }
            } else {
                HookDecision::Continue
            }
        })
        .hook("post_tool", &["*__echo"], |_, _| HookDecision::AppendNote {
            text: "checked by the test plugin".into(),
        })
        .system_prompt(|_| vec!["TEST PLUGIN SYSTEM BLOCK".into()])
        .turn_reminders(|cx| {
            if cx.turn() == 1 {
                vec!["first-turn reminder from the test plugin".into()]
            } else {
                Vec::new()
            }
        })
        .command("hello", |args, _| CommandRunResult {
            message: Some(format!("hello, {args}")),
            prompt: None,
        })
        .on_events(&["tool_call_end"], move |event, _| {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("{dir}/events.log"))
            {
                let _ = writeln!(f, "{} {}", event["type"], event["name"]);
            }
        });
    let _ = plugin.run();
    if flag("--linger") {
        std::thread::sleep(Duration::from_secs(120));
    }
}
