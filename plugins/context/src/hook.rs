//! The `pre_tool` hook that refuses a call copying what a trim wrote.

use std::sync::Arc;

use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Hook, HookDecision, HookEvent, HookPoint};
use serde_json::Value;

use crate::strategy::Shared;
use crate::stub::is_marker;

/// Why such a call is refused.
pub const REFUSED: &str = "This call's input holds a placeholder for text that was removed from an \
                           earlier call to save context, not the text itself. Send the full text \
                           instead: read the file again, or write it out.";

/// Refuses a call when one of its input strings is an input marker, or contains a stub
/// or marker this session's trims wrote: the model copied a shortened input or output
/// instead of the text it stands for. A string that only mentions the marker's wording,
/// such as this source file, is not refused.
pub struct MarkerHook {
    shared: Arc<Shared>,
}

impl MarkerHook {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl Hook for MarkerHook {
    fn id(&self) -> &str {
        crate::ID
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PreTool]
    }

    async fn on(&self, event: &HookEvent, cx: &Cx) -> HookDecision {
        let HookEvent::PreTool { call, .. } = event else {
            return HookDecision::Continue;
        };
        let guard = self.shared.lock();
        let written = Shared::of(&guard, &cx.session_id).map(|s| &s.written);
        let copied = |s: &str| {
            is_marker(s) || written.is_some_and(|w| w.iter().any(|t| s.contains(t.as_str())))
        };
        if any_string(&call.input, &copied) {
            HookDecision::deny(REFUSED)
        } else {
            HookDecision::Continue
        }
    }
}

fn any_string(value: &Value, test: &dyn Fn(&str) -> bool) -> bool {
    match value {
        Value::String(s) => test(s),
        Value::Array(items) => items.iter().any(|v| any_string(v, test)),
        Value::Object(map) => map.values().any(|v| any_string(v, test)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use ch_kernel::ToolCall;
    use serde_json::json;

    use super::*;

    fn pre_tool(input: Value) -> HookEvent {
        HookEvent::PreTool {
            call: ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input,
            },
            read_only: false,
        }
    }

    #[tokio::test]
    async fn a_copied_marker_is_refused_and_a_mention_is_not() {
        let hook = MarkerHook::new(Arc::default());
        let cx = Cx::detached("/ws");
        let marker = "[elided from an earlier call to save context: 81,220 characters]";
        for copied in [marker.to_string(), format!("{marker}\n")] {
            let call = pre_tool(json!({"path": "a.txt", "content": copied}));
            assert_eq!(hook.on(&call, &cx).await, HookDecision::deny(REFUSED));
        }
        let mention = pre_tool(json!({"path": "doc.md", "content": format!("A stub: {marker}")}));
        assert_eq!(hook.on(&mention, &cx).await, HookDecision::Continue);
    }
}
