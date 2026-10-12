# Libraries, Linking and Plugin Loading in Rust

A tutorial covering how Rust builds and links code, what an **ABI** is, why loading Rust dynamic libraries at runtime is fragile, and the alternatives: compile-time plugins, C-ABI dynamic libraries, separate processes and WASM. Each section ends with how it applies to `ch` ([architecture](architecture.md)).

---

## 1. How Rust builds a program

### 1.1 Crates and crate types

A **crate** is Rust's unit of compilation. The `crate-type` setting in `Cargo.toml` controls what the compiler produces:

| `crate-type` | Output | Used for |
|---|---|---|
| `bin` | executable | your program (`src/main.rs`) |
| `lib` / `rlib` | `.rlib` (Rust-only archive) | **normal Rust libraries**, the default for `src/lib.rs` |
| `dylib` | `.so` / `.dylib` / `.dll` with the **Rust ABI** | a dynamic library meant to be loaded by other Rust code built by the *same* compiler (rarely used) |
| `cdylib` | `.so` / `.dylib` / `.dll` with the **C ABI** | a dynamic library that *any* language can load (Python extensions, plugins) |
| `staticlib` | `.a` / `.lib` | linking Rust into a C/C++ program statically |
| `proc-macro` | compiler plugin | `#[derive(...)]` and other macros |

```toml
# Cargo.toml of a library that should produce a C-compatible .so/.dylib
[lib]
crate-type = ["cdylib"]
```

### 1.2 Static linking is the default

When you `cargo build` a binary, every Rust dependency (`serde`, `tokio`, your own crates) is compiled to `.rlib` and **statically linked** into a single executable. The standard library is linked statically too. Only system libraries (libc, and OpenSSL if you use it) are usually linked dynamically.

Consequences:
- Rust binaries are self-contained and easy to ship.
- The compiler sees all the code at once, so it can inline across crates, specialize generics and optimize layouts freely.
- Nothing about Rust's internal data layout has to stay stable, because everything is compiled together in one build. This is the root of section 2.

### 1.3 Static vs dynamic, in one picture

```
Static (default)                        Dynamic (loaded at runtime)
┌──────────────────────────┐            ┌──────────────┐     ┌──────────────┐
│ ch binary                │            │ ch binary    │     │ plugin.dylib │
│  ├─ your code            │            │  your code   │◄───►│  plugin code │
│  ├─ tokio, serde, ...    │            │  tokio,serde │     │  its own std │
│  └─ std                  │            │  std         │     │  ...         │
└──────────────────────────┘            └──────────────┘     └──────────────┘
 compiled together, one build             two separate builds that must agree
                                          on the exact layout of everything
                                          passed between them
```

---

## 2. What an ABI is and why Rust's isn't stable

### 2.1 API vs ABI

- The **API** (Application *Programming* Interface) is what you see in source code: function names, types and signatures. It's checked by the compiler.
- The **ABI** (Application *Binary* Interface) is how that looks **in machine code**: how big a struct is, the order of its fields in memory, which CPU registers hold function arguments, how a function's name appears in the binary (name mangling) and how a trait object's method table (vtable) is laid out.

Two pieces of compiled code can call each other safely only if they agree on the ABI. The compiler checks the API; **nobody checks the ABI at runtime**. If the two sides disagree, the program just reads the wrong bytes.

### 2.2 Rust deliberately leaves its ABI unspecified

The C ABI is fixed per platform and has been for decades. That's why every language can call C. Rust's default representation (`repr(Rust)`) is **intentionally unspecified**, so the compiler is free to optimize it. Concrete examples:

**Field reordering.** Rust can reorder struct fields to reduce padding:

```rust
struct A    { a: u8, b: u32, c: u8 }   // repr(Rust): compiler may store as b, a, c → size 8
#[repr(C)]
struct B    { a: u8, b: u32, c: u8 }   // repr(C): declared order, with padding → size 12
```

If the host was built by a compiler that orders `A` one way and the plugin by a compiler that orders it another way, reading `a.b` reads the wrong bytes. There's even a nightly flag (`-Z randomize-layout`) that shuffles layouts on purpose, to catch code that depends on them.

**Niche optimizations.** `Option<&T>` is the same size as `&T`, because Rust uses the null pointer to mean `None`. Similar tricks apply to enums. Those choices can change between compiler versions.

**Trait objects.** A `Box<dyn Tool>` is a data pointer plus a vtable pointer. The vtable's layout (which slot holds which method, and where the destructor, size and alignment live) is not specified.

**Generics.** `fn foo<T>()` has no machine code until it's used with a concrete `T` (this is called monomorphization). A plugin can't export "a generic function". Only concrete instances exist.

**std types.** `String`, `Vec`, `HashMap`, `Arc`, `Result` and `Future` (so also `async fn`) all have unspecified layouts. They work across crates only because those crates were compiled together in the same build.

**Name mangling.** Rust encodes a function's full path, generics and a hash into its symbol name (for example `_ZN5mylib3foo17h8a3f...E`). The scheme and the hash differ between builds.

### 2.3 What goes wrong in practice

Suppose you use `crate-type = "dylib"` (Rust ABI) and load the plugin at runtime:

- The plugin must be built with the **exact same rustc version** and target, and usually the **same versions and features of every shared dependency** (for example `serde_json` if you pass `serde_json::Value` across).
- If they differ, you don't get an error. You get **undefined behavior**: garbage values, crashes far from the actual cause, or silent memory corruption.
- Every `ch` release would force every plugin author to rebuild with matching toolchains. That's not workable for an ecosystem.

### 2.4 Other hazards of loading code into your process

Even with a stable ABI, an in-process plugin shares your fate:

| Hazard | Explanation |
|---|---|
| **Crashes** | A segfault or `abort` in the plugin kills `ch`, losing the session. |
| **Panics across the boundary** | Unwinding a panic across `extern "C"` was undefined behavior for years. Since Rust 1.81 it **aborts the process**. Plugins must catch panics with `std::panic::catch_unwind` at every exported function (or use the `"C-unwind"` ABI on both sides). |
| **Allocator mismatch** | Memory allocated by the plugin must be freed by the plugin. A `cdylib` has its own copy of std, and possibly a different global allocator (for example mimalloc vs the system allocator). Freeing the plugin's `Box` in the host can corrupt the heap. |
| **Unloading** | Unloading a library while the host still holds one of its function pointers, vtables or thread-locals is undefined behavior. Most hosts simply never unload. |
| **Security** | The plugin can read anything in the process, including API keys in memory, and do anything the user can do. |
| **Global state** | Each `cdylib` gets its own copy of statics, such as `tracing`'s global subscriber or a tokio runtime handle, so the host and the plugin don't share them. |

---

## 3. Option A: compile-time plugins (what `ch` uses for built-ins)

The simplest plugin system needs no runtime loading at all. You define **traits** in a core crate, implement them in separate crates, and **link everything into one binary**. Since everything is compiled together, the ABI problems don't apply.

### 3.1 Traits plus a registry

```rust
// ch-kernel/src/lib.rs
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn call(&self, input: serde_json::Value) -> Result<String, String>;
}

pub trait Plugin {
    fn id(&self) -> &'static str;
    fn register(&self, reg: &mut Registry);
}

#[derive(Default)]
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    pub fn add_tool(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }
}
```

```rust
// plugins/tool-shout/src/lib.rs
use ch_kernel::{Plugin, Registry, Tool};

struct Shout;

impl Tool for Shout {
    fn name(&self) -> &str { "shout" }
    fn call(&self, input: serde_json::Value) -> Result<String, String> {
        let text = input["text"].as_str().ok_or("missing `text`")?;
        Ok(text.to_uppercase())
    }
}

pub struct ShoutPlugin;

impl Plugin for ShoutPlugin {
    fn id(&self) -> &'static str { "tool-shout" }
    fn register(&self, reg: &mut Registry) {
        reg.add_tool(Box::new(Shout));
    }
}
```

```rust
// ch-cli/src/main.rs: explicit list of built-in plugins
fn builtin_plugins() -> Vec<Box<dyn ch_kernel::Plugin>> {
    vec![
        Box::new(tool_shout::ShoutPlugin),
        // Box::new(tools_fs::FsPlugin), ...
    ]
}
```

### 3.2 Cargo features to include or exclude plugins

```toml
# ch-cli/Cargo.toml
[features]
default = ["tool-shout", "web-fetch"]
web-fetch = ["dep:tool-web-fetch"]

[dependencies]
tool-shout = { path = "../../plugins/tool-shout", optional = true }
tool-web-fetch = { path = "../../plugins/tool-web-fetch", optional = true }
```

```rust
fn builtin_plugins() -> Vec<Box<dyn ch_kernel::Plugin>> {
    let mut v: Vec<Box<dyn ch_kernel::Plugin>> = Vec::new();
    #[cfg(feature = "tool-shout")]
    v.push(Box::new(tool_shout::ShoutPlugin));
    #[cfg(feature = "web-fetch")]
    v.push(Box::new(tool_web_fetch::WebFetchPlugin));
    v
}
```

`cargo build --no-default-features --features tool-shout` builds a smaller binary without web fetch.

### 3.3 Optional: automatic registration with `inventory`

The [`inventory`](https://docs.rs/inventory) crate lets each plugin crate register itself, with no central list:

```rust
// ch-kernel
pub struct PluginEntry { pub make: fn() -> Box<dyn Plugin> }
inventory::collect!(PluginEntry);

// plugins/tool-shout
inventory::submit! { ch_kernel::PluginEntry { make: || Box::new(ShoutPlugin) } }

// ch-cli
for entry in inventory::iter::<ch_kernel::PluginEntry> {
    let plugin = (entry.make)();
    plugin.register(&mut registry);
}
```

The trade-off is convenience against explicitness: with `inventory` it's harder to see which plugins are compiled in. For `ch`, an explicit list plus cargo features is easier to follow.

**Pros:** zero runtime cost, full Rust types (async, generics, `Box<dyn Trait>`), and no ABI risk.
**Cons:** adding a plugin means recompiling `ch`. **Use in `ch`:** all built-in plugins (Tier 1).

---

## 4. Option B: C-ABI dynamic libraries (`cdylib` + `libloading`)

If you really need to load native code at runtime, the stable way is to talk **C at the boundary**: only `#[repr(C)]` types, raw pointers and `extern "C"` functions. The Rust inside each side can be anything.

### 4.1 The plugin

```toml
# shout-plugin/Cargo.toml
[lib]
crate-type = ["cdylib"]
```

```rust
// shout-plugin/src/lib.rs
use std::ffi::{c_char, CStr, CString};

pub const API_VERSION: u32 = 1;

#[repr(C)]
pub struct PluginInfo {
    pub api_version: u32,
    pub name: *const c_char,       // static, NUL-terminated
}

// Edition 2021: #[no_mangle]. Edition 2024: #[unsafe(no_mangle)].
#[no_mangle]
pub extern "C" fn ch_plugin_info() -> PluginInfo {
    PluginInfo { api_version: API_VERSION, name: c"shout".as_ptr() }
}

/// Returns a newly allocated C string. The host MUST free it with `ch_free_string`.
#[no_mangle]
pub extern "C" fn ch_shout(input: *const c_char) -> *mut c_char {
    // Never let a panic unwind into the host (since Rust 1.81 it aborts).
    let result = std::panic::catch_unwind(|| {
        let text = unsafe { CStr::from_ptr(input) }.to_string_lossy();
        CString::new(text.to_uppercase()).unwrap()
    });
    match result {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Frees a string returned by this plugin, using the plugin's own allocator.
#[no_mangle]
pub extern "C" fn ch_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}
```

### 4.2 The host

```toml
[dependencies]
libloading = "0.8"
```

```rust
use libloading::{Library, Symbol};
use std::ffi::{c_char, CStr, CString};

// Must match the plugin's definition exactly. In practice, put it in a shared
// "plugin-api" crate that both sides depend on.
#[repr(C)]
struct PluginInfo { api_version: u32, name: *const c_char }

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Everything here is `unsafe`: the compiler cannot verify that the symbol
    // really has the signature we claim.
    unsafe {
        let lib = Library::new("target/release/libshout_plugin.dylib")?; // .so on Linux, .dll on Windows

        let info: Symbol<unsafe extern "C" fn() -> PluginInfo> = lib.get(b"ch_plugin_info")?;
        let info = info();
        if info.api_version != 1 {
            return Err("incompatible plugin API version".into());
        }
        println!("loaded plugin: {}", CStr::from_ptr(info.name).to_string_lossy());

        let shout: Symbol<unsafe extern "C" fn(*const c_char) -> *mut c_char> = lib.get(b"ch_shout")?;
        let free: Symbol<unsafe extern "C" fn(*mut c_char)> = lib.get(b"ch_free_string")?;

        let input = CString::new("hello")?;
        let out = shout(input.as_ptr());
        if !out.is_null() {
            println!("{}", CStr::from_ptr(out).to_string_lossy()); // HELLO
            free(out); // freed by the side that allocated it
        }

        // Don't drop `lib` while any pointer obtained from it is still in use.
    }
    Ok(())
}
```

### 4.3 What this costs you

Look at how much ceremony a single `string → string` function needed: `repr(C)` structs, raw pointers, manual freeing, `catch_unwind` and version checks. Now imagine doing that for our real interfaces, which use async streams, `serde_json::Value`, `Result` with rich errors, trait objects and cancellation tokens. Each of them has to be flattened into C.

Crates that help:
- [`abi_stable`](https://docs.rs/abi_stable) provides FFI-safe replacements for std types (`RString`, `RVec`, `RResult`, …) and FFI-safe trait objects (`#[sabi_trait]`), and **checks layouts when the library is loaded**, so a mismatch becomes an error instead of undefined behavior.
- [`stabby`](https://docs.rs/stabby) has a similar goal and also preserves Rust's niche optimizations.

These make the approach workable, but the plugin is still in-process: no crash isolation, the same security exposure, Rust (or C) only, and one build per OS and CPU architecture.

**Use in `ch`:** not used.

---

## 5. Option C: separate processes and a protocol (what `ch` uses for external plugins)

The plugin is a **separate program**. The host starts it and they exchange messages over **stdin/stdout**. The ABI problem disappears: the only contract is the message format, which is plain text (JSON). This is how **LSP** language servers (rust-analyzer and others), **MCP** servers and git's credential helpers work.

### 5.1 A JSON-RPC exchange

We use JSON-RPC 2.0 with one JSON message per line:

```
host → plugin   {"jsonrpc":"2.0","id":1,"method":"tool/call","params":{"name":"shout","input":{"text":"hi"}}}
plugin → host   {"jsonrpc":"2.0","id":1,"result":{"content":"HI"}}
```

### 5.2 The plugin, in Python

```python
#!/usr/bin/env python3
# plugin.py: stdout is reserved for protocol messages; log to stderr.
import json, sys

for line in sys.stdin:
    req = json.loads(line)
    if req["method"] == "tool/call":
        text = req["params"]["input"]["text"]
        resp = {"jsonrpc": "2.0", "id": req["id"], "result": {"content": text.upper()}}
    else:
        resp = {"jsonrpc": "2.0", "id": req["id"],
                "error": {"code": -32601, "message": "method not found"}}
    print(json.dumps(resp), flush=True)
```

### 5.3 The host, in Rust (tokio)

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
serde_json = "1"
anyhow = "1"
```

```rust
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut child = Command::new("python3")
        .arg("plugin.py")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true) // don't leave orphan plugin processes behind
        .spawn()?;

    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();

    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tool/call",
        "params": { "name": "shout", "input": { "text": "hi" } }
    });
    stdin.write_all(format!("{req}\n").as_bytes()).await?;
    stdin.flush().await?;

    // In real code: wrap this in tokio::time::timeout, match responses by `id`,
    // and treat EOF as "plugin crashed" (disable it, keep the agent running).
    let line = lines.next_line().await?.ok_or_else(|| anyhow::anyhow!("plugin exited"))?;
    let resp: Value = serde_json::from_str(&line)?;
    println!("{}", resp["result"]["content"]); // "HI"
    Ok(())
}
```

### 5.4 What makes it production-grade

The minimal version above becomes `ch`'s `external-host` plugin by adding:

- **A handshake.** The first message is `initialize`. The plugin answers with its protocol version and capabilities (which tools, hooks and commands it provides), so the host knows what to register.
- **Request IDs.** Several calls can be in flight at once; responses are matched by `id`.
- **Timeouts and cancellation** on every call.
- **Crash handling.** If stdout reaches EOF or the process exits, mark the plugin dead, report errors for its pending calls, warn the user and keep the agent running.
- **Lifecycle.** Start plugins lazily (on first use) and send `shutdown` on exit.
- **Trust.** Ask before running plugins that come from a project directory (a cloned repo could ship a malicious plugin).

**Pros:**
- Any language works.
- Crash isolation.
- No ABI risk.
- The protocol can be versioned and documented.
- The plugin can run somewhere else (in a container, or on another machine over HTTP).

**Cons:**
- About a millisecond of overhead per call. Negligible next to a model call that takes seconds.
- Data has to be serialized to and from JSON.
- The plugin is **not sandboxed**: it runs with your user's permissions.

**Use in `ch`:** external plugins (Tier 2) and MCP servers (Tier 3; MCP is itself JSON-RPC).

---

## 6. Option D: WebAssembly components (`wasmtime`), a possible later addition

The plugin is compiled to **WebAssembly** and runs **inside** the host process, in a **sandbox**. It can only touch memory and capabilities the host explicitly gives it (no file system, network or environment variables unless granted). The **Component Model** and its interface language **WIT** provide a stable, typed, cross-language ABI, which is what Rust's native ABI lacks.

### 6.1 Interface (WIT)

```wit
// wit/plugin.wit
package ch:plugin@0.1.0;

interface tool {
  record tool-spec {
    name: string,
    description: string,
    input-schema: string,   // JSON schema as text
  }
  spec: func() -> tool-spec;
  call: func(input: string) -> result<string, string>;
}

world plugin {
  export tool;
}
```

### 6.2 Plugin (Rust guest, using `wit-bindgen`)

```rust
// Sketch: check the current wit-bindgen docs for exact module paths.
wit_bindgen::generate!({ world: "plugin", path: "../wit" });

use exports::ch::plugin::tool::{Guest, ToolSpec};

struct Shout;

impl Guest for Shout {
    fn spec() -> ToolSpec {
        ToolSpec {
            name: "shout".into(),
            description: "Uppercase text".into(),
            input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}}}"#.into(),
        }
    }
    fn call(input: String) -> Result<String, String> {
        let v: serde_json::Value = serde_json::from_str(&input).map_err(|e| e.to_string())?;
        Ok(v["text"].as_str().unwrap_or_default().to_uppercase())
    }
}

export!(Shout);
```

Build with `cargo build --target wasm32-wasip2 --release`. Plugins can also be written in other languages that have Component Model tooling (for example Go, Python, JavaScript or C).

### 6.3 Host (Rust, using `wasmtime`)

```rust
// Sketch: the generated API follows the WIT names; check the wasmtime docs for your version.
use wasmtime::{component::{bindgen, Component, Linker}, Engine, Store};

bindgen!({ world: "plugin", path: "wit" });

fn main() -> anyhow::Result<()> {
    let engine = Engine::default();
    let component = Component::from_file(&engine, "shout_plugin.wasm")?;
    let linker = Linker::new(&engine);          // grants nothing: no files, no network
    let mut store = Store::new(&engine, ());

    let plugin = Plugin::instantiate(&mut store, &component, &linker)?;
    let tool = plugin.ch_plugin_tool();
    let spec = tool.call_spec(&mut store)?;
    let out = tool.call_call(&mut store, r#"{"text":"hi"}"#)?;
    println!("{} -> {:?}", spec.name, out);     // shout -> Ok("HI")
    Ok(())
}
```

(Plugins that need files or the network would get them through WASI, with only the specific directories or hosts you allow.)

**Pros:**
- Real sandboxing with fine-grained permissions.
- A stable, typed ABI.
- Runs in the same process, so calls are fast.
- One `.wasm` file runs on every OS and CPU.
- Plugins can't crash the host.

**Cons:**
- The tooling is younger.
- Plugins can't just call any native library or shell command; the host has to explicitly provide that capability.
- More upfront design (WIT interfaces, granting capabilities).

**Use in `ch`:** a possible Tier 4 for untrusted plugins ([roadmap](../ROADMAP.md) task 10.4).

---

## 7. Comparison

| | Compile-time (traits) | `cdylib` + C ABI | Separate process (JSON-RPC) | WASM component |
|---|---|---|---|---|
| Add a plugin without rebuilding the host | ✗ | ✓ | ✓ | ✓ |
| Rich Rust types across the boundary | ✓ full | ✗ C types only (or `abi_stable`) | ✓ via JSON | ✓ via WIT types |
| ABI risk | none | high unless strictly C | none | none |
| Plugin crash takes down host | yes | yes | **no** | **no** |
| Sandboxed | no | no | no (user permissions) | **yes** |
| Languages | Rust | Rust, C, C++ | **any** | many (with WASM tooling) |
| Call overhead | ~0 | ~0 | ~1 ms | microseconds |
| Ship one artifact for all platforms | n/a | ✗ per OS and CPU | depends on the language | ✓ |
| Real-world examples | most Rust apps | Python native extensions, game mods | LSP, MCP, git helpers | Zed extensions, Envoy filters, Extism |

## 8. How `ch` puts these together

```
Tier 1  Built-in plugins     → Option A (traits, cargo features)        fast, full Rust types
Tier 2  External plugins     → Option C (JSON-RPC over stdio)           any language, crash-isolated
Tier 3  MCP / shell hooks    → Option C variants                        reuse existing ecosystems
Tier 4  (later) Sandboxed    → Option D (wasmtime + WIT)                for untrusted plugins
Not used                     → Option B (dynamic libraries)             ABI and safety cost too high for the benefit
```

All four tiers end up as the **same kernel traits** (`Tool`, `Hook`, …). An external plugin is wrapped in an adapter that implements `Tool` by sending JSON-RPC messages, so the agent loop never knows how a plugin was loaded.

---

## 9. Try it yourself

1. **See layout differences:** print `std::mem::size_of::<A>()` and `size_of::<B>()` for the structs in §2.2 (expect 8 and 12 on common 64-bit targets).
2. **Build a `cdylib`:** create the §4 plugin, run `cargo build --release`, then inspect the exported symbols with `nm -gU target/release/libshout_plugin.dylib | grep ch_` (macOS) or `nm -D --defined-only target/release/libshout_plugin.so | grep ch_` (Linux). Compare against a `lib` crate's mangled names.
3. **Break it on purpose:** in the host, change `PluginInfo.api_version` to `u64` and see what gets printed. That is undefined behavior with no compiler error.
4. **Process plugin:** run §5's host and Python plugin. Then add `raise Exception()` to the plugin and make the host survive it.
5. **WASM:** install the target with `rustup target add wasm32-wasip2` and follow §6.

## 10. Further reading

- Rust Reference, *Linkage*: <https://doc.rust-lang.org/reference/linkage.html>
- Rust Reference, *Type layout*: <https://doc.rust-lang.org/reference/type-layout.html>
- The Rustonomicon, *FFI*: <https://doc.rust-lang.org/nomicon/ffi.html>
- `libloading`: <https://docs.rs/libloading>
- `abi_stable`: <https://docs.rs/abi_stable>
- JSON-RPC 2.0 spec: <https://www.jsonrpc.org/specification>
- Model Context Protocol: <https://modelcontextprotocol.io>
- WebAssembly Component Model: <https://component-model.bytecodealliance.org>
- `wasmtime`: <https://docs.wasmtime.dev>
