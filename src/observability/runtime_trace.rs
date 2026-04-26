use crate::config::ObservabilityConfig;
use anyhow::Result;
use chrono::{Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use uuid::Uuid;

const DEFAULT_TRACE_REL_PATH: &str = "state/runtime-trace.jsonl";

tokio::task_local! {
    /// Provider session ID for the current message-processing scope.
    /// Set in `channels::process_channel_message` so that every
    /// `record_event` / `trace_api_request` inside the scope automatically
    /// carries the session identifier.
    pub static RUNTIME_TRACE_SESSION_ID: Option<String>;
}

/// Runtime trace storage policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTraceStorageMode {
    None,
    Rolling,
    Full,
}

impl RuntimeTraceStorageMode {
    fn from_raw(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "rolling" => Self::Rolling,
            "full" => Self::Full,
            _ => Self::None,
        }
    }
}

/// Structured runtime trace event for tool-call and model-reply diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeTraceEvent {
    pub id: String,
    pub timestamp: String,
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

struct RuntimeTraceLogger {
    mode: RuntimeTraceStorageMode,
    max_entries: usize,
    path: PathBuf,
    write_lock: std::sync::Mutex<()>,
}

impl RuntimeTraceLogger {
    fn new(mode: RuntimeTraceStorageMode, max_entries: usize, path: PathBuf) -> Self {
        Self {
            mode,
            max_entries: max_entries.max(1),
            path,
            write_lock: std::sync::Mutex::new(()),
        }
    }

    fn append(&self, event: &RuntimeTraceEvent) -> Result<()> {
        if self.mode == RuntimeTraceStorageMode::None {
            return Ok(());
        }

        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let line = serde_json::to_string(event)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = options.open(&self.path)?;
        writeln!(file, "{line}")?;
        file.sync_data()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }

        if self.mode == RuntimeTraceStorageMode::Rolling {
            self.trim_to_last_entries()?;
        }

        Ok(())
    }

    fn trim_to_last_entries(&self) -> Result<()> {
        let raw = fs::read_to_string(&self.path).unwrap_or_default();
        let lines: Vec<&str> = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();

        if lines.len() <= self.max_entries {
            return Ok(());
        }

        let keep_from = lines.len().saturating_sub(self.max_entries);
        let kept = &lines[keep_from..];
        let mut rewritten = kept.join("\n");
        rewritten.push('\n');

        let tmp = self.path.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::write(&tmp, rewritten)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }

        fs::rename(tmp, &self.path)?;
        Ok(())
    }
}

static TRACE_LOGGER: LazyLock<RwLock<Option<Arc<RuntimeTraceLogger>>>> =
    LazyLock::new(|| RwLock::new(None));

/// Resolve runtime trace storage mode from config.
pub fn storage_mode_from_config(config: &ObservabilityConfig) -> RuntimeTraceStorageMode {
    let mode = RuntimeTraceStorageMode::from_raw(&config.runtime_trace_mode);
    if mode == RuntimeTraceStorageMode::None
        && !config.runtime_trace_mode.trim().is_empty()
        && !config.runtime_trace_mode.eq_ignore_ascii_case("none")
    {
        tracing::warn!(
            mode = %config.runtime_trace_mode,
            "Unknown observability.runtime_trace_mode; falling back to none"
        );
    }
    mode
}

/// Resolve runtime trace path from config.
pub fn resolve_trace_path(config: &ObservabilityConfig, workspace_dir: &Path) -> PathBuf {
    let raw = config.runtime_trace_path.trim();
    let fallback = workspace_dir.join(DEFAULT_TRACE_REL_PATH);
    if raw.is_empty() {
        return fallback;
    }

    let configured = PathBuf::from(raw);
    if configured.is_absolute() {
        configured
    } else {
        workspace_dir.join(configured)
    }
}

/// Initialize (or disable) runtime trace logging.
pub fn init_from_config(config: &ObservabilityConfig, workspace_dir: &Path) {
    let mode = storage_mode_from_config(config);
    let logger = if mode == RuntimeTraceStorageMode::None {
        None
    } else {
        Some(Arc::new(RuntimeTraceLogger::new(
            mode,
            config.runtime_trace_max_entries.max(1),
            resolve_trace_path(config, workspace_dir),
        )))
    };

    let mut guard = TRACE_LOGGER.write().unwrap_or_else(|e| e.into_inner());
    *guard = logger;
}

/// Record a runtime trace event.
pub fn record_event(
    event_type: &str,
    channel: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    turn_id: Option<&str>,
    success: Option<bool>,
    message: Option<&str>,
    payload: Value,
) {
    let logger = TRACE_LOGGER
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(logger) = logger else {
        return;
    };

    let session_id = RUNTIME_TRACE_SESSION_ID
        .try_with(|sid| sid.clone())
        .ok()
        .flatten();

    let event = RuntimeTraceEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: Local::now().to_rfc3339(),
        event_type: event_type.to_string(),
        channel: channel.map(str::to_string),
        provider: provider.map(str::to_string),
        model: model.map(str::to_string),
        session_id,
        turn_id: turn_id.map(str::to_string),
        success,
        message: message.map(str::to_string),
        payload,
    };

    if let Err(err) = logger.append(&event) {
        tracing::warn!("Failed to write runtime trace event: {err}");
    }
}

/// Record a `provider_api_request` trace event with the full serialized request payload.
///
/// On serialization failure, records the event with `success: false` and the error message.
pub fn trace_api_request<T: serde::Serialize>(
    request: &T,
    provider: &str,
    model: &str,
    turn_id: Option<&str>,
) {
    if TRACE_LOGGER
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_none()
    {
        return;
    }

    match serde_json::to_value(request) {
        Ok(payload) => {
            record_event(
                "provider_api_request",
                None,
                Some(provider),
                Some(model),
                turn_id,
                None,
                None,
                payload,
            );
        }
        Err(err) => {
            record_event(
                "provider_api_request",
                None,
                Some(provider),
                Some(model),
                turn_id,
                Some(false),
                Some(&format!("serialization failed: {err}")),
                serde_json::Value::Null,
            );
        }
    }
}

/// Load recent runtime trace events from storage.
pub fn load_events(
    path: &Path,
    limit: usize,
    event_filter: Option<&str>,
    contains: Option<&str>,
) -> Result<Vec<RuntimeTraceEvent>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(path)?;
    let mut events = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<RuntimeTraceEvent>(trimmed) {
            Ok(event) => events.push(event),
            Err(err) => tracing::warn!("Skipping malformed runtime trace line: {err}"),
        }
    }

    if let Some(filter) = event_filter.map(str::trim).filter(|f| !f.is_empty()) {
        let normalized = filter.to_ascii_lowercase();
        events.retain(|event| event.event_type.to_ascii_lowercase() == normalized);
    }

    if let Some(needle) = contains.map(str::trim).filter(|s| !s.is_empty()) {
        let needle = needle.to_ascii_lowercase();
        events.retain(|event| {
            let mut haystack = format!(
                "{} {} {}",
                event.event_type,
                event.message.as_deref().unwrap_or_default(),
                event.payload
            );
            if let Some(channel) = &event.channel {
                haystack.push_str(channel);
            }
            if let Some(provider) = &event.provider {
                haystack.push_str(provider);
            }
            if let Some(model) = &event.model {
                haystack.push_str(model);
            }
            haystack.to_ascii_lowercase().contains(&needle)
        });
    }

    if events.len() > limit {
        let keep_from = events.len() - limit;
        events = events.split_off(keep_from);
    }

    events.reverse();
    Ok(events)
}

/// Find a runtime trace event by id.
pub fn find_event_by_id(path: &Path, id: &str) -> Result<Option<RuntimeTraceEvent>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(path)?;
    for line in raw.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<RuntimeTraceEvent>(trimmed) {
            if event.id == id {
                return Ok(Some(event));
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_observability_config() -> ObservabilityConfig {
        ObservabilityConfig {
            backend: "none".to_string(),
            otel_endpoint: None,
            otel_service_name: None,
            runtime_trace_mode: "rolling".to_string(),
            runtime_trace_path: "state/runtime-trace.jsonl".to_string(),
            runtime_trace_max_entries: 3,
        }
    }

    #[test]
    fn resolve_trace_path_relative_joins_workspace() {
        let cfg = test_observability_config();
        let workspace = tempfile::tempdir().unwrap();
        let path = resolve_trace_path(&cfg, workspace.path());
        assert_eq!(path, workspace.path().join("state/runtime-trace.jsonl"));
    }

    #[test]
    fn storage_mode_parses_known_values() {
        let mut cfg = test_observability_config();
        cfg.runtime_trace_mode = "none".into();
        assert_eq!(
            storage_mode_from_config(&cfg),
            RuntimeTraceStorageMode::None
        );

        cfg.runtime_trace_mode = "rolling".into();
        assert_eq!(
            storage_mode_from_config(&cfg),
            RuntimeTraceStorageMode::Rolling
        );

        cfg.runtime_trace_mode = "full".into();
        assert_eq!(
            storage_mode_from_config(&cfg),
            RuntimeTraceStorageMode::Full
        );
    }

    #[test]
    fn rolling_mode_keeps_latest_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trace.jsonl");
        let logger = RuntimeTraceLogger::new(RuntimeTraceStorageMode::Rolling, 2, path.clone());

        for i in 0..5 {
            let event = RuntimeTraceEvent {
                id: format!("id-{i}"),
                timestamp: Utc::now().to_rfc3339(),
                event_type: "test".into(),
                channel: None,
                provider: None,
                model: None,
                session_id: None,
                turn_id: None,
                success: None,
                message: Some(format!("event-{i}")),
                payload: serde_json::json!({ "i": i }),
            };
            logger.append(&event).unwrap();
        }

        let events = load_events(&path, 10, None, None).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].message.as_deref(), Some("event-4"));
        assert_eq!(events[1].message.as_deref(), Some("event-3"));
    }

    #[test]
    fn find_event_by_id_returns_match() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trace.jsonl");
        let logger = RuntimeTraceLogger::new(RuntimeTraceStorageMode::Full, 100, path.clone());

        let target_id = "target-event";
        let event = RuntimeTraceEvent {
            id: target_id.into(),
            timestamp: Utc::now().to_rfc3339(),
            event_type: "tool_call_result".into(),
            channel: Some("telegram".into()),
            provider: Some("openrouter".into()),
            model: Some("x".into()),
            session_id: None,
            turn_id: Some("turn-1".into()),
            success: Some(false),
            message: Some("boom".into()),
            payload: serde_json::json!({ "error": "boom" }),
        };
        logger.append(&event).unwrap();

        let found = find_event_by_id(&path, target_id).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, target_id);
    }

    #[test]
    fn trace_api_request_happy_path_unit() {
        // Test the logic directly without the global logger:
        // trace_api_request calls serde_json::to_value then record_event.
        // We verify that serde_json::to_value produces the expected payload.
        #[derive(serde::Serialize)]
        struct FakeRequest {
            model: String,
            messages: Vec<String>,
        }

        let req = FakeRequest {
            model: "test-model".into(),
            messages: vec!["hello".into()],
        };

        let payload = serde_json::to_value(&req).unwrap();
        assert_eq!(payload["model"], "test-model");
        assert_eq!(payload["messages"][0], "hello");
    }

    #[test]
    fn trace_api_request_serialization_failure_unit() {
        // Verify that a non-serializable type produces the expected error path.
        struct BadSerialize;
        impl serde::Serialize for BadSerialize {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentional failure"))
            }
        }

        let result = serde_json::to_value(&BadSerialize);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("intentional failure")
        );
    }

    #[test]
    fn trace_api_request_integration() {
        // Full integration test via the global logger.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trace.jsonl");

        {
            let logger = Arc::new(RuntimeTraceLogger::new(
                RuntimeTraceStorageMode::Full,
                10000,
                path.clone(),
            ));
            let mut guard = TRACE_LOGGER.write().unwrap();
            *guard = Some(logger);
        }

        #[derive(serde::Serialize)]
        struct Req {
            model: String,
        }

        trace_api_request(&Req { model: "m1".into() }, "prov1", "m1", Some("tid-1"));

        {
            let mut guard = TRACE_LOGGER.write().unwrap();
            *guard = None;
        }

        let events = load_events(&path, 100, None, None).unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.event_type, "provider_api_request");
        assert_eq!(ev.provider.as_deref(), Some("prov1"));
        assert_eq!(ev.model.as_deref(), Some("m1"));
        assert_eq!(ev.turn_id.as_deref(), Some("tid-1"));
        assert!(ev.success.is_none());
        assert_eq!(ev.payload["model"], "m1");
    }
}
