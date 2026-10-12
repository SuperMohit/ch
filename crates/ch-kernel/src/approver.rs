//! Approvers answer `Ask` decisions (interactive prompt, auto-deny, auto-allow).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool: String,
    pub input: Value,
    /// Human summary of the call (`Tool::summarize`).
    pub summary: String,
    /// Why approval is needed (from the hook).
    pub reason: String,
    /// What "always" would cover; `None` means only once is possible.
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "approval", rename_all = "snake_case")]
pub enum Approval {
    Allow,
    /// Allow, and don't ask again this session for the same `key`.
    AllowAlways,
    Deny {
        #[serde(default)]
        reason: Option<String>,
    },
}

#[async_trait]
pub trait Approver: Send + Sync {
    fn id(&self) -> &str;
    async fn ask(&self, request: &ApprovalRequest) -> Approval;
}
