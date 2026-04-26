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
    Telegram {
        chat_id: i64,
        message_id: i32,
    },
    Slack {
        channel_id: String,
        thread_ts: String,
    },
    Gateway {
        connection_id: String,
    },
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
    async fn send_approval_request(&self, request: &ApprovalRequest) -> Result<PendingApproval>;

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
    async fn send_approval_request(&self, request: &ApprovalRequest) -> Result<PendingApproval> {
        let prompt = super::format_approval_prompt(request);
        eprintln!();
        for line in prompt.lines() {
            eprintln!("   {line}");
        }
        eprint!("   [Y]es / [N]o / [A]lways for {}: ", request.tool_name);
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

// ── Shared helpers ──────────────────────────────────────────────────

/// Build a Telegram Bot API URL. Shared by `TelegramApprovalAdapter` and
/// `TelegramChannel` (which has its own copy for historical reasons).
fn telegram_api_url(api_base: &str, bot_token: &str, method: &str) -> String {
    format!("{api_base}/bot{bot_token}/{method}")
}

// ── Telegram Adapter ────────────────────────────────────────────────

/// Approval adapter for Telegram — sends a message and polls for reply-to
/// correlation.
pub struct TelegramApprovalAdapter {
    client: reqwest::Client,
    api_base: String,
    bot_token: String,
    chat_id: String,
    thread_id: Option<String>,
}

impl TelegramApprovalAdapter {
    pub fn new(
        client: reqwest::Client,
        api_base: impl Into<String>,
        bot_token: impl Into<String>,
        chat_id: impl Into<String>,
        thread_id: Option<String>,
    ) -> Self {
        Self {
            client,
            api_base: api_base.into(),
            bot_token: bot_token.into(),
            chat_id: chat_id.into(),
            thread_id,
        }
    }
}

#[async_trait]
impl ChannelApprovalAdapter for TelegramApprovalAdapter {
    async fn send_approval_request(&self, request: &ApprovalRequest) -> Result<PendingApproval> {
        let mut text = super::format_approval_prompt(request);
        text.push_str("\n\nReply to this message with:\n• y — approve once\n• n — deny\n• a — always approve this tool");

        let mut body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "reply_markup": { "force_reply": true, "selective": true }
        });
        if let Some(ref tid) = self.thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.clone());
        }

        let resp = self
            .client
            .post(telegram_api_url(
                &self.api_base,
                &self.bot_token,
                "sendMessage",
            ))
            .json(&body)
            .send()
            .await?;
        let json: serde_json::Value = resp.json().await?;
        let message_id_raw = json["result"]["message_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("missing message_id in sendMessage response"))?;
        let message_id = i32::try_from(message_id_raw)
            .map_err(|_| anyhow::anyhow!("message_id {message_id_raw} exceeds i32 range"))?;

        let chat_id = json["result"]["chat"]["id"]
            .as_i64()
            .unwrap_or_else(|| self.chat_id.parse::<i64>().unwrap_or(0));

        Ok(PendingApproval {
            request_id: request.request_id.clone(),
            platform_ref: PlatformRef::Telegram {
                chat_id,
                message_id,
            },
        })
    }

    async fn receive_approval_response(
        &self,
        pending: &PendingApproval,
    ) -> Result<ApprovalResponse> {
        let (expected_chat_id, expected_msg_id) = match &pending.platform_ref {
            PlatformRef::Telegram {
                chat_id,
                message_id,
            } => (*chat_id, *message_id),
            _ => anyhow::bail!("TelegramApprovalAdapter got non-Telegram PlatformRef"),
        };

        let mut offset: i64 = 0;
        loop {
            let resp = self
                .client
                .post(telegram_api_url(
                    &self.api_base,
                    &self.bot_token,
                    "getUpdates",
                ))
                .json(&serde_json::json!({
                    "offset": offset,
                    "timeout": 5,
                    "allowed_updates": ["message"]
                }))
                .send()
                .await?;
            let json: serde_json::Value = resp.json().await?;

            if let Some(updates) = json["result"].as_array() {
                for update in updates {
                    if let Some(uid) = update["update_id"].as_i64() {
                        offset = uid + 1;
                    }
                    let msg = &update["message"];
                    let reply_to = &msg["reply_to_message"];
                    let reply_msg_id = reply_to["message_id"].as_i64().unwrap_or(-1);
                    let chat_id = msg["chat"]["id"].as_i64().unwrap_or(0);

                    if reply_msg_id == i64::from(expected_msg_id) && chat_id == expected_chat_id {
                        let text = msg["text"].as_str().unwrap_or("");
                        return Ok(super::parse_approval_input(text));
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    fn channel_name(&self) -> &str {
        "telegram"
    }
}

// ── Slack Adapter ───────────────────────────────────────────────────

/// Approval adapter for Slack — sends a thread reply and polls for responses.
pub struct SlackApprovalAdapter {
    client: reqwest::Client,
    bot_token: String,
    channel_id: String,
    thread_ts: Option<String>,
}

impl SlackApprovalAdapter {
    pub fn new(
        bot_token: impl Into<String>,
        channel_id: impl Into<String>,
        thread_ts: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            bot_token: bot_token.into(),
            channel_id: channel_id.into(),
            thread_ts,
        }
    }
}

#[async_trait]
impl ChannelApprovalAdapter for SlackApprovalAdapter {
    async fn send_approval_request(&self, request: &ApprovalRequest) -> Result<PendingApproval> {
        let mut text = super::format_approval_prompt(request);
        text.push_str("\n\nReply in this thread with:\n• y — approve once\n• n — deny\n• a — always approve this tool");

        let mut body = serde_json::json!({
            "channel": self.channel_id,
            "text": text,
        });
        if let Some(ref ts) = self.thread_ts {
            body["thread_ts"] = serde_json::Value::String(ts.clone());
        }

        let resp = self
            .client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await?;
        let json: serde_json::Value = resp.json().await?;

        let ts = json["ts"]
            .as_str()
            .or_else(|| json["message"]["ts"].as_str())
            .ok_or_else(|| anyhow::anyhow!("missing ts in Slack postMessage response"))?
            .to_string();

        Ok(PendingApproval {
            request_id: request.request_id.clone(),
            platform_ref: PlatformRef::Slack {
                channel_id: self.channel_id.clone(),
                thread_ts: ts,
            },
        })
    }

    async fn receive_approval_response(
        &self,
        pending: &PendingApproval,
    ) -> Result<ApprovalResponse> {
        let (channel_id, thread_ts) = match &pending.platform_ref {
            PlatformRef::Slack {
                channel_id,
                thread_ts,
            } => (channel_id.clone(), thread_ts.clone()),
            _ => anyhow::bail!("SlackApprovalAdapter got non-Slack PlatformRef"),
        };

        loop {
            let resp = self
                .client
                .get("https://slack.com/api/conversations.replies")
                .bearer_auth(&self.bot_token)
                .query(&[
                    ("channel", channel_id.as_str()),
                    ("ts", thread_ts.as_str()),
                    ("limit", "10"),
                ])
                .send()
                .await?;
            let json: serde_json::Value = resp.json().await?;

            if let Some(messages) = json["messages"].as_array() {
                for msg in messages {
                    let msg_ts = msg["ts"].as_str().unwrap_or("");
                    // Only consider replies after the approval message
                    if msg_ts > thread_ts.as_str() {
                        let text = msg["text"].as_str().unwrap_or("");
                        return Ok(super::parse_approval_input(text));
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }

    fn channel_name(&self) -> &str {
        "slack"
    }
}

// ── Gateway (WebSocket) Adapter ─────────────────────────────────────

/// Approval adapter for the WebSocket gateway — sends JSON frames and
/// waits for a matching `approval_response` frame via a oneshot channel.
pub struct GatewayApprovalAdapter {
    frame_tx: tokio::sync::mpsc::Sender<String>,
    pending_responses: std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<String, tokio::sync::oneshot::Sender<ApprovalResponse>>,
        >,
    >,
    response_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<ApprovalResponse>>>,
}

impl GatewayApprovalAdapter {
    pub fn new(
        frame_tx: tokio::sync::mpsc::Sender<String>,
        pending_responses: std::sync::Arc<
            tokio::sync::Mutex<
                std::collections::HashMap<String, tokio::sync::oneshot::Sender<ApprovalResponse>>,
            >,
        >,
    ) -> Self {
        Self {
            frame_tx,
            pending_responses,
            response_rx: tokio::sync::Mutex::new(None),
        }
    }

    /// Resolve an incoming `approval_response` frame from the WebSocket client.
    ///
    /// Called by the WebSocket receive loop when it sees a frame with
    /// `{"type":"approval_response", "request_id":"...", "decision":"..."}`.
    pub async fn resolve_approval_response(
        pending: &std::sync::Arc<
            tokio::sync::Mutex<
                std::collections::HashMap<String, tokio::sync::oneshot::Sender<ApprovalResponse>>,
            >,
        >,
        request_id: &str,
        decision: &str,
    ) -> bool {
        let mut map = pending.lock().await;
        if let Some(tx) = map.remove(request_id) {
            let response = super::parse_approval_input(decision);
            tx.send(response).is_ok()
        } else {
            false
        }
    }
}

#[async_trait]
impl ChannelApprovalAdapter for GatewayApprovalAdapter {
    async fn send_approval_request(&self, request: &ApprovalRequest) -> Result<PendingApproval> {
        let frame = serde_json::json!({
            "type": "approval_request",
            "request_id": request.request_id,
            "tool_name": request.tool_name,
            "arguments": request.arguments,
            "risk_level": request.risk_level,
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut map = self.pending_responses.lock().await;
            map.insert(request.request_id.clone(), tx);
        }
        *self.response_rx.lock().await = Some(rx);

        self.frame_tx
            .send(frame.to_string())
            .await
            .map_err(|e| anyhow::anyhow!("failed to send approval frame: {e}"))?;

        Ok(PendingApproval {
            request_id: request.request_id.clone(),
            platform_ref: PlatformRef::Gateway {
                connection_id: request.request_id.clone(),
            },
        })
    }

    async fn receive_approval_response(
        &self,
        _pending: &PendingApproval,
    ) -> Result<ApprovalResponse> {
        let rx = self
            .response_rx
            .lock()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("no pending approval response receiver"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("approval response channel closed"))
    }

    fn channel_name(&self) -> &str {
        "gateway"
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
            "test",
        );

        let pending = adapter.send_approval_request(&request).await.unwrap();
        assert_eq!(pending.request_id, request.request_id);

        let response = adapter.receive_approval_response(&pending).await.unwrap();
        assert_eq!(response, ApprovalResponse::Always);
    }

    #[tokio::test]
    async fn failing_adapter_returns_error() {
        let adapter = FailingApprovalAdapter;
        let request = ApprovalRequest::new("file_write".into(), serde_json::json!({}), "test");

        let result = adapter.send_approval_request(&request).await;
        assert!(result.is_err());
    }
}
