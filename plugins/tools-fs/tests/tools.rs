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

#[tokio::test]
async fn edit_reindents_a_unique_whitespace_match() {
    let ws = Ws::new();
    ws.write(
        "a.txt",
        "block:\n        item one\n            detail = 1\n\n        item two\n            detail = 2\n",
    );
    ws.ok("read_file", json!({"path": "a.txt"})).await;
    // Copied with 4 spaces where the file has 8; nested lines keep their extra 4.
    let out = ws
        .ok(
            "edit_file",
            json!({
                "path": "a.txt",
                "old_text": "    item one\n        detail = 1\n\n    item two\n",
                "new_text": "    item one\n        detail = 10\n        extra = 3\n\n    item two\n",
            }),
        )
        .await;
    assert_eq!(
        ws.read("a.txt"),
        "block:\n        item one\n            detail = 10\n            extra = 3\n\n        \
         item two\n            detail = 2\n"
    );
    assert!(
        out.content.starts_with(
            "Edited `a.txt` (+2 -1). old_text matched lines 2–5 only after adjusting whitespace \
             (indentation `4 spaces` → `8 spaces`); new_text was re-indented the same way. \
             Check the diff.\n"
        ),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("+            extra = 3"),
        "{}",
        out.content
    );
    assert_eq!(out.metadata["whitespace_adjusted"], true);
    assert_eq!(out.metadata["path"], "a.txt");
    // Our own write is known, so the next edit needs no re-read.
    ws.ok(
        "edit_file",
        json!({"path": "a.txt", "old_text": "extra = 3", "new_text": "extra = 4"}),
    )
    .await;
}

#[tokio::test]
async fn edit_reindents_spaces_to_a_tab() {
    let ws = Ws::new();
    ws.write("m.txt", "start {\n\tlet x = 1;\n\tlet y = 2;\n}\n");
    ws.ok("read_file", json!({"path": "m.txt"})).await;
    let out = ws
        .ok(
            "edit_file",
            json!({
                "path": "m.txt",
                "old_text": "    let x = 1;\n    let y = 2;\n",
                "new_text": "    let x = 1;\n    let y = 3;\n",
            }),
        )
        .await;
    assert_eq!(ws.read("m.txt"), "start {\n\tlet x = 1;\n\tlet y = 3;\n}\n");
    assert!(
        out.content.contains("(indentation `4 spaces` → `1 tab`)"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn edit_ignores_trailing_whitespace_in_the_file() {
    let ws = Ws::new();
    ws.write("t.txt", "def:\n    x = 1   \n    y = 2\n");
    ws.ok("read_file", json!({"path": "t.txt"})).await;
    let out = ws
        .ok(
            "edit_file",
            json!({"path": "t.txt", "old_text": "    x = 1\n    y = 2", "new_text": "    x = 5\n    y = 2"}),
        )
        .await;
    assert_eq!(ws.read("t.txt"), "def:\n    x = 5\n    y = 2\n");
    assert!(
        out.content.contains(
            "old_text matched lines 2–3 only after adjusting whitespace (trailing whitespace \
             ignored). Check the diff."
        ),
        "{}",
        out.content
    );
    assert!(!out.content.contains("re-indented"), "{}", out.content);
}

#[tokio::test]
async fn edit_reindents_in_a_crlf_file() {
    let ws = Ws::new();
    ws.write(
        "w.txt",
        "class A:\r\n    def f(self):\r\n        return 1\r\nend\r\n",
    );
    ws.ok("read_file", json!({"path": "w.txt"})).await;
    let out = ws
        .ok(
            "edit_file",
            json!({
                "path": "w.txt",
                "old_text": "def f(self):\n    return 1\n",
                "new_text": "def f(self):\n    return 2\n",
            }),
        )
        .await;
    assert_eq!(
        ws.read("w.txt"),
        "class A:\r\n    def f(self):\r\n        return 2\r\nend\r\n"
    );
    assert!(
        out.content.contains("(indentation `none` → `4 spaces`)"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn edit_refuses_ambiguous_or_unsafe_whitespace_matches() {
    let ws = Ws::new();
    let cases = [
        // Two places match once whitespace is ignored.
        (
            "two.txt",
            "a:\n    x = 1\n    y = 2\nb:\n    x = 1\n    y = 2\n",
            json!({"old_text": "x = 1\ny = 2", "new_text": "x = 1\ny = 3"}),
            "whitespace_ambiguous",
            "Ignoring whitespace it matches 2 places (lines 2–3, 5–6), so nothing was changed.",
        ),
        // Only the method matches with a consistent shift, but either could be meant.
        (
            "align.txt",
            "def a():\n    return f(1,\n             2)\n\nclass C:\n    def a(self):\n        \
             return f(1,\n            2)\n",
            json!({"old_text": "  return f(1,\n      2)", "new_text": "  return f(1,\n      3)"}),
            "whitespace_ambiguous",
            "matches 2 places (lines 2–3, 7–8)",
        ),
        // A new_text line indented less than old_text's base can't be re-indented.
        (
            "less.txt",
            "list:\n        a = 1\n        b = 2\n",
            json!({"old_text": "    a = 1\n    b = 2", "new_text": "    a = 1\n  b = 3"}),
            "not_reindentable",
            "It matches lines 2–3 with the indentation changed (`4 spaces` → `8 spaces`), but \
             new_text line 2 doesn't start with old_text's indentation `4 spaces`",
        ),
        // Re-indenting would put spaces after the file's tabs.
        (
            "tabs.txt",
            "start {\n\tif x {\n\t\ty();\n\t}\n}\n",
            json!({
                "old_text": "    if x {\n    \ty();\n    }",
                "new_text": "    if x {\n    \ty();\n        w = 0;\n    }",
            }),
            "not_reindentable",
            "but the file indents these lines with tabs and new_text line 3 is indented further \
             with spaces, so re-indenting it would mix them.",
        ),
        // Nested lines must keep the same indentation after the base: here spaces vs tabs.
        (
            "nested.txt",
            "start {\n\tif x {\n\t\ty();\n\t}\n}\n",
            json!({
                "old_text": "    if x {\n        y();\n    }",
                "new_text": "    if x {\n        z();\n    }",
            }),
            "not_reindentable",
            "Ignoring whitespace it matches lines 2–4, but no single change of indentation turns \
             it into those lines, which read:\n\tif x {\n\t\ty();\n\t}\n",
        ),
        // Changing only indentation would be shifted as well.
        (
            "indent.txt",
            "list:\n        a = 1\n        b = 2\n",
            json!({"old_text": "    a = 1\n    b = 2", "new_text": "        a = 1\n        b = 2"}),
            "indentation_only",
            "new_text changes only indentation",
        ),
    ];
    for (name, text, mut input, reason, message) in cases {
        ws.write(name, text);
        ws.ok("read_file", json!({ "path": name })).await;
        input["path"] = json!(name);
        let out = ws.call("edit_file", input).await;
        assert!(out.is_error, "{name}: {}", out.content);
        assert!(out.content.contains(message), "{name}: {}", out.content);
        assert_eq!(out.metadata["failure"], reason, "{name}");
        assert_eq!(ws.read(name), text, "{name}");
    }
}

#[tokio::test]
async fn replace_all_never_adjusts_whitespace() {
    let ws = Ws::new();
    let text = "start {\n\tlet x = 1;\n\tlet y = 2;\n}\n";
    ws.write("m.txt", text);
    ws.ok("read_file", json!({"path": "m.txt"})).await;
    let err = ws
        .err(
            "edit_file",
            json!({
                "path": "m.txt",
                "old_text": "    let x = 1;\n    let y = 2;",
                "new_text": "    let x = 1;\n    let y = 3;",
                "replace_all": true,
            }),
        )
        .await;
    assert!(
        err.contains(
            "Ignoring whitespace, it matches lines 2–3, which read:\n\tlet x = 1;\n\tlet y = \
             2;\nreplace_all only replaces exact matches. Re-read"
        ),
        "{err}"
    );
    assert_eq!(ws.read("m.txt"), text);
}

#[tokio::test]
async fn edit_not_found_shows_the_closest_lines() {
    let ws = Ws::new();
    ws.write(
        "s.txt",
        "steps:\n    one()\n    two()\n    three()\nend\n\nagain:\n    one()\n    two()\n    three()\n",
    );
    ws.ok("read_file", json!({"path": "s.txt"})).await;
    let err = ws
        .err(
            "edit_file",
            json!({"path": "s.txt", "old_text": "    one()\n    TWO()\n    three()", "new_text": "x"}),
        )
        .await;
    assert!(
        err.starts_with(
            "old_text was not found in `s.txt`. The closest part of the file is lines 2–4 (2 of \
             old_text's 3 non-blank lines appear there). Diff from old_text (-) to the file \
             (+):\n     one()\n-    TWO()\n+    two()\n     three()\nAs close: lines 8–10.\n\
             Re-read the file with read_file"
        ),
        "{err}"
    );
    // Nothing close: just the advice.
    let err = ws
        .err(
            "edit_file",
            json!({"path": "s.txt", "old_text": "unrelated\ntext here", "new_text": "x"}),
        )
        .await;
    assert_eq!(
        err,
        "old_text was not found in `s.txt`. Re-read the file with read_file and copy the text \
         exactly; whitespace and indentation must match, and line-number prefixes are not part \
         of the file."
    );
}

#[tokio::test]
async fn edit_failures_carry_a_reason() {
    let ws = Ws::new();
    ws.write("a.txt", "x = 1\nx = 1\n");
    ws.write("unread.txt", "hello\n");
    ws.write("latin1.txt", b"caf\xe9\n");
    ws.write("t.txt", "k:\n    a  \n    b\n");
    std::fs::create_dir_all(ws.path("dir")).unwrap();
    for p in ["a.txt", "latin1.txt", "t.txt"] {
        ws.ok("read_file", json!({ "path": p })).await;
    }
    ws.ok(
        "write_file",
        json!({"path": "nul.txt", "content": "a\u{0}b\n"}),
    )
    .await;
    let cases = [
        (json!({"path": "a.txt"}), "bad_input", None),
        (
            json!({"path": "a.txt", "old_text": "", "new_text": "y"}),
            "empty_old_text",
            None,
        ),
        (
            json!({"path": "a.txt", "old_text": "y", "new_text": "y"}),
            "identical",
            None,
        ),
        (
            json!({"path": "../x", "old_text": "a", "new_text": "b"}),
            "bad_path",
            None,
        ),
        (
            json!({"path": "dir", "old_text": "a", "new_text": "b"}),
            "is_dir",
            Some("dir"),
        ),
        (
            json!({"path": "missing.txt", "old_text": "a", "new_text": "b"}),
            "missing",
            Some("missing.txt"),
        ),
        (
            json!({"path": "unread.txt", "old_text": "hello", "new_text": "b"}),
            "unread_or_stale",
            Some("unread.txt"),
        ),
        (
            json!({"path": "nul.txt", "old_text": "a", "new_text": "b"}),
            "binary",
            Some("nul.txt"),
        ),
        (
            json!({"path": "latin1.txt", "old_text": "caf", "new_text": "b"}),
            "not_utf8",
            Some("latin1.txt"),
        ),
        (
            json!({"path": "a.txt", "old_text": "nope", "new_text": "b"}),
            "not_found",
            Some("a.txt"),
        ),
        (
            json!({"path": "a.txt", "old_text": "x = 1", "new_text": "b"}),
            "not_unique",
            Some("a.txt"),
        ),
        // Matches once trailing whitespace is ignored, but changes nothing.
        (
            json!({"path": "t.txt", "old_text": "    a\n    b", "new_text": "    a  \n    b"}),
            "identical",
            Some("t.txt"),
        ),
    ];
    for (input, reason, path) in cases {
        let out = ws.call("edit_file", input.clone()).await;
        assert!(out.is_error, "{input}: {}", out.content);
        assert_eq!(out.metadata["failure"], reason, "{input}: {}", out.content);
        assert_eq!(
            out.metadata.get("path").and_then(Value::as_str),
            path,
            "{input}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn edit_write_failure_carries_a_reason() {
    use std::os::unix::fs::PermissionsExt;
    let ws = Ws::new();
    ws.write("ro.txt", "a\n");
    ws.ok("read_file", json!({"path": "ro.txt"})).await;
    std::fs::set_permissions(ws.path("ro.txt"), std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new()
        .write(true)
        .open(ws.path("ro.txt"))
        .is_ok()
    {
        return; // Running as root: read-only files are still writable.
    }
    let out = ws
        .call(
            "edit_file",
            json!({"path": "ro.txt", "old_text": "a", "new_text": "b"}),
        )
        .await;
    assert!(out.content.contains("Could not write"), "{}", out.content);
    assert_eq!(out.metadata["failure"], "write_failed");
    assert_eq!(out.metadata["path"], "ro.txt");
}

#[tokio::test]
async fn edit_hints_stay_fast_on_big_files() {
    let ws = Ws::new();
    let text: String = (0..20_000)
        .map(|i| format!("    entry {i} = value {}\n", i % 7))
        .collect();
    ws.ok("write_file", json!({"path": "big.txt", "content": text}))
        .await;
    let start = std::time::Instant::now();
    let err = ws
        .err(
            "edit_file",
            json!({
                "path": "big.txt",
                "old_text": "    entry 19990 = value 5\n    entry 19991 = CHANGED\n    entry 19992 = value 0\n",
                "new_text": "x",
            }),
        )
        .await;
    assert!(
        err.contains("The closest part of the file is lines 19991–19993"),
        "{err}"
    );
    let out = ws
        .ok(
            "edit_file",
            json!({
                "path": "big.txt",
                "old_text": "entry 19998 = value 6\nentry 19999 = value 0\n",
                "new_text": "entry 19998 = six\nentry 19999 = value 0\n",
            }),
        )
        .await;
    assert!(out.content.contains("lines 19999–20000"), "{}", out.content);
    assert!(ws.read("big.txt").ends_with(
        "    entry 19997 = value 5\n    entry 19998 = six\n    entry 19999 = value 0\n"
    ));
    let took = start.elapsed();
    assert!(took < Duration::from_secs(2), "took {took:?}");
}

#[tokio::test]
async fn edit_search_is_linear_on_repetitive_files() {
    // 100k windows of `x` lines each agree with old_text up to its last line, so
    // comparing old_text with each window in turn takes 10^10 line comparisons.
    let ws = Ws::new();
    let (lines, n) = (200_000, 100_000);
    let mut text = "  x\n".repeat(lines);
    text.push_str("  y\n");
    ws.ok("write_file", json!({"path": "big.txt", "content": text}))
        .await;
    let start = std::time::Instant::now();
    let mut old_text = "x\n".repeat(n - 1);
    old_text.push('y');
    let new_text = old_text.replace('y', "z");
    let out = ws
        .ok(
            "edit_file",
            json!({"path": "big.txt", "old_text": old_text, "new_text": new_text}),
        )
        .await;
    assert!(out.content.contains("(+1 -1)"), "{}", out.content);
    assert!(ws.read("big.txt").ends_with("  x\n  x\n  z\n"));
    // Nearly matching everywhere, but not at all: the hint is cheap too.
    let mut old_text = "x\n".repeat(n - 1);
    old_text.push('q');
    let err = ws
        .err(
            "edit_file",
            json!({"path": "big.txt", "old_text": old_text, "new_text": "x"}),
        )
        .await;
    assert!(
        err.contains("The closest part of the file is lines"),
        "{err}"
    );
    let took = start.elapsed();
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

#[tokio::test]
async fn cancelled_edits_write_nothing() {
    let ws = Ws::new();
    let text = "a:\n        x = 1\n        y = 2\n";
    ws.write("c.txt", text);
    ws.ok("read_file", json!({"path": "c.txt"})).await;
    let mut cx = ws.cx.clone();
    cx.cancel = ch_kernel::reexports::CancellationToken::new();
    cx.cancel.cancel();
    // An exact match, then one found by the whitespace fallback.
    for (old_text, new_text) in [
        ("x = 1", "x = 2"),
        ("    x = 1\n    y = 2", "    x = 1\n    y = 3"),
    ] {
        let out = ws
            .tool("edit_file")
            .call(
                json!({"path": "c.txt", "old_text": old_text, "new_text": new_text}),
                &cx,
            )
            .await;
        assert_eq!(out.content, "Cancelled.");
        assert_eq!(out.metadata["failure"], "cancelled");
        assert_eq!(ws.read("c.txt"), text);
    }
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
    // One file: no path on each line.
    assert_eq!(out.content, "2:    println!(\"hi\");");
}

#[tokio::test]
async fn grep_stays_under_the_output_budget() {
    let ws = Ws::new();
    let long = "x".repeat(290);
    let text: String = (1..=150).map(|i| format!("hit {i} {long}\n")).collect();
    ws.write("big.txt", &text);
    let out = ws
        .ok("grep_text", json!({"query": "hit", "path": "big.txt"}))
        .await;
    assert!(
        out.content.chars().count() <= 24_000,
        "{}",
        out.content.chars().count()
    );
    assert!(
        out.content.starts_with("1:hit 1 "),
        "{}",
        &out.content[..40]
    );
    assert!(
        out.content.contains("(Showing the first "),
        "the note says more exist"
    );
    assert_eq!(out.metadata["matches"], 150);
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

// ------------------------------------------------------------------ read-only roots

#[tokio::test]
async fn the_session_dir_can_be_read_not_changed() {
    let ws = Ws::new();
    let outside = tempfile::tempdir().unwrap();
    let session = outside.path().join("s1");
    std::fs::create_dir_all(session.join("spill")).unwrap();
    let out = session.join("spill/out.txt");
    std::fs::write(&out, "line one\nneedle here\nline three\n").unwrap();
    let file = out.to_str().unwrap().to_string();
    let dir = session.join("spill").to_str().unwrap().to_string();

    let mut cx = TestCx::from_registry(&ws.registry, ws.root()).build();
    cx.session_dir = Some(Arc::new(session.clone()));
    let call = |name: &str, input: Value| {
        let tool = ws.tool(name);
        let cx = cx.clone();
        async move { tool.call(input, &cx).await }
    };

    let read = call("read_file", json!({"path": file, "offset": 2, "limit": 1})).await;
    assert!(!read.is_error, "{}", read.content);
    assert!(read.content.contains("needle here"), "{}", read.content);
    let grep = call("grep_text", json!({"query": "needle", "path": file})).await;
    assert!(!grep.is_error, "{}", grep.content);
    assert!(grep.content.contains("needle here"), "{}", grep.content);

    let edit = call(
        "edit_file",
        json!({"path": file, "old_text": "needle", "new_text": "pin"}),
    )
    .await;
    assert!(
        edit.is_error && edit.content.contains("outside the workspace"),
        "{}",
        edit.content
    );
    assert!(
        call("write_file", json!({"path": file, "content": "x"}))
            .await
            .is_error
    );
    assert!(call("list_files", json!({"path": dir})).await.is_error);
    assert!(std::fs::read_to_string(&out).unwrap().contains("needle"));

    // Without a session dir the same read is refused, as before.
    let err = ws.err("read_file", json!({"path": file})).await;
    assert!(err.contains("outside the workspace"), "{err}");
}
