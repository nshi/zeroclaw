//! Routes user replies to in-flight tools waiting for input (e.g. `ask_user`,
//! `escalate_to_human`).
//!
//! ## Why this exists
//!
//! When a tool like `ask_user` blocks waiting for the user to reply, the same
//! channel listener that originally delivered the prompt also delivers the
//! reply. Without coordination, the dispatch loop would treat the reply as a
//! brand-new conversation — interrupting the in-flight task or queuing the
//! reply behind it. Either way, the waiting tool never sees the reply.
//!
//! This is especially broken on Slack DMs: there is no native threading, so
//! every reply lands in the same scope as the in-flight task. The reply is
//! either dropped, used to interrupt the agent (when `interrupt_on_new_message`
//! is enabled), or stalls behind the still-running task.
//!
//! ## How it works
//!
//! 1. `dispatch_worker` records a [`MessageContext`] for the message it is
//!    processing in [`CURRENT_MESSAGE_CONTEXT`].
//! 2. When a tool wants to wait for a reply, it calls
//!    [`register_waiter`], which inserts an mpsc sender into the global
//!    registry keyed by the message's [`scope_key`].
//! 3. The dispatch loop checks the registry before treating an inbound message
//!    as a fresh conversation. If a waiter is registered for the same scope,
//!    the message is forwarded to the waiter and dispatch is skipped.
//! 4. When the tool finishes (response received, timeout, or error), the
//!    `WaiterGuard` deregisters the waiter on drop.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;
use tokio::sync::mpsc;

use crate::channels::traits::ChannelMessage;

/// The scope of a message currently being processed by the dispatch worker.
///
/// Tools running inside `process_channel_message` can read this via
/// [`current_message_context`] to discover the channel/sender they are
/// answering on behalf of.
#[derive(Clone, Debug)]
pub struct MessageContext {
    pub channel: String,
    pub reply_target: String,
    pub sender: String,
    pub thread_ts: Option<String>,
    pub interruption_scope_id: Option<String>,
}

impl MessageContext {
    pub fn from_message(msg: &ChannelMessage) -> Self {
        Self {
            channel: msg.channel.clone(),
            reply_target: msg.reply_target.clone(),
            sender: msg.sender.clone(),
            thread_ts: msg.thread_ts.clone(),
            interruption_scope_id: msg.interruption_scope_id.clone(),
        }
    }
}

tokio::task_local! {
    /// The message context for the in-flight dispatch worker task.
    pub static CURRENT_MESSAGE_CONTEXT: MessageContext;
}

/// Returns a clone of the current dispatch worker's message context, if any.
///
/// Returns `None` when called from a task that was not entered via
/// `CURRENT_MESSAGE_CONTEXT.scope(...)` (e.g. CLI runs, tests).
pub fn current_message_context() -> Option<MessageContext> {
    CURRENT_MESSAGE_CONTEXT.try_with(|c| c.clone()).ok()
}

/// Build the registry key matching `interruption_scope_key` in
/// `channels::mod` so a reply lands in the same scope as the in-flight task
/// that called `ask_user`.
pub fn scope_key(
    channel: &str,
    reply_target: &str,
    sender: &str,
    interruption_scope_id: Option<&str>,
) -> String {
    match interruption_scope_id {
        Some(scope) => format!("{channel}_{reply_target}_{sender}_{scope}"),
        None => format!("{channel}_{reply_target}_{sender}"),
    }
}

/// Build the registry key for a [`ChannelMessage`].
pub fn scope_key_for_message(msg: &ChannelMessage) -> String {
    scope_key(
        &msg.channel,
        &msg.reply_target,
        &msg.sender,
        msg.interruption_scope_id.as_deref(),
    )
}

/// Build the registry key for a [`MessageContext`].
pub fn scope_key_for_context(ctx: &MessageContext) -> String {
    scope_key(
        &ctx.channel,
        &ctx.reply_target,
        &ctx.sender,
        ctx.interruption_scope_id.as_deref(),
    )
}

type WaiterMap = HashMap<String, mpsc::Sender<ChannelMessage>>;

static REGISTRY: OnceLock<Arc<RwLock<WaiterMap>>> = OnceLock::new();

fn registry() -> &'static Arc<RwLock<WaiterMap>> {
    REGISTRY.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

/// Drop guard that deregisters a waiter when the calling tool finishes.
pub struct WaiterGuard {
    key: String,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut map = registry().write();
        map.remove(&self.key);
    }
}

/// Register an mpsc sender that will receive the next inbound message matching
/// `key`. Replaces any existing waiter for the same key. Drop the returned
/// guard to deregister.
pub fn register_waiter(key: String, tx: mpsc::Sender<ChannelMessage>) -> WaiterGuard {
    let mut map = registry().write();
    map.insert(key.clone(), tx);
    WaiterGuard { key }
}

/// If a waiter is registered for `msg`'s scope, take it out of the registry,
/// forward the message, and return `true`. Subsequent messages on the same
/// scope (e.g. a follow-up from the same user before the tool finishes) fall
/// through to the normal dispatch path.
pub async fn try_route(msg: &ChannelMessage) -> bool {
    let key = scope_key_for_message(msg);
    let tx = {
        let mut map = registry().write();
        map.remove(&key)
    };
    let Some(tx) = tx else {
        return false;
    };
    // Receiver may have already been dropped (timeout race) — that's fine,
    // the message simply falls through.
    tx.send(msg.clone()).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(channel: &str, reply_target: &str, sender: &str) -> ChannelMessage {
        ChannelMessage {
            id: "id".into(),
            sender: sender.into(),
            reply_target: reply_target.into(),
            content: "hi".into(),
            channel: channel.into(),
            timestamp: 1,
            thread_ts: None,
            interruption_scope_id: None,
            attachments: vec![],
        }
    }

    #[test]
    fn scope_key_matches_interruption_format() {
        assert_eq!(scope_key("slack", "C1", "U1", None), "slack_C1_U1");
        assert_eq!(scope_key("slack", "C1", "U1", Some("ts")), "slack_C1_U1_ts");
    }

    #[tokio::test]
    async fn try_route_forwards_to_registered_waiter() {
        let msg = make_msg("slack", "DABC", "Ualice");
        let key = scope_key_for_message(&msg);
        let (tx, mut rx) = mpsc::channel(1);
        let _guard = register_waiter(key.clone(), tx);

        assert!(try_route(&msg).await);
        let received = rx.recv().await.expect("waiter should receive");
        assert_eq!(received.sender, "Ualice");
    }

    #[tokio::test]
    async fn try_route_returns_false_without_waiter() {
        let msg = make_msg("slack", "DZZZ", "Uunknown");
        assert!(!try_route(&msg).await);
    }

    #[tokio::test]
    async fn drop_guard_removes_waiter() {
        let msg = make_msg("slack", "DGUARD", "Uguard");
        let key = scope_key_for_message(&msg);
        let (tx, _rx) = mpsc::channel(1);
        {
            let _guard = register_waiter(key.clone(), tx);
            assert!(registry().read().contains_key(&key));
        }
        assert!(!registry().read().contains_key(&key));
    }

    #[tokio::test]
    async fn try_route_returns_false_when_receiver_dropped() {
        let msg = make_msg("slack", "DDEAD", "Udead");
        let key = scope_key_for_message(&msg);
        let (tx, rx) = mpsc::channel(1);
        let _guard = register_waiter(key.clone(), tx);
        drop(rx);

        // Take the waiter — send fails because rx is gone.
        assert!(!try_route(&msg).await);
        // Routing removes the entry whether or not delivery succeeded.
        assert!(!registry().read().contains_key(&key));
    }

    #[tokio::test]
    async fn try_route_only_delivers_first_message() {
        let msg = make_msg("slack", "DONCE", "Uonce");
        let key = scope_key_for_message(&msg);
        let (tx, mut rx) = mpsc::channel(1);
        let _guard = register_waiter(key.clone(), tx);

        assert!(try_route(&msg).await);
        // Subsequent messages on the same scope are no longer routed.
        assert!(!try_route(&msg).await);
        let _first = rx.recv().await.expect("first message");
    }

    #[tokio::test]
    async fn current_message_context_is_none_outside_scope() {
        assert!(current_message_context().is_none());
    }

    #[tokio::test]
    async fn current_message_context_within_scope() {
        let ctx = MessageContext {
            channel: "slack".into(),
            reply_target: "DABC".into(),
            sender: "Ualice".into(),
            thread_ts: None,
            interruption_scope_id: None,
        };
        let result = CURRENT_MESSAGE_CONTEXT
            .scope(ctx.clone(), async {
                let observed = current_message_context().expect("context set");
                (observed.channel, observed.reply_target, observed.sender)
            })
            .await;
        assert_eq!(result, ("slack".into(), "DABC".into(), "Ualice".into()));
    }
}
