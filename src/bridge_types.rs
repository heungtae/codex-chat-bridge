use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug)]
pub(crate) struct BridgeRequest {
    pub(crate) chat_request: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponsesToolCallKind {
    Function,
    Custom,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatChunk {
    #[allow(dead_code)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) choices: Vec<ChatChoice>,
    #[serde(default)]
    pub(crate) usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatChoice {
    #[serde(default)]
    pub(crate) delta: Option<ChatDelta>,
    #[allow(dead_code)]
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatDelta {
    #[serde(default)]
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) reasoning_content: Option<String>,
    #[serde(default)]
    pub(crate) reasoning_details: Option<Value>,
    #[serde(default)]
    pub(crate) thinking_blocks: Option<Value>,
    #[serde(default)]
    pub(crate) signature: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_empty_tool_calls")]
    pub(crate) tool_calls: Option<Vec<ChatToolCallDelta>>,
}

fn deserialize_non_empty_tool_calls<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ChatToolCallDelta>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let tool_calls = Option::<Vec<ChatToolCallDelta>>::deserialize(deserializer)?;
    Ok(tool_calls.filter(|calls| !calls.is_empty()))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatToolCallDelta {
    #[serde(default)]
    pub(crate) index: Option<usize>,
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatFunctionDelta {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) arguments: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct ChatUsage {
    #[serde(default)]
    pub(crate) prompt_tokens: i64,
    #[serde(default)]
    pub(crate) completion_tokens: i64,
    #[serde(default)]
    pub(crate) total_tokens: i64,
}

#[derive(Debug, Default)]
pub(crate) struct ToolCallAccumulator {
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) arguments: String,
    pub(crate) added_emitted: bool,
}

#[derive(Debug, Default)]
pub(crate) struct StreamAccumulator {
    pub(crate) assistant_text: String,
    pub(crate) reasoning_text: String,
    pub(crate) tool_calls: BTreeMap<usize, ToolCallAccumulator>,
    pub(crate) usage: Option<ChatUsage>,
}

#[derive(Debug, Default)]
pub(crate) struct SseParser {
    pub(crate) buffer: String,
    pub(crate) current_event_name: Option<String>,
    pub(crate) current_data_lines: Vec<String>,
}
