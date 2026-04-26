//! Interactive approval workflow for supervised mode.
//!
//! Provides a pre-execution hook that prompts the user before tool calls,
//! with session-scoped "Always" allowlists and audit logging. Each channel
//! implements [`ChannelApprovalAdapter`] to deliver approval prompts through
//! its native messaging primitives.

pub mod adapter;

pub use adapter::{
    ChannelApprovalAdapter, CliApprovalAdapter, SlackApprovalAdapter, TelegramApprovalAdapter,
};
// GatewayApprovalAdapter is constructed directly from the gateway/ws.rs module
// via crate::approval::adapter::GatewayApprovalAdapter when needed.

use crate::config::AutonomyConfig;
use crate::security::{AutonomyLevel, CommandRiskLevel, SecurityPolicy};
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::time::Duration;

// ── Types ────────────────────────────────────────────────────────

/// A request to approve a tool call before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    /// Unique correlation ID (UUID v4).
    #[serde(default)]
    pub request_id: String,
    /// Originating channel name.
    #[serde(default)]
    pub channel: String,
    /// User/chat to send the prompt to.
    #[serde(default)]
    pub recipient: String,
    /// Thread context for threaded replies (Slack, Telegram).
    #[serde(default)]
    pub thread_ts: Option<String>,
    /// Harness-classified risk level (for shell commands).
    #[serde(default)]
    pub risk_level: Option<CommandRiskLevel>,
    /// When the request was created.
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ApprovalRequest {
    /// Create a minimal request (backward-compatible with existing callers).
    pub fn new(tool_name: String, arguments: serde_json::Value) -> Self {
        Self {
            tool_name,
            arguments,
            request_id: uuid::Uuid::new_v4().to_string(),
            channel: String::new(),
            recipient: String::new(),
            thread_ts: None,
            risk_level: None,
            created_at: Some(Utc::now()),
        }
    }
}

/// The user's response to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalResponse {
    /// Execute this one call.
    Yes,
    /// Deny this call.
    No,
    /// Execute and add tool to session-scoped allowlist.
    Always,
    /// No response within timeout period.
    Timeout,
}

/// A single audit log entry for an approval decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalLogEntry {
    pub timestamp: String,
    pub tool_name: String,
    pub arguments_summary: String,
    pub decision: ApprovalResponse,
    pub channel: String,
    /// Harness-classified risk level (for shell commands).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<String>,
    /// Correlation ID for channel-prompted decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Time from prompt sent to response received (ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_time_ms: Option<u64>,
}

// ── ApprovalManager ──────────────────────────────────────────────

/// Manages the approval workflow for tool calls.
///
/// - Checks config-level `auto_approve` / `always_ask` lists
/// - Maintains a session-scoped "always" allowlist
/// - Records an audit trail of all decisions
/// - Delegates to a [`ChannelApprovalAdapter`] for channel-specific prompting
pub struct ApprovalManager {
    /// Tools that never need approval (from config).
    auto_approve: HashSet<String>,
    /// Tools that always need approval, ignoring session allowlist.
    always_ask: HashSet<String>,
    /// Autonomy level from config.
    autonomy_level: AutonomyLevel,
    /// When `true`, tools that would require interactive approval are
    /// auto-denied instead. Used for channel-driven runs that have not
    /// yet been wired with a [`ChannelApprovalAdapter`].
    non_interactive: bool,
    /// Approval timeout duration.
    approval_timeout: Duration,
    /// Session-scoped allowlist built from "Always" responses.
    session_allowlist: Mutex<HashSet<String>>,
    /// Audit trail of approval decisions.
    audit_log: Mutex<Vec<ApprovalLogEntry>>,
}

impl ApprovalManager {
    /// Create an interactive (CLI) approval manager from autonomy config.
    pub fn from_config(config: &AutonomyConfig) -> Self {
        Self {
            auto_approve: config.auto_approve.iter().cloned().collect(),
            always_ask: config.always_ask.iter().cloned().collect(),
            autonomy_level: config.level,
            non_interactive: false,
            approval_timeout: Duration::from_secs(config.approval_timeout_secs),
            session_allowlist: Mutex::new(HashSet::new()),
            audit_log: Mutex::new(Vec::new()),
        }
    }

    /// Create a non-interactive approval manager for channel-driven runs
    /// that do not yet have a [`ChannelApprovalAdapter`].
    ///
    /// Tools requiring approval are auto-denied instead of prompting.
    pub fn for_non_interactive(config: &AutonomyConfig) -> Self {
        Self {
            auto_approve: config.auto_approve.iter().cloned().collect(),
            always_ask: config.always_ask.iter().cloned().collect(),
            autonomy_level: config.level,
            non_interactive: true,
            approval_timeout: Duration::from_secs(config.approval_timeout_secs),
            session_allowlist: Mutex::new(HashSet::new()),
            audit_log: Mutex::new(Vec::new()),
        }
    }

    /// Returns `true` when this manager operates in non-interactive mode.
    pub fn is_non_interactive(&self) -> bool {
        self.non_interactive
    }

    /// Check whether a tool call requires interactive approval.
    ///
    /// Returns `true` if the call needs a prompt, `false` if it can proceed.
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        // Full autonomy never prompts.
        if self.autonomy_level == AutonomyLevel::Full {
            return false;
        }

        // ReadOnly blocks everything — handled elsewhere; no prompt needed.
        if self.autonomy_level == AutonomyLevel::ReadOnly {
            return false;
        }

        // always_ask overrides everything.
        if self.always_ask.contains("*") || self.always_ask.contains(tool_name) {
            return true;
        }

        // Channel-driven shell execution is still guarded by the shell tool's
        // own command allowlist and risk policy. Skipping the outer approval
        // gate here lets low-risk allowlisted commands (e.g. `ls`) work in
        // non-interactive channels without silently allowing medium/high-risk
        // commands. This path is transitional — once channels have adapters,
        // shell approval is routed through needs_shell_approval() instead.
        if self.non_interactive && tool_name == "shell" {
            return false;
        }

        // auto_approve skips the prompt.
        if self.auto_approve.contains("*") || self.auto_approve.contains(tool_name) {
            return false;
        }

        // Session allowlist (from prior "Always" responses).
        let allowlist = self.session_allowlist.lock();
        if allowlist.contains(tool_name) {
            return false;
        }

        // Default: supervised mode requires approval.
        true
    }

    /// Check whether a shell command requires approval based on its risk level.
    ///
    /// Uses the harness `SecurityPolicy` to classify risk, returning `true` for
    /// Medium+ risk commands in supervised mode when `require_approval_for_medium_risk`
    /// is set. Low-risk commands pass without prompting.
    pub fn needs_shell_approval(&self, command: &str, security: &SecurityPolicy) -> bool {
        if self.autonomy_level == AutonomyLevel::Full {
            return false;
        }
        if self.autonomy_level == AutonomyLevel::ReadOnly {
            return false;
        }

        let risk = security.command_risk_level(command);
        match risk {
            CommandRiskLevel::Low => false,
            CommandRiskLevel::Medium => {
                security.require_approval_for_medium_risk
                    && self.autonomy_level == AutonomyLevel::Supervised
            }
            CommandRiskLevel::High => self.autonomy_level == AutonomyLevel::Supervised,
        }
    }

    /// Send an approval request through the adapter and wait for a response,
    /// with timeout.
    ///
    /// Returns the user's decision. On adapter error or timeout, returns a
    /// denial (`No` or `Timeout` respectively).
    pub async fn request_approval(
        &self,
        adapter: &dyn ChannelApprovalAdapter,
        request: &ApprovalRequest,
    ) -> ApprovalResponse {
        let start = std::time::Instant::now();

        let pending = match adapter.send_approval_request(request).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    tool = %request.tool_name,
                    error = %e,
                    "failed to send approval request, treating as denial"
                );
                return ApprovalResponse::No;
            }
        };

        let response = match tokio::time::timeout(
            self.approval_timeout,
            adapter.receive_approval_response(&pending),
        )
        .await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                tracing::warn!(
                    tool = %request.tool_name,
                    error = %e,
                    "error receiving approval response, treating as denial"
                );
                ApprovalResponse::No
            }
            Err(_elapsed) => {
                tracing::info!(
                    tool = %request.tool_name,
                    timeout_secs = self.approval_timeout.as_secs(),
                    "approval request timed out"
                );
                ApprovalResponse::Timeout
            }
        };

        #[allow(clippy::cast_possible_truncation)]
        let elapsed_ms = start.elapsed().as_millis() as u64;
        self.record_decision_ext(
            &request.tool_name,
            &request.arguments,
            response,
            adapter.channel_name(),
            request
                .risk_level
                .as_ref()
                .map(|r| format!("{r:?}").to_ascii_lowercase()),
            Some(request.request_id.clone()),
            Some(elapsed_ms),
        );

        // Handle "Always" → add to session allowlist (unless in always_ask).
        if response == ApprovalResponse::Always
            && !self.always_ask.contains(&request.tool_name)
            && !self.always_ask.contains("*")
        {
            let mut allowlist = self.session_allowlist.lock();
            allowlist.insert(request.tool_name.clone());
        }

        response
    }

    /// Record that a tool call was auto-approved (no prompt needed).
    pub fn record_auto_approved(&self, tool_name: &str, channel: &str) {
        tracing::debug!(tool = %tool_name, channel, "auto-approved tool call");
        self.record_decision_ext(
            tool_name,
            &serde_json::Value::Null,
            ApprovalResponse::Yes,
            channel,
            None,
            None,
            None,
        );
    }

    /// Record an approval decision and update session state.
    pub fn record_decision(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        decision: ApprovalResponse,
        channel: &str,
    ) {
        match decision {
            ApprovalResponse::Yes => {
                tracing::info!(tool = %tool_name, channel, "tool call approved by user");
            }
            ApprovalResponse::No => {
                tracing::info!(tool = %tool_name, channel, "tool call denied by user");
            }
            ApprovalResponse::Always => {
                tracing::info!(tool = %tool_name, channel, "tool call always-approved by user");
            }
            ApprovalResponse::Timeout => {
                tracing::info!(tool = %tool_name, channel, "tool call approval timed out");
            }
        }

        self.record_decision_ext(tool_name, args, decision, channel, None, None, None);

        // If "Always", add to session allowlist (unless in always_ask).
        if decision == ApprovalResponse::Always
            && !self.always_ask.contains(tool_name)
            && !self.always_ask.contains("*")
        {
            let mut allowlist = self.session_allowlist.lock();
            allowlist.insert(tool_name.to_string());
        }
    }

    /// Record an approval decision with extended audit fields.
    fn record_decision_ext(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        decision: ApprovalResponse,
        channel: &str,
        risk_level: Option<String>,
        request_id: Option<String>,
        response_time_ms: Option<u64>,
    ) {
        let summary = summarize_args(args);
        let entry = ApprovalLogEntry {
            timestamp: Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            arguments_summary: summary,
            decision,
            channel: channel.to_string(),
            risk_level,
            request_id,
            response_time_ms,
        };
        let mut log = self.audit_log.lock();
        log.push(entry);
    }

    /// Get a snapshot of the audit log.
    pub fn audit_log(&self) -> Vec<ApprovalLogEntry> {
        self.audit_log.lock().clone()
    }

    /// Get the current session allowlist.
    pub fn session_allowlist(&self) -> HashSet<String> {
        self.session_allowlist.lock().clone()
    }

    /// Prompt the user on the CLI and return their decision.
    ///
    /// Only called for interactive (CLI) managers when no adapter is available.
    pub fn prompt_cli(&self, request: &ApprovalRequest) -> ApprovalResponse {
        prompt_cli_interactive(request)
    }

    /// The configured approval timeout duration.
    pub fn approval_timeout(&self) -> Duration {
        self.approval_timeout
    }
}

// ── CLI prompt ───────────────────────────────────────────────────

/// Display the approval prompt and read user input from stdin.
fn prompt_cli_interactive(request: &ApprovalRequest) -> ApprovalResponse {
    let summary = summarize_args(&request.arguments);
    eprintln!();
    eprintln!("🔧 Agent wants to execute: {}", request.tool_name);
    eprintln!("   {summary}");
    if let Some(ref risk) = request.risk_level {
        eprintln!("   Risk: {risk:?}");
    }
    eprint!("   [Y]es / [N]o / [A]lways for {}: ", request.tool_name);
    let _ = io::stderr().flush();

    let stdin = io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return ApprovalResponse::No;
    }

    parse_approval_input(&line)
}

/// Parse user input into an approval response. Case-insensitive.
/// Unrecognized input is treated as denial (fail-safe).
pub fn parse_approval_input(input: &str) -> ApprovalResponse {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalResponse::Yes,
        "a" | "always" => ApprovalResponse::Always,
        _ => ApprovalResponse::No,
    }
}

/// Produce a short human-readable summary of tool arguments.
pub fn summarize_args(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => truncate_for_summary(s, 80),
                        other => {
                            let s = other.to_string();
                            truncate_for_summary(&s, 80)
                        }
                    };
                    format!("{k}: {val}")
                })
                .collect();
            parts.join(", ")
        }
        other => {
            let s = other.to_string();
            truncate_for_summary(&s, 120)
        }
    }
}

fn truncate_for_summary(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        input.to_string()
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutonomyConfig;

    fn supervised_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Supervised,
            auto_approve: vec!["file_read".into(), "memory_recall".into()],
            always_ask: vec!["shell".into()],
            ..AutonomyConfig::default()
        }
    }

    fn full_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Full,
            ..AutonomyConfig::default()
        }
    }

    // ── needs_approval ───────────────────────────────────────

    #[test]
    fn auto_approve_tools_skip_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(!mgr.needs_approval("file_read"));
        assert!(!mgr.needs_approval("memory_recall"));
    }

    #[test]
    fn always_ask_tools_always_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn unknown_tool_needs_approval_in_supervised() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));
        assert!(mgr.needs_approval("http_request"));
    }

    #[test]
    fn full_autonomy_never_prompts() {
        let mgr = ApprovalManager::from_config(&full_config());
        assert!(!mgr.needs_approval("shell"));
        assert!(!mgr.needs_approval("file_write"));
        assert!(!mgr.needs_approval("anything"));
    }

    #[test]
    fn readonly_never_prompts() {
        let config = AutonomyConfig {
            level: AutonomyLevel::ReadOnly,
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&config);
        assert!(!mgr.needs_approval("shell"));
    }

    // ── session allowlist ────────────────────────────────────

    #[test]
    fn always_response_adds_to_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));

        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "test.txt"}),
            ApprovalResponse::Always,
            "cli",
        );

        // Now file_write should be in session allowlist.
        assert!(!mgr.needs_approval("file_write"));
    }

    #[test]
    fn always_ask_overrides_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        // Even after "Always" for shell, it should still prompt.
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Always,
            "cli",
        );

        // shell is in always_ask, so it still needs approval.
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn yes_response_does_not_add_to_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "file_write",
            &serde_json::json!({}),
            ApprovalResponse::Yes,
            "cli",
        );
        assert!(mgr.needs_approval("file_write"));
    }

    // ── audit log ────────────────────────────────────────────

    #[test]
    fn audit_log_records_decisions() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "rm -rf ./build/"}),
            ApprovalResponse::No,
            "cli",
        );
        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "out.txt", "content": "hello"}),
            ApprovalResponse::Yes,
            "cli",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].tool_name, "shell");
        assert_eq!(log[0].decision, ApprovalResponse::No);
        assert_eq!(log[1].tool_name, "file_write");
        assert_eq!(log[1].decision, ApprovalResponse::Yes);
    }

    #[test]
    fn audit_log_contains_timestamp_and_channel() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Yes,
            "telegram",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert!(!log[0].timestamp.is_empty());
        assert_eq!(log[0].channel, "telegram");
    }

    // ── summarize_args ───────────────────────────────────────

    #[test]
    fn summarize_args_object() {
        let args = serde_json::json!({"command": "ls -la", "cwd": "/tmp"});
        let summary = summarize_args(&args);
        assert!(summary.contains("command: ls -la"));
        assert!(summary.contains("cwd: /tmp"));
    }

    #[test]
    fn summarize_args_truncates_long_values() {
        let long_val = "x".repeat(200);
        let args = serde_json::json!({ "content": long_val });
        let summary = summarize_args(&args);
        assert!(summary.contains('…'));
        assert!(summary.len() < 200);
    }

    #[test]
    fn summarize_args_unicode_safe_truncation() {
        let long_val = "🦀".repeat(120);
        let args = serde_json::json!({ "content": long_val });
        let summary = summarize_args(&args);
        assert!(summary.contains("content:"));
        assert!(summary.contains('…'));
    }

    #[test]
    fn summarize_args_non_object() {
        let args = serde_json::json!("just a string");
        let summary = summarize_args(&args);
        assert!(summary.contains("just a string"));
    }

    // ── ApprovalResponse serde ───────────────────────────────

    #[test]
    fn approval_response_serde_roundtrip() {
        let json = serde_json::to_string(&ApprovalResponse::Always).unwrap();
        assert_eq!(json, "\"always\"");
        let parsed: ApprovalResponse = serde_json::from_str("\"no\"").unwrap();
        assert_eq!(parsed, ApprovalResponse::No);
    }

    #[test]
    fn approval_response_timeout_serde_roundtrip() {
        let json = serde_json::to_string(&ApprovalResponse::Timeout).unwrap();
        assert_eq!(json, "\"timeout\"");
        let parsed: ApprovalResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ApprovalResponse::Timeout);
    }

    // ── ApprovalRequest ──────────────────────────────────────

    #[test]
    fn approval_request_serde() {
        let req = ApprovalRequest::new("shell".into(), serde_json::json!({"command": "echo hi"}));
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ApprovalRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_name, "shell");
        assert!(!parsed.request_id.is_empty());
    }

    #[test]
    fn approval_request_new_has_defaults() {
        let req = ApprovalRequest::new("file_write".into(), serde_json::json!({}));
        assert!(!req.request_id.is_empty());
        assert!(req.created_at.is_some());
        assert!(req.risk_level.is_none());
        assert!(req.thread_ts.is_none());
    }

    // ── parse_approval_input ─────────────────────────────────

    #[test]
    fn parse_approval_input_yes_variants() {
        assert_eq!(parse_approval_input("y"), ApprovalResponse::Yes);
        assert_eq!(parse_approval_input("Y"), ApprovalResponse::Yes);
        assert_eq!(parse_approval_input("yes"), ApprovalResponse::Yes);
        assert_eq!(parse_approval_input("YES"), ApprovalResponse::Yes);
        assert_eq!(parse_approval_input("  y  "), ApprovalResponse::Yes);
    }

    #[test]
    fn parse_approval_input_no_variants() {
        assert_eq!(parse_approval_input("n"), ApprovalResponse::No);
        assert_eq!(parse_approval_input("no"), ApprovalResponse::No);
        assert_eq!(parse_approval_input("N"), ApprovalResponse::No);
    }

    #[test]
    fn parse_approval_input_always_variants() {
        assert_eq!(parse_approval_input("a"), ApprovalResponse::Always);
        assert_eq!(parse_approval_input("always"), ApprovalResponse::Always);
        assert_eq!(parse_approval_input("ALWAYS"), ApprovalResponse::Always);
    }

    #[test]
    fn parse_approval_input_unrecognized_is_no() {
        assert_eq!(parse_approval_input("maybe"), ApprovalResponse::No);
        assert_eq!(parse_approval_input(""), ApprovalResponse::No);
        assert_eq!(parse_approval_input("xyz"), ApprovalResponse::No);
    }

    // ── needs_shell_approval ─────────────────────────────────

    #[test]
    fn needs_shell_approval_low_risk_no_prompt() {
        let config = supervised_config();
        let mgr = ApprovalManager::from_config(&config);
        let security = SecurityPolicy::default();
        assert!(!mgr.needs_shell_approval("ls -la", &security));
        assert!(!mgr.needs_shell_approval("git status", &security));
    }

    #[test]
    fn needs_shell_approval_medium_risk_prompts() {
        let config = supervised_config();
        let mgr = ApprovalManager::from_config(&config);
        let security = SecurityPolicy::default();
        assert!(mgr.needs_shell_approval("touch file.txt", &security));
    }

    #[test]
    fn needs_shell_approval_high_risk_prompts() {
        let config = supervised_config();
        let mgr = ApprovalManager::from_config(&config);
        let security = SecurityPolicy::default();
        assert!(mgr.needs_shell_approval("rm -rf /tmp/test", &security));
    }

    #[test]
    fn needs_shell_approval_full_autonomy_never_prompts() {
        let config = full_config();
        let mgr = ApprovalManager::from_config(&config);
        let security = SecurityPolicy::default();
        assert!(!mgr.needs_shell_approval("rm -rf /tmp/test", &security));
    }

    #[test]
    fn needs_shell_approval_medium_risk_no_prompt_when_disabled() {
        let config = AutonomyConfig {
            level: AutonomyLevel::Supervised,
            require_approval_for_medium_risk: false,
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&config);
        let security = SecurityPolicy {
            require_approval_for_medium_risk: false,
            ..SecurityPolicy::default()
        };
        assert!(!mgr.needs_shell_approval("touch file.txt", &security));
    }

    // ── audit log extended fields ────────────────────────────

    #[test]
    fn record_decision_ext_includes_risk_and_timing() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision_ext(
            "shell",
            &serde_json::json!({"command": "rm -rf ./build/"}),
            ApprovalResponse::Yes,
            "telegram",
            Some("high".into()),
            Some("req-123".into()),
            Some(450),
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].risk_level.as_deref(), Some("high"));
        assert_eq!(log[0].request_id.as_deref(), Some("req-123"));
        assert_eq!(log[0].response_time_ms, Some(450));
    }

    // ── Backward compat: non-interactive patterns ────────────

    #[test]
    fn auto_approve_tools_skip_approval_any_config() {
        let config = AutonomyConfig::default();
        let mgr = ApprovalManager::from_config(&config);

        for tool in &config.auto_approve {
            assert!(
                !mgr.needs_approval(tool),
                "default auto_approve tool '{tool}' should not need approval"
            );
        }
    }

    #[test]
    fn unknown_tools_need_approval() {
        let config = AutonomyConfig::default();
        let mgr = ApprovalManager::from_config(&config);
        assert!(
            mgr.needs_approval("some_unknown_tool"),
            "unknown tool should need approval"
        );
    }

    #[test]
    fn weather_is_auto_approved() {
        let config = AutonomyConfig::default();
        let mgr = ApprovalManager::from_config(&config);
        assert!(
            !mgr.needs_approval("weather"),
            "weather tool must not need approval — it is in the default auto_approve list"
        );
    }

    #[test]
    fn always_ask_overrides_auto_approve() {
        let mut config = AutonomyConfig::default();
        config.always_ask = vec!["weather".into()];
        let mgr = ApprovalManager::from_config(&config);
        assert!(
            mgr.needs_approval("weather"),
            "always_ask must override auto_approve"
        );
    }

    // ── request_approval with mock adapter ───────────────────

    #[tokio::test]
    async fn request_approval_yes_flow() {
        use super::adapter::tests::MockApprovalAdapter;

        let config = supervised_config();
        let mgr = ApprovalManager::from_config(&config);
        let adapter = MockApprovalAdapter::new(ApprovalResponse::Yes);

        let request =
            ApprovalRequest::new("file_write".into(), serde_json::json!({"path": "test.txt"}));

        let result = mgr.request_approval(&adapter, &request).await;
        assert_eq!(result, ApprovalResponse::Yes);

        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].decision, ApprovalResponse::Yes);
        assert!(log[0].response_time_ms.is_some());
    }

    #[tokio::test]
    async fn request_approval_always_adds_to_allowlist() {
        use super::adapter::tests::MockApprovalAdapter;

        let config = supervised_config();
        let mgr = ApprovalManager::from_config(&config);
        let adapter = MockApprovalAdapter::new(ApprovalResponse::Always);

        let request =
            ApprovalRequest::new("file_write".into(), serde_json::json!({"path": "test.txt"}));

        let result = mgr.request_approval(&adapter, &request).await;
        assert_eq!(result, ApprovalResponse::Always);
        assert!(!mgr.needs_approval("file_write"));
    }

    #[tokio::test]
    async fn request_approval_always_for_always_ask_tool_does_not_allowlist() {
        use super::adapter::tests::MockApprovalAdapter;

        let config = supervised_config();
        let mgr = ApprovalManager::from_config(&config);
        let adapter = MockApprovalAdapter::new(ApprovalResponse::Always);

        let request = ApprovalRequest::new("shell".into(), serde_json::json!({"command": "ls"}));

        let result = mgr.request_approval(&adapter, &request).await;
        assert_eq!(result, ApprovalResponse::Always);
        // shell is in always_ask, so it should still need approval.
        assert!(mgr.needs_approval("shell"));
    }

    #[tokio::test]
    async fn request_approval_timeout_flow() {
        use super::adapter::tests::HangingApprovalAdapter;

        let config = AutonomyConfig {
            approval_timeout_secs: 1, // 1 second timeout for test speed
            ..supervised_config()
        };
        let mgr = ApprovalManager::from_config(&config);
        let adapter = HangingApprovalAdapter;

        let request =
            ApprovalRequest::new("file_write".into(), serde_json::json!({"path": "test.txt"}));

        let result = mgr.request_approval(&adapter, &request).await;
        assert_eq!(result, ApprovalResponse::Timeout);

        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].decision, ApprovalResponse::Timeout);
    }

    #[tokio::test]
    async fn request_approval_deny_flow() {
        use super::adapter::tests::MockApprovalAdapter;

        let config = supervised_config();
        let mgr = ApprovalManager::from_config(&config);
        let adapter = MockApprovalAdapter::new(ApprovalResponse::No);

        let request =
            ApprovalRequest::new("file_write".into(), serde_json::json!({"path": "test.txt"}));

        let result = mgr.request_approval(&adapter, &request).await;
        assert_eq!(result, ApprovalResponse::No);
        assert!(mgr.needs_approval("file_write"));
    }
}
