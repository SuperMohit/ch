//! Test helpers and conformance suites for ch plugins.
//!
//! - [`assert_tool_conforms`] checks a tool's spec: name, input schema and description
//!   format.
//! - [`assert_errors_are_observations`] checks that bad inputs come back as error
//!   observations instead of panics.
//! - [`test_cx`] / [`TestCx`] build a [`Cx`] with chosen services and executor, and
//!   [`build_registry`] registers real plugins against a temp workspace.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ch_kernel::reexports::futures::FutureExt as _;
use ch_kernel::{
    Config, Cx, Executor, Plugin, Registry, RegistryBuilder, Services, Tool, ToolSpec,
};
use serde_json::Value;

/// Descriptions longer than this get a warning on stderr: every tool's description is
/// sent with every request.
pub const DESCRIPTION_SOFT_LIMIT: usize = 2500;

/// Sections a tool description must contain, in this order.
pub const DESCRIPTION_SECTIONS: [&str; 4] = ["When:", "Protocol:", "Error modes:", "Side effects:"];

/// Panics with a clear message unless `tool`'s spec follows the conventions:
///
/// - the name is snake_case (`[a-z][a-z0-9_]*`, no double or trailing underscore);
/// - the input schema is `{"type": "object", "properties": {...}}` and every `required`
///   key is a property;
/// - the description has `When:`, `Protocol:`, `Error modes:` and `Side effects:` in
///   that order (warns on stderr above [`DESCRIPTION_SOFT_LIMIT`] chars).
pub fn assert_tool_conforms(tool: &dyn Tool) {
    let spec = tool.spec();
    assert_spec_conforms(&spec);
}

/// [`assert_tool_conforms`] for a bare spec (external tools, MCP tools).
pub fn assert_spec_conforms(spec: &ToolSpec) {
    let name = &spec.name;
    if !is_snake_case(name) {
        panic!(
            "tool name `{name}` is not snake_case (use lowercase letters, digits and single underscores)"
        );
    }

    let schema = &spec.input_schema;
    let Some(obj) = schema.as_object() else {
        panic!("tool `{name}`: input schema must be a JSON object, got {schema}");
    };
    if obj.get("type").and_then(Value::as_str) != Some("object") {
        panic!("tool `{name}`: input schema must have \"type\": \"object\", got {schema}");
    }
    let Some(props) = obj.get("properties").and_then(Value::as_object) else {
        panic!("tool `{name}`: input schema must have a \"properties\" object, got {schema}");
    };
    for (key, prop) in props {
        if !prop.is_object() {
            panic!("tool `{name}`: property `{key}` must be a schema object, got {prop}");
        }
    }
    if let Some(required) = obj.get("required") {
        let Some(list) = required.as_array() else {
            panic!("tool `{name}`: \"required\" must be an array, got {required}");
        };
        for key in list {
            let Some(key) = key.as_str() else {
                panic!("tool `{name}`: \"required\" entries must be strings, got {key}");
            };
            if !props.contains_key(key) {
                panic!("tool `{name}`: required key `{key}` is not in \"properties\"");
            }
        }
    }

    let desc = &spec.description;
    let mut from = 0usize;
    for section in DESCRIPTION_SECTIONS {
        match desc[from..].find(section) {
            Some(i) => from += i + section.len(),
            None => {
                if desc.contains(section) {
                    panic!(
                        "tool `{name}`: description sections must appear in the order {}; \
                         `{section}` is out of order",
                        DESCRIPTION_SECTIONS.join(" / ")
                    );
                }
                panic!(
                    "tool `{name}`: description is missing the `{section}` section (expected {})",
                    DESCRIPTION_SECTIONS.join(" / ")
                );
            }
        }
    }
    if desc.chars().count() > DESCRIPTION_SOFT_LIMIT {
        eprintln!(
            "warning: tool `{name}` has a {}-char description (over {DESCRIPTION_SOFT_LIMIT}); \
             it is sent with every request, consider trimming it",
            desc.chars().count()
        );
    }
}

fn is_snake_case(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.ends_with('_')
        && !name.contains("__")
}

/// Calls `tool` with each of `bad_inputs` and panics unless every call returns
/// `is_error: true` with a non-empty message. A panic inside the tool is reported with
/// the input that caused it.
pub async fn assert_errors_are_observations(tool: &dyn Tool, bad_inputs: &[Value], cx: &Cx) {
    let name = tool.spec().name;
    for input in bad_inputs {
        let result = AssertUnwindSafe(tool.call(input.clone(), cx))
            .catch_unwind()
            .await;
        let output = match result {
            Ok(o) => o,
            Err(panic) => panic!(
                "tool `{name}` panicked on input {input}: {}",
                panic_message(&panic)
            ),
        };
        if !output.is_error {
            panic!(
                "tool `{name}`: input {input} should be an error observation, but it succeeded \
                 with: {}",
                output.content
            );
        }
        if output.content.trim().is_empty() {
            panic!("tool `{name}`: input {input} returned an error with an empty message");
        }
    }
}

fn panic_message(panic: &Box<dyn Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic)".to_string()
    }
}

/// A [`Cx`] like [`Cx::detached`] rooted at `workspace`, with no services.
pub fn test_cx(workspace: &Path) -> Cx {
    TestCx::new(workspace).build()
}

/// Builds a [`Cx`] for tests.
///
/// ```
/// use std::sync::Arc;
/// use ch_testkit::TestCx;
///
/// struct MyService;
/// let cx = TestCx::new("/tmp/workspace")
///     .service(Arc::new(MyService))
///     .build();
/// assert!(cx.services.get::<MyService>().is_some());
/// ```
pub struct TestCx {
    workspace: PathBuf,
    services: Services,
    executor: Option<Arc<dyn Executor>>,
    config: Config,
    tools: Vec<ToolSpec>,
}

impl TestCx {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            services: Services::default(),
            executor: None,
            config: Config::default(),
            tools: Vec::new(),
        }
    }

    /// Starts from a built registry: its services and tool specs.
    pub fn from_registry(registry: &Registry, workspace: impl Into<PathBuf>) -> Self {
        let mut t = Self::new(workspace);
        t.services = (*registry.services).clone();
        t.tools = registry.tool_specs();
        t
    }

    /// Adds a service. Panics if one of the same type is already there.
    pub fn service<T: Any + Send + Sync>(mut self, service: Arc<T>) -> Self {
        if !self.services.provide(service) {
            panic!(
                "TestCx: service {} provided twice",
                std::any::type_name::<T>()
            );
        }
        self
    }

    pub fn executor(mut self, executor: Arc<dyn Executor>) -> Self {
        self.executor = Some(executor);
        self
    }

    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Tool specs visible through `cx.tools` / `cx.has_tool`.
    pub fn tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    pub fn build(self) -> Cx {
        let mut cx = Cx::detached(self.workspace);
        cx.session_id = Arc::from("test");
        cx.services = Arc::new(self.services);
        cx.executor = self.executor;
        cx.config = Arc::new(self.config);
        cx.tools = Arc::new(self.tools);
        cx
    }
}

/// Registers `plugins` against `workspace` with the config parsed from `config_toml`.
/// Panics with the registration error if it fails.
pub fn build_registry(
    plugins: Vec<Arc<dyn Plugin>>,
    workspace: &Path,
    config_toml: &str,
) -> Registry {
    let config =
        Config::parse(config_toml).unwrap_or_else(|e| panic!("test config doesn't parse: {e:#}"));
    RegistryBuilder::build(plugins, &config, workspace)
        .unwrap_or_else(|e| panic!("plugin registration failed: {e:#}"))
}

/// The tool named `name` from `registry`; panics if it isn't registered.
pub fn registry_tool(registry: &Registry, name: &str) -> Arc<dyn Tool> {
    registry
        .tool(name)
        .unwrap_or_else(|| panic!("tool `{name}` is not registered"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_kernel::ToolOutput;
    use ch_kernel::reexports::async_trait;
    use serde_json::json;

    struct Good;

    #[async_trait]
    impl Tool for Good {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "good_tool".into(),
                description: "Does a thing.\nWhen: always.\nProtocol: call it.\nError modes: \
                              bad input.\nSide effects: none."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"x": {"type": "string"}},
                    "required": ["x"]
                }),
                read_only: true,
            }
        }
        async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
            match input.get("x").and_then(Value::as_str) {
                Some(x) => ToolOutput::ok(x),
                None => ToolOutput::error("`x` is required"),
            }
        }
    }

    struct WithSpec(ToolSpec);

    #[async_trait]
    impl Tool for WithSpec {
        fn spec(&self) -> ToolSpec {
            self.0.clone()
        }
        async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
            if input.is_null() {
                panic!("boom");
            }
            ToolOutput::ok("fine")
        }
    }

    fn spec_with(f: impl FnOnce(&mut ToolSpec)) -> WithSpec {
        let mut s = Good.spec();
        f(&mut s);
        WithSpec(s)
    }

    fn panics(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("should panic");
        panic_message(&err)
    }

    #[test]
    fn good_tool_conforms() {
        assert_tool_conforms(&Good);
    }

    #[test]
    fn bad_names() {
        for n in ["BadName", "bad-name", "bad__name", "_x", "x_", ""] {
            let t = spec_with(|s| s.name = n.into());
            let msg = panics(move || assert_tool_conforms(&t));
            assert!(msg.contains("snake_case"), "{msg}");
        }
    }

    #[test]
    fn bad_schemas() {
        let t = spec_with(|s| s.input_schema = json!({"type": "string"}));
        assert!(panics(move || assert_tool_conforms(&t)).contains("\"type\": \"object\""));
        let t = spec_with(|s| s.input_schema = json!({"type": "object"}));
        assert!(panics(move || assert_tool_conforms(&t)).contains("properties"));
        let t = spec_with(|s| {
            s.input_schema = json!({"type": "object", "properties": {}, "required": ["y"]})
        });
        assert!(panics(move || assert_tool_conforms(&t)).contains("required key `y`"));
    }

    #[test]
    fn bad_descriptions() {
        let t = spec_with(|s| s.description = "When: x. Protocol: y. Side effects: z.".into());
        assert!(panics(move || assert_tool_conforms(&t)).contains("Error modes:"));
        let t = spec_with(|s| {
            s.description = "Protocol: y. When: x. Error modes: e. Side effects: z.".into()
        });
        assert!(panics(move || assert_tool_conforms(&t)).contains("order"));
    }

    #[tokio::test]
    async fn errors_are_observations() {
        let cx = test_cx(Path::new("/tmp"));
        assert_errors_are_observations(&Good, &[json!({}), json!({"x": 1})], &cx).await;
    }

    #[tokio::test]
    async fn success_is_reported() {
        let cx = test_cx(Path::new("/tmp"));
        let r = AssertUnwindSafe(assert_errors_are_observations(
            &Good,
            &[json!({"x": "ok"})],
            &cx,
        ))
        .catch_unwind()
        .await;
        assert!(panic_message(&r.unwrap_err()).contains("should be an error"));
    }

    #[tokio::test]
    async fn panics_are_reported() {
        let cx = test_cx(Path::new("/tmp"));
        let t = WithSpec(Good.spec());
        let r = AssertUnwindSafe(assert_errors_are_observations(&t, &[Value::Null], &cx))
            .catch_unwind()
            .await;
        assert!(panic_message(&r.unwrap_err()).contains("panicked on input null"));
    }

    #[test]
    fn test_cx_builder() {
        struct Marker;
        let cx = TestCx::new("/w").service(Arc::new(Marker)).build();
        assert!(cx.services.get::<Marker>().is_some());
        assert_eq!(cx.workspace(), Path::new("/w"));
        assert!(cx.executor.is_none());
    }
}
