use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::io::Write;

#[derive(Default)]
struct ChatToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct ChatStream {
    id: Option<String>,
    model: Option<String>,
    text: String,
    tools: BTreeMap<usize, ChatToolCall>,
    usage: Value,
    finish_reason: Option<String>,
}

#[derive(Default)]
struct AnthropicToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct AnthropicStream {
    id: Option<String>,
    model: Option<String>,
    text: String,
    tools: BTreeMap<usize, AnthropicToolCall>,
    usage: Map<String, Value>,
    stop_reason: Option<String>,
}

#[derive(Default)]
pub(crate) struct ConversionContext {
    custom_tools: HashSet<String>,
}

pub(crate) struct ConvertedRequest {
    pub(crate) body: Value,
    pub(crate) context: ConversionContext,
}

pub(crate) fn convert_request(
    protocol: &str,
    body: &Value,
) -> Result<ConvertedRequest, Box<dyn Error>> {
    let mut body = body.clone();
    sanitize_responses_body(&mut body);
    let context = conversion_context(&body);
    let converted = match protocol {
        "openai_chat" => responses_to_chat(&body),
        "anthropic_messages" => responses_to_anthropic(&body),
        _ => Err(format!("本地路由不支持的协议：{protocol}").into()),
    }?;
    Ok(ConvertedRequest {
        body: converted,
        context,
    })
}

pub(crate) fn sanitize_responses_body(body: &mut Value) {
    sanitize_item_list(body.get_mut("input"));
    sanitize_item_list(body.get_mut("output"));
}

fn sanitize_item_list(value: Option<&mut Value>) {
    let Some(Value::Array(items)) = value else {
        return;
    };
    items.retain(keep_input_item);
    for item in items {
        sanitize_reasoning_item(item);
    }
}

fn keep_input_item(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => persistable_reasoning(item),
        _ => true,
    }
}

fn persistable_reasoning(item: &Value) -> bool {
    item.get("encrypted_content")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|content| !content.is_empty())
}

fn sanitize_reasoning_item(item: &mut Value) {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return;
    }
    let Some(object) = item.as_object_mut() else {
        return;
    };
    let valid_id = object
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.starts_with("rs"));
    if !valid_id {
        object.remove("id");
    }
    if !object.get("summary").is_some_and(Value::is_array) {
        object.insert("summary".into(), json!([]));
    }
}

pub(crate) fn convert_response(
    protocol: &str,
    content_type: &str,
    bytes: &[u8],
    context: &ConversionContext,
) -> Result<Value, Box<dyn Error>> {
    if content_type.contains("text/event-stream") {
        let body = std::str::from_utf8(bytes)
            .map_err(|error| format!("上游流式响应不是 UTF-8：{error}"))?;
        return match protocol {
            "openai_chat" => chat_stream_to_response(body, context),
            "anthropic_messages" => anthropic_stream_to_response(body, context),
            _ => Err(format!("本地路由不支持的协议：{protocol}").into()),
        };
    }
    let body: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("上游响应不是有效 JSON：{error}"))?;
    match protocol {
        "openai_chat" => chat_to_response(&body, context),
        "anthropic_messages" => anthropic_to_response(&body, context),
        _ => Err(format!("本地路由不支持的协议：{protocol}").into()),
    }
}

fn conversion_context(body: &Value) -> ConversionContext {
    let custom_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("custom"))
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    ConversionContext { custom_tools }
}

fn responses_to_chat(body: &Value) -> Result<Value, Box<dyn Error>> {
    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions") {
        let text = content_text(instructions);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }
    let mut pending_calls = Vec::new();
    match body.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                match item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message")
                {
                    "message" => {
                        flush_chat_calls(&mut messages, &mut pending_calls);
                        messages.push(json!({
                            "role": item.get("role").and_then(Value::as_str).unwrap_or("user"),
                            "content": response_content_to_chat(item.get("content"))
                        }));
                    }
                    "function_call" | "custom_tool_call" => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .cloned()
                            .unwrap_or_else(|| json!(new_id("call")));
                        let arguments = if item.get("type").and_then(Value::as_str)
                            == Some("custom_tool_call")
                        {
                            Value::String(canonical_json_string(Some(&json!({
                                "input": item.get("input").cloned().unwrap_or_else(|| json!(""))
                            }))))
                        } else {
                            Value::String(canonical_json_string(item.get("arguments")))
                        };
                        pending_calls.push(json!({
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": item.get("name").cloned().unwrap_or(Value::Null),
                                "arguments": arguments
                            }
                        }));
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        flush_chat_calls(&mut messages, &mut pending_calls);
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                            "content": content_text(item.get("output").unwrap_or(&Value::Null))
                        }));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    flush_chat_calls(&mut messages, &mut pending_calls);

    let mut result = json!({
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "messages": messages,
        "stream": body.get("stream").and_then(Value::as_bool).unwrap_or(false)
    });
    for (source, target) in [
        ("max_output_tokens", "max_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
    ] {
        if let Some(value) = body.get(source) {
            result[target] = value.clone();
        }
    }
    if result["stream"] == Value::Bool(true) {
        result["stream_options"] = json!({"include_usage": true});
    }
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(response_tool_to_chat)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !tools.is_empty() {
        result["tools"] = Value::Array(tools);
        if let Some(choice) = body.get("tool_choice") {
            result["tool_choice"] = response_tool_choice_to_chat(choice);
        }
        if let Some(parallel) = body.get("parallel_tool_calls") {
            result["parallel_tool_calls"] = parallel.clone();
        }
    }
    if let Some(format) = body.pointer("/text/format") {
        result["response_format"] = format.clone();
    }
    if let Some(effort) = body.pointer("/reasoning/effort") {
        result["reasoning_effort"] = effort.clone();
    }
    Ok(result)
}

fn flush_chat_calls(messages: &mut Vec<Value>, pending_calls: &mut Vec<Value>) {
    if !pending_calls.is_empty() {
        messages.push(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": std::mem::take(pending_calls)
        }));
    }
}

fn response_content_to_chat(content: Option<&Value>) -> Value {
    let Some(content) = content else {
        return Value::String(String::new());
    };
    if let Some(text) = content.as_str() {
        return Value::String(text.to_string());
    }
    let converted = content
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text") => Some(json!({
                "type": "text",
                "text": part.get("text").cloned().unwrap_or_else(|| json!(""))
            })),
            Some("input_image") => {
                let value = part
                    .get("image_url")
                    .or_else(|| part.get("url"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let url = value.as_str().map(|url| json!(url)).unwrap_or(value);
                Some(json!({"type": "image_url", "image_url": {"url": url}}))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    Value::Array(converted)
}

fn response_tool_to_chat(tool: &Value) -> Option<Value> {
    let tool_type = tool.get("type").and_then(Value::as_str);
    if !matches!(tool_type, Some("function") | Some("custom")) {
        return None;
    }
    let function = tool.get("function").unwrap_or(tool);
    if tool_type == Some("custom") {
        let name = function.get("name")?.clone();
        let mut converted = json!({
            "type": "function",
            "function": {
                "name": name,
                "parameters": {
                    "type": "object",
                    "properties": {"input": {"type": "string"}},
                    "required": ["input"]
                }
            }
        });
        if let Some(description) = function.get("description") {
            converted["function"]["description"] = description.clone();
        }
        return Some(converted);
    }
    let mut converted = json!({
        "type": "function",
        "function": {
            "name": function.get("name")?.clone(),
            "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({}))
        }
    });
    if let Some(description) = function.get("description").filter(|value| !value.is_null()) {
        converted["function"]["description"] = description.clone();
    }
    if let Some(strict) = function.get("strict").filter(|value| !value.is_null()) {
        converted["function"]["strict"] = strict.clone();
    }
    Some(converted)
}

fn response_tool_choice_to_chat(choice: &Value) -> Value {
    if let Some(name) = choice.get("name") {
        json!({"type": "function", "function": {"name": name}})
    } else {
        choice.clone()
    }
}

fn chat_to_response(body: &Value, context: &ConversionContext) -> Result<Value, Box<dyn Error>> {
    let mut output = Vec::new();
    let mut finish_reason = None;
    for choice in body
        .get("choices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        finish_reason = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let message = choice.get("message").unwrap_or(&Value::Null);
        push_message(&mut output, message.get("content"));
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                output.push(chat_tool_to_response(call, context)?);
            }
        }
        if let Some(call) = message.get("function_call") {
            output.push(chat_tool_to_response(call, context)?);
        }
    }
    if output.is_empty() {
        return Err("Chat Completions 响应中没有文本或工具调用".into());
    }
    Ok(response_envelope(
        body.get("id").and_then(Value::as_str),
        body.get("model").cloned().unwrap_or(Value::Null),
        output,
        chat_usage(body.get("usage")),
        finish_reason.as_deref() == Some("length"),
    ))
}

fn chat_tool_to_response(
    call: &Value,
    context: &ConversionContext,
) -> Result<Value, Box<dyn Error>> {
    let function = call.get("function").unwrap_or(call);
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or("Chat Completions 返回了没有名称的工具调用")?;
    let call_id = call
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| new_id("call"));
    let arguments = canonical_json_string(function.get("arguments"));
    if context.custom_tools.contains(name) {
        return Ok(json!({
            "type": "custom_tool_call",
            "id": format!("ctc_{call_id}"),
            "call_id": call_id,
            "name": name,
            "input": custom_tool_input(&arguments),
            "status": "completed"
        }));
    }
    Ok(json!({
        "type": "function_call",
        "id": new_id("fc"),
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
        "status": "completed"
    }))
}

fn responses_to_anthropic(body: &Value) -> Result<Value, Box<dyn Error>> {
    let mut messages = Vec::new();
    let mut assistant_blocks = Vec::new();
    match body.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                match item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message")
                {
                    "message" => {
                        flush_anthropic_assistant(&mut messages, &mut assistant_blocks);
                        messages.push(json!({
                            "role": item.get("role").cloned().unwrap_or_else(|| json!("user")),
                            "content": response_content_to_anthropic(item.get("content"))
                        }));
                    }
                    "function_call" | "custom_tool_call" => {
                        let input = item.get("input").cloned();
                        assistant_blocks.push(json!({
                            "type": "tool_use",
                            "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or_else(|| json!(new_id("call"))),
                            "name": item.get("name").cloned().unwrap_or(Value::Null),
                            "input": input.map(|value| json!({"input": value})).unwrap_or_else(|| parse_json_value(item.get("arguments")))
                        }));
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        flush_anthropic_assistant(&mut messages, &mut assistant_blocks);
                        messages.push(json!({
                            "role": "user",
                            "content": [{"type": "tool_result", "tool_use_id": item.get("call_id").cloned().unwrap_or(Value::Null), "content": content_text(item.get("output").unwrap_or(&Value::Null))}]
                        }));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    flush_anthropic_assistant(&mut messages, &mut assistant_blocks);
    let mut result = json!({
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "messages": messages,
        "max_tokens": body.get("max_output_tokens").cloned().unwrap_or_else(|| json!(4096)),
        "stream": body.get("stream").and_then(Value::as_bool).unwrap_or(false)
    });
    if let Some(instructions) = body.get("instructions") {
        result["system"] = instructions.clone();
    }
    for field in ["temperature", "top_p"] {
        if let Some(value) = body.get(field) {
            result[field] = value.clone();
        }
    }
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(response_tool_to_anthropic)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !tools.is_empty() {
        result["tools"] = Value::Array(tools);
        if let Some(choice) = body.get("tool_choice") {
            result["tool_choice"] = response_tool_choice_to_anthropic(choice);
        }
    }
    Ok(result)
}

fn flush_anthropic_assistant(messages: &mut Vec<Value>, blocks: &mut Vec<Value>) {
    if !blocks.is_empty() {
        messages.push(json!({"role": "assistant", "content": std::mem::take(blocks)}));
    }
}

fn response_content_to_anthropic(content: Option<&Value>) -> Value {
    let Some(content) = content else {
        return Value::String(String::new());
    };
    if let Some(text) = content.as_str() {
        return Value::String(text.to_string());
    }
    Value::Array(
        content
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text") => Some(json!({"type": "text", "text": part.get("text").cloned().unwrap_or_else(|| json!(""))})),
                Some("input_image") => image_to_anthropic(part),
                _ => None,
            })
            .collect(),
    )
}

fn image_to_anthropic(part: &Value) -> Option<Value> {
    let url = part
        .get("image_url")
        .or_else(|| part.get("url"))?
        .as_str()?;
    let data = url.strip_prefix("data:")?;
    let (media_type, encoded) = data.split_once(";base64,")?;
    Some(
        json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": encoded}}),
    )
}

fn response_tool_to_anthropic(tool: &Value) -> Option<Value> {
    if !matches!(
        tool.get("type").and_then(Value::as_str),
        Some("function") | Some("custom")
    ) {
        return None;
    }
    let function = tool.get("function").unwrap_or(tool);
    let mut converted = json!({
        "name": function.get("name")?.clone(),
        "input_schema": if tool.get("type").and_then(Value::as_str) == Some("custom") {
            json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]})
        } else {
            function.get("parameters").cloned().unwrap_or_else(|| json!({}))
        }
    });
    if let Some(description) = function.get("description").filter(|value| !value.is_null()) {
        converted["description"] = description.clone();
    }
    Some(converted)
}

fn response_tool_choice_to_anthropic(choice: &Value) -> Value {
    if let Some(name) = choice.get("name") {
        json!({"type": "tool", "name": name})
    } else {
        match choice.as_str() {
            Some("required") => json!({"type": "any"}),
            Some("none") => Value::Null,
            _ => json!({"type": "auto"}),
        }
    }
}

fn anthropic_to_response(
    body: &Value,
    context: &ConversionContext,
) -> Result<Value, Box<dyn Error>> {
    let mut output = Vec::new();
    if let Some(content) = body.get("content").and_then(Value::as_array) {
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => push_message(&mut output, block.get("text")),
                Some("tool_use") => {
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .ok_or("Anthropic Messages 返回了没有名称的工具调用")?;
                    let call_id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| new_id("call"));
                    let arguments = canonical_json_string(block.get("input"));
                    if context.custom_tools.contains(name) {
                        output.push(json!({"type": "custom_tool_call", "id": format!("ctc_{call_id}"), "call_id": call_id, "name": name, "input": custom_tool_input(&arguments), "status": "completed"}));
                    } else {
                        output.push(json!({"type": "function_call", "id": new_id("fc"), "call_id": call_id, "name": name, "arguments": arguments, "status": "completed"}));
                    }
                }
                _ => {}
            }
        }
    }
    if output.is_empty() {
        return Err("Anthropic Messages 响应中没有文本或工具调用".into());
    }
    Ok(response_envelope(
        body.get("id").and_then(Value::as_str),
        body.get("model").cloned().unwrap_or(Value::Null),
        output,
        anthropic_usage(body.get("usage")),
        body.get("stop_reason").and_then(Value::as_str) == Some("max_tokens"),
    ))
}

fn chat_stream_to_response(
    body: &str,
    context: &ConversionContext,
) -> Result<Value, Box<dyn Error>> {
    let mut stream = ChatStream::default();
    for value in sse_values(body) {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            stream.id = Some(id.to_string());
        }
        if let Some(model) = value.get("model").and_then(Value::as_str) {
            stream.model = Some(model.to_string());
        }
        if let Some(usage) = value.get("usage")
            && !usage.is_null()
        {
            stream.usage = usage.clone();
        }
        for choice in value
            .get("choices")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                stream.finish_reason = Some(reason.to_string());
            }
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            append_content(&mut stream.text, delta.get("content"));
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let state = stream.tools.entry(index).or_default();
                    append_string(&mut state.id, call.get("id"));
                    let function = call.get("function").unwrap_or(&Value::Null);
                    append_string(&mut state.name, function.get("name"));
                    append_string(&mut state.arguments, function.get("arguments"));
                }
            }
        }
    }
    let mut output = Vec::new();
    push_message(&mut output, Some(&Value::String(stream.text)));
    for (_, call) in stream.tools {
        output.push(chat_tool_to_response(
            &json!({"id": call.id, "function": {"name": call.name, "arguments": call.arguments}}),
            context,
        )?);
    }
    if output.is_empty() {
        return Err("Chat Completions 流中没有文本或工具调用".into());
    }
    Ok(response_envelope(
        stream.id.as_deref(),
        stream.model.map(Value::String).unwrap_or(Value::Null),
        output,
        chat_usage(Some(&stream.usage)),
        stream.finish_reason.as_deref() == Some("length"),
    ))
}

fn anthropic_stream_to_response(
    body: &str,
    context: &ConversionContext,
) -> Result<Value, Box<dyn Error>> {
    let mut stream = AnthropicStream::default();
    for value in sse_values(body) {
        if let Some(message) = value.get("message") {
            stream.id = message
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string);
            stream.model = message
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            merge_usage(&mut stream.usage, message.get("usage"));
        }
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        if let Some(block) = value.get("content_block") {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => append_content(&mut stream.text, block.get("text")),
                Some("tool_use") => {
                    let state = stream.tools.entry(index).or_default();
                    append_string(&mut state.id, block.get("id"));
                    append_string(&mut state.name, block.get("name"));
                }
                _ => {}
            }
        }
        if let Some(delta) = value.get("delta") {
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => append_content(&mut stream.text, delta.get("text")),
                Some("input_json_delta") => {
                    let state = stream.tools.entry(index).or_default();
                    append_string(&mut state.arguments, delta.get("partial_json"));
                }
                _ => {}
            }
            if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                stream.stop_reason = Some(reason.to_string());
            }
        }
        merge_usage(&mut stream.usage, value.get("usage"));
    }
    let mut output = Vec::new();
    push_message(&mut output, Some(&Value::String(stream.text)));
    for (_, call) in stream.tools {
        let arguments = if call.arguments.is_empty() {
            "{}"
        } else {
            &call.arguments
        };
        let call_id = if call.id.is_empty() {
            new_id("call")
        } else {
            call.id
        };
        let arguments = canonical_json_string(Some(&Value::String(arguments.to_string())));
        if context.custom_tools.contains(&call.name) {
            output.push(json!({"type": "custom_tool_call", "id": format!("ctc_{call_id}"), "call_id": call_id, "name": call.name, "input": custom_tool_input(&arguments), "status": "completed"}));
        } else {
            output.push(json!({"type": "function_call", "id": new_id("fc"), "call_id": call_id, "name": call.name, "arguments": arguments, "status": "completed"}));
        }
    }
    if output.is_empty() {
        return Err("Anthropic Messages 流中没有文本或工具调用".into());
    }
    Ok(response_envelope(
        stream.id.as_deref(),
        stream.model.map(Value::String).unwrap_or(Value::Null),
        output,
        anthropic_usage(Some(&Value::Object(stream.usage))),
        stream.stop_reason.as_deref() == Some("max_tokens"),
    ))
}

fn response_envelope(
    id: Option<&str>,
    model: Value,
    output: Vec<Value>,
    usage: Value,
    incomplete: bool,
) -> Value {
    json!({
        "id": id.map(str::to_string).unwrap_or_else(|| new_id("resp")),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": if incomplete { "incomplete" } else { "completed" },
        "error": Value::Null,
        "incomplete_details": if incomplete { json!({"reason": "max_output_tokens"}) } else { Value::Null },
        "instructions": Value::Null,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "usage": usage
    })
}

pub(crate) fn response_to_sse(response: &Value) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut events = Vec::new();
    let mut sequence = 0_u64;
    let mut created = response.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    push_sse(
        &mut events,
        "response.created",
        json!({"type": "response.created", "sequence_number": sequence, "response": created}),
    )?;
    sequence += 1;
    for (output_index, item) in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let mut started = item.clone();
        started["status"] = json!("in_progress");
        if started.get("type").and_then(Value::as_str) == Some("message") {
            started["content"] = json!([]);
        }
        if started.get("type").and_then(Value::as_str) == Some("function_call") {
            started["arguments"] = json!("");
        }
        if started.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
            started["input"] = json!("");
        }
        push_sse(
            &mut events,
            "response.output_item.added",
            json!({"type": "response.output_item.added", "sequence_number": sequence, "output_index": output_index, "item": started}),
        )?;
        sequence += 1;
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let item_id = item
                    .get("id")
                    .cloned()
                    .unwrap_or_else(|| json!(new_id("msg")));
                for (content_index, part) in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                    push_sse(
                        &mut events,
                        "response.content_part.added",
                        json!({"type": "response.content_part.added", "sequence_number": sequence, "item_id": item_id, "output_index": output_index, "content_index": content_index, "part": {"type": "output_text", "text": "", "annotations": []}}),
                    )?;
                    sequence += 1;
                    if !text.is_empty() {
                        push_sse(
                            &mut events,
                            "response.output_text.delta",
                            json!({"type": "response.output_text.delta", "sequence_number": sequence, "item_id": item_id, "output_index": output_index, "content_index": content_index, "delta": text, "logprobs": []}),
                        )?;
                        sequence += 1;
                    }
                    push_sse(
                        &mut events,
                        "response.output_text.done",
                        json!({"type": "response.output_text.done", "sequence_number": sequence, "item_id": item_id, "output_index": output_index, "content_index": content_index, "text": text, "logprobs": []}),
                    )?;
                    sequence += 1;
                    push_sse(
                        &mut events,
                        "response.content_part.done",
                        json!({"type": "response.content_part.done", "sequence_number": sequence, "item_id": item_id, "output_index": output_index, "content_index": content_index, "part": part}),
                    )?;
                    sequence += 1;
                }
            }
            Some("function_call") => {
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                push_sse(
                    &mut events,
                    "response.function_call_arguments.delta",
                    json!({"type": "response.function_call_arguments.delta", "sequence_number": sequence, "item_id": item.get("id"), "output_index": output_index, "delta": arguments}),
                )?;
                sequence += 1;
                push_sse(
                    &mut events,
                    "response.function_call_arguments.done",
                    json!({"type": "response.function_call_arguments.done", "sequence_number": sequence, "item_id": item.get("id"), "output_index": output_index, "arguments": arguments}),
                )?;
                sequence += 1;
            }
            Some("custom_tool_call") => {
                let input = item
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                push_sse(
                    &mut events,
                    "response.custom_tool_call_input.delta",
                    json!({"type": "response.custom_tool_call_input.delta", "sequence_number": sequence, "item_id": item.get("id"), "output_index": output_index, "delta": input}),
                )?;
                sequence += 1;
                push_sse(
                    &mut events,
                    "response.custom_tool_call_input.done",
                    json!({"type": "response.custom_tool_call_input.done", "sequence_number": sequence, "item_id": item.get("id"), "output_index": output_index, "input": input}),
                )?;
                sequence += 1;
            }
            _ => {}
        }
        push_sse(
            &mut events,
            "response.output_item.done",
            json!({"type": "response.output_item.done", "sequence_number": sequence, "output_index": output_index, "item": item}),
        )?;
        sequence += 1;
    }
    push_sse(
        &mut events,
        "response.completed",
        json!({"type": "response.completed", "sequence_number": sequence, "response": response}),
    )?;
    Ok(events)
}

fn push_sse(target: &mut Vec<u8>, event: &str, data: Value) -> Result<(), Box<dyn Error>> {
    writeln!(target, "event: {event}")?;
    writeln!(target, "data: {}", serde_json::to_string(&data)?)?;
    writeln!(target)?;
    Ok(())
}

fn push_message(output: &mut Vec<Value>, content: Option<&Value>) {
    let Some(content) = content else {
        return;
    };
    let text = content_text(content);
    if !text.is_empty() {
        output.push(json!({"type": "message", "id": new_id("msg"), "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]}));
    }
}

fn chat_usage(usage: Option<&Value>) -> Value {
    let usage = usage.unwrap_or(&Value::Null);
    let input = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({"input_tokens": input, "input_tokens_details": {"cached_tokens": cached}, "output_tokens": output, "output_tokens_details": {"reasoning_tokens": reasoning}, "total_tokens": usage.get("total_tokens").and_then(Value::as_u64).unwrap_or(input + output)})
}

fn anthropic_usage(usage: Option<&Value>) -> Value {
    let usage = usage.unwrap_or(&Value::Null);
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({"input_tokens": input + cached, "input_tokens_details": {"cached_tokens": cached}, "output_tokens": output, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": input + cached + output})
}

fn merge_usage(target: &mut Map<String, Value>, usage: Option<&Value>) {
    if let Some(usage) = usage.and_then(Value::as_object) {
        for (key, value) in usage {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn sse_values(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| !data.is_empty() && *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .collect()
}

fn append_content(target: &mut String, value: Option<&Value>) {
    match value {
        Some(Value::String(text)) => target.push_str(text),
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    target.push_str(text);
                }
            }
        }
        _ => {}
    }
}

fn append_string(target: &mut String, value: Option<&Value>) {
    if let Some(value) = value.and_then(Value::as_str) {
        target.push_str(value);
    }
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.as_str()
                    .map(str::to_string)
                    .or_else(|| part.get("text").and_then(Value::as_str).map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn canonical_json_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text)
            .and_then(|value| serde_json::to_string(&value))
            .unwrap_or_else(|_| text.clone()),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
    }
}

fn custom_tool_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("input")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| arguments.to_string())
}

fn parse_json_value(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
        Some(value) => value.clone(),
        None => json!({}),
    }
}

pub(crate) fn normalize_error(body: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let value = serde_json::from_slice::<Value>(body).unwrap_or_else(
        |_| json!({"error": {"message": String::from_utf8_lossy(body), "type": "upstream_error"}}),
    );
    if value.get("error").is_some() {
        Ok(serde_json::to_vec(&value)?)
    } else {
        Ok(serde_json::to_vec(
            &json!({"error": {"message": content_text(&value), "type": "upstream_error"}}),
        )?)
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_cswitch_{}", uuid_like())
}
fn uuid_like() -> String {
    format!("{:032x}", rand::random::<u128>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_preserves_tool_history_and_definitions() {
        let converted = responses_to_chat(&json!({"model": "fixture-model", "input": [{"type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}, {"type": "function_call_output", "call_id": "call_1", "output": "hello"}], "tools": [{"type": "function", "name": "read_file", "description": "Read", "parameters": {"type": "object"}}], "tool_choice": {"type": "function", "name": "read_file"}, "parallel_tool_calls": true})).unwrap();
        assert_eq!(converted["messages"][0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(converted["messages"][1]["tool_call_id"], "call_1");
        assert_eq!(converted["tools"][0]["function"]["name"], "read_file");
        assert_eq!(converted["tool_choice"]["function"]["name"], "read_file");
    }

    #[test]
    fn chat_tool_call_becomes_a_responses_function_call() {
        let converted = chat_to_response(&json!({"id": "chat_1", "model": "fixture-model", "choices": [{"message": {"tool_calls": [{"id": "call_1", "function": {"name": "shell", "arguments": "{\"command\":\"pwd\"}"}}]}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7}}), &ConversionContext::default()).unwrap();
        assert_eq!(converted["output"][0]["type"], "function_call");
        assert_eq!(converted["output"][0]["call_id"], "call_1");
        assert_eq!(converted["usage"]["total_tokens"], 7);
    }

    #[test]
    fn anthropic_request_maps_tool_use_and_tool_result() {
        let converted = responses_to_anthropic(&json!({"model": "fixture-model", "input": [{"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":\"pwd\"}"}, {"type": "function_call_output", "call_id": "call_1", "output": "done"}], "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}}]})).unwrap();
        assert_eq!(converted["messages"][0]["content"][0]["type"], "tool_use");
        assert_eq!(
            converted["messages"][1]["content"][0]["type"],
            "tool_result"
        );
        assert_eq!(converted["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn custom_tools_round_trip_through_chat_without_losing_freeform_input() {
        let request = json!({
            "model": "fixture-model",
            "tools": [{"type": "custom", "name": "apply_patch", "description": "Apply a patch"}],
            "input": [{"type": "custom_tool_call", "call_id": "call_old", "name": "apply_patch", "input": "*** Begin Patch"}]
        });
        let converted = convert_request("openai_chat", &request).unwrap();
        assert_eq!(
            converted.body["tools"][0]["function"]["name"],
            "apply_patch"
        );
        assert_eq!(
            converted.body["tools"][0]["function"]["parameters"]["required"][0],
            "input"
        );
        assert_eq!(
            converted.body["messages"][0]["tool_calls"][0]["function"]["arguments"],
            r#"{"input":"*** Begin Patch"}"#
        );

        let response = chat_to_response(
            &json!({"choices": [{"message": {"tool_calls": [{"id": "call_new", "function": {"name": "apply_patch", "arguments": "{\"input\":\"*** Begin Patch\"}"}}]}}]}),
            &converted.context,
        )
        .unwrap();
        assert_eq!(response["output"][0]["type"], "custom_tool_call");
        assert_eq!(response["output"][0]["input"], "*** Begin Patch");
        let events = String::from_utf8(response_to_sse(&response).unwrap()).unwrap();
        assert!(events.contains("response.custom_tool_call_input.done"));
    }

    #[test]
    fn custom_tools_round_trip_through_anthropic() {
        let request = json!({
            "model": "fixture-model",
            "tools": [{"type": "custom", "name": "apply_patch"}],
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "edit"}]}]
        });
        let converted = convert_request("anthropic_messages", &request).unwrap();
        assert_eq!(converted.body["tools"][0]["name"], "apply_patch");
        assert_eq!(
            converted.body["tools"][0]["input_schema"]["required"][0],
            "input"
        );

        let response = anthropic_to_response(
            &json!({"content": [{"type": "tool_use", "id": "call_1", "name": "apply_patch", "input": {"input": "*** Begin Patch"}}]}),
            &converted.context,
        )
        .unwrap();
        assert_eq!(response["output"][0]["type"], "custom_tool_call");
        assert_eq!(response["output"][0]["input"], "*** Begin Patch");
    }

    #[test]
    fn streaming_response_contains_text_and_tool_events() {
        let response = response_envelope(
            Some("resp_1"),
            json!("fixture-model"),
            vec![
                json!({"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "hello", "annotations": []}]}),
                json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "shell", "arguments": "{}", "status": "completed"}),
            ],
            chat_usage(None),
            false,
        );
        let mut bytes = Vec::new();
        let mut created = response.clone();
        created["status"] = json!("in_progress");
        push_sse(&mut bytes, "response.created", json!({"response": created})).unwrap();
        push_sse(
            &mut bytes,
            "response.function_call_arguments.done",
            json!({"arguments": "{}"}),
        )
        .unwrap();
        push_sse(
            &mut bytes,
            "response.completed",
            json!({"response": response}),
        )
        .unwrap();
        let events = String::from_utf8(bytes).unwrap();
        assert!(events.contains("response.created"));
        assert!(events.contains("response.function_call_arguments.done"));
        assert!(events.contains("response.completed"));
    }

    #[test]
    fn chat_conversion_does_not_emit_synthetic_reasoning_items() {
        let converted = chat_to_response(
            &json!({
                "id": "chat_1",
                "model": "fixture-model",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "done",
                        "reasoning_content": "secret chain of thought"
                    },
                    "finish_reason": "stop"
                }]
            }),
            &ConversionContext::default(),
        )
        .unwrap();
        assert_eq!(converted["output"].as_array().unwrap().len(), 1);
        assert_eq!(converted["output"][0]["type"], "message");
        assert!(
            converted["output"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "reasoning")
        );
    }

    #[test]
    fn sanitizes_poisoned_reasoning_ids_like_sub2api() {
        let mut body = json!({
            "input": [
                {"type": "reasoning", "id": "msg_not_rs", "summary": [{"type": "summary_text", "text": "synthetic"}]},
                {"type": "reasoning", "id": "thought_1", "encrypted_content": "blob"},
                {"type": "reasoning", "id": "rs_ok", "encrypted_content": "blob", "summary": "bad"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ]
        });
        sanitize_responses_body(&mut body);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "reasoning");
        assert!(input[0].get("id").is_none());
        assert_eq!(input[0]["encrypted_content"], "blob");
        assert_eq!(input[1]["id"], "rs_ok");
        assert_eq!(input[1]["summary"], json!([]));
        assert_eq!(input[2]["type"], "message");
    }
}
