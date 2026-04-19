use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    ConversationMessage, Provider, ProviderCapabilities, StreamChunk, StreamError, StreamEvent,
    StreamOptions, StreamResult, TokenUsage, ToolCall as ProviderToolCall,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};

// ── Provider struct ─────────────────────────────────────────────────────────

pub struct OllamaProvider {
    base_url: String,
    reasoning_enabled: Option<bool>,
    timeout_secs: Option<u64>,
    max_tokens: Option<u32>,
    num_ctx: Option<u32>,
}

impl OllamaProvider {
    pub fn new() -> Self {
        Self {
            base_url: "http://localhost:11434".to_string(),
            reasoning_enabled: None,
            timeout_secs: None,
            max_tokens: None,
            num_ctx: None,
        }
    }

    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.trim_end_matches('/').to_string();
        self
    }

    pub fn with_timeout_secs(mut self, timeout_secs: Option<u64>) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_reasoning(mut self, enabled: Option<bool>) -> Self {
        self.reasoning_enabled = enabled;
        self
    }

    pub fn with_context_window(mut self, context_window: Option<u32>) -> Self {
        self.num_ctx = context_window;
        self
    }

    fn http_client(&self) -> Client {
        let timeout = self.timeout_secs.unwrap_or(120);
        crate::config::build_runtime_proxy_client_with_timeouts("provider.ollama", timeout, 10)
    }

    fn build_options(&self, temperature: f64) -> Options {
        Options {
            temperature,
            top_p: 0.95,
            top_k: 64,
            num_predict: self.max_tokens.map(i64::from),
            num_ctx: self.num_ctx.map(i64::from),
        }
    }

    fn prepare_messages(
        &self,
        messages: &[ChatMessage],
        model: &str,
    ) -> (Vec<OllamaMessage>, Option<bool>) {
        let mut ollama_messages = convert_chat_messages(messages);
        let reasoning_enabled = self.reasoning_enabled.unwrap_or(false);
        let think = if reasoning_enabled { Some(true) } else { None };
        if reasoning_enabled && is_gemma_model(model) {
            inject_thinking_token(&mut ollama_messages);
        }
        (ollama_messages, think)
    }
}

// ── Ollama API serde types ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
    options: Options,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OllamaMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Options {
    temperature: f64,
    top_p: f64,
    top_k: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_ctx: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OllamaToolCall {
    function: OllamaFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OllamaFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiChatResponse {
    #[serde(default)]
    message: Option<ResponseMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
    #[serde(default)]
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaErrorResponse {
    #[serde(default)]
    error: String,
}

// ── Model-family detection ──────────────────────────────────────────────────

/// Some model families require a special token in the system prompt to activate
/// thinking mode, rather than (or in addition to) the `think` API parameter.
fn is_gemma_model(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("gemma")
}

/// Prepend the `<|think|>` control token to the first system message.
///
/// Gemma 4 (and Gemma 3) activate reasoning when this token appears at the
/// start of the system prompt. Without it, `think: true` alone has no effect.
fn inject_thinking_token(messages: &mut [OllamaMessage]) {
    if let Some(sys) = messages.iter_mut().find(|m| m.role == "system") {
        sys.content = format!("<|think|>\n{}", sys.content);
    }
}

// ── Thinking content extraction ─────────────────────────────────────────────

/// Extract thinking content from a response.
///
/// Priority chain:
/// 1. Dedicated `thinking` field (highest priority)
/// 2. `<|channel>thought\n...<channel|>` markers (used by some models)
/// 3. Generic `<think>...</think>` XML tags
///
/// All marker formats are tried unconditionally — the marker check is cheap
/// and avoids coupling the provider to specific model families.
fn extract_thinking_content(
    content: &str,
    thinking_field: Option<&str>,
) -> (String, Option<String>) {
    // 1. Dedicated field takes priority
    if let Some(thinking) = thinking_field {
        if !thinking.is_empty() {
            return (content.to_string(), Some(thinking.to_string()));
        }
    }

    // 2. Channel format markers
    if let Some(result) = extract_channel_thinking(content) {
        return result;
    }

    // 3. Generic <think> tags
    if let Some(result) = extract_think_tags(content) {
        return result;
    }

    (content.to_string(), None)
}

fn extract_between_markers(
    content: &str,
    start_marker: &str,
    end_marker: &str,
) -> Option<(String, Option<String>)> {
    let start = content.find(start_marker)?;
    let inner_start = start + start_marker.len();
    let inner_len = content[inner_start..].find(end_marker)?;
    let thinking = content[inner_start..inner_start + inner_len].to_string();

    let mut text = String::with_capacity(content.len());
    text.push_str(&content[..start]);
    text.push_str(&content[inner_start + inner_len + end_marker.len()..]);
    let text = text.trim().to_string();

    if thinking.is_empty() {
        None
    } else {
        Some((text, Some(thinking)))
    }
}

fn extract_channel_thinking(content: &str) -> Option<(String, Option<String>)> {
    extract_between_markers(content, "<|channel>thought\n", "<channel|>")
}

fn extract_think_tags(content: &str) -> Option<(String, Option<String>)> {
    extract_between_markers(content, "<think>", "</think>")
}

// ── Message conversion helpers ──────────────────────────────────────────────

/// Extract base64 image data from parsed image marker references.
fn extract_base64_images(image_refs: &[String]) -> Option<Vec<String>> {
    let images: Vec<String> = image_refs
        .iter()
        .filter_map(|r| crate::multimodal::extract_ollama_image_payload(r))
        .collect();
    if images.is_empty() {
        None
    } else {
        Some(images)
    }
}

/// Convert a single ChatMessage to OllamaMessage with full reconstruction.
///
/// For assistant and tool roles, parses JSON-encoded tool call history back
/// into native Ollama structures. Also strips reasoning content from assistant
/// messages so the model doesn't see its own prior thinking.
fn convert_chat_message_rich(chat: &ChatMessage) -> OllamaMessage {
    if chat.role == "system" {
        OllamaMessage {
            role: "system".to_string(),
            content: chat.content.clone(),
            images: None,
            tool_calls: None,
        }
    } else if chat.role == "user" {
        let (cleaned, image_refs) = crate::multimodal::parse_image_markers(&chat.content);
        OllamaMessage {
            role: "user".to_string(),
            content: cleaned,
            images: extract_base64_images(&image_refs),
            tool_calls: None,
        }
    } else if chat.role == "assistant" {
        // The agent loop serializes tool calls as JSON in
        // ChatMessage.content. Parse them back into native
        // Ollama tool_calls so the model sees proper
        // structured history, not raw JSON blobs.
        if let Some(msg) = try_parse_assistant_tool_call_json(&chat.content) {
            msg
        } else {
            let (text, _) = extract_thinking_content(&chat.content, None);
            OllamaMessage {
                role: "assistant".to_string(),
                content: text,
                images: None,
                tool_calls: None,
            }
        }
    } else if chat.role == "tool" {
        // Tool results are serialized as JSON with
        // `tool_call_id` and `content` fields. Extract the
        // inner content so the model sees clean tool output.
        if let Some(msg) = try_parse_tool_result_json(&chat.content) {
            msg
        } else {
            OllamaMessage {
                role: "tool".to_string(),
                content: chat.content.clone(),
                images: None,
                tool_calls: None,
            }
        }
    } else {
        OllamaMessage {
            role: chat.role.clone(),
            content: chat.content.clone(),
            images: None,
            tool_calls: None,
        }
    }
}

fn convert_chat_messages(messages: &[ChatMessage]) -> Vec<OllamaMessage> {
    messages.iter().map(convert_chat_message_rich).collect()
}

/// Convert ConversationMessage history to Ollama messages.
fn convert_conversation_messages(messages: &[ConversationMessage]) -> Vec<OllamaMessage> {
    let mut result = Vec::new();

    for msg in messages {
        match msg {
            ConversationMessage::Chat(chat) => {
                result.push(convert_chat_message_rich(chat));
            }
            ConversationMessage::AssistantToolCalls {
                text,
                tool_calls,
                reasoning_content: _,
                provider_attrs: _,
            } => {
                let content = text.clone().unwrap_or_default();
                let (cleaned, _) = extract_thinking_content(&content, None);
                let ollama_tool_calls: Vec<OllamaToolCall> = tool_calls
                    .iter()
                    .map(|tc| {
                        let arguments = serde_json::from_str(&tc.arguments)
                            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                        OllamaToolCall {
                            function: OllamaFunction {
                                name: tc.name.clone(),
                                arguments,
                            },
                        }
                    })
                    .collect();
                result.push(OllamaMessage {
                    role: "assistant".to_string(),
                    content: cleaned,
                    images: None,
                    tool_calls: if ollama_tool_calls.is_empty() {
                        None
                    } else {
                        Some(ollama_tool_calls)
                    },
                });
            }
            ConversationMessage::ToolResults(results) => {
                for tr in results {
                    result.push(OllamaMessage {
                        role: "tool".to_string(),
                        content: tr.content.clone(),
                        images: None,
                        tool_calls: None,
                    });
                }
            }
        }
    }
    result
}

// ── JSON history reconstruction ─────────────────────────────────────────────
//
// The agent loop stores tool-call history in ChatMessage.content as
// JSON strings. These helpers parse that JSON back into native Ollama
// message structures so the model sees proper protocol messages.

/// Parse an assistant ChatMessage whose content is JSON-encoded tool calls.
///
/// Expected format from `build_native_assistant_history`:
/// ```json
/// {"content": "text or null", "tool_calls": [{"id":"..","name":"..","arguments":".."}]}
/// ```
///
/// The content may also have trailing text after the JSON object (e.g. when the
/// agent loop appends the model's follow-up text). We handle this by splitting
/// at the first valid JSON parse boundary.
fn try_parse_assistant_tool_call_json(content: &str) -> Option<OllamaMessage> {
    // The content may be pure JSON, or JSON followed by trailing text
    // (when the agent loop appends the model's follow-up to the tool-call JSON).
    // Try the whole string first, then look for a JSON object prefix.
    let trimmed = content.trim();
    let (value, trailing) = if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        (v, "")
    } else {
        // Find the end of the first JSON object by looking for the matching '}'
        let json_end = find_json_object_end(trimmed)?;
        let json_part = &trimmed[..json_end];
        let rest = trimmed[json_end..].trim();
        let v = serde_json::from_str::<serde_json::Value>(json_part).ok()?;
        (v, rest)
    };

    let tool_calls_value = value.get("tool_calls")?;
    let tool_calls_arr = tool_calls_value.as_array()?;

    let ollama_tool_calls: Vec<OllamaToolCall> = tool_calls_arr
        .iter()
        .filter_map(|tc| {
            let name = tc.get("name")?.as_str()?.to_string();
            let arguments_str = tc.get("arguments")?.as_str()?;
            let arguments: serde_json::Value = serde_json::from_str(arguments_str)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
            Some(OllamaToolCall {
                function: OllamaFunction { name, arguments },
            })
        })
        .collect();

    if ollama_tool_calls.is_empty() {
        return None;
    }

    // Combine the JSON "content" field with any trailing text
    let json_content = value
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let text_content = if trailing.is_empty() {
        json_content.to_string()
    } else if json_content.is_empty() {
        trailing.to_string()
    } else {
        format!("{json_content}\n{trailing}")
    };
    let (cleaned, _) = extract_thinking_content(&text_content, None);

    Some(OllamaMessage {
        role: "assistant".to_string(),
        content: cleaned,
        images: None,
        tool_calls: Some(ollama_tool_calls),
    })
}

/// Re-export of the shared JSON object boundary finder.
fn find_json_object_end(s: &str) -> Option<usize> {
    crate::agent::loop_::find_json_end(s)
}

/// Parse a tool-role ChatMessage whose content is JSON with `tool_call_id` and `content`.
///
/// Expected format:
/// ```json
/// {"tool_call_id": "ollama-tc-0", "content": "result text"}
/// ```
fn try_parse_tool_result_json(content: &str) -> Option<OllamaMessage> {
    let value: serde_json::Value = serde_json::from_str(content.trim()).ok()?;
    let inner_content = value.get("content")?.as_str()?;
    Some(OllamaMessage {
        role: "tool".to_string(),
        content: inner_content.to_string(),
        images: None,
        tool_calls: None,
    })
}

// ── Tool conversion ─────────────────────────────────────────────────────────

fn convert_tool_specs(tools: &[ToolSpec]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

// ── Tool call parsing ───────────────────────────────────────────────────────

fn parse_tool_calls(tool_calls: &[OllamaToolCall]) -> Vec<ProviderToolCall> {
    tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| {
            let arguments =
                serde_json::to_string(&tc.function.arguments).unwrap_or_else(|_| "{}".to_string());
            ProviderToolCall {
                id: format!("ollama-tc-{i}"),
                name: tc.function.name.clone(),
                arguments,
            }
        })
        .collect()
}

// ── Response parsing ────────────────────────────────────────────────────────

fn parse_chat_response(api_resp: ApiChatResponse) -> ProviderChatResponse {
    let usage = match (api_resp.prompt_eval_count, api_resp.eval_count) {
        (None, None) => None,
        _ => Some(TokenUsage {
            input_tokens: api_resp.prompt_eval_count,
            output_tokens: api_resp.eval_count,
            cached_input_tokens: None,
        }),
    };

    let msg = api_resp.message.unwrap_or(ResponseMessage {
        content: String::new(),
        tool_calls: None,
        thinking: None,
    });

    let tool_calls = msg
        .tool_calls
        .as_deref()
        .map(parse_tool_calls)
        .unwrap_or_default();

    let (text, reasoning_content) = extract_thinking_content(&msg.content, msg.thinking.as_deref());

    ProviderChatResponse {
        text: if text.is_empty() && !tool_calls.is_empty() {
            None
        } else {
            Some(text)
        },
        tool_calls,
        usage,
        reasoning_content,
        provider_attrs: None,
    }
}

// ── Error handling ──────────────────────────────────────────────────────────

async fn handle_ollama_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if let Ok(err_resp) = serde_json::from_str::<OllamaErrorResponse>(&body) {
        if !err_resp.error.is_empty() {
            return anyhow::anyhow!("Ollama API error ({status}): {}", err_resp.error);
        }
    }

    let sanitized = super::sanitize_api_error(&body);
    anyhow::anyhow!("Ollama API error ({status}): {sanitized}")
}

// ── Trace logging ──────────────────────────────────────────────────────────

/// Log the actual API request payload to the runtime trace for debugging.
fn trace_api_request(request: &OllamaChatRequest) {
    if let Ok(payload) = serde_json::to_value(request) {
        crate::observability::runtime_trace::record_event(
            "provider_api_request",
            None,
            Some("ollama"),
            Some(&request.model),
            None,
            None,
            None,
            payload,
        );
    }
}

// ── Provider trait implementation ───────────────────────────────────────────

#[async_trait]
impl Provider for OllamaProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            // Default to prompt-guided tool calling (XML instructions in system
            // prompt) because many Ollama-served models silently ignore the
            // native /api/chat tools array and need explicit text instructions.
            // See: https://github.com/zeroclaw-labs/zeroclaw/issues/3999
            native_tool_calling: false,
            vision: true,
            prompt_caching: false,
        }
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        let url = format!("{}/api/version", self.base_url);
        match self.http_client().get(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => {
                let status = resp.status();
                Err(anyhow::anyhow!(
                    "Ollama server at {} returned {status}. Is Ollama running?",
                    self.base_url
                ))
            }
            Err(e) => Err(anyhow::anyhow!(
                "Cannot reach Ollama at {}. Is Ollama running? Error: {e}",
                self.base_url
            )),
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let mut chat_messages = Vec::new();
        if let Some(sys) = system_prompt {
            chat_messages.push(ChatMessage::system(sys));
        }
        chat_messages.push(ChatMessage::user(message));
        let (messages, think) = self.prepare_messages(&chat_messages, model);

        let request = OllamaChatRequest {
            model: model.to_string(),
            messages,
            stream: false,
            tools: None,
            think,
            options: self.build_options(temperature),
        };

        let response = self
            .http_client()
            .post(format!("{}/api/chat", self.base_url))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(handle_ollama_error(response).await);
        }

        let api_resp: ApiChatResponse = response.json().await?;
        Ok(api_resp.message.map(|m| m.content).unwrap_or_default())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (ollama_messages, think) = self.prepare_messages(messages, model);

        let request = OllamaChatRequest {
            model: model.to_string(),
            messages: ollama_messages,
            stream: false,
            tools: None,
            think,
            options: self.build_options(temperature),
        };
        trace_api_request(&request);

        let response = self
            .http_client()
            .post(format!("{}/api/chat", self.base_url))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(handle_ollama_error(response).await);
        }

        let api_resp: ApiChatResponse = response.json().await?;
        Ok(api_resp.message.map(|m| m.content).unwrap_or_default())
    }

    // chat() intentionally NOT overridden: the trait default handles
    // prompt-guided tool injection when native_tool_calling is false.

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let (ollama_messages, think) = self.prepare_messages(messages, model);

        let api_request = OllamaChatRequest {
            model: model.to_string(),
            messages: ollama_messages,
            stream: false,
            tools: if tools.is_empty() {
                None
            } else {
                Some(tools.to_vec())
            },
            think,
            options: self.build_options(temperature),
        };
        trace_api_request(&api_request);

        let response = self
            .http_client()
            .post(format!("{}/api/chat", self.base_url))
            .json(&api_request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(handle_ollama_error(response).await);
        }

        let api_resp: ApiChatResponse = response.json().await?;
        Ok(parse_chat_response(api_resp))
    }

    // ── Streaming ───────────────────────────────────────────────────────────

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_streaming_tool_events(&self) -> bool {
        // With native_tool_calling disabled, tools are injected into the
        // system prompt as text. Streaming works fine for that path since
        // tools aren't in the API request — but the agent loop uses this
        // flag to decide whether to stream when tool specs are present.
        // Return false so the non-streaming chat() path handles prompt-guided
        // tool injection via the trait default.
        false
    }

    fn stream_chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
        _options: StreamOptions,
    ) -> futures_util::stream::BoxStream<'static, StreamResult<StreamEvent>> {
        let (ollama_messages, think) = self.prepare_messages(request.messages, model);
        let tools = request.tools.map(convert_tool_specs);

        let api_request = OllamaChatRequest {
            model: model.to_string(),
            messages: ollama_messages,
            stream: true,
            tools,
            think,
            options: self.build_options(temperature),
        };
        trace_api_request(&api_request);

        let client = self.http_client();
        let url = format!("{}/api/chat", self.base_url);
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamEvent>>(64);

        tokio::spawn(async move {
            let response = match client.post(&url).json(&api_request).send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let msg = if let Ok(err) = serde_json::from_str::<OllamaErrorResponse>(&body) {
                    if err.error.is_empty() {
                        body
                    } else {
                        err.error
                    }
                } else {
                    body
                };
                let _ = tx
                    .send(Err(StreamError::Provider(format!(
                        "Ollama API error ({status}): {msg}"
                    ))))
                    .await;
                return;
            }

            let mut stream = response.bytes_stream();
            let mut buffer = Vec::new();
            let mut accumulated_tool_calls: Vec<OllamaToolCall> = Vec::new();

            while let Some(chunk_result) = stream.next().await {
                let bytes = match chunk_result {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(StreamError::Http(e))).await;
                        return;
                    }
                };

                buffer.extend_from_slice(&bytes);

                // Process complete lines from buffer
                while let Some(newline_pos) = buffer.iter().position(|&b| b == b'\n') {
                    let line = buffer[..newline_pos].trim_ascii();
                    if line.is_empty() {
                        buffer.drain(..=newline_pos);
                        continue;
                    }

                    let chunk: ApiChatResponse = match serde_json::from_slice(line) {
                        Ok(c) => {
                            buffer.drain(..=newline_pos);
                            c
                        }
                        Err(e) => {
                            let _ = tx.send(Err(StreamError::Json(e))).await;
                            return;
                        }
                    };

                    if let Some(ref msg) = chunk.message {
                        // Accumulate tool calls
                        if let Some(ref tcs) = msg.tool_calls {
                            accumulated_tool_calls.extend(tcs.iter().cloned());
                        }

                        // Emit reasoning delta
                        if let Some(ref thinking) = msg.thinking {
                            if !thinking.is_empty()
                                && tx
                                    .send(Ok(StreamEvent::TextDelta(StreamChunk::reasoning(
                                        thinking.as_str(),
                                    ))))
                                    .await
                                    .is_err()
                            {
                                return;
                            }
                        }

                        // Emit text delta
                        if !msg.content.is_empty()
                            && tx
                                .send(Ok(StreamEvent::TextDelta(StreamChunk::delta(&msg.content))))
                                .await
                                .is_err()
                        {
                            return;
                        }
                    }

                    if chunk.done {
                        // Emit accumulated tool calls
                        let tool_calls = parse_tool_calls(&accumulated_tool_calls);
                        for tc in tool_calls {
                            if tx.send(Ok(StreamEvent::ToolCall(tc))).await.is_err() {
                                return;
                            }
                        }

                        let _ = tx.send(Ok(StreamEvent::Final)).await;
                        return;
                    }
                }
            }

            // If we exit without seeing done: true, still send Final
            if !accumulated_tool_calls.is_empty() {
                let tool_calls = parse_tool_calls(&accumulated_tool_calls);
                for tc in tool_calls {
                    if tx.send(Ok(StreamEvent::ToolCall(tc))).await.is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send(Ok(StreamEvent::Final)).await;
        });

        tokio_stream::wrappers::ReceiverStream::new(rx).boxed()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::ToolResultMessage;

    // ── build_options ───────────────────────────────────────────────────────

    #[test]
    fn build_options_default_values() {
        let provider = OllamaProvider::new();
        let opts = provider.build_options(0.7);
        assert!((opts.temperature - 0.7).abs() < f64::EPSILON);
        assert!((opts.top_p - 0.95).abs() < f64::EPSILON);
        assert_eq!(opts.top_k, 64);
        assert!(opts.num_predict.is_none());
    }

    #[test]
    fn build_options_with_max_tokens() {
        let provider = OllamaProvider::new().with_max_tokens(Some(4096));
        let opts = provider.build_options(1.0);
        assert_eq!(opts.num_predict, Some(4096));
    }

    #[test]
    fn build_options_temperature_override() {
        let provider = OllamaProvider::new();
        let opts = provider.build_options(0.0);
        assert!((opts.temperature - 0.0).abs() < f64::EPSILON);
        // top_p and top_k remain defaults
        assert!((opts.top_p - 0.95).abs() < f64::EPSILON);
        assert_eq!(opts.top_k, 64);
    }

    // ── convert_chat_messages ───────────────────────────────────────────────

    #[test]
    fn convert_chat_messages_basic_roles() {
        let messages = vec![
            ChatMessage::system("Be helpful"),
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there"),
        ];
        let converted = convert_chat_messages(&messages);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, "system");
        assert_eq!(converted[0].content, "Be helpful");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[1].content, "Hello");
        assert_eq!(converted[2].role, "assistant");
        assert_eq!(converted[2].content, "Hi there");
    }

    #[test]
    fn convert_chat_messages_user_with_images() {
        let msg = ChatMessage::user("Check this [IMAGE:data:image/png;base64,iVBORw0KGgo]");
        let converted = convert_chat_messages(&[msg]);
        assert_eq!(converted.len(), 1);
        assert!(converted[0].images.is_some());
        let images = converted[0].images.as_ref().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0], "iVBORw0KGgo");
        assert!(!converted[0].content.contains("[IMAGE:"));
    }

    #[test]
    fn convert_chat_messages_no_images_field_when_empty() {
        let msg = ChatMessage::user("Just text");
        let converted = convert_chat_messages(&[msg]);
        assert!(converted[0].images.is_none());
    }

    #[test]
    fn convert_chat_messages_multi_turn_round_trip() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("q1"),
            ChatMessage::assistant("a1"),
            ChatMessage::user("q2"),
            ChatMessage::assistant("a2"),
        ];
        let converted = convert_chat_messages(&messages);
        assert_eq!(converted.len(), 5);
        assert_eq!(converted[3].content, "q2");
        assert_eq!(converted[4].content, "a2");
    }

    // ── convert_conversation_messages ────────────────────────────────────────

    #[test]
    fn convert_conversation_messages_chat_variants() {
        let msgs = vec![
            ConversationMessage::Chat(ChatMessage::system("sys")),
            ConversationMessage::Chat(ChatMessage::user("hi")),
            ConversationMessage::Chat(ChatMessage::assistant("hello")),
        ];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, "system");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[2].role, "assistant");
    }

    #[test]
    fn convert_conversation_messages_assistant_tool_calls() {
        let msgs = vec![ConversationMessage::AssistantToolCalls {
            text: Some("Let me check".to_string()),
            tool_calls: vec![ProviderToolCall {
                id: "tc-1".to_string(),
                name: "shell".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
            }],
            reasoning_content: Some("thinking...".to_string()),
            provider_attrs: None,
        }];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[0].content, "Let me check");
        assert!(converted[0].tool_calls.is_some());
        let tcs = converted[0].tool_calls.as_ref().unwrap();
        assert_eq!(tcs[0].function.name, "shell");
    }

    #[test]
    fn convert_conversation_messages_tool_results() {
        let msgs = vec![ConversationMessage::ToolResults(vec![
            ToolResultMessage {
                tool_call_id: "tc-0".to_string(),
                content: "file1.txt".to_string(),
            },
            ToolResultMessage {
                tool_call_id: "tc-1".to_string(),
                content: "file2.txt".to_string(),
            },
        ])];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].role, "tool");
        assert_eq!(converted[0].content, "file1.txt");
        assert_eq!(converted[1].role, "tool");
        assert_eq!(converted[1].content, "file2.txt");
    }

    #[test]
    fn convert_conversation_messages_strips_reasoning_from_history() {
        let msgs = vec![ConversationMessage::Chat(ChatMessage::assistant(
            "<think>deep thought</think>The answer is 42".to_string(),
        ))];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted[0].content, "The answer is 42");
        assert!(!converted[0].content.contains("<think>"));
    }

    #[test]
    fn convert_conversation_messages_system_prompt_no_think_prefix() {
        // Message conversion itself never injects thinking tokens.
        // That happens at request-building time via inject_thinking_token().
        let msgs = vec![ConversationMessage::Chat(ChatMessage::system(
            "You are helpful".to_string(),
        ))];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted[0].content, "You are helpful");
        assert!(!converted[0].content.contains("<|think|>"));
    }

    // ── JSON history reconstruction ────────────────────────────────────────

    #[test]
    fn assistant_tool_call_json_is_reconstructed() {
        let json = r#"{"content":null,"tool_calls":[{"id":"ollama-tc-0","name":"use_skill","arguments":"{\"name\":\"hardcover\"}"}]}"#;
        let msgs = vec![ConversationMessage::Chat(ChatMessage::assistant(json))];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "assistant");
        assert!(converted[0].content.is_empty());
        let tcs = converted[0]
            .tool_calls
            .as_ref()
            .expect("should have tool_calls");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "use_skill");
        assert_eq!(tcs[0].function.arguments["name"], "hardcover");
    }

    #[test]
    fn assistant_tool_call_json_with_trailing_text() {
        let content = "{\"content\":null,\"tool_calls\":[{\"id\":\"ollama-tc-0\",\"name\":\"model_switch\",\"arguments\":\"{\\\"action\\\":\\\"get\\\"}\"}]}\n\nI am Skippy.";
        let msgs = vec![ConversationMessage::Chat(ChatMessage::assistant(content))];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted.len(), 1);
        let tcs = converted[0]
            .tool_calls
            .as_ref()
            .expect("should have tool_calls");
        assert_eq!(tcs[0].function.name, "model_switch");
        assert_eq!(converted[0].content, "I am Skippy.");
    }

    #[test]
    fn plain_assistant_text_not_parsed_as_tool_call() {
        let msgs = vec![ConversationMessage::Chat(ChatMessage::assistant(
            "Hello, I can help you.".to_string(),
        ))];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted[0].content, "Hello, I can help you.");
        assert!(converted[0].tool_calls.is_none());
    }

    #[test]
    fn tool_result_json_is_reconstructed() {
        let json = r#"{"tool_call_id":"ollama-tc-0","content":"[Skill 'hardcover' activated]\n\nInstructions here..."}"#;
        let msgs = vec![ConversationMessage::Chat(ChatMessage::tool(json))];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "tool");
        assert!(
            converted[0]
                .content
                .starts_with("[Skill 'hardcover' activated]")
        );
        assert!(!converted[0].content.contains("tool_call_id"));
    }

    #[test]
    fn plain_tool_result_passed_through() {
        let msgs = vec![ConversationMessage::Chat(ChatMessage::tool(
            "plain text result".to_string(),
        ))];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted[0].role, "tool");
        assert_eq!(converted[0].content, "plain text result");
    }

    #[test]
    fn find_json_object_end_basic() {
        assert_eq!(find_json_object_end(r#"{"a":1}"#), Some(7));
        assert_eq!(find_json_object_end(r#"{"a":1} trailing"#), Some(7));
        assert_eq!(find_json_object_end("not json"), None);
    }

    #[test]
    fn find_json_object_end_nested() {
        let s = r#"{"a":{"b":"c"},"d":[1,2]} rest"#;
        assert_eq!(find_json_object_end(s), Some(25));
    }

    #[test]
    fn find_json_object_end_escaped_braces_in_strings() {
        let s = r#"{"key":"val with \" and { } chars"} tail"#;
        assert_eq!(find_json_object_end(s), Some(35));
    }

    // ── convert_tool_specs ──────────────────────────────────────────────────

    #[test]
    fn convert_tool_specs_output_shape() {
        let tools = vec![ToolSpec {
            name: "shell".to_string(),
            description: "Run a command".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"}
                },
                "required": ["command"]
            }),
        }];
        let converted = convert_tool_specs(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["function"]["name"], "shell");
        assert_eq!(converted[0]["function"]["description"], "Run a command");
        assert!(converted[0]["function"]["parameters"]["properties"]["command"].is_object());
    }

    // ── parse_tool_calls ────────────────────────────────────────────────────

    #[test]
    fn parse_tool_calls_single() {
        let tcs = vec![OllamaToolCall {
            function: OllamaFunction {
                name: "shell".to_string(),
                arguments: serde_json::json!({"command": "ls -la"}),
            },
        }];
        let parsed = parse_tool_calls(&tcs);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "ollama-tc-0");
        assert_eq!(parsed[0].name, "shell");
        assert!(parsed[0].arguments.contains("ls -la"));
    }

    #[test]
    fn parse_tool_calls_parallel() {
        let tcs = vec![
            OllamaToolCall {
                function: OllamaFunction {
                    name: "shell".to_string(),
                    arguments: serde_json::json!({"command": "ls"}),
                },
            },
            OllamaToolCall {
                function: OllamaFunction {
                    name: "file_read".to_string(),
                    arguments: serde_json::json!({"path": "test.txt"}),
                },
            },
            OllamaToolCall {
                function: OllamaFunction {
                    name: "shell".to_string(),
                    arguments: serde_json::json!({"command": "pwd"}),
                },
            },
        ];
        let parsed = parse_tool_calls(&tcs);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].id, "ollama-tc-0");
        assert_eq!(parsed[1].id, "ollama-tc-1");
        assert_eq!(parsed[2].id, "ollama-tc-2");
        assert_eq!(parsed[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_empty() {
        let parsed = parse_tool_calls(&[]);
        assert!(parsed.is_empty());
    }

    // ── extract_thinking_content ────────────────────────────────────────────

    #[test]
    fn extract_thinking_dedicated_field() {
        let (text, reasoning) =
            extract_thinking_content("The answer is 42", Some("I need to think about this"));
        assert_eq!(text, "The answer is 42");
        assert_eq!(reasoning.as_deref(), Some("I need to think about this"));
    }

    #[test]
    fn extract_thinking_dedicated_field_wins_over_tags() {
        let (text, reasoning) = extract_thinking_content(
            "<think>inline thinking</think>answer",
            Some("dedicated thinking"),
        );
        // Dedicated field wins — content is NOT stripped of tags
        assert_eq!(text, "<think>inline thinking</think>answer");
        assert_eq!(reasoning.as_deref(), Some("dedicated thinking"));
    }

    #[test]
    fn extract_thinking_think_tags() {
        let (text, reasoning) =
            extract_thinking_content("<think>Let me reason</think>The answer is 42", None);
        assert_eq!(text, "The answer is 42");
        assert_eq!(reasoning.as_deref(), Some("Let me reason"));
    }

    #[test]
    fn extract_thinking_channel_format() {
        let content = "<|channel>thought\nI'm thinking about this\n<channel|>The answer is 42";
        let (text, reasoning) = extract_thinking_content(content, None);
        assert_eq!(text, "The answer is 42");
        assert_eq!(reasoning.as_deref(), Some("I'm thinking about this\n"));
    }

    #[test]
    fn extract_thinking_channel_wins_over_think_tags() {
        let content = "<|channel>thought\ngemma thinking\n<channel|><think>generic</think>answer";
        let (text, reasoning) = extract_thinking_content(content, None);
        // Channel format takes priority over <think> tags
        assert_eq!(reasoning.as_deref(), Some("gemma thinking\n"));
        assert!(text.contains("answer"));
    }

    #[test]
    fn extract_thinking_none_present() {
        let (text, reasoning) = extract_thinking_content("Just a normal answer", None);
        assert_eq!(text, "Just a normal answer");
        assert!(reasoning.is_none());
    }

    #[test]
    fn extract_thinking_empty_dedicated_field_falls_through() {
        let (text, reasoning) =
            extract_thinking_content("<think>fallback thinking</think>answer", Some(""));
        assert_eq!(text, "answer");
        assert_eq!(reasoning.as_deref(), Some("fallback thinking"));
    }

    #[test]
    fn extract_thinking_empty_think_tags() {
        let (text, reasoning) = extract_thinking_content("<think></think>Just answer", None);
        assert_eq!(text, "<think></think>Just answer");
        assert!(reasoning.is_none());
    }

    // ── User message image extraction ─────────────────────────────────────

    #[test]
    fn user_message_images_present() {
        let msgs = vec![ConversationMessage::Chat(ChatMessage::user(
            "Describe this [IMAGE:data:image/png;base64,AAAA]".to_string(),
        ))];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted.len(), 1);
        assert!(converted[0].images.is_some());
        assert_eq!(converted[0].images.as_ref().unwrap()[0], "AAAA");
    }

    // ── Tool call round-trip ────────────────────────────────────────────────

    #[test]
    fn tool_call_round_trip_in_conversation() {
        let msgs = vec![
            ConversationMessage::Chat(ChatMessage::user("List files")),
            ConversationMessage::AssistantToolCalls {
                text: None,
                tool_calls: vec![ProviderToolCall {
                    id: "ollama-tc-0".to_string(),
                    name: "shell".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                }],
                reasoning_content: None,
                provider_attrs: None,
            },
            ConversationMessage::ToolResults(vec![ToolResultMessage {
                tool_call_id: "ollama-tc-0".to_string(),
                content: "file1.txt\nfile2.txt".to_string(),
            }]),
        ];
        let converted = convert_conversation_messages(&msgs);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, "user");
        assert_eq!(converted[1].role, "assistant");
        assert!(converted[1].tool_calls.is_some());
        assert_eq!(converted[2].role, "tool");
        assert_eq!(converted[2].content, "file1.txt\nfile2.txt");
    }

    // ── NDJSON parsing ──────────────────────────────────────────────────────

    #[test]
    fn api_chat_response_non_streaming_parse() {
        let json = r#"{"message":{"role":"assistant","content":"Hello!","thinking":"hmm"},"done":true,"prompt_eval_count":42,"eval_count":15}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.done);
        assert_eq!(resp.prompt_eval_count, Some(42));
        assert_eq!(resp.eval_count, Some(15));
        let msg = resp.message.unwrap();
        assert_eq!(msg.content, "Hello!");
        assert_eq!(msg.thinking.as_deref(), Some("hmm"));
    }

    #[test]
    fn api_chat_response_streaming_chunk_parse() {
        let json = r#"{"message":{"role":"assistant","content":"Hello"},"done":false}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.done);
        assert!(resp.prompt_eval_count.is_none());
    }

    #[test]
    fn api_chat_response_streaming_final_chunk() {
        let json = r#"{"message":{"role":"assistant","content":""},"done":true,"prompt_eval_count":42,"eval_count":15}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.done);
        assert_eq!(resp.prompt_eval_count, Some(42));
        assert_eq!(resp.eval_count, Some(15));
    }

    #[test]
    fn api_chat_response_with_tool_calls() {
        let json = r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"shell","arguments":{"command":"ls"}}}]},"done":true,"prompt_eval_count":42,"eval_count":8}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = resp.message.unwrap();
        let tcs = msg.tool_calls.unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "shell");
        assert_eq!(tcs[0].function.arguments["command"], "ls");
    }

    #[test]
    fn api_chat_response_empty_message() {
        let json = r#"{"done":true}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.done);
        assert!(resp.message.is_none());
    }

    // ── Error response parsing ──────────────────────────────────────────────

    #[test]
    fn ollama_error_response_parse() {
        let json = r#"{"error":"model \"nonexistent\" not found, try pulling it first"}"#;
        let resp: OllamaErrorResponse = serde_json::from_str(json).unwrap();
        assert!(resp.error.contains("not found"));
    }

    // ── Ollama request serialization ────────────────────────────────────────

    #[test]
    fn ollama_chat_request_serializes_correctly() {
        let request = OllamaChatRequest {
            model: "llama3.3".to_string(),
            messages: vec![OllamaMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
                images: None,
                tool_calls: None,
            }],
            stream: false,
            tools: None,
            think: None,
            options: Options {
                temperature: 1.0,
                top_p: 0.95,
                top_k: 64,
                num_predict: None,
                num_ctx: None,
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"model\":\"llama3.3\""));
        assert!(json.contains("\"stream\":false"));
        assert!(!json.contains("\"tools\""));
        assert!(!json.contains("\"think\""));
    }

    #[test]
    fn ollama_chat_request_with_think() {
        let request = OllamaChatRequest {
            model: "deepseek-r1".to_string(),
            messages: vec![],
            stream: false,
            tools: None,
            think: Some(true),
            options: Options {
                temperature: 1.0,
                top_p: 0.95,
                top_k: 64,
                num_predict: None,
                num_ctx: None,
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"think\":true"));
    }

    // ── Provider capabilities ───────────────────────────────────────────────

    #[test]
    fn provider_capabilities_disable_native_tools() {
        let provider = OllamaProvider::new();
        let caps = provider.capabilities();
        assert!(
            !caps.native_tool_calling,
            "Ollama should default to prompt-guided tool calling"
        );
        assert!(caps.vision);
        assert!(!caps.prompt_caching);
    }

    #[test]
    fn provider_supports_streaming() {
        let provider = OllamaProvider::new();
        assert!(provider.supports_streaming());
        assert!(
            !provider.supports_streaming_tool_events(),
            "Ollama should not stream tool events (prompt-guided mode)"
        );
    }

    // ── Builder methods ─────────────────────────────────────────────────────

    #[test]
    fn provider_builder_methods() {
        let provider = OllamaProvider::new()
            .with_base_url("http://192.168.1.50:11434")
            .with_timeout_secs(Some(300))
            .with_max_tokens(Some(8192))
            .with_reasoning(Some(true));

        assert_eq!(provider.base_url, "http://192.168.1.50:11434");
        assert_eq!(provider.timeout_secs, Some(300));
        assert_eq!(provider.max_tokens, Some(8192));
        assert_eq!(provider.reasoning_enabled, Some(true));
    }

    #[test]
    fn provider_default_base_url() {
        let provider = OllamaProvider::new();
        assert_eq!(provider.base_url, "http://localhost:11434");
    }

    #[test]
    fn provider_base_url_strips_trailing_slash() {
        let provider = OllamaProvider::new().with_base_url("http://localhost:11434/");
        assert_eq!(provider.base_url, "http://localhost:11434");
    }

    // ── Options serialization ───────────────────────────────────────────────

    #[test]
    fn options_serializes_without_num_predict_when_none() {
        let opts = Options {
            temperature: 1.0,
            top_p: 0.95,
            top_k: 64,
            num_predict: None,
            num_ctx: None,
        };
        let json = serde_json::to_string(&opts).unwrap();
        assert!(!json.contains("num_predict"));
        assert!(!json.contains("num_ctx"));
    }

    #[test]
    fn options_serializes_with_num_predict() {
        let opts = Options {
            temperature: 1.0,
            top_p: 0.95,
            top_k: 64,
            num_predict: Some(4096),
            num_ctx: None,
        };
        let json = serde_json::to_string(&opts).unwrap();
        assert!(json.contains("\"num_predict\":4096"));
    }

    #[test]
    fn options_serializes_with_num_ctx() {
        let opts = Options {
            temperature: 1.0,
            top_p: 0.95,
            top_k: 64,
            num_predict: None,
            num_ctx: Some(32768),
        };
        let json = serde_json::to_string(&opts).unwrap();
        assert!(json.contains("\"num_ctx\":32768"));
    }

    // ── Model-family detection & thinking token injection ───────────────────

    #[test]
    fn is_gemma_model_positive() {
        assert!(is_gemma_model("gemma4"));
        assert!(is_gemma_model("gemma3"));
        assert!(is_gemma_model("gemma4:27b"));
        assert!(is_gemma_model("Gemma4"));
        assert!(is_gemma_model("GEMMA3:latest"));
    }

    #[test]
    fn is_gemma_model_negative() {
        assert!(!is_gemma_model("deepseek-r1"));
        assert!(!is_gemma_model("llama3.3"));
        assert!(!is_gemma_model("qwen3:32b"));
        assert!(!is_gemma_model("my-gemma-finetune"));
    }

    #[test]
    fn inject_thinking_token_prepends_to_system() {
        let mut messages = vec![
            OllamaMessage {
                role: "system".to_string(),
                content: "You are helpful".to_string(),
                images: None,
                tool_calls: None,
            },
            OllamaMessage {
                role: "user".to_string(),
                content: "Hi".to_string(),
                images: None,
                tool_calls: None,
            },
        ];
        inject_thinking_token(&mut messages);
        assert!(messages[0].content.starts_with("<|think|>\n"));
        assert!(messages[0].content.contains("You are helpful"));
        assert_eq!(messages[1].content, "Hi");
    }

    #[test]
    fn inject_thinking_token_noop_without_system() {
        let mut messages = vec![OllamaMessage {
            role: "user".to_string(),
            content: "Hi".to_string(),
            images: None,
            tool_calls: None,
        }];
        inject_thinking_token(&mut messages);
        assert_eq!(messages[0].content, "Hi");
    }

    // ── prepare_messages integration ────────────────────────────────────────

    #[test]
    fn prepare_messages_gemma_with_reasoning_injects_token() {
        let provider = OllamaProvider::new().with_reasoning(Some(true));
        let messages = vec![
            ChatMessage::system("You are helpful"),
            ChatMessage::user("Hello"),
        ];
        let (ollama_msgs, think) = provider.prepare_messages(&messages, "gemma4");
        assert!(
            ollama_msgs[0].content.starts_with("<|think|>\n"),
            "system prompt should start with <|think|> for gemma: {}",
            &ollama_msgs[0].content[..30]
        );
        assert_eq!(think, Some(true));
    }

    #[test]
    fn prepare_messages_gemma_without_reasoning_no_token() {
        let provider = OllamaProvider::new();
        let messages = vec![
            ChatMessage::system("You are helpful"),
            ChatMessage::user("Hello"),
        ];
        let (ollama_msgs, think) = provider.prepare_messages(&messages, "gemma4");
        assert!(
            !ollama_msgs[0].content.contains("<|think|>"),
            "should not inject token when reasoning is disabled"
        );
        assert_eq!(think, None);
    }

    #[test]
    fn prepare_messages_non_gemma_with_reasoning_no_token() {
        let provider = OllamaProvider::new().with_reasoning(Some(true));
        let messages = vec![
            ChatMessage::system("You are helpful"),
            ChatMessage::user("Hello"),
        ];
        let (ollama_msgs, think) = provider.prepare_messages(&messages, "deepseek-r1");
        assert!(
            !ollama_msgs[0].content.contains("<|think|>"),
            "should not inject token for non-gemma models"
        );
        assert_eq!(think, Some(true));
    }
}
