use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use anyhow::Result;
use async_channel::Sender;
use futures_util::StreamExt;

use crate::config::{Role, XenonConfig};
use crate::events::{AgentAction, PetState, ToolCall, ToolResult};
use crate::provider::{ChatMessage, ChatRequest, ChatRole, ContentPart, MessageContent, Provider};
use crate::tools::{tools_to_schemas, Notes, Tool};

/// Appended to the system prompt so the model knows the computer-control
/// contract: it must look before acting, and every action returns a fresh
/// screenshot for verification.
fn tool_hint(tools: &[Box<dyn Tool>], vision: bool) -> String {
    let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    names.sort_unstable();
    let verify = if vision {
        "Every action returns a fresh screenshot so you can verify the result."
    } else {
        "This model cannot see screenshots. Use list_windows, frontmost_app and browser_read \
         for text descriptions, and each tool's summary to verify the result."
    };
    format!(
        "\n\nYou have computer-control tools: {}. \
         Call look to see the screen. Describe click targets in words (e.g. \"the blue \
         Spotify icon\"). {verify} Keep going until the user's request is done, then call done \
         with a short summary (or fail with the reason).",
        names.join(", ")
    )
}

/// Prompt protocol for models without native tool calling (many local GGUFs):
/// one JSON action per turn inside a fenced block.
fn action_protocol_hint(tools: &[Box<dyn Tool>], vision: bool) -> String {
    let mut s = String::from(
        "\n\nTOOL PROTOCOL (this model has no native tool calling). Reply with exactly one \
         JSON object per turn, inside a fenced ```json block, and nothing else:\n\
         {\"thought\": \"<one short sentence>\", \"tool\": \"<tool name>\", \"args\": { ... }}\n\
         Available tools:\n",
    );
    for tool in tools {
        s.push_str(&format!(
            "- {}: {}\n  args: {}\n",
            tool.name(),
            tool.description(),
            tool.schema()
        ));
    }
    s.push_str(if vision {
        "Call \"done\" with a short summary when the task is complete, or \"fail\" with the \
         reason if it cannot be done. Wait for the screenshot result before the next action."
    } else {
        "This model cannot see screenshots. Use list_windows, frontmost_app and browser_read \
         for text descriptions, and each tool's summary to verify. Call \"done\" with a short \
         summary when the task is complete, or \"fail\" with the reason if it cannot be done."
    });
    s
}

/// Remove Qwen3-style `<think>…</think>` reasoning (and an unterminated
/// `<think>` block) plus `<|…|>` control tokens, so only the answer is left.
pub fn strip_think(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<think>".len()..];
        match after.find("</think>") {
            Some(end) => rest = &after[end + "</think>".len()..],
            // Unterminated: the model is still thinking, drop the rest.
            None => return out,
        }
    }
    out.push_str(rest);

    let mut cleaned = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' && chars.peek() == Some(&'|') {
            // Skip to the closing '|>'.
            let it = chars.by_ref();
            let mut closed = false;
            while let Some(ch) = it.next() {
                if ch == '|' && it.peek() == Some(&'>') {
                    it.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                cleaned.push_str("<|");
            }
            continue;
        }
        cleaned.push(c);
    }
    cleaned
}

/// One action parsed out of a model reply in the text protocol.
#[derive(Debug, Clone, PartialEq)]
pub struct TextAction {
    pub name: String,
    pub args: serde_json::Value,
}

/// Parse the text-action protocol from a raw model reply.
///
/// Accepts a fenced ```json block or a bare object, with or without
/// surrounding prose, in these shapes:
/// `{"tool": "click", "args": {...}}`, `{"name": …, "arguments": {...}}`,
/// `{"action": {"tool": …, "args": …}}`, and the flat
/// `{"thought": …, "tool": "scroll", "direction": "down"}`.
/// Returns `None` when there is no action (the reply is speech).
pub fn parse_text_action(response: &str) -> Option<TextAction> {
    let cleaned = strip_think(response);

    // Prefer the last fenced block: models often echo the protocol example
    // first and their real action after it.
    let body = fenced_blocks(&cleaned).last().cloned().unwrap_or_else(|| cleaned.clone());

    let json_str = match (body.find('{'), body.rfind('}')) {
        (Some(a), Some(b)) if b > a => body[a..=b].to_string(),
        _ => return None,
    };

    let value: serde_json::Value = serde_json::from_str(&json_str).ok()?;
    action_from_value(&value)
}

/// ```json … ``` (or ``` … ```) block bodies, in order.
fn fenced_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let after = after
            .strip_prefix("json")
            .or_else(|| after.strip_prefix("JSON"))
            .unwrap_or(after);
        match after.find("```") {
            Some(end) => {
                let block = after[..end].trim();
                if !block.is_empty() {
                    blocks.push(block.to_string());
                }
                rest = &after[end + 3..];
            }
            None => break,
        }
    }
    blocks
}

fn action_from_value(value: &serde_json::Value) -> Option<TextAction> {
    let obj = value.as_object()?;

    // `{"action": {…}}` wraps the real action one level down.
    if let Some(inner @ serde_json::Value::Object(_)) = obj.get("action") {
        if obj.len() == 1 || obj.get("tool").is_none() {
            return action_from_value(inner);
        }
    }

    let name = ["tool", "action", "name"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();

    // Explicit args container wins.
    for key in ["args", "arguments", "parameters"] {
        if let Some(args) = obj.get(key) {
            if args.is_object() {
                return Some(TextAction { name, args: args.clone() });
            }
        }
    }

    // Flat form: every key that is not protocol metadata is an argument.
    let mut args = serde_json::Map::new();
    for (k, v) in obj {
        if matches!(k.as_str(), "thought" | "tool" | "action" | "name" | "reasoning") {
            continue;
        }
        args.insert(k.clone(), v.clone());
    }
    Some(TextAction { name, args: serde_json::Value::Object(args) })
}

/// Text to actually speak: the reply with any fenced block removed.
pub fn speakable_text(response: &str) -> String {
    let cleaned = strip_think(response);
    let mut out = String::with_capacity(cleaned.len());
    let mut rest = cleaned.as_str();
    while let Some(start) = rest.find("```") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        match after.find("```") {
            Some(end) => rest = &after[end + 3..],
            None => break,
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A model that does not implement `tools` answers 400 with a message like
/// "unsupported parameter: tools". Detect that so the turn can fall back to
/// the JSON text protocol instead of failing.
fn is_tool_capability_error(err: &anyhow::Error) -> bool {
    let m = err.to_string().to_ascii_lowercase();
    let mentions_tools = m.contains("tool") || m.contains("function");
    let says_unsupported = m.contains("support")
        || m.contains("invalid")
        || m.contains("unknown")
        || m.contains("not found")
        || m.contains("unrecognized")
        || m.contains("must be");
    mentions_tools && says_unsupported
}

/// True when the provider's error text says it cannot accept image parts
/// (OpenAI-compatible servers answer 400/415/5xx with these phrases). A false
/// positive just retries once without images and stops.
fn is_image_unsupported_error(err: &anyhow::Error) -> bool {
    let m = err.to_string().to_ascii_lowercase();
    m.contains("image")
        || m.contains("vision")
        || m.contains("multimodal")
        || m.contains("invalid content type")
        || m.contains("unsupported content type")
}

pub struct Agent {
    /// Agent-role provider: the loop itself.
    provider: Arc<dyn Provider>,
    /// Chat-role provider: planning / plain chat.
    chat_provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    max_iterations: usize,
    config: XenonConfig,
    /// Set by the Cmd+Shift+Escape abort hotkey to stop the turn early.
    abort: Arc<AtomicBool>,
    /// False when the model has no native tool calling (text protocol).
    native_tools: bool,
    /// Whether the agent-role model accepts image parts.
    vision: AtomicBool,
    /// One-time notice when a text-only model first gets a screenshot result.
    vision_notice: AtomicBool,
    /// Task-scoped notes written by the `remember` tool.
    notes: Notes,
}

impl Agent {
    /// Convenience constructor with a fresh abort flag and no notes. Test-only;
    /// production uses `with_roles`.
    #[cfg(test)]
    pub fn new(provider: Arc<dyn Provider>, tools: Vec<Box<dyn Tool>>, config: XenonConfig) -> Self {
        Self::with_abort(provider, tools, config, Arc::new(AtomicBool::new(false)))
    }

    /// Test-only constructor with an explicit abort flag and no notes;
    /// production uses `with_roles`.
    #[cfg(test)]
    pub fn with_abort(
        provider: Arc<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        config: XenonConfig,
        abort: Arc<AtomicBool>,
    ) -> Self {
        Self::with_roles(
            provider.clone(),
            provider,
            tools,
            config,
            abort,
            Arc::new(Mutex::new(Vec::new())),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_roles(
        provider: Arc<dyn Provider>,
        chat_provider: Arc<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        config: XenonConfig,
        abort: Arc<AtomicBool>,
        notes: Notes,
    ) -> Self {
        let native_tools = !config.role_lacks_tools(Role::Agent);
        let vision = config.role_profile(Role::Agent).supports_vision;
        Self {
            provider,
            chat_provider,
            tools,
            max_iterations: config.max_steps.max(1),
            config,
            abort,
            native_tools,
            vision: AtomicBool::new(vision),
            vision_notice: AtomicBool::new(false),
            notes,
        }
    }

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::SeqCst)
    }

    fn model(&self, role: Role) -> String {
        self.config.role_profile(role).model
    }

    /// One planning call: a short numbered plan, spoken once before the first
    /// action. Failure is not fatal — the agent just starts acting.
    async fn make_plan(&self, task: &str) -> Option<String> {
        let req = ChatRequest {
            model: self.model(Role::Chat),
            messages: vec![
                ChatMessage::text(ChatRole::System, self.config.system_prompt.clone()),
                ChatMessage::text(
                    ChatRole::User,
                    format!(
                        "Make a short numbered plan (at most 4 steps) for this task. \
                         Reply with the plan only.\n\nTask: {task}"
                    ),
                ),
            ],
            tools: None,
            stream: false,
            temperature: Some(0.2),
            max_tokens: Some(160),
        };
        match self.chat_provider.chat(req).await {
            Ok(plan) if !plan.trim().is_empty() => Some(speakable_text(&plan)),
            Ok(_) => None,
            Err(e) => {
                log::warn!("planning call failed: {e}");
                None
            }
        }
    }

    /// One model turn: streams speech when `native` (tool-call mode) and
    /// always accumulates the raw text. Returns the text and any tool calls.
    async fn chat_turn(
        &self,
        messages: &[ChatMessage],
        native: bool,
        emit: &Sender<AgentAction>,
    ) -> Result<(String, Vec<ToolCall>)> {
        let schemas = tools_to_schemas(&self.tools);
        let req = ChatRequest {
            model: self.model(Role::Agent),
            messages: messages.to_vec(),
            tools: if native { Some(schemas) } else { None },
            stream: true,
            temperature: Some(0.8),
            max_tokens: None,
        };

        let mut stream = self.provider.chat_stream(req).await?;
        let mut buffer = String::new();
        let mut full_content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        while let Some(chunk) = stream.next().await {
            if self.aborted() {
                break;
            }
            let chunk = chunk?;
            if !chunk.content.is_empty() {
                full_content.push_str(&chunk.content);
                if native {
                    buffer.push_str(&chunk.content);
                    for sentence in split_sentences(&mut buffer) {
                        let _ = emit.send(AgentAction::Speak(sentence)).await;
                    }
                }
            }
            if !chunk.tool_calls.is_empty() {
                tool_calls = chunk.tool_calls;
            }
        }

        if native {
            let remainder = buffer.trim().to_string();
            if !remainder.is_empty() {
                let _ = emit.send(AgentAction::Speak(remainder)).await;
            }
        }

        Ok((full_content, tool_calls))
    }

    /// Execute one tool call, emitting the pet's Agentic state and result.
    async fn run_tool(&self, tool_call: &ToolCall, emit: &Sender<AgentAction>) -> ToolResult {
        let _ = emit
            .send(AgentAction::ExecuteTool(tool_call.clone()))
            .await;

        let result = match self.tools.iter().find(|t| t.name() == tool_call.name) {
            Some(tool) => match tool.execute(tool_call.args.clone()).await {
                Ok(mut r) => {
                    r.call_id = tool_call.call_id.clone();
                    r
                }
                Err(e) => ToolResult {
                    call_id: tool_call.call_id.clone(),
                    result: serde_json::json!({}),
                    is_error: true,
                    error_message: Some(e.to_string()),
                    image: None,
                },
            },
            None => ToolResult {
                call_id: tool_call.call_id.clone(),
                result: serde_json::json!({}),
                is_error: true,
                error_message: Some(format!("Tool not found: {}", tool_call.name)),
                image: None,
            },
        };

        let _ = emit.send(AgentAction::ToolResult(result.clone())).await;
        result
    }

    /// Run a full ReAct turn for *text*, streaming sentences to *emit* as
    /// they are recognised. Tool errors are fed back into the conversation
    /// so the model can react; the loop caps at `max_steps`. If the abort
    /// flag is set, the turn stops immediately.
    pub async fn run_utterance(&self, text: &str, emit: Sender<AgentAction>) -> Result<()> {
        let _ = emit
            .send(AgentAction::SetPetState(PetState::Thinking))
            .await;

        let vision = self.vision.load(Ordering::SeqCst);
        let mut system = self.config.system_prompt.clone();
        if self.native_tools {
            system.push_str(&tool_hint(&self.tools, vision));
        } else {
            system.push_str(&action_protocol_hint(&self.tools, vision));
        }

        let mut messages = vec![
            ChatMessage::text(ChatRole::System, system),
            ChatMessage::text(ChatRole::User, text.to_string()),
        ];

        // Native tool calling is decided by the profile's capability flag, but
        // a model that rejects `tools` (400) drops to the text protocol for
        // the rest of the turn instead of failing.
        let mut native = self.native_tools;
        let mut seen_notes = 0usize;
        let mut planned = false;

        for _ in 0..self.max_iterations {
            let vision = self.vision.load(Ordering::SeqCst);
            if self.aborted() {
                log::info!("agent turn aborted");
                break;
            }

            if !planned {
                planned = true;
                if let Some(plan) = self.make_plan(text).await {
                    let _ = emit.send(AgentAction::Speak(plan.clone())).await;
                    messages.push(ChatMessage::text(ChatRole::Assistant, plan));
                }
            }

            // Notes written by `remember` are re-injected when they change.
            let notes = self.notes.lock().unwrap().clone();
            if notes.len() != seen_notes {
                seen_notes = notes.len();
                messages.push(ChatMessage::text(
                    ChatRole::System,
                    format!("Notes you have taken so far: {}", notes.join("; ")),
                ));
            }
            drop(notes);

            let (content, tool_calls) = match self.chat_turn(&messages, native, &emit).await {
                Ok(turn) => turn,
                Err(e) => {
                    if native && is_tool_capability_error(&e) {
                        log::info!("model rejected tool calling; using the text protocol");
                        native = false;
                        continue;
                    }
                    if self.vision.load(Ordering::SeqCst) && is_image_unsupported_error(&e) {
                        self.vision.store(false, Ordering::SeqCst);
                        log::warn!(
                            "provider reports images unsupported; dropping images for this \
                             task and retrying without them"
                        );
                        if !self.vision_notice.swap(true, Ordering::SeqCst) {
                            let _ = emit
                                .send(AgentAction::Speak(
                                    "This provider doesn't support images, so I'll continue \
                                     without pictures for this task."
                                        .to_string(),
                                ))
                                .await;
                        }
                        // Strip image parts from the conversation and retry the turn.
                        for m in messages.iter_mut() {
                            if let MessageContent::Parts(parts) = &mut m.content {
                                let had_image = parts
                                    .iter()
                                    .any(|p| matches!(p, ContentPart::ImageUrl { .. }));
                                if had_image {
                                    let text = parts
                                        .iter()
                                        .find_map(|p| match p {
                                            ContentPart::Text { text } => Some(text.clone()),
                                            _ => None,
                                        })
                                        .unwrap_or_else(|| "Sent a screenshot earlier.".into());
                                    let role = m.role.clone();
                                    *m = ChatMessage::text(role, text);
                                }
                            }
                        }
                        continue;
                    }
                    return Err(e);
                }
            };
            let content = content.trim().to_string();

            if native {
                if let Some(tool_call) = tool_calls.into_iter().next() {
                    self.push_assistant(&mut messages, &content, &tool_call);
                    let result = self.run_tool(&tool_call, &emit).await;
                    self.notice_text_only(&emit, &result).await;
                    let terminal = matches!(tool_call.name.as_str(), "done" | "fail");
                    push_tool_result(&mut messages, &tool_call, &result, vision)?;
                    if terminal {
                        speak_terminal(&emit, &tool_call.name, &result).await;
                        break;
                    }
                    continue;
                }
                // No tool call: the streamed text was the final reply.
                break;
            }

            // Text protocol: one JSON action per turn, anything else is speech.
            match parse_text_action(&content) {
                Some(action) => {
                    let tool_call = ToolCall {
                        call_id: uuid::Uuid::new_v4().to_string(),
                        name: action.name,
                        args: action.args,
                    };
                    messages.push(ChatMessage::text(ChatRole::Assistant, content.clone()));
                    let result = self.run_tool(&tool_call, &emit).await;
                    self.notice_text_only(&emit, &result).await;
                    let terminal = matches!(tool_call.name.as_str(), "done" | "fail");
                    push_tool_result(&mut messages, &tool_call, &result, vision)?;
                    if terminal {
                        speak_terminal(&emit, &tool_call.name, &result).await;
                        break;
                    }
                    continue;
                }
                None => {
                    let said = speakable_text(&content);
                    if !said.is_empty() {
                        let _ = emit.send(AgentAction::Speak(said)).await;
                    }
                    break;
                }
            }
        }

        let _ = emit.send(AgentAction::TurnFinished).await;
        Ok(())
    }

    /// Tell the user once that this model cannot see the screenshots that
    /// other tools still return, and that the agent is using text instead.
    async fn notice_text_only(&self, emit: &Sender<AgentAction>, result: &ToolResult) {
        if self.vision.load(Ordering::SeqCst) || result.image.is_none() {
            return;
        }
        if !self.vision_notice.swap(true, Ordering::SeqCst) {
            let _ = emit
                .send(AgentAction::Speak(
                    "This model can't see screenshots, so I'll use text descriptions from the \
                     tools instead."
                        .to_string(),
                ))
                .await;
        }
    }

    fn push_assistant(&self, messages: &mut Vec<ChatMessage>, content: &str, call: &ToolCall) {
        let text = if content.is_empty() {
            format!("Called tool {} with args: {}", call.name, call.args)
        } else {
            content.to_string()
        };
        messages.push(ChatMessage::text(ChatRole::Assistant, text));
    }
}

/// Feed a tool result (and its verification screenshot) back into the
/// conversation. The reflection is folded into the next turn rather than
/// costing a call of its own: the model is asked to check the screenshot and
/// then act.
fn push_tool_result(
    messages: &mut Vec<ChatMessage>,
    tool_call: &ToolCall,
    result: &ToolResult,
    vision: bool,
) -> Result<()> {
    let result_json = if result.is_error {
        serde_json::json!({ "error": result.error_message })
    } else {
        result.result.clone()
    };
    messages.push(ChatMessage::text(
        ChatRole::Tool,
        serde_json::to_string(&result_json)?,
    ));

    if vision {
        if let Some(shot) = &result.image {
            let summary = result.result["summary"]
                .as_str()
                .unwrap_or("Screenshot after the action.")
                .to_string();
            messages.push(ChatMessage::user_with_image(
                format!(
                    "{summary} (tool: {}). Check it: did the screen change as expected? \
                     Then take the next action.",
                    tool_call.name
                ),
                shot.data_uri(),
            ));
        }
    }
    Ok(())
}

async fn speak_terminal(emit: &Sender<AgentAction>, name: &str, result: &ToolResult) {
    let said = result.result["summary"]
        .as_str()
        .or_else(|| result.result["reason"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{name}: {}", result.result));
    let said = if name == "fail" && !said.starts_with("I ") {
        format!("I couldn't finish: {said}")
    } else {
        said
    };
    if !said.trim().is_empty() {
        let _ = emit.send(AgentAction::Speak(said)).await;
    }
}

/// Split *buffer* at sentence boundaries, returning complete sentences and
/// leaving the incomplete tail in *buffer*.
///
/// Boundaries are `.` / `!` / `?` followed by whitespace (or end-of-input)
/// and newlines.  Decimals (`3.14`), common abbreviations (`Mr.`, `Dr.`)
/// and single-letter initials (`J.`) are skipped.
fn split_sentences(buffer: &mut String) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut sentence_start = 0;
    {
        let bytes = buffer.as_bytes();
        for i in 0..bytes.len() {
            if is_sentence_end(bytes, i) {
                let s = buffer[sentence_start..=i].trim().to_string();
                if !s.is_empty() {
                    sentences.push(s);
                }
                sentence_start = i + 1;
            }
        }
    }
    let remainder = buffer[sentence_start..].to_string();
    buffer.clear();
    buffer.push_str(&remainder);
    sentences
}

fn is_sentence_end(bytes: &[u8], i: usize) -> bool {
    let b = bytes[i];
    if b == b'\n' {
        return true;
    }
    if b != b'.' && b != b'!' && b != b'?' {
        return false;
    }

    // Must be at end or followed by whitespace / closing quote.
    match bytes.get(i + 1) {
        None => {}
        Some(&c) if c == b' ' || c == b'\t' || c == b'\n' || c == b'"' => {}
        Some(_) => return false,
    }

    if b == b'.' {
        // Decimal: digit.digit
        let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
        let next_digit = bytes
            .get(i + 1)
            .map(|&c| c.is_ascii_digit())
            .unwrap_or(false);
        if prev_digit && next_digit {
            return false;
        }

        // Walk back to find the word before the period.
        let mut ws = i;
        while ws > 0 && bytes[ws - 1].is_ascii_alphanumeric() {
            ws -= 1;
        }
        let word = &bytes[ws..i];
        // Single-letter initial (J. Smith).
        if word.len() == 1 {
            return false;
        }
        // Common abbreviations.
        const ABBREVS: &[&[u8]] = &[
            b"mr", b"mrs", b"ms", b"dr", b"prof", b"sr", b"jr", b"st", b"vs", b"etc",
        ];
        let lower: Vec<u8> = word.iter().map(|b| b.to_ascii_lowercase()).collect();
        if ABBREVS.contains(&lower.as_slice()) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatChunk, ChatRequest, ChatStream};
    use async_trait::async_trait;
    use serde_json::Value;

    // ---- sentence splitter --------------------------------------------------

    #[test]
    fn test_split_simple() {
        let mut buf = "Hello world.".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["Hello world."]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_split_two_sentences() {
        let mut buf = "Hello world. How are you?".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["Hello world.", "How are you?"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_split_decimal_not_boundary() {
        let mut buf = "3.14 is pi.".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["3.14 is pi."]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_split_abbreviation_not_boundary() {
        let mut buf = "Mr. Smith went home.".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["Mr. Smith went home."]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_split_newline_boundary() {
        let mut buf = "Hello\nWorld".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["Hello"]);
        assert_eq!(buf, "World");
    }

    #[test]
    fn test_split_no_boundary_buffers_all() {
        let mut buf = "No ending period".to_string();
        let s = split_sentences(&mut buf);
        assert!(s.is_empty());
        assert_eq!(buf, "No ending period");
    }

    // ---- text-action protocol ----------------------------------------------

    #[test]
    fn test_strip_think_removes_reasoning() {
        let raw = "<think>I should click OK.</think>{\"tool\":\"click\"}";
        assert_eq!(strip_think(raw), "{\"tool\":\"click\"}");
    }

    #[test]
    fn test_strip_think_handles_unterminated_block() {
        let raw = "Answer first.<think>still thinking";
        assert_eq!(strip_think(raw), "Answer first.");
    }

    #[test]
    fn test_strip_think_removes_control_tokens() {
        let raw = "<|im_start|>assistant\n{\"tool\":\"look\"}<|im_end|>";
        assert_eq!(strip_think(raw), "assistant\n{\"tool\":\"look\"}");
    }

    #[test]
    fn test_parse_plain_fenced_action() {
        let raw = "I will click it.\n```json\n{\"tool\": \"click\", \"args\": {\"target\": \"OK\"}}\n```";
        let a = parse_text_action(raw).unwrap();
        assert_eq!(a.name, "click");
        assert_eq!(a.args["target"], "OK");
    }

    #[test]
    fn test_parse_after_qwen_think_block() {
        let raw = "<think>Need to type.</think>\n{\"thought\": \"typing\", \"tool\": \"type_text\", \"args\": {\"text\": \"hi\"}}";
        let a = parse_text_action(raw).unwrap();
        assert_eq!(a.name, "type_text");
        assert_eq!(a.args["text"], "hi");
    }

    #[test]
    fn test_parse_tolerates_trailing_prose() {
        let raw = "```json\n{\"tool\": \"wait\", \"args\": {\"ms\": 500}}\n```\nLet me know if you need more.";
        let a = parse_text_action(raw).unwrap();
        assert_eq!(a.name, "wait");
        assert_eq!(a.args["ms"], 500);
    }

    #[test]
    fn test_parse_name_and_arguments_shape() {
        let raw = r#"{"name": "scroll", "arguments": {"direction": "down", "amount": 2}}"#;
        let a = parse_text_action(raw).unwrap();
        assert_eq!(a.name, "scroll");
        assert_eq!(a.args["amount"], 2);
    }

    #[test]
    fn test_parse_nested_action_shape() {
        let raw = r#"{"action": {"tool": "open", "args": {"app": "Safari"}}}"#;
        let a = parse_text_action(raw).unwrap();
        assert_eq!(a.name, "open");
        assert_eq!(a.args["app"], "Safari");
    }

    #[test]
    fn test_parse_flat_shape_collects_remaining_keys() {
        let raw = r#"{"thought": "scroll down", "tool": "scroll", "direction": "down"}"#;
        let a = parse_text_action(raw).unwrap();
        assert_eq!(a.name, "scroll");
        assert_eq!(a.args["direction"], "down");
        assert!(a.args.get("thought").is_none());
    }

    #[test]
    fn test_parse_takes_last_fenced_block() {
        let raw = "Example:\n```json\n{\"tool\": \"click\", \"args\": {\"target\": \"example\"}}\n```\nNow the real one:\n```json\n{\"tool\": \"click\", \"args\": {\"target\": \"real\"}}\n```";
        let a = parse_text_action(raw).unwrap();
        assert_eq!(a.args["target"], "real");
    }

    #[test]
    fn test_parse_plain_prose_is_none() {
        assert!(parse_text_action("I opened the app for you.").is_none());
    }

    #[test]
    fn test_parse_json_without_a_tool_name_is_none() {
        assert!(parse_text_action(r#"{"foo": "bar"}"#).is_none());
    }

    #[test]
    fn test_speakable_text_drops_fences_and_reasoning() {
        let raw = "<think>hmm</think>Sure.\n```json\n{\"tool\":\"look\"}\n```";
        assert_eq!(speakable_text(raw), "Sure.");
    }

    // ---- agent streaming ----------------------------------------------------

    struct MockProvider {
        chunks: Vec<Vec<ChatChunk>>,
        call_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
            use std::sync::atomic::Ordering;
            let idx = self.call_count.fetch_add(1, Ordering::Relaxed);
            let chunks = self.chunks.get(idx).cloned().unwrap_or_else(|| {
                vec![ChatChunk {
                    content: "done".to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".to_string()),
                }]
            });
            Ok(Box::pin(futures::stream::iter(
                chunks.into_iter().map(Ok),
            )))
        }

        async fn chat(&self, _req: ChatRequest) -> Result<String> {
            Ok("{}".to_string())
        }
    }

    struct MockTool {
        name: String,
        result: Value,
        is_error: bool,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "Mock tool"
        }

        fn schema(&self) -> Value {
            serde_json::json!({})
        }

        async fn execute(&self, _args: Value) -> Result<ToolResult> {
            Ok(ToolResult {
                call_id: "test".to_string(),
                result: self.result.clone(),
                is_error: self.is_error,
                error_message: if self.is_error {
                    Some("mock error".to_string())
                } else {
                    None
                },
                image: None,
            })
        }
    }

    #[tokio::test]
    async fn test_agent_handles_tool_call() {
        let provider = Arc::new(MockProvider {
            chunks: vec![
                vec![ChatChunk {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        name: "test_tool".to_string(),
                        args: serde_json::json!({"key": "value"}),
                        call_id: "call_1".to_string(),
                    }],
                    finish_reason: Some("tool_calls".to_string()),
                }],
                vec![
                    ChatChunk {
                        content: "Tool executed successfully".to_string(),
                        tool_calls: vec![],
                        finish_reason: None,
                    },
                    ChatChunk {
                        content: ".".to_string(),
                        tool_calls: vec![],
                        finish_reason: Some("stop".to_string()),
                    },
                ],
            ],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let tool = Box::new(MockTool {
            name: "test_tool".to_string(),
            result: serde_json::json!({"status": "ok"}),
            is_error: false,
        });

        let agent = Agent::new(provider, vec![tool], crate::config::XenonConfig::default());
        let (tx, rx) = async_channel::bounded(32);

        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::ExecuteTool(_))));
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::Speak(s) if s == "Tool executed successfully.")));
        assert!(actions.iter().any(|a| matches!(a, AgentAction::TurnFinished)));
    }

    #[tokio::test]
    async fn test_agent_handles_direct_response() {
        let provider = Arc::new(MockProvider {
            chunks: vec![vec![
                ChatChunk {
                    content: "Direct answer".to_string(),
                    tool_calls: vec![],
                    finish_reason: None,
                },
                ChatChunk {
                    content: ".".to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".to_string()),
                },
            ]],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let agent = Agent::new(provider, vec![], crate::config::XenonConfig::default());
        let (tx, rx) = async_channel::bounded(32);

        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::Speak(s) if s == "Direct answer.")));
    }

    #[tokio::test]
    async fn test_agent_max_iterations() {
        let provider = Arc::new(MockProvider {
            chunks: vec![vec![ChatChunk {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    name: "test_tool".to_string(),
                    args: serde_json::json!({}),
                    call_id: "call_1".to_string(),
                }],
                finish_reason: Some("tool_calls".to_string()),
            }]],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let tool = Box::new(MockTool {
            name: "test_tool".to_string(),
            result: serde_json::json!({"status": "ok"}),
            is_error: false,
        });

        let agent = Agent::new(provider, vec![tool], crate::config::XenonConfig::default());
        let (tx, rx) = async_channel::bounded(32);

        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        assert!(actions.iter().any(|a| matches!(a, AgentAction::Speak(_))));
        assert!(actions.iter().any(|a| matches!(a, AgentAction::TurnFinished)));
    }

    /// When the abort flag is already set, the agent must not call the
    /// provider and must still emit TurnFinished so the pet returns to Idle.
    #[tokio::test]
    async fn test_agent_aborts_without_provider_call() {
        let provider = Arc::new(MockProvider {
            chunks: vec![],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let flag = Arc::new(AtomicBool::new(true));
        let agent = Agent::with_abort(provider, vec![], crate::config::XenonConfig::default(), flag);
        let (tx, rx) = async_channel::bounded(32);

        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        assert!(actions.iter().any(|a| matches!(a, AgentAction::TurnFinished)));
        assert!(!actions.iter().any(|a| matches!(a, AgentAction::Speak(_))));
    }

    /// A tool that errors must still produce a TurnFinished so the pet
    /// does not get stuck in Thinking.
    #[tokio::test]
    async fn test_tool_error_fed_back_and_turn_finishes() {
        let provider = Arc::new(MockProvider {
            chunks: vec![
                vec![ChatChunk {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        name: "test_tool".to_string(),
                        args: serde_json::json!({}),
                        call_id: "call_1".to_string(),
                    }],
                    finish_reason: Some("tool_calls".to_string()),
                }],
                vec![ChatChunk {
                    content: "I see the error.".to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".to_string()),
                }],
            ],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let tool = Box::new(MockTool {
            name: "test_tool".to_string(),
            result: serde_json::json!({}),
            is_error: true,
        });

        let agent = Agent::new(provider, vec![tool], crate::config::XenonConfig::default());
        let (tx, rx) = async_channel::bounded(32);

        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        // ToolResult with is_error must be present
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::ToolResult(tr) if tr.is_error)));
        // The loop must have continued after the error and produced speech
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::Speak(s) if s.contains("error"))));
        // And the turn must have finished
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::TurnFinished)));
    }

    /// Models without native tool calling: the reply is JSON, parsed and run,
    /// and the turn ends on `done`.
    #[tokio::test]
    async fn test_agent_text_protocol_runs_action_then_done() {
        let provider = Arc::new(MockProvider {
            chunks: vec![
                vec![ChatChunk {
                    content: "```json\n{\"tool\": \"test_tool\", \"args\": {}}\n```".to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".to_string()),
                }],
                vec![ChatChunk {
                    content: "```json\n{\"tool\": \"done\", \"args\": {\"summary\": \"all set\"}}\n```"
                        .to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".to_string()),
                }],
            ],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let mut config = crate::config::XenonConfig::default();
        config.providers = vec![crate::config::ProviderProfile {
            supports_tools: false,
            ..crate::config::ProviderProfile::new("local", "http://x/v1", None, "m", "m")
        }];
        config.roles.agent = "local".to_string();

        let tool = Box::new(MockTool {
            name: "test_tool".to_string(),
            result: serde_json::json!({"status": "ok"}),
            is_error: false,
        });
        let done = Box::new(MockTool {
            name: "done".to_string(),
            result: serde_json::json!({"summary": "all set"}),
            is_error: false,
        });

        let agent = Agent::new(provider, vec![tool, done], config);
        let (tx, rx) = async_channel::bounded(32);
        agent.run_utterance("Do it", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, AgentAction::ExecuteTool(_)))
                .count(),
            2
        );
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::Speak(s) if s.contains("all set"))));
        assert!(actions.iter().any(|a| matches!(a, AgentAction::TurnFinished)));
    }

    /// A model that rejects `tools` (400) falls back to the text protocol
    /// instead of failing the turn.
    #[tokio::test]
    async fn test_agent_falls_back_when_tools_unsupported() {
        struct RejectToolsProvider {
            calls: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl Provider for RejectToolsProvider {
            async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
                if req.tools.is_some() {
                    return Err(anyhow::anyhow!(
                        "API request failed: 400 - unsupported parameter: tools"
                    ));
                }
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                    content: "text protocol works".to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".to_string()),
                })])))
            }

            async fn chat(&self, _req: ChatRequest) -> Result<String> {
                Ok(String::new())
            }
        }

        let provider = Arc::new(RejectToolsProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let agent = Agent::new(provider, vec![], crate::config::XenonConfig::default());
        let (tx, rx) = async_channel::bounded(32);
        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::Speak(s) if s.contains("text protocol"))));
    }

    /// `remember` notes are fed back into the next turn.
    #[tokio::test]
    async fn test_agent_feeds_notes_back() {
        let notes: Notes = Arc::new(Mutex::new(Vec::new()));
        notes.lock().unwrap().push("user prefers dark mode".to_string());

        let provider = Arc::new(MockProvider {
            chunks: vec![vec![ChatChunk {
                content: "ok".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
            }]],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let agent = Agent::with_roles(
            provider.clone(),
            provider.clone(),
            vec![],
            crate::config::XenonConfig::default(),
            Arc::new(AtomicBool::new(false)),
            notes,
        );
        let (tx, _rx) = async_channel::bounded(32);
        agent.run_utterance("Test", tx).await.unwrap();
        assert_eq!(provider.call_count.load(Ordering::Relaxed), 1);
    }
}