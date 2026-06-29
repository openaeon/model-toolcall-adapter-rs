use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use uuid::Uuid;

use crate::types::{UnifiedContent, UnifiedMessage, UnifiedRequest};

#[derive(Clone, Default)]
pub struct ResponseStore {
    inner: Arc<Mutex<HashMap<String, StoredResponse>>>,
}

#[derive(Clone)]
struct StoredResponse {
    body: Value,
    input_items: Vec<Value>,
    context_items: Vec<Value>,
    output_items: Vec<Value>,
    background: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    NotFound,
    NotBackground,
}

impl ResponseStore {
    pub fn insert(
        &self,
        body: Value,
        input_items: Vec<Value>,
        context_items: Vec<Value>,
        background: bool,
    ) {
        let Some(id) = body
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            return;
        };
        let output_items = body
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut guard = self.inner.lock().expect("response store mutex poisoned");
        guard.insert(
            id,
            StoredResponse {
                body,
                input_items,
                context_items,
                output_items,
                background,
            },
        );
    }

    pub fn retrieve(&self, response_id: &str) -> Option<Value> {
        self.inner
            .lock()
            .expect("response store mutex poisoned")
            .get(response_id)
            .map(|entry| entry.body.clone())
    }

    pub fn context_items_for(&self, response_id: &str) -> Option<Vec<Value>> {
        self.inner
            .lock()
            .expect("response store mutex poisoned")
            .get(response_id)
            .map(|entry| {
                let mut items = entry
                    .context_items
                    .iter()
                    .filter(|item| {
                        let role = item.get("role").and_then(Value::as_str);
                        !matches!(role, Some("system" | "developer"))
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                items.extend(entry.output_items.clone());
                items
            })
    }

    pub fn list_input_items(
        &self,
        response_id: &str,
        after: Option<&str>,
        limit: Option<usize>,
        ascending: bool,
    ) -> Option<Value> {
        let entry = self
            .inner
            .lock()
            .expect("response store mutex poisoned")
            .get(response_id)?
            .clone();
        let mut items = entry.input_items;
        if !ascending {
            items.reverse();
        }
        let start_index = after
            .and_then(|after_id| {
                items
                    .iter()
                    .position(|item| item.get("id").and_then(Value::as_str) == Some(after_id))
            })
            .map(|index| index + 1)
            .unwrap_or(0);
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let slice = items
            .into_iter()
            .skip(start_index)
            .take(limit)
            .collect::<Vec<_>>();
        let first_id = slice
            .first()
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let last_id = slice
            .last()
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        Some(json!({
            "object": "list",
            "data": slice,
            "first_id": first_id,
            "last_id": last_id,
            "has_more": false,
        }))
    }

    pub fn cancel(&self, response_id: &str) -> Result<Value, StoreError> {
        let mut guard = self.inner.lock().expect("response store mutex poisoned");
        let entry = guard.get_mut(response_id).ok_or(StoreError::NotFound)?;
        if !entry.background {
            return Err(StoreError::NotBackground);
        }
        if let Some(status) = entry.body.get_mut("status") {
            *status = Value::String("cancelled".to_string());
        }
        Ok(entry.body.clone())
    }
}

pub fn input_items_from_request(request: &UnifiedRequest) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(system) = request
        .system
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        items.push(json!({
            "id": format!("msg_{}", Uuid::new_v4()),
            "type": "message",
            "role": "developer",
            "status": "completed",
            "content": [{ "type": "input_text", "text": system }],
        }));
    }

    for message in &request.messages {
        items.extend(message_to_items(message));
    }
    items
}

fn message_to_items(message: &UnifiedMessage) -> Vec<Value> {
    if message.content.len() == 1 {
        match &message.content[0] {
            UnifiedContent::ToolUse { .. } | UnifiedContent::ToolResult { .. } => {
                return vec![content_to_item(&message.content[0])];
            }
            _ => {}
        }
    }
    vec![message_to_item(message)]
}

fn message_to_item(message: &UnifiedMessage) -> Value {
    let id = format!("msg_{}", Uuid::new_v4());
    let content = message
        .content
        .iter()
        .map(content_to_item)
        .collect::<Vec<_>>();
    json!({
        "id": id,
        "type": "message",
        "role": message.role,
        "status": "completed",
        "content": content,
    })
}

fn content_to_item(content: &UnifiedContent) -> Value {
    match content {
        UnifiedContent::Text { text } => json!({
            "type": "input_text",
            "text": text,
        }),
        UnifiedContent::ImageUrl { url } => json!({
            "type": "input_image",
            "image_url": url,
        }),
        UnifiedContent::ToolUse { id, name, input } => json!({
            "id": format!("fc_{}", Uuid::new_v4()),
            "type": "function_call",
            "status": "completed",
            "call_id": id,
            "name": name,
            "arguments": input.to_string(),
        }),
        UnifiedContent::ToolResult {
            tool_use_id,
            content,
            is_error: _,
        } => json!({
            "id": format!("fco_{}", Uuid::new_v4()),
            "type": "function_call_output",
            "call_id": tool_use_id,
            "output": content,
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{input_items_from_request, ResponseStore, StoreError};
    use crate::types::{UnifiedContent, UnifiedMessage, UnifiedRequest};

    #[test]
    fn converts_request_to_input_items_with_ids() {
        let request = UnifiedRequest {
            model: "m".to_string(),
            max_tokens: 16,
            system: Some("policy".to_string()),
            messages: vec![UnifiedMessage {
                role: "user".to_string(),
                content: vec![UnifiedContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: Vec::new(),
            tool_choice: Default::default(),
            parallel_tool_calls: false,
            stream: false,
            background: false,
            previous_response_id: None,
        };

        let items = input_items_from_request(&request);

        assert_eq!(items.len(), 2);
        assert!(items[0]["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(items[1]["role"], "user");
    }

    #[test]
    fn rejects_cancelling_non_background_response() {
        let store = ResponseStore::default();
        store.insert(
            json!({
                "id": "resp_1",
                "object": "response",
                "status": "completed"
            }),
            vec![],
            vec![],
            false,
        );

        assert_eq!(store.cancel("resp_1"), Err(StoreError::NotBackground));
    }

    #[test]
    fn separates_current_input_items_from_context_items() {
        let store = ResponseStore::default();
        store.insert(
            json!({
                "id": "resp_1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "id": "msg_out",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type":"output_text","text":"hello"}]
                }]
            }),
            vec![json!({
                "id": "msg_current",
                "type": "message",
                "role": "user",
                "content": [{"type":"input_text","text":"current"}]
            })],
            vec![
                json!({
                    "id": "msg_previous",
                    "type": "message",
                    "role": "user",
                    "content": [{"type":"input_text","text":"previous"}]
                }),
                json!({
                    "id": "msg_current",
                    "type": "message",
                    "role": "user",
                    "content": [{"type":"input_text","text":"current"}]
                }),
            ],
            false,
        );

        let listed = store
            .list_input_items("resp_1", None, None, true)
            .expect("input items");
        let context = store.context_items_for("resp_1").expect("context items");

        assert_eq!(listed["data"].as_array().unwrap().len(), 1);
        assert_eq!(context.len(), 3);
        assert_eq!(context[0]["id"], "msg_previous");
        assert_eq!(context[2]["id"], "msg_out");
    }

    #[test]
    fn stores_tool_items_as_top_level_response_items() {
        let request = UnifiedRequest {
            model: "m".to_string(),
            max_tokens: 16,
            system: None,
            messages: vec![
                UnifiedMessage {
                    role: "assistant".to_string(),
                    content: vec![UnifiedContent::ToolUse {
                        id: "call_a".to_string(),
                        name: "search".to_string(),
                        input: json!({"q":"rs"}),
                    }],
                },
                UnifiedMessage {
                    role: "user".to_string(),
                    content: vec![UnifiedContent::ToolResult {
                        tool_use_id: "call_a".to_string(),
                        content: "ok".to_string(),
                        is_error: false,
                    }],
                },
            ],
            tools: Vec::new(),
            tool_choice: Default::default(),
            parallel_tool_calls: false,
            stream: false,
            background: false,
            previous_response_id: None,
        };

        let items = input_items_from_request(&request);

        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_a");
        assert_eq!(items[1]["type"], "function_call_output");
        assert_eq!(items[1]["call_id"], "call_a");
    }
}
