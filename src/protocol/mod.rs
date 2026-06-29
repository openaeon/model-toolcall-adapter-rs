use serde_json::Value;

use crate::types::{ParsedToolCall, ToolDefinition};

const TOOL_TAGS: &[(&str, &str)] = &[
    ("<tool_call", "</tool_call>"),
    ("tool_call", "</tool_call>"),
    ("<previous_tool_call", "</previous_tool_call>"),
    ("previous_tool_call", "</previous_tool_call>"),
    ("<tool_invoke", "</tool_invoke>"),
    ("tool_invoke", "</tool_invoke>"),
    ("<aeon_tool_call", "</aeon_tool_call>"),
    ("aeon_tool_call", "</aeon_tool_call>"),
    ("<aeon_tool_call", "</aeon_tool_calls>"),
    ("aeon_tool_call", "</aeon_tool_calls>"),
];

pub fn render_tool_protocol_prompt(tools: &[ToolDefinition]) -> String {
    let mut prompt = String::from(
        "CRITICAL adapter tool-call protocol override.\n\
You are behind an adapter. The client cannot see DeepSeek native actions or free-form tool descriptions.\n\
When the user's request requires reading files, inspecting the current project, running commands, editing files, opening local resources, or using any external/runtime capability, you MUST output exactly one XML tool call and no markdown, explanation, greeting, or extra text.\n\
Do not claim you are ready, do not ask what to work on, and do not answer from memory when a listed tool can inspect the real state.\n\
For Chinese user messages, respond in Chinese. Preserve the user's language for tool arguments and final answers.\n\
Tool call format:\n\n\
<tool_call id=\"call_unique_id\" name=\"tool_name\">\n\
{\"arg\":\"value\"}\n\
</tool_call>\n\n\
Rules:\n\
- Use only tools listed below.\n\
- The body must be valid JSON.\n\
- For project inspection requests, prefer command/file-reading tools such as exec_command when available.\n\
- Treat tools as actions, never as prose. If you decide to use a tool, output only the XML tool call and stop.\n\
- After a tool result, decide the next step from that result: call another tool when more real state is needed, otherwise give the final answer.\n\
- For skill/plugin/MCP work, first read the referenced skill or instruction file with the available file/resource tool, then call the relevant MCP/plugin tool only with arguments matching its schema.\n\
- For shell commands, use the exact schema of the listed command tool. Do not invent fields. Prefer small read-only commands before edits.\n\
- For editing, use apply_patch when it is available; do not describe an edit as finished unless the tool call has been returned and the client executed it.\n\
- If no tool is needed, answer normally in the user's language.\n",
    );
    if tools.is_empty() {
        prompt.push_str("\nNo tools are available.");
        return prompt;
    }
    prompt.push_str("\nAvailable tools:\n");
    for tool in tools {
        prompt.push_str(&format!(
            "- name: {}\n  description: {}\n  input_schema: {}\n",
            tool.name,
            tool.description.clone().unwrap_or_default(),
            tool.input_schema
        ));
    }
    prompt
}

pub fn parse_tool_calls(text: &str) -> Vec<ParsedToolCall> {
    let mut out = Vec::new();
    collect_xml_tool_calls(text, &mut out);
    if out.is_empty() {
        collect_json_array_tool_calls(text, &mut out);
    }
    if out.is_empty() {
        if let Some(fenced) = extract_fenced_json(text) {
            collect_json_array_tool_calls(&fenced, &mut out);
        }
    }
    if out.is_empty() {
        if let Some(call) = parse_json_tool_call(text) {
            out.push(call);
        } else if let Some(fenced) = extract_fenced_json(text) {
            if let Some(call) = parse_json_tool_call(&fenced) {
                out.push(call);
            }
        }
    }
    out
}

fn collect_xml_tool_calls(text: &str, out: &mut Vec<ParsedToolCall>) {
    let mut cursor = 0;
    while cursor < text.len() {
        let Some((start, open_tag, close_tag)) = next_tool_open(&text[cursor..]) else {
            break;
        };
        cursor += start;
        let rest = &text[cursor..];
        let Some(open_end) = rest.find('>') else {
            break;
        };
        let tag_text = &rest[..=open_end];
        if !tag_text.starts_with(open_tag) {
            cursor += open_end + 1;
            continue;
        }
        if is_self_closing_tag(tag_text) {
            if let Some(call) = parse_self_closing(tag_text, out.len() + 1) {
                out.push(call);
            }
            cursor += open_end + 1;
            continue;
        }
        let after_open = &rest[open_end + 1..];
        let close_tag = best_close_tag(tag_text, after_open, close_tag);
        let Some(close_start) = after_open.find(close_tag) else {
            break;
        };
        let body = &after_open[..close_start];
        if let Some(call) = parse_one(tag_text, body, out.len() + 1) {
            out.push(call);
        }
        cursor += open_end + 1 + close_start + close_tag.len();
    }
}

fn next_tool_open(text: &str) -> Option<(usize, &'static str, &'static str)> {
    TOOL_TAGS
        .iter()
        .filter_map(|(open, close)| text.find(open).map(|index| (index, *open, *close)))
        .min_by_key(|(index, _, _)| *index)
}

fn best_close_tag(open_tag: &str, after_open: &str, fallback: &'static str) -> &'static str {
    if open_tag.contains("aeon_tool_call") {
        if after_open.contains("</aeon_tool_call>") {
            return "</aeon_tool_call>";
        }
        if after_open.contains("</aeon_tool_calls>") {
            return "</aeon_tool_calls>";
        }
    }
    if open_tag.contains("tool_invoke") {
        return "</tool_invoke>";
    }
    if open_tag.contains("previous_tool_call") && after_open.contains("</tool_call>") {
        return "</tool_call>";
    }
    fallback
}

fn parse_one(open_tag: &str, body: &str, index: usize) -> Option<ParsedToolCall> {
    let body_value = parse_jsonish(body)?;
    let name = attr(open_tag, "name").or_else(|| {
        body_value
            .get("name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })?;
    let id = attr(open_tag, "id").unwrap_or_else(|| format!("call_{index}"));
    let value = body_value
        .get("arguments")
        .or_else(|| body_value.get("args"))
        .or_else(|| body_value.get("input"))
        .cloned()
        .unwrap_or(body_value);
    Some(ParsedToolCall {
        id,
        name,
        arguments: value.to_string(),
    })
}

fn parse_self_closing(open_tag: &str, index: usize) -> Option<ParsedToolCall> {
    let name = attr(open_tag, "name")?;
    let id = attr(open_tag, "id").unwrap_or_else(|| format!("call_{index}"));
    let mut args = serde_json::Map::new();
    for (key, value) in attrs(open_tag) {
        if key != "id" && key != "name" {
            args.insert(key, Value::String(value));
        }
    }
    Some(ParsedToolCall {
        id,
        name,
        arguments: Value::Object(args).to_string(),
    })
}

fn parse_json_tool_call(text: &str) -> Option<ParsedToolCall> {
    let value = parse_jsonish(text)?;
    let call = value
        .get("tool_call")
        .or_else(|| value.get("toolCall"))
        .or_else(|| value.get("function_call"))
        .unwrap_or(&value);
    let name = call.get("name")?.as_str()?.to_string();
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("call_1")
        .to_string();
    let args = call
        .get("arguments")
        .or_else(|| call.get("args"))
        .or_else(|| call.get("input"))
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    Some(ParsedToolCall {
        id,
        name,
        arguments: if args.is_string() {
            args.as_str().unwrap_or("{}").to_string()
        } else {
            args.to_string()
        },
    })
}

fn collect_json_array_tool_calls(text: &str, out: &mut Vec<ParsedToolCall>) {
    let Some(value) = parse_jsonish(text) else {
        return;
    };
    let Some(calls) = value
        .get("tool_calls")
        .or_else(|| value.get("toolCalls"))
        .or_else(|| value.get("tools"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for (index, call) in calls.iter().enumerate() {
        let Some(name) = call.get("name").and_then(Value::as_str).or_else(|| {
            call.get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
        }) else {
            continue;
        };
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("call_{}", index + 1));
        let args = call
            .get("arguments")
            .or_else(|| call.get("args"))
            .or_else(|| call.get("input"))
            .cloned()
            .or_else(|| {
                call.get("function")
                    .and_then(|function| function.get("arguments"))
                    .cloned()
            })
            .unwrap_or(Value::Object(Default::default()));
        out.push(ParsedToolCall {
            id,
            name: name.to_string(),
            arguments: normalize_arguments(args),
        });
    }
}

fn parse_jsonish(raw: &str) -> Option<Value> {
    let cleaned = clean_jsonish(raw);
    serde_json::from_str::<Value>(&cleaned).ok()
}

fn clean_jsonish(raw: &str) -> String {
    let mut value = raw.trim();
    if let Some(stripped) = value.strip_prefix("```json") {
        value = stripped;
    } else if let Some(stripped) = value.strip_prefix("```") {
        value = stripped;
    }
    if let Some(stripped) = value.strip_suffix("```") {
        value = stripped;
    }
    let value = value.trim();
    if let (Some(start), Some(end)) = (value.find('{'), value.rfind('}')) {
        if end >= start {
            return value[start..=end].to_string();
        }
    }
    value.to_string()
}

fn extract_fenced_json(text: &str) -> Option<String> {
    let marker = text.find("```json").or_else(|| text.find("```"))?;
    let after_marker = &text[marker..];
    let body_start = after_marker.find('\n').map(|index| index + 1).unwrap_or(3);
    let body = &after_marker[body_start..];
    let end = body.find("```")?;
    Some(body[..end].to_string())
}

fn attr(open_tag: &str, key: &str) -> Option<String> {
    attrs(open_tag)
        .into_iter()
        .find_map(|(attr_key, value)| (attr_key == key).then_some(value))
}

fn attrs(open_tag: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = open_tag
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_end_matches('/')
        .trim();
    if let Some(space) = rest.find(char::is_whitespace) {
        rest = &rest[space + 1..];
    } else {
        return found;
    }
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().to_string();
        let after_eq = rest[eq + 1..].trim_start();
        let Some(quote) = after_eq
            .chars()
            .next()
            .filter(|ch| *ch == '"' || *ch == '\'')
        else {
            break;
        };
        let value_start = quote.len_utf8();
        let Some(value_end) = after_eq[value_start..].find(quote) else {
            break;
        };
        let value = after_eq[value_start..value_start + value_end].to_string();
        if !key.is_empty() {
            found.push((key, value));
        }
        rest = after_eq[value_start + value_end + quote.len_utf8()..].trim_start();
    }
    found
}

fn normalize_arguments(args: Value) -> String {
    if let Some(raw) = args.as_str() {
        clean_jsonish(raw)
    } else {
        args.to_string()
    }
}

fn is_self_closing_tag(open_tag: &str) -> bool {
    open_tag.trim_end().ends_with("/>")
}

#[cfg(test)]
mod tests {
    use super::parse_tool_calls;

    #[test]
    fn parses_xml_tool_call() {
        let calls =
            parse_tool_calls(r#"<tool_call id="call_a" name="search">{"q":"rs"}</tool_call>"#);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments, r#"{"q":"rs"}"#);
    }

    #[test]
    fn parses_json_tool_call() {
        let calls = parse_tool_calls(r#"{"tool_call":{"name":"search","arguments":{"q":"rs"}}}"#);
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments, r#"{"q":"rs"}"#);
    }

    #[test]
    fn parses_aeon_tool_call_with_single_quotes() {
        let calls =
            parse_tool_calls(r#"<aeon_tool_call id='a' name='search'>{"q":"rs"}</aeon_tool_call>"#);
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[0].name, "search");
    }

    #[test]
    fn parses_openai_style_tool_calls_array() {
        let calls = parse_tool_calls(
            r#"{"tool_calls":[{"id":"call_a","function":{"name":"search","arguments":"{\"q\":\"rs\"}"}}]}"#,
        );
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments, r#"{"q":"rs"}"#);
    }

    #[test]
    fn parses_self_closing_tool_call_attrs() {
        let calls =
            parse_tool_calls(r#"<tool_call id="a" name="get" url="https://example.com" />"#);
        assert_eq!(calls[0].arguments, r#"{"url":"https://example.com"}"#);
    }

    #[test]
    fn parses_previous_tool_call_as_tool_call_candidate() {
        let calls = parse_tool_calls(
            r#"<previous_tool_call id="call_a" name="exec_command">{"cmd":"cargo check"}</previous_tool_call>FINISHED"#,
        );

        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "exec_command");
        assert_eq!(calls[0].arguments, r#"{"cmd":"cargo check"}"#);
    }

    #[test]
    fn parses_previous_tool_call_with_mismatched_close_tag() {
        let calls = parse_tool_calls(
            r#"<previous_tool_call id="call_a" name="shell_command">{"command":"pwd"}</tool_call>"#,
        );

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "shell_command");
        assert_eq!(calls[0].arguments, r#"{"command":"pwd"}"#);
    }
}
