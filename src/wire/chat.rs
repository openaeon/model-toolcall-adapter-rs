use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AdapterError;
use crate::types::{
    ParsedToolCall, ToolChoice, ToolDefinition, UnifiedContent, UnifiedMessage, UnifiedRequest,
};

pub fn parse_request(payload: Value) -> Result<UnifiedRequest, AdapterError> {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let max_tokens = payload
        .get("max_tokens")
        .or_else(|| payload.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(1024) as u32;
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let tools = parse_tools(payload.get("tools"));
    let tool_choice = parse_tool_choice(payload.get("tool_choice"));
    let parallel_tool_calls = payload
        .get("parallel_tool_calls")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut system = None;
    let mut messages = Vec::new();

    for message in payload
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| AdapterError::InvalidRequest("messages must be an array".to_string()))?
    {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_string();
        if role == "system" {
            system = Some(extract_chat_content(message.get("content")));
            continue;
        }
        if role == "tool" {
            let id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("call_unknown")
                .to_string();
            messages.push(UnifiedMessage {
                role: "user".to_string(),
                content: vec![UnifiedContent::ToolResult {
                    tool_use_id: id,
                    content: extract_chat_content(message.get("content")),
                    is_error: false,
                }],
            });
            continue;
        }
        let mut content = Vec::new();
        let text = extract_chat_content(message.get("content"));
        if !text.trim().is_empty() {
            content.push(UnifiedContent::Text { text });
        }
        if role == "assistant" {
            content.extend(parse_chat_tool_calls(message.get("tool_calls")));
        }
        messages.push(UnifiedMessage { role, content });
    }

    Ok(UnifiedRequest {
        model,
        max_tokens,
        system,
        messages,
        tools,
        tool_choice,
        parallel_tool_calls,
        stream,
        background: false,
        previous_response_id: None,
    })
}

fn parse_chat_tool_calls(value: Option<&Value>) -> Vec<UnifiedContent> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool_call| {
            let function = tool_call.get("function").unwrap_or(tool_call);
            Some(UnifiedContent::ToolUse {
                id: tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown")
                    .to_string(),
                name: function.get("name").and_then(Value::as_str)?.to_string(),
                input: function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .or_else(|| function.get("arguments").cloned())
                    .unwrap_or_else(|| json!({})),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_request, response};
    use crate::types::ParsedToolCall;

    #[test]
    fn parses_assistant_tool_call_history() {
        let request = parse_request(json!({
            "model": "m",
            "messages": [
                {"role":"assistant","tool_calls":[{
                    "id":"call_a",
                    "type":"function",
                    "function":{"name":"search","arguments":"{\"q\":\"rs\"}"}
                }]},
                {"role":"tool","tool_call_id":"call_a","content":"ok"}
            ]
        }))
        .unwrap();

        assert_eq!(
            request.messages[0].content_text(),
            r#"<previous_tool_call id="call_a" name="search">{"q":"rs"}</previous_tool_call>"#
        );
        assert!(request.messages[1].content_text().contains("tool_result"));
    }

    #[test]
    fn emits_chat_tool_calls() {
        let request = parse_request(json!({
            "model": "m",
            "messages": [{"role":"user","content":"hi"}]
        }))
        .unwrap();
        let body = response(
            &request,
            "",
            &[ParsedToolCall {
                id: "call_a".to_string(),
                name: "search".to_string(),
                arguments: r#"{"q":"rs"}"#.to_string(),
            }],
        );

        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "search"
        );
    }
}

pub fn response(
    request: &UnifiedRequest,
    model_text: &str,
    tool_calls: &[ParsedToolCall],
) -> Value {
    let finish_reason = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let mut message = json!({
        "role": "assistant",
        "content": if tool_calls.is_empty() { model_text } else { "" },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments
                    }
                })
            })
            .collect::<Vec<_>>());
    }
    json!({
        "id": format!("chatcmpl-{}", Uuid::new_v4()),
        "object": "chat.completion",
        "created": Utc::now().timestamp(),
        "model": request.model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason
        }],
        "usage": null
    })
}

pub(crate) fn parse_tools(value: Option<&Value>) -> Vec<ToolDefinition> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            let name = function.get("name")?.as_str()?.to_string();
            Some(ToolDefinition {
                name,
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                input_schema: function
                    .get("parameters")
                    .or_else(|| function.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object" })),
            })
        })
        .collect()
}

pub(crate) fn parse_tool_choice(value: Option<&Value>) -> ToolChoice {
    match value {
        Some(Value::String(value)) if value.eq_ignore_ascii_case("none") => ToolChoice::None,
        Some(Value::String(value)) if value.eq_ignore_ascii_case("required") => {
            ToolChoice::Required
        }
        Some(Value::String(value)) if value.eq_ignore_ascii_case("auto") => ToolChoice::Auto,
        Some(Value::Object(object)) => object
            .get("function")
            .and_then(|function| function.get("name"))
            .or_else(|| object.get("name"))
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Function {
                name: name.to_string(),
            })
            .unwrap_or(ToolChoice::Auto),
        _ => ToolChoice::Auto,
    }
}

pub(crate) fn extract_chat_content(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    Some(text.to_string())
                } else {
                    part.pointer("/image_url/url")
                        .and_then(Value::as_str)
                        .map(|url| format!("[Image: {url}]"))
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}
