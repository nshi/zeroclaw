//! Channel-agnostic approval adapter trait and platform-specific implementations.

use anyhow::Result;
use async_trait::async_trait;

use super::{ApprovalRequest, ApprovalResponse};

// ── Platform Correlation ────────────────────────────────────────────

/// Platform-specific correlation data for matching approval responses
/// to their originating requests.
#[derive(Debug, Clone)]
pub enum PlatformRef {
    Cli,
    Telegram { chat_id: i64, message_id: i32 },
    Slack { channel_id: String, thread_ts: String },
    Gateway { connection_id: String },
}

/// A pending approval request awaiting a user response.
///
/// Carries the `request_id` for correlation and platform-specific
/// state needed by the adapter to match the response.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: String,
    pub platform_ref: PlatformRef,
}

// ── Trait ────────────────────────────────────────────────────────────

/// Channel-agnostic interface for prompting users to approve tool calls.
///
/// Each channel (CLI, Telegram, Slack, Gateway) implements this trait using
/// its platform-native messaging primitives. The `ApprovalManager` delegates
/// to the adapter when a tool call requires user approval.
#[async_trait]
pub trait ChannelApprovalAdapter: Send + Sync {
    /// Send an approval prompt to the user and return a handle for
    /// awaiting their response.
    ///
    /// # Errors
    /// Returns an error if the prompt could not be delivered. The caller
    /// should treat delivery failures as a denial.
    async fn send_approval_request(
        &self,
        request: &ApprovalRequest,
    ) -> Result<PendingApproval>;

    /// Wait for the user's response to a previously sent approval request.
    ///
    /// This method blocks (async) until the user responds or the caller
    /// cancels (via timeout or cancellation token). It does NOT implement
    /// timeout internally — the caller wraps this in `tokio::time::timeout`.
    ///
    /// # Errors
    /// Returns an error if the channel disconnected or an unrecoverable
    /// error occurred. The caller should treat this as a denial.
    async fn receive_approval_response(
        &self,
        pending: &PendingApproval,
    ) -> Result<ApprovalResponse>;

    /// Human-readable name of this adapter's channel (for audit logging).
    fn channel_name(&self) -> &str;
}

// ── CLI Adapter ─────────────────────────────────────────────────────

/// Approval adapter for CLI (stdin/stderr) interaction.
///
/// Prompts the user on stderr and reads their response from stdin via
/// `spawn_blocking` to avoid blocking the tokio executor.
pub struct CliApprovalAdapter;

#[async_trait]
impl ChannelApprovalAdapter for CliApprovalAdapter {
    async fn send_approval_request(
        &self,
        request: &ApprovalRequest,
    ) -> Result<PendingApproval> {
        let summary = super::summarize_args(&request.arguments);
        let tool_name = request.tool_name.clone();
        let risk = request.risk_level;

        // Write the prompt to stderr (non-blocking for the runtime).
        eprintln!();
        eprintln!("🔧 Agent wants to execute: {tool_name}");
        eprintln!("   {summary}");
        if let Some(ref r) = risk {
            eprintln!("   Risk: {r:?}");
        }
        eprint!("   [Y]es / [N]o / [A]lways for {tool_name}: ");
        let _ = std::io::Write::flush(&mut std::io::stderr());

        Ok(PendingApproval {
            request_id: request.request_id.clone(),
            platform_ref: PlatformRef::Cli,
        })
    }

    async fn receive_approval_response(
        &self,
        _pending: &PendingApproval,
    ) -> Result<ApprovalResponse> {
        // stdin is blocking I/O — must not block the tokio executor.
        let line = tokio::task::spawn_blocking(|| {
            let stdin = std::io::stdin();
            let mut buf = String::new();
            std::io::BufRead::read_line(&mut stdin.lock(), &mut buf)?;
            Ok::<_, std::io::Error>(buf)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))??;

        Ok(super::parse_approval_input(&line))
    }

    fn channel_name(&self) -> &str {
        "cli"
    }
}

// ── Test fixtures ───────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A mock adapter that returns a configurable response.
    pub struct MockApprovalAdapter {
        response: ApprovalResponse,
    }

    impl MockApprovalAdapter {
        pub fn new(response: ApprovalResponse) -> Self {
            Self { response }
        }
    }

    #[async_trait]
    impl ChannelApprovalAdapter for MockApprovalAdapter {
        async fn send_approval_request(
            &self,
            request: &ApprovalRequest,
        ) -> Result<PendingApproval> {
            Ok(PendingApproval {
                request_id: request.request_id.clone(),
                platform_ref: PlatformRef::Cli,
            })
        }

        async fn receive_approval_response(
            &self,
            _pending: &PendingApproval,
        ) -> Result<ApprovalResponse> {
            Ok(self.response)
        }

        fn channel_name(&self) -> &str {
            "mock"
        }
    }

    /// An adapter that never responds (hangs forever). Used to test timeout.
    pub struct HangingApprovalAdapter;

    #[async_trait]
    impl ChannelApprovalAdapter for HangingApprovalAdapter {
        async fn send_approval_request(
            &self,
            request: &ApprovalRequest,
        ) -> Result<PendingApproval> {
            Ok(PendingApproval {
                request_id: request.request_id.clone(),
                platform_ref: PlatformRef::Cli,
            })
        }

        async fn receive_approval_response(
            &self,
            _pending: &PendingApproval,
        ) -> Result<ApprovalResponse> {
            // Never resolves — the caller's timeout will fire.
            std::future::pending().await
        }

        fn channel_name(&self) -> &str {
            "hanging"
        }
    }

    /// An adapter that simulates a delivery failure.
    pub struct FailingApprovalAdapter;

    #[async_trait]
    impl ChannelApprovalAdapter for FailingApprovalAdapter {
        async fn send_approval_request(
            &self,
            _request: &ApprovalRequest,
        ) -> Result<PendingApproval> {
            Err(anyhow::anyhow!("channel disconnected"))
        }

        async fn receive_approval_response(
            &self,
            _pending: &PendingApproval,
        ) -> Result<ApprovalResponse> {
            Err(anyhow::anyhow!("channel disconnected"))
        }

        fn channel_name(&self) -> &str {
            "failing"
        }
    }

    #[test]
    fn mock_adapter_constructs() {
        let adapter = MockApprovalAdapter::new(ApprovalResponse::Yes);
        assert_eq!(adapter.channel_name(), "mock");
    }

    #[tokio::test]
    async fn mock_adapter_send_receive_lifecycle() {
        let adapter = MockApprovalAdapter::new(ApprovalResponse::Always);
        let request = ApprovalRequest::new(
            "file_write".into(),
            serde_json::json!({"path": "test.txt"}),
        );

        let pending = adapter.send_approval_request(&request).await.unwrap();
        assert_eq!(pending.request_id, request.request_id);

        let response = adapter.receive_approval_response(&pending).await.unwrap();
        assert_eq!(response, ApprovalResponse::Always);
    }

    #[tokio::test]
    async fn failing_adapter_returns_error() {
        let adapter = FailingApprovalAdapter;
        let request = ApprovalRequest::new(
            "file_write".into(),
            serde_json::json!({}),
        );

        let result = adapter.send_approval_request(&request).await;
        assert!(result.is_err());
    }
}
