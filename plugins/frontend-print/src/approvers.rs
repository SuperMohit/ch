//! Non-interactive approvers.

use async_trait::async_trait;
use ch_kernel::{Approval, ApprovalRequest, Approver};

/// Denies every call that needs approval, telling the model how the user could allow it.
pub struct AutoDeny;

#[async_trait]
impl Approver for AutoDeny {
    fn id(&self) -> &str {
        "auto-deny"
    }

    async fn ask(&self, request: &ApprovalRequest) -> Approval {
        Approval::Deny {
            reason: Some(deny_reason(request)),
        }
    }
}

/// Allows every call that needs approval. The safety plugin's deny list still applies,
/// because denied calls never reach an approver.
pub struct AutoAllow;

#[async_trait]
impl Approver for AutoAllow {
    fn id(&self) -> &str {
        "auto-allow"
    }

    async fn ask(&self, _request: &ApprovalRequest) -> Approval {
        Approval::Allow
    }
}

fn deny_reason(request: &ApprovalRequest) -> String {
    let rule = request.key.clone().unwrap_or_else(|| request.tool.clone());
    format!(
        "this is a non-interactive run and nobody can approve `{}` ({}). The user can allow \
         it by re-running with `--allow '{rule}'`",
        request.tool, request.reason
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(key: Option<&str>) -> ApprovalRequest {
        ApprovalRequest {
            tool: "bash".into(),
            input: json!({"command": "git commit -m x"}),
            summary: "git commit -m x".into(),
            reason: "bash command is not read-only".into(),
            key: key.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn auto_deny_mentions_the_allow_rule() {
        let Approval::Deny { reason: Some(r) } = AutoDeny.ask(&request(Some("bash:git *"))).await
        else {
            panic!("expected a deny with a reason");
        };
        assert!(r.contains("--allow 'bash:git *'"), "{r}");
        let Approval::Deny { reason: Some(r) } = AutoDeny.ask(&request(None)).await else {
            panic!("expected a deny with a reason");
        };
        assert!(r.contains("--allow 'bash'"), "{r}");
    }

    #[tokio::test]
    async fn auto_allow_allows() {
        assert_eq!(AutoAllow.ask(&request(None)).await, Approval::Allow);
    }
}
