//! Tool tests on temp workspaces, with the real `safety` plugin providing the services.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ch_kernel::{Cx, Registry, Tool, ToolOutput};
use ch_testkit::{
    TestCx, assert_errors_are_observations, assert_tool_conforms, build_registry, registry_tool,
};
use serde_json::{Value, json};

struct Ws {
    dir: tempfile::TempDir,
    registry: Registry,
    cx: Cx,
}

impl Ws {
    fn new() -> Self {
        Self::with_config("")
    }

    fn with_config(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let registry = build_registry(
            vec![ch_safety::plugin(), ch_tools_fs::plugin()],
            dir.path(),
            config,
        );
        let cx = TestCx::from_registry(&registry, dir.path()).build();
        Self { dir, registry, cx }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap()
    }

    fn tool(&self, name: &str) -> Arc<dyn Tool> {
        registry_tool(&self.registry, name)
    }

    async fn call(&self, name: &str, input: Value) -> ToolOutput {
        self.tool(name).call(input, &self.cx).await
    }

    async fn ok(&self, name: &str, input: Value) -> ToolOutput {
        let out = self.call(name, input.clone()).await;
        assert!(!out.is_error, "{name} {input} failed: {}", out.content);
        out
    }

    async fn err(&self, name: &str, input: Value) -> String {
        let out = self.call(name, input.clone()).await;
        assert!(
            out.is_error,
            "{name} {input} should fail, got: {}",
            out.content
        );
        out.content
    }
}

// ------------------------------------------------------------------ conformance

#[test]
fn all_tools_conform() {
    let ws = Ws::new();
    for name in ch_tools_fs::TOOL_NAMES {
        let tool = ws.tool(name);
        assert_tool_conforms(tool.as_ref());
        let read_only = tool.spec().read_only;
        let expected = !matches!(name, "edit_file" | "write_file");
        assert_eq!(read_only, expected, "{name} read_only flag");
    }
}

#[tokio::test]
async fn bad_inputs_are_observations() {
    let ws = Ws::new();
    ws.write("a.txt", "hello\n");
    ws.write("bin.dat", b"\x00\x01\x02");
    std::fs::create_dir_all(ws.path("dir")).unwrap();
    let cases: Vec<(&str, Vec<Value>)> = vec![
        (
            "read_file",
            vec![
                json!({}),
                json!({"path": 3}),
                json!({"path": ""}),
                json!({"path": "missing.txt"}),
                json!({"path": "dir"}),
                json!({"path": "bin.dat"}),
                json!({"path": "../outside"}),
                json!({"path": "/etc/hosts"}),
                json!({"path": "a.txt", "offset": 99}),
                json!({"path": "a.txt", "offset": 0}),
                json!({"path": "a.txt", "limit": -1}),
                Value::Null,
            ],
        ),
        (
            "edit_file",
            vec![
                json!({}),
                json!({"path": "a.txt"}),
                json!({"path": "a.txt", "old_text": "hello", "new_text": "bye"}),
                json!({"path": "a.txt", "old_text": "", "new_text": "x"}),
                json!({"path": "a.txt", "old_text": "x", "new_text": "x"}),
                json!({"path": "missing.txt", "old_text": "a", "new_text": "b"}),
                json!({"path": "dir", "old_text": "a", "new_text": "b"}),
                json!({"path": "../x", "old_text": "a", "new_text": "b"}),
            ],
        ),
        (
            "write_file",
            vec![
                json!({}),
                json!({"path": "a.txt"}),
                json!({"path": "a.txt", "content": "x"}),
                json!({"path": "a.txt", "content": "x", "overwrite": true}),
                json!({"path": "dir", "content": "x"}),
                json!({"path": "../x", "content": "x"}),
                json!({"path": "/tmp/ch-outside", "content": "x"}),
            ],
        ),
        (
            "list_files",
            vec![
                json!({"path": "missing"}),
                json!({"path": "a.txt"}),
                json!({"path": ".."}),
                json!({"recursive": "yes"}),
            ],
        ),
        (
            "glob_files",
            vec![
                json!({}),
                json!({"pattern": ""}),
                json!({"pattern": "a["}),
                json!({"pattern": "../*"}),
                json!({"pattern": "/etc/*"}),
                json!({"pattern": "*", "path": "missing"}),
                json!({"pattern": "*", "path": "a.txt"}),
                json!({"pattern": "*", "path": "../"}),
            ],
        ),
        (
            "grep_text",
            vec![
                json!({}),
                json!({"query": ""}),
                json!({"query": "foo("}),
                json!({"query": "x", "include": "a["}),
                json!({"query": "x", "path": "missing"}),
                json!({"query": "x", "path": "../"}),
            ],
        ),
    ];
    for (name, inputs) in cases {
        assert_errors_are_observations(ws.tool(name).as_ref(), &inputs, &ws.cx).await;
    }
}

#[test]
fn registration_requires_safety() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ch_kernel::Config::default();
    let err = ch_kernel::RegistryBuilder::build(vec![ch_tools_fs::plugin()], &cfg, dir.path())
        .err()
        .expect("should fail without safety");
    assert!(err.to_string().contains("safety"), "{err}");
}

#[test]
fn summaries() {
    let ws = Ws::new();
    let s = |name: &str, input: Value| ws.tool(name).summarize(&input);
    assert_eq!(
        s("read_file", json!({"path": "src/main.rs"})),
        "src/main.rs"
    );
    assert_eq!(
        s(
            "read_file",
            json!({"path": "a.rs", "offset": 10, "limit": 5})
        ),
        "a.rs:10-14"
    );
    assert_eq!(
        s(
            "edit_file",
            json!({"path": "a.rs", "old_text": "a", "new_text": "b\nc\nd"})
        ),
        "a.rs (+3 -1)"
    );
    assert_eq!(
        s("write_file", json!({"path": "n.rs", "content": ""})),
        "n.rs"
    );
    assert_eq!(s("list_files", json!({"recursive": true})), ". (recursive)");
    assert_eq!(
        s("glob_files", json!({"pattern": "*.rs", "path": "src"})),
        "*.rs in src"
    );
    assert_eq!(
        s("grep_text", json!({"query": "fn main", "include": "*.rs"})),
        "\"fn main\" (*.rs)"
    );
}

// ------------------------------------------------------------------ read_file

#[tokio::test]
async fn read_is_line_numbered() {
    let ws = Ws::new();
    ws.write("a.txt", "alpha\nbeta\n\tgamma\n");
    let out = ws.ok("read_file", json!({"path": "a.txt"})).await;
    assert_eq!(
        out.content,
        "     1\talpha\n     2\tbeta\n     3\t\tgamma\n"
    );
    assert_eq!(out.metadata["total_lines"], 3);
}

#[tokio::test]
async fn read_pages_with_offset_and_limit() {
    let ws = Ws::new();
    let text: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    ws.write("a.txt", &text);
    let out = ws
        .ok(
            "read_file",
            json!({"path": "a.txt", "offset": 3, "limit": 2}),
        )
        .await;
    assert!(
        out.content.starts_with("     3\tline 3\n     4\tline 4\n"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("Showing lines 3-4 of 10"),
        "{}",
        out.content
    );
    assert!(out.content.contains("offset=5"), "{}", out.content);
    let out = ws
        .ok("read_file", json!({"path": "a.txt", "offset": 9}))
        .await;
    assert!(out.content.contains("    10\tline 10"));
    let err = ws
        .err("read_file", json!({"path": "a.txt", "offset": 11}))
        .await;
    assert!(err.contains("10 lines"), "{err}");
}

#[tokio::test]
async fn read_default_limit_and_long_lines() {
    let ws = Ws::new();
    let text: String = (1..=2500).map(|i| format!("{i}\n")).collect();
    ws.write("long.txt", &text);
    let out = ws.ok("read_file", json!({"path": "long.txt"})).await;
    // Either the 2000-line default or the output budget stops it, with a footer.
    assert!(
        out.content.contains("Call read_file with offset="),
        "{}",
        &out.content[out.content.len() - 200..]
    );
    assert!(!out.content.contains("\t2001\n"));

    ws.write("wide.txt", format!("{}\nshort\n", "x".repeat(5000)));
    let out = ws.ok("read_file", json!({"path": "wide.txt"})).await;
    assert!(out.content.contains("[line cut at 2000 chars]"));
    assert!(
        out.content
            .contains("1 line(s) longer than 2000 chars were cut")
    );
    assert!(out.content.contains("     2\tshort"));
}

#[tokio::test]
async fn read_rejects_binary_and_directories() {
    let ws = Ws::new();
    ws.write("img.png", b"\x89PNG\r\n\x1a\n\x00\x00\x00");
    std::fs::create_dir_all(ws.path("src")).unwrap();
    let err = ws.err("read_file", json!({"path": "img.png"})).await;
    assert!(err.contains("binary"), "{err}");
    let err = ws.err("read_file", json!({"path": "src"})).await;
    assert!(err.contains("list_files"), "{err}");
}

#[tokio::test]
async fn read_empty_and_missing_files() {
    let ws = Ws::new();
    ws.write("empty.txt", "");
    let out = ws.ok("read_file", json!({"path": "empty.txt"})).await;
    assert!(out.content.contains("is empty"), "{}", out.content);
    ws.write("src/main.rs", "fn main() {}\n");
    let err = ws.err("read_file", json!({"path": "src/main.py"})).await;
    assert!(err.contains("does not exist"), "{err}");
    assert!(err.contains("Did you mean `src/main.rs`"), "{err}");
}

#[tokio::test]
async fn read_stays_under_the_output_budget_and_pages_count_as_full() {
    let ws = Ws::with_config("[agent]\ntool_result_max_chars = 3000\n");
    let text: String = (1..=400).map(|i| format!("line number {i:04}\n")).collect();
    ws.write("big.txt", &text);
    let mut offset = 1;
    let mut pages = 0;
    loop {
        let out = ws
            .ok("read_file", json!({"path": "big.txt", "offset": offset}))
            .await;
        assert!(
            out.content.len() <= 3000,
            "page is {} chars",
            out.content.len()
        );
        pages += 1;
        let end = out.metadata["end_line"].as_u64().unwrap() as usize;
        if end >= 400 {
            break;
        }
        // Not editable until every line was seen.
        let err = ws
            .err(
                "edit_file",
                json!({"path": "big.txt", "old_text": "line number 0001", "new_text": "x"}),
            )
            .await;
        assert!(err.contains("only read part"), "{err}");
        offset = end + 1;
    }
    assert!(pages > 1);
    ws.ok(
        "edit_file",
        json!({"path": "big.txt", "old_text": "line number 0001\n", "new_text": "first\n"}),
    )
    .await;
    assert!(ws.read("big.txt").starts_with("first\nline number 0002"));
}

// ------------------------------------------------------------------ edit_file

#[tokio::test]
async fn edit_replaces_and_returns_a_diff() {
    let ws = Ws::new();
    ws.write("src/lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    ws.ok("read_file", json!({"path": "src/lib.rs"})).await;
    let out = ws
        .ok(
            "edit_file",
            json!({"path": "src/lib.rs", "old_text": "fn b() {}", "new_text": "fn b() { todo!() }"}),
        )
        .await;
    assert_eq!(
        ws.read("src/lib.rs"),
        "fn a() {}\nfn b() { todo!() }\nfn c() {}\n"
    );
    assert!(
        out.content.contains("Edited `src/lib.rs` (+1 -1)"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("-fn b() {}\n+fn b() { todo!() }"),
        "{}",
        out.content
    );
    assert_eq!(out.metadata["path"], "src/lib.rs");
    let diff = out.metadata["diff"].as_str().unwrap();
    assert!(diff.contains("--- a/src/lib.rs") && diff.contains("+++ b/src/lib.rs"));
    // A follow-up edit works without re-reading (our own write is known).
    ws.ok(
        "edit_file",
        json!({"path": "src/lib.rs", "old_text": "fn c() {}", "new_text": "fn d() {}"}),
    )
    .await;
}

#[tokio::test]
async fn edit_not_found_gives_a_hint() {
    let ws = Ws::new();
    ws.write("a.py", "def f():\n    return 1\n");
    ws.ok("read_file", json!({"path": "a.py"})).await;
    let err = ws
        .err(
            "edit_file",
            json!({"path": "a.py", "old_text": "def f():\n  return 1", "new_text": "x"}),
        )
        .await;
    assert!(err.contains("whitespace"), "{err}");
    let err = ws
        .err(
            "edit_file",
            json!({"path": "a.py", "old_text": "nope", "new_text": "x"}),
        )
        .await;
    assert!(err.contains("Re-read the file"), "{err}");
}

#[tokio::test]
async fn edit_multiple_matches_and_replace_all() {
    let ws = Ws::new();
    ws.write("a.txt", "x = 1\ny = 2\nx = 1\nx = 1\n");
    ws.ok("read_file", json!({"path": "a.txt"})).await;
    let err = ws
        .err(
            "edit_file",
            json!({"path": "a.txt", "old_text": "x = 1", "new_text": "x = 9"}),
        )
        .await;
    assert!(err.contains("appears 3 times"), "{err}");
    assert!(err.contains("lines 1, 3, 4"), "{err}");
    assert!(err.contains("replace_all"), "{err}");
    let out = ws
        .ok(
            "edit_file",
            json!({"path": "a.txt", "old_text": "x = 1", "new_text": "x = 9", "replace_all": true}),
        )
        .await;
    assert!(out.content.contains("3 occurrences"), "{}", out.content);
    assert_eq!(ws.read("a.txt"), "x = 9\ny = 2\nx = 9\nx = 9\n");
}

#[tokio::test]
async fn edit_rejects_identical_text() {
    let ws = Ws::new();
    ws.write("a.txt", "same\n");
    ws.ok("read_file", json!({"path": "a.txt"})).await;
    let err = ws
        .err(
            "edit_file",
            json!({"path": "a.txt", "old_text": "same", "new_text": "same"}),
        )
        .await;
    assert!(err.contains("identical"), "{err}");
}

#[tokio::test]
async fn edit_keeps_crlf_line_endings() {
    let ws = Ws::new();
    ws.write("w.txt", "one\r\ntwo\r\nthree\r\n");
    ws.ok("read_file", json!({"path": "w.txt"})).await;
    ws.ok(
        "edit_file",
        json!({"path": "w.txt", "old_text": "one\ntwo", "new_text": "one\n1.5\ntwo"}),
    )
    .await;
    assert_eq!(ws.read("w.txt"), "one\r\n1.5\r\ntwo\r\nthree\r\n");
}

// --------------------------------------------------------------- safety integration

#[tokio::test]
async fn unread_edit_is_rejected() {
    let ws = Ws::new();
    ws.write("a.txt", "hello\n");
    let err = ws
        .err(
            "edit_file",
            json!({"path": "a.txt", "old_text": "hello", "new_text": "bye"}),
        )
        .await;
    assert!(err.contains("Read the whole file with read_file"), "{err}");
    assert_eq!(ws.read("a.txt"), "hello\n");
}

#[tokio::test]
async fn partial_read_then_edit_is_rejected() {
    let ws = Ws::new();
    ws.write("a.txt", "one\ntwo\nthree\n");
    ws.ok("read_file", json!({"path": "a.txt", "limit": 1}))
        .await;
    let err = ws
        .err(
            "edit_file",
            json!({"path": "a.txt", "old_text": "one", "new_text": "1"}),
        )
        .await;
    assert!(err.contains("only read part"), "{err}");
    // Reading the rest completes it.
    ws.ok("read_file", json!({"path": "a.txt", "offset": 2}))
        .await;
    ws.ok(
        "edit_file",
        json!({"path": "a.txt", "old_text": "one", "new_text": "1"}),
    )
    .await;
}

#[tokio::test]
async fn change_on_disk_after_read_is_detected() {
    let ws = Ws::new();
    ws.write("a.txt", "v1\n");
    ws.ok("read_file", json!({"path": "a.txt"})).await;
    // Simulates `bash` (or the user) changing the file behind the agent's back.
    ws.write("a.txt", "v2 from bash\n");
    let err = ws
        .err(
            "edit_file",
            json!({"path": "a.txt", "old_text": "v2", "new_text": "v3"}),
        )
        .await;
    assert!(
        err.contains("changed on disk since you last read it"),
        "{err}"
    );
    let err = ws
        .err(
            "write_file",
            json!({"path": "a.txt", "content": "x", "overwrite": true}),
        )
        .await;
    assert!(err.contains("changed on disk"), "{err}");
    ws.ok("read_file", json!({"path": "a.txt"})).await;
    ws.ok(
        "edit_file",
        json!({"path": "a.txt", "old_text": "v2", "new_text": "v3"}),
    )
    .await;
    assert_eq!(ws.read("a.txt"), "v3 from bash\n");
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escape_is_rejected() {
    let ws = Ws::new();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path("escape")).unwrap();
    for (name, input) in [
        ("read_file", json!({"path": "escape/secret.txt"})),
        (
            "write_file",
            json!({"path": "escape/new.txt", "content": "x"}),
        ),
        ("list_files", json!({"path": "escape"})),
        ("grep_text", json!({"query": "secret", "path": "escape"})),
        ("glob_files", json!({"pattern": "*", "path": "escape"})),
    ] {
        let err = ws.err(name, input).await;
        assert!(err.contains("symlink"), "{name}: {err}");
    }
    assert!(!outside.path().join("new.txt").exists());
    // Walking the workspace doesn't follow the link either.
    let out = ws.ok("grep_text", json!({"query": "secret"})).await;
    assert!(out.content.starts_with("No matches"), "{}", out.content);
}

#[cfg(unix)]
#[tokio::test]
async fn hard_linked_files_are_not_changed() {
    let ws = Ws::new();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("bashrc");
    std::fs::write(&target, "export A=1\n").unwrap();
    std::fs::hard_link(&target, ws.path("rc")).unwrap();
    // Reading through the link is allowed; changing it isn't.
    ws.ok("read_file", json!({"path": "rc"})).await;
    let err = ws
        .err(
            "edit_file",
            json!({"path": "rc", "old_text": "A=1", "new_text": "A=2"}),
        )
        .await;
    assert!(err.contains("2 hard links"), "{err}");
    let err = ws
        .err(
            "write_file",
            json!({"path": "rc", "content": "x", "overwrite": true}),
        )
        .await;
    assert!(err.contains("2 hard links"), "{err}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "export A=1\n");
}

#[tokio::test]
async fn path_escape_is_rejected() {
    let ws = Ws::new();
    for (name, input) in [
        ("read_file", json!({"path": "../etc/passwd"})),
        ("read_file", json!({"path": "/etc/passwd"})),
        (
            "write_file",
            json!({"path": "sub/../../evil.txt", "content": "x"}),
        ),
        (
            "edit_file",
            json!({"path": "../a", "old_text": "a", "new_text": "b"}),
        ),
    ] {
        let err = ws.err(name, input).await;
        assert!(err.contains("outside the workspace"), "{name}: {err}");
    }
    assert!(!ws.root().parent().unwrap().join("evil.txt").exists());
}

// ------------------------------------------------------------------ write_file

#[tokio::test]
async fn write_creates_files_and_parents() {
    let ws = Ws::new();
    let out = ws
        .ok(
            "write_file",
            json!({"path": "a/b/c.txt", "content": "hi\nthere\n"}),
        )
        .await;
    assert!(
        out.content.contains("Created `a/b/c.txt` (2 lines"),
        "{}",
        out.content
    );
    assert_eq!(ws.read("a/b/c.txt"), "hi\nthere\n");
    assert_eq!(out.metadata["path"], "a/b/c.txt");
    // The new file counts as fully read: it can be edited right away.
    ws.ok(
        "edit_file",
        json!({"path": "a/b/c.txt", "old_text": "hi", "new_text": "hello"}),
    )
    .await;
}

#[tokio::test]
async fn write_without_overwrite_is_rejected() {
    let ws = Ws::new();
    ws.write("a.txt", "old\n");
    let err = ws
        .err("write_file", json!({"path": "a.txt", "content": "new\n"}))
        .await;
    assert!(
        err.contains("already exists") && err.contains("overwrite=true"),
        "{err}"
    );
    let err = ws
        .err(
            "write_file",
            json!({"path": "a.txt", "content": "new\n", "overwrite": true}),
        )
        .await;
    assert!(err.contains("Read the whole file"), "{err}");
    assert_eq!(ws.read("a.txt"), "old\n");
    ws.ok("read_file", json!({"path": "a.txt"})).await;
    let out = ws
        .ok(
            "write_file",
            json!({"path": "a.txt", "content": "new\n", "overwrite": true}),
        )
        .await;
    assert!(
        out.content.contains("Overwrote `a.txt` (+1 -1)"),
        "{}",
        out.content
    );
    assert!(
        out.metadata["diff"]
            .as_str()
            .unwrap()
            .contains("-old\n+new")
    );
    assert_eq!(ws.read("a.txt"), "new\n");
}

// ------------------------------------------------------------------ list_files

#[tokio::test]
async fn list_shows_a_tree_with_sizes_and_filters() {
    let ws = Ws::new();
    ws.write("src/main.rs", "fn main() {}\n");
    ws.write("src/deep/mod.rs", "");
    ws.write("README.md", "x".repeat(2048));
    ws.write("target/debug/big", "x");
    ws.write("node_modules/pkg/index.js", "x");
    ws.write(".git/HEAD", "ref");
    ws.write(".gitignore", "ignored.log\n");
    ws.write("ignored.log", "x");

    let out = ws.ok("list_files", json!({})).await;
    assert!(
        out.content.contains("README.md (2.0 KB)"),
        "{}",
        out.content
    );
    assert!(out.content.contains("src/"), "{}", out.content);
    assert!(
        !out.content.contains("main.rs"),
        "not recursive: {}",
        out.content
    );
    assert!(!out.content.contains("ignored.log"), "{}", out.content);
    assert!(!out.content.contains("HEAD"), "{}", out.content);
    assert!(
        out.content
            .contains("Not listed: .git/, node_modules/, target/"),
        "{}",
        out.content
    );

    let out = ws.ok("list_files", json!({"recursive": true})).await;
    assert!(
        out.content
            .contains("src/\n  deep/\n    mod.rs (0 B)\n  main.rs (13 B)"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("big"), "{}", out.content);
    assert!(!out.content.contains("index.js"), "{}", out.content);

    let out = ws.ok("list_files", json!({"path": "src"})).await;
    assert!(out.content.starts_with("src/\n"), "{}", out.content);

    let err = ws.err("list_files", json!({"path": "README.md"})).await;
    assert!(err.contains("read_file"), "{err}");
}

#[tokio::test]
async fn list_is_capped() {
    let ws = Ws::new();
    for i in 0..520 {
        ws.write(&format!("many/f{i:04}.txt"), "");
    }
    let out = ws.ok("list_files", json!({"path": "many"})).await;
    assert!(
        out.content.contains("Showing 500 of 520 entries"),
        "{}",
        &out.content[out.content.len() - 200..]
    );
    std::fs::create_dir_all(ws.path("emptydir")).unwrap();
    let out = ws.ok("list_files", json!({"path": "emptydir"})).await;
    assert!(out.content.contains("(empty)"));
}

// ------------------------------------------------------------------ glob_files

#[tokio::test]
async fn glob_sorts_newest_first() {
    let ws = Ws::new();
    ws.write("src/old.rs", "");
    ws.write("src/new.rs", "");
    ws.write("src/nested/mid.rs", "");
    ws.write("notes.md", "");
    ws.write("target/gen.rs", "");
    let now = SystemTime::now();
    let set = |rel: &str, secs_ago: u64| {
        let f = std::fs::File::options()
            .write(true)
            .open(ws.path(rel))
            .unwrap();
        f.set_modified(now - Duration::from_secs(secs_ago)).unwrap();
    };
    set("src/old.rs", 300);
    set("src/nested/mid.rs", 200);
    set("src/new.rs", 100);

    let out = ws.ok("glob_files", json!({"pattern": "**/*.rs"})).await;
    assert_eq!(out.content, "src/new.rs\nsrc/nested/mid.rs\nsrc/old.rs");
    let out = ws
        .ok("glob_files", json!({"pattern": "*.rs", "path": "src"}))
        .await;
    assert_eq!(out.content, "src/new.rs\nsrc/nested/mid.rs\nsrc/old.rs");
    let out = ws.ok("glob_files", json!({"pattern": "src/*.rs"})).await;
    assert_eq!(out.content, "src/new.rs\nsrc/old.rs");
}

#[tokio::test]
async fn glob_no_match_is_not_an_error() {
    let ws = Ws::new();
    ws.write("a.txt", "");
    let out = ws.ok("glob_files", json!({"pattern": "**/*.zig"})).await;
    assert!(out.content.contains("No files match"), "{}", out.content);
    assert_eq!(out.metadata["matches"], 0);
}

#[tokio::test]
async fn glob_is_capped() {
    let ws = Ws::new();
    for i in 0..210 {
        ws.write(&format!("d/f{i}.txt"), "");
    }
    let out = ws.ok("glob_files", json!({"pattern": "*.txt"})).await;
    assert!(
        out.content
            .contains("Showing the 200 newest of 210 matches")
    );
}

// ------------------------------------------------------------------ grep_text

#[tokio::test]
async fn grep_prints_path_line_text() {
    let ws = Ws::new();
    ws.write("src/a.rs", "fn main() {\n    println!(\"hi\");\n}\n");
    ws.write("src/b.py", "def main():\n    pass\n");
    ws.write("target/x.rs", "fn main() {}\n");
    let out = ws.ok("grep_text", json!({"query": "main"})).await;
    assert_eq!(
        out.content,
        "src/a.rs:1:fn main() {\nsrc/b.py:1:def main():"
    );
    let out = ws
        .ok("grep_text", json!({"query": "main", "include": "*.py"}))
        .await;
    assert_eq!(out.content, "src/b.py:1:def main():");
    let out = ws
        .ok(
            "grep_text",
            json!({"query": "(?i)PRINTLN", "path": "src/a.rs"}),
        )
        .await;
    assert_eq!(out.content, "src/a.rs:2:    println!(\"hi\");");
}

#[tokio::test]
async fn grep_skips_binaries_and_reports_no_match() {
    let ws = Ws::new();
    ws.write("bin.dat", b"needle\x00\x01\x02needle");
    ws.write("text.txt", "no luck\n");
    let out = ws.ok("grep_text", json!({"query": "needle"})).await;
    assert!(
        out.content.starts_with("No matches for `needle`"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn grep_invalid_regex_suggests_escaping() {
    let ws = Ws::new();
    let err = ws.err("grep_text", json!({"query": "foo(bar"})).await;
    assert!(
        err.contains("Invalid regex") && err.contains("Escape"),
        "{err}"
    );
}

#[tokio::test]
async fn grep_is_capped() {
    let ws = Ws::new();
    let text: String = (0..250).map(|i| format!("hit {i}\n")).collect();
    ws.write("many.txt", &text);
    let out = ws.ok("grep_text", json!({"query": "hit"})).await;
    assert_eq!(
        out.content
            .lines()
            .filter(|l| l.starts_with("many.txt:"))
            .count(),
        200
    );
    assert!(
        out.content.contains("Showing the first 200 of 250 matches"),
        "{}",
        out.content
    );
    assert!(out.content.contains("Narrow your search"));
}
