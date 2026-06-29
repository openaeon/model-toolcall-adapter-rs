use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AdapterError;
use crate::wire::{chat, messages, responses, WireMode};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: Option<String>,
    pub messages: Vec<UnifiedMessage>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoice,
    pub parallel_tool_calls: bool,
    pub stream: bool,
    pub background: bool,
    pub previous_response_id: Option<String>,
}

impl UnifiedRequest {
    pub fn from_wire_payload(mode: WireMode, payload: Value) -> Result<Self, AdapterError> {
        match mode {
            WireMode::ChatCompletions => chat::parse_request(payload),
            WireMode::Messages => messages::parse_request(payload),
            WireMode::Responses => responses::parse_request(payload),
        }
    }

    pub fn render_prompt_with_tool_protocol(&self, protocol: &str) -> String {
        let mut sections = Vec::new();
        let tool_protocol = (!self.tools.is_empty()).then(|| {
            format!(
                "<tool_protocol priority=\"critical\">\n{}\n</tool_protocol>",
                render_tool_choice_prompt(protocol, &self.tool_choice, self.parallel_tool_calls)
            )
        });
        if let Some(protocol) = tool_protocol.as_deref() {
            sections.push(protocol.to_string());
        }
        if let Some(system) = self
            .system
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            sections.push(format!("<system>\n{system}\n</system>"));
        }
        for message in &self.messages {
            sections.push(format!("{}: {}", message.role, message.content_text()));
        }
        if self.tool_choice.allows_tools() && !self.tools.is_empty() {
            sections.push(
                "<adapter_reminder priority=\"critical\">If the next correct step needs real project state, commands, files, browser, skills, MCP resources, or runtime information, output one <tool_call> now and no prose. After tool results, either call the next required tool or give the final answer. Match the user's language.</adapter_reminder>"
                    .to_string(),
            );
        }
        sections.join("\n\n")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedMessage {
    pub role: String,
    pub content: Vec<UnifiedContent>,
}

impl UnifiedMessage {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: vec![UnifiedContent::Text { text: text.into() }],
        }
    }

    pub fn content_text(&self) -> String {
        self.content
            .iter()
            .map(UnifiedContent::as_prompt_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UnifiedContent {
    Text {
        text: String,
    },
    ImageUrl {
        url: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

impl UnifiedContent {
    fn as_prompt_text(&self) -> String {
        match self {
            Self::Text { text } => text.clone(),
            Self::ImageUrl { url } => format!("[Image: {url}]"),
            Self::ToolUse { id, name, input } => {
                format!("<previous_tool_call id=\"{id}\" name=\"{name}\">{input}</previous_tool_call>")
            }
            Self::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => format!(
                "<tool_result id=\"{tool_use_id}\" is_error=\"{is_error}\">\n{content}\n</tool_result>"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Function { name: String },
}

impl Default for ToolChoice {
    fn default() -> Self {
        Self::Auto
    }
}

impl ToolChoice {
    pub fn allows_tools(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn requires_tool(&self) -> bool {
        matches!(self, Self::Required | Self::Function { .. })
    }

    pub fn required_name(&self) -> Option<&str> {
        match self {
            Self::Function { name } => Some(name.as_str()),
            _ => None,
        }
    }

    pub fn to_wire_value(&self) -> Value {
        match self {
            Self::Auto => Value::String("auto".to_string()),
            Self::None => Value::String("none".to_string()),
            Self::Required => Value::String("required".to_string()),
            Self::Function { name } => serde_json::json!({
                "type": "function",
                "name": name,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

fn render_tool_choice_prompt(protocol: &str, tool_choice: &ToolChoice, parallel: bool) -> String {
    let mut prompt = protocol.to_string();
    match tool_choice {
        ToolChoice::Auto => {
            prompt.push_str("\nTool choice: auto. Use a tool only when it is needed.");
        }
        ToolChoice::None => {
            prompt.push_str("\nTool choice: none. Do not call tools. Answer directly.");
        }
        ToolChoice::Required => {
            prompt.push_str("\nTool choice: required. You must call one available tool.");
        }
        ToolChoice::Function { name } => {
            prompt.push_str(&format!(
                "\nTool choice: required function `{name}`. You must call this tool name exactly."
            ));
        }
    }
    if parallel {
        prompt.push_str("\nParallel tool calls: allowed when independent.");
    } else {
        prompt.push_str("\nParallel tool calls: disabled. Emit at most one tool call.");
    }
    prompt
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ToolChoice, ToolDefinition, UnifiedContent, UnifiedMessage, UnifiedRequest};

    #[test]
    fn renders_tool_protocol_before_system_and_adds_final_reminder() {
        let request = UnifiedRequest {
            model: "m".to_string(),
            max_tokens: 16,
            system: Some("system prompt".to_string()),
            messages: vec![UnifiedMessage {
                role: "user".to_string(),
                content: vec![UnifiedContent::Text {
                    text: "看看当前项目".to_string(),
                }],
            }],
            tools: vec![ToolDefinition {
                name: "exec_command".to_string(),
                description: Some("run command".to_string()),
                input_schema: json!({"type":"object"}),
            }],
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: false,
            stream: false,
            background: false,
            previous_response_id: None,
        };

        let prompt = request.render_prompt_with_tool_protocol("protocol");

        assert!(prompt.starts_with("<tool_protocol priority=\"critical\">"));
        assert!(prompt.contains("<system>"));
        assert!(prompt.ends_with("</adapter_reminder>"));
    }
}
