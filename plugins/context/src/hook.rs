//! The `pre_tool` hook that refuses a call copying a marker of trimmed input.

use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Hook, HookDecision, HookEvent, HookPoint};

use crate::stub::has_marker;

/// Why such a call is refused.
pub const REFUSED: &str = "This call's input holds a placeholder for text that was removed from an \
                           earlier call to save context, not the text itself. Send the full text \
                           instead: read the file again, or write it out.";

/// Refuses a call whose input has a string that is an elision marker: the model copied
/// a shortened input instead of the text it stands for.
pub struct MarkerHook;

#[async_trait]
impl Hook for MarkerHook {
    fn id(&self) -> &str {
        crate::ID
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PreTool]
    }

    async fn on(&self, event: &HookEvent, _cx: &Cx) -> HookDecision {
        match event {
            HookEvent::PreTool { call, .. } if has_marker(&call.input) => {
                HookDecision::deny(REFUSED)
            }
            _ => HookDecision::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use ch_kernel::ToolCall;
    use serde_json::{Value, json};

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
        let cx = Cx::detached("/ws");
        let marker = "[elided from an earlier call to save context: 81,220 characters]";
        let copied = pre_tool(json!({"path": "a.txt", "content": marker}));
        assert_eq!(
            MarkerHook.on(&copied, &cx).await,
            HookDecision::deny(REFUSED)
        );
        let mention = pre_tool(json!({"path": "doc.md", "content": format!("A stub: {marker}")}));
        assert_eq!(MarkerHook.on(&mention, &cx).await, HookDecision::Continue);
    }
}
