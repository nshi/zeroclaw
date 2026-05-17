use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// A message received from or sent to a channel
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub id: String,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
    pub channel: String,
    pub timestamp: u64,
    /// Platform thread identifier (e.g. Slack `ts`, Discord thread ID).
    /// When set, replies should be posted as threaded responses.
    pub thread_ts: Option<String>,
    /// Thread scope identifier for interruption/cancellation grouping.
    /// Distinct from `thread_ts` (reply anchor): this is `Some` only when the message
    /// is genuinely inside a reply thread and should be isolated from other threads.
    /// `None` means top-level — scope is sender+channel only.
    pub interruption_scope_id: Option<String>,
    /// Media attachments (audio, images, video) for the media pipeline.
    /// Channels populate this when they receive media alongside a text message.
    /// Defaults to empty — existing channels are unaffected.
    pub attachments: Vec<super::media_pipeline::MediaAttachment>,
}

/// Message to send through a channel
#[derive(Debug, Clone)]
pub struct SendMessage {
    pub content: String,
    pub recipient: String,
    pub subject: Option<String>,
    /// Platform thread identifier for threaded replies (e.g. Slack `thread_ts`).
    pub thread_ts: Option<String>,
    /// Optional cancellation token for interruptible delivery (e.g. multi-message mode).
    pub cancellation_token: Option<CancellationToken>,
}

impl SendMessage {
    /// Create a new message with content and recipient
    pub fn new(content: impl Into<String>, recipient: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            recipient: recipient.into(),
            subject: None,
            thread_ts: None,
            cancellation_token: None,
        }
    }

    /// Create a new message with content, recipient, and subject
    pub fn with_subject(
        content: impl Into<String>,
        recipient: impl Into<String>,
        subject: impl Into<String>,
    ) -> Self {
        Self {
            content: content.into(),
            recipient: recipient.into(),
            subject: Some(subject.into()),
            thread_ts: None,
            cancellation_token: None,
        }
    }

    /// Set the thread identifier for threaded replies.
    pub fn in_thread(mut self, thread_ts: Option<String>) -> Self {
        self.thread_ts = thread_ts;
        self
    }

    /// Attach a cancellation token for interruptible delivery.
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }
}

/// Core channel trait — implement for any messaging platform
#[async_trait]
pub trait Channel: Send + Sync {
    /// Human-readable channel name
    fn name(&self) -> &str;

    /// Send a message through this channel
    async fn send(&self, message: &SendMessage) -> anyhow::Result<()>;

    /// Start listening for incoming messages (long-running)
    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()>;

    /// Check if channel is healthy
    async fn health_check(&self) -> bool {
        true
    }

    /// Signal that the bot is processing a response (e.g. "typing" indicator).
    /// Implementations should repeat the indicator as needed for their platform.
    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Stop any active typing indicator.
    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether this channel supports progressive message updates via draft edits.
    fn supports_draft_updates(&self) -> bool {
        false
    }

    /// Whether this channel supports multi-message streaming delivery, where
    /// the response is sent as multiple separate messages at paragraph
    /// boundaries as tokens arrive from the provider.
    fn supports_multi_message_streaming(&self) -> bool {
        false
    }

    /// Minimum delay (ms) between sending each paragraph in multi-message mode.
    /// Channels should override this to avoid platform rate limits.
    fn multi_message_delay_ms(&self) -> u64 {
        800
    }

    /// Send an initial draft message. Returns a platform-specific message ID for later edits.
    async fn send_draft(&self, _message: &SendMessage) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Update a previously sent draft message with new accumulated content.
    async fn update_draft(
        &self,
        _recipient: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Show a progress/status update (e.g. tool execution status).
    /// Channels can display this in a status bar rather than in the message body.
    /// Default: no-op (progress is ignored).
    async fn update_draft_progress(
        &self,
        _recipient: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Finalize a draft with the complete response (e.g. apply Markdown formatting).
    async fn finalize_draft(
        &self,
        _recipient: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Cancel and remove a previously sent draft message if the channel supports it.
    async fn cancel_draft(&self, _recipient: &str, _message_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Add a reaction (emoji) to a message.
    ///
    /// `channel_id` is the platform channel/conversation identifier (e.g. Discord channel ID).
    /// `message_id` is the platform-scoped message identifier (e.g. `discord_<snowflake>`).
    /// `emoji` is the Unicode emoji to react with (e.g. "👀", "✅").
    async fn add_reaction(
        &self,
        _channel_id: &str,
        _message_id: &str,
        _emoji: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Remove a reaction (emoji) from a message previously added by this bot.
    async fn remove_reaction(
        &self,
        _channel_id: &str,
        _message_id: &str,
        _emoji: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Pin a message in the channel.
    async fn pin_message(&self, _channel_id: &str, _message_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Unpin a previously pinned message.
    async fn unpin_message(&self, _channel_id: &str, _message_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Redact (delete) a message from the channel.
    ///
    /// `channel_id` is the platform channel/conversation identifier.
    /// `message_id` is the platform-scoped message identifier.
    /// `reason` is an optional reason for the redaction (may be visible in audit logs).
    async fn redact_message(
        &self,
        _channel_id: &str,
        _message_id: &str,
        _reason: Option<String>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Create a channel-specific approval adapter for interactive approval prompting.
    ///
    /// The `msg` provides context for the adapter (chat_id, thread_ts, etc.).
    /// Returns `None` if this channel does not support interactive approval.
    fn create_approval_adapter(
        &self,
        _msg: &ChannelMessage,
    ) -> Option<Box<dyn crate::approval::ChannelApprovalAdapter>> {
        None
    }

    /// Register a one-shot intercept for the next inbound message whose
    /// `reply_target` matches the supplied value. The intercepted message is
    /// diverted away from the normal `listen()` pipeline so it won't be
    /// processed as a new turn.
    ///
    /// Exists so tools like `ask_user` don't have to start a second
    /// `listen()` task that contends with the supervised listener for the
    /// platform's single in-flight long-poll slot (Telegram returns HTTP 409
    /// when two getUpdates calls overlap).
    async fn await_next_reply(
        &self,
        _reply_target: &str,
        _timeout: std::time::Duration,
    ) -> NextReply {
        NextReply::Unsupported
    }
}

/// Result of `Channel::await_next_reply`. The `Unsupported` variant lets
/// callers fall back to spawning their own listener for channels (e.g. test
/// stubs, CLI) that can't tap the supervised listener.
#[derive(Debug)]
pub enum NextReply {
    Received(ChannelMessage),
    Timeout,
    Unsupported,
}

/// Shared one-shot intercept registry used by channel impls (Telegram,
/// Slack) to divert specific inbound messages from the supervised listener
/// to an awaiting tool (e.g. `ask_user`). Each entry consumes itself when
/// matched; cancelled awaiters drop-clean their own entry to avoid leaks.
#[derive(Debug, Default)]
pub struct InboxInterceptor {
    pending: std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<ChannelMessage>>,
    >,
}

impl InboxInterceptor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pop a pending intercept matching `reply_target`. Listen-loop dispatch
    /// calls this before forwarding a message to the main mpsc.
    pub fn take(
        &self,
        reply_target: &str,
    ) -> Option<tokio::sync::oneshot::Sender<ChannelMessage>> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(reply_target)
    }

    /// Register a one-shot intercept and wait up to `timeout` for a match.
    /// Cleans up the entry on timeout *and* on caller cancellation (the
    /// returned future's `Drop` removes the key) so a stale `Sender` can't
    /// swallow a later message bound for the supervisor.
    pub async fn await_reply(
        &self,
        reply_target: &str,
        timeout: std::time::Duration,
    ) -> NextReply {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(reply_target.to_string(), tx);

        // RAII guard: removes our entry from the map if this future is
        // dropped before completing (caller cancellation) or returns on
        // timeout. On `Received` we leave cleanup to the listen loop's
        // `take`, which already consumed the entry.
        struct CleanupGuard<'a> {
            interceptor: &'a InboxInterceptor,
            key: &'a str,
            armed: bool,
        }
        impl Drop for CleanupGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    self.interceptor
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(self.key);
                }
            }
        }
        let mut guard = CleanupGuard {
            interceptor: self,
            key: reply_target,
            armed: true,
        };

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(msg)) => {
                guard.armed = false; // listen loop already removed the entry
                NextReply::Received(msg)
            }
            _ => NextReply::Timeout, // guard's Drop removes the entry
        }
    }

    /// Whether the registry currently holds any pending intercepts.
    /// Test-only utility — production code shouldn't depend on this.
    pub fn is_empty(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyChannel;

    #[async_trait]
    impl Channel for DummyChannel {
        fn name(&self) -> &str {
            "dummy"
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(
            &self,
            tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            tx.send(ChannelMessage {
                id: "1".into(),
                sender: "tester".into(),
                reply_target: "tester".into(),
                content: "hello".into(),
                channel: "dummy".into(),
                timestamp: 123,
                thread_ts: None,
                interruption_scope_id: None,
                attachments: vec![],
            })
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
    }

    #[tokio::test]
    async fn inbox_interceptor_cleans_up_on_caller_cancel() {
        let interceptor = std::sync::Arc::new(InboxInterceptor::new());
        let interceptor_clone = interceptor.clone();

        // Spawn an awaiter then drop the handle before it resolves.
        let handle = tokio::spawn(async move {
            interceptor_clone
                .await_reply("dropped", std::time::Duration::from_secs(60))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!interceptor.is_empty(), "intercept should be registered");
        handle.abort();
        // Give the runtime a tick to actually run the future's Drop.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            interceptor.is_empty(),
            "abort/cancellation must remove the orphaned intercept"
        );
    }

    #[tokio::test]
    async fn inbox_interceptor_received_clears_entry() {
        let interceptor = std::sync::Arc::new(InboxInterceptor::new());
        let interceptor_for_send = interceptor.clone();

        let awaiter = tokio::spawn(async move {
            interceptor
                .await_reply("k", std::time::Duration::from_secs(2))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let sender = interceptor_for_send.take("k").expect("registered");
        sender
            .send(ChannelMessage {
                id: "1".into(),
                sender: "u".into(),
                reply_target: "k".into(),
                content: "hi".into(),
                channel: "test".into(),
                timestamp: 0,
                thread_ts: None,
                interruption_scope_id: None,
                attachments: vec![],
            })
            .unwrap();

        assert!(matches!(awaiter.await.unwrap(), NextReply::Received(_)));
        assert!(interceptor_for_send.is_empty());
    }

    #[test]
    fn channel_message_clone_preserves_fields() {
        let message = ChannelMessage {
            id: "42".into(),
            sender: "alice".into(),
            reply_target: "alice".into(),
            content: "ping".into(),
            channel: "dummy".into(),
            timestamp: 999,
            thread_ts: None,
            interruption_scope_id: None,
            attachments: vec![],
        };

        let cloned = message.clone();
        assert_eq!(cloned.id, "42");
        assert_eq!(cloned.sender, "alice");
        assert_eq!(cloned.reply_target, "alice");
        assert_eq!(cloned.content, "ping");
        assert_eq!(cloned.channel, "dummy");
        assert_eq!(cloned.timestamp, 999);
    }

    #[tokio::test]
    async fn default_trait_methods_return_success() {
        let channel = DummyChannel;

        assert!(channel.health_check().await);
        assert!(channel.start_typing("bob").await.is_ok());
        assert!(channel.stop_typing("bob").await.is_ok());
        assert!(
            channel
                .send(&SendMessage::new("hello", "bob"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn default_reaction_methods_return_success() {
        let channel = DummyChannel;

        assert!(
            channel
                .add_reaction("chan_1", "msg_1", "\u{1F440}")
                .await
                .is_ok()
        );
        assert!(
            channel
                .remove_reaction("chan_1", "msg_1", "\u{1F440}")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn default_draft_methods_return_success() {
        let channel = DummyChannel;

        assert!(!channel.supports_draft_updates());
        assert!(
            channel
                .send_draft(&SendMessage::new("draft", "bob"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(channel.update_draft("bob", "msg_1", "text").await.is_ok());
        assert!(
            channel
                .finalize_draft("bob", "msg_1", "final text")
                .await
                .is_ok()
        );
        assert!(channel.cancel_draft("bob", "msg_1").await.is_ok());
    }

    #[tokio::test]
    async fn listen_sends_message_to_channel() {
        let channel = DummyChannel;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        channel.listen(tx).await.unwrap();

        let received = rx.recv().await.expect("message should be sent");
        assert_eq!(received.sender, "tester");
        assert_eq!(received.content, "hello");
        assert_eq!(received.channel, "dummy");
    }

    #[tokio::test]
    async fn default_redact_message_returns_success() {
        let channel = DummyChannel;

        assert!(
            channel
                .redact_message("chan_1", "msg_1", Some("spam".to_string()))
                .await
                .is_ok()
        );
        assert!(
            channel
                .redact_message("chan_1", "msg_2", None)
                .await
                .is_ok()
        );
    }
}
