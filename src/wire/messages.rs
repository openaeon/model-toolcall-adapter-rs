use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AdapterError;
use crate::types::{ParsedToolCall, UnifiedContent, UnifiedMessage, UnifiedRequest};
use crate::wire::chat;

pub fn parse_request(payload: Value) -> Result<UnifiedRequest, AdapterError> {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let max_tokens = payload
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(1024) as u32;
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let system = payload
        .get("system")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let tools = chat::parse_tools(payload.get("tools"));
    let tool_choice = chat::parse_tool_choice(payload.get("tool_choice"));
    let parallel_tool_calls = payload
        .get("parallel_tool_calls")
        .and_then(Value::as_bool)
        .unwrap_or(false);
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
        messages.push(UnifiedMessage {
            role,
            content: parse_content(message.get("content")),
        });
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

pub fn response(
    request: &UnifiedRequest,
    model_text: &str,
    tool_calls: &[ParsedToolCall],
) -> Value {
    let content = if tool_calls.is_empty() {
        json!([{ "type": "text", "text": model_text }])
    } else {
        json!(tool_calls
            .iter()
            .map(|call| {
                let input =
                    serde_json::from_str::<Value>(&call.arguments).unwrap_or_else(|_| json!({}));
                json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": input
                })
            })
            .collect::<Vec<_>>())
    };
    json!({
        "id": format!("msg_{}", Uuid::new_v4()),
        "type": "message",
        "role": "assistant",
        "model": request.model,
        "content": content,
        "stop_reason": if tool_calls.is_empty() { "end_turn" } else { "tool_use" },
        "stop_sequence": null,
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0
        }
    })
}

fn parse_content(value: Option<&Value>) -> Vec<UnifiedContent> {
    match value {
        Some(Value::String(text)) => vec![UnifiedContent::Text { text: text.clone() }],
        Some(Value::Array(parts)) => parts.iter().filter_map(parse_part).collect(),
        Some(other) => vec![UnifiedContent::Text {
            text: other.to_string(),
        }],
        None => Vec::new(),
    }
}

fn parse_part(part: &Value) -> Option<UnifiedContent> {
    match part.get("type").and_then(Value::as_str).unwrap_or("text") {
        "text" => Some(UnifiedContent::Text {
            text: part.get("text").and_then(Value::as_str)?.to_string(),
        }),
        "image" | "image_url" => Some(UnifiedContent::ImageUrl {
            url: part
                .get("url")
                .or_else(|| part.pointer("/source/url"))
                .or_else(|| part.pointer("/image_url/url"))
                .and_then(Value::as_str)?
                .to_string(),
        }),
        "tool_use" => Some(UnifiedContent::ToolUse {
            id: part.get("id").and_then(Value::as_str)?.to_string(),
            name: part.get("name").and_then(Value::as_str)?.to_string(),
            input: part.get("input").cloned().unwrap_or_else(|| json!({})),
        }),
        "tool_result" => Some(UnifiedContent::ToolResult {
            tool_use_id: part
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("call_unknown")
                .to_string(),
            content: chat::extract_chat_content(part.get("content")),
            is_error: part
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_request, response};
    use crate::types::ParsedToolCall;

    #[test]
    fn parses_messages_tool_schema() {
        let request = parse_request(json!({
            "model": "m",
            "max_tokens": 20,
            "system": "policy",
            "messages": [{"role":"user","content":[{"type":"text","text":"hi"}]}],
            "tools": [{"name":"search","input_schema":{"type":"object"}}]
        }))
        .unwrap();

        assert_eq!(request.system.as_deref(), Some("policy"));
        assert_eq!(request.tools[0].name, "search");
    }

    #[test]
    fn emits_messages_tool_use() {
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

        assert_eq!(body["stop_reason"], "tool_use");
        assert_eq!(body["content"][0]["type"], "tool_use");
    }
}
