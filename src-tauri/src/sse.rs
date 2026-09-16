use crate::gateway_transform::{self, ConversionContext};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_EVENT: usize = 2 * 1024 * 1024;
#[derive(Default)]
pub(crate) struct Decoder {
    buffer: Vec<u8>,
}
impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            let boundary = self
                .buffer
                .windows(2)
                .position(|part| part == b"\n\n")
                .map(|pos| (pos, 2));
            let crlf = self
                .buffer
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .map(|pos| (pos, 4));
            let boundary = [boundary, crlf]
                .into_iter()
                .flatten()
                .min_by_key(|(pos, _)| *pos);
            let Some((pos, len)) = boundary else { break };
            if pos > MAX_EVENT {
                return Err("上游单个流式事件超过 2 MiB".into());
            }
            let block =
                std::str::from_utf8(&self.buffer[..pos]).map_err(|_| "上游流式事件不是 UTF-8")?;
            let data = block
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|line| line.strip_prefix(' ').unwrap_or(line))
                .collect::<Vec<_>>()
                .join("\n");
            if data == "[DONE]" {
                events.push(json!({"cswitch_done":true}));
            } else if !data.is_empty() {
                events.push(serde_json::from_str(&data).map_err(|_| "上游流式事件 JSON 损坏")?);
            }
            self.buffer.drain(..pos + len);
        }
        if self.buffer.len() > MAX_EVENT {
            return Err("上游流式事件缺少分隔符或超过 2 MiB".into());
        }
        Ok(events)
    }
    pub fn finish(&self) -> Result<(), String> {
        if self.buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
            Err("上游流式响应在事件中途断开".into())
        } else {
            Ok(())
        }
    }
}

pub(crate) struct Converter {
    protocol: String,
    context: ConversionContext,
    response: Value,
    slots: BTreeMap<String, usize>,
    sequence: u64,
    terminal: bool,
    incomplete: bool,
}
impl Converter {
    pub fn new(protocol: &str, model: Value, context: ConversionContext) -> Self {
        Self {
            protocol: protocol.into(),
            context,
            slots: BTreeMap::new(),
            sequence: 0,
            terminal: false,
            incomplete: false,
            response: json!({"id":format!("resp_cswitch_{:032x}",rand::random::<u128>()), "object":"response", "created_at":chrono::Utc::now().timestamp(), "status":"in_progress", "model":model, "output":[], "error":null,"usage":null}),
        }
    }
    fn event(&mut self, kind: &str, mut body: Value, output: &mut Vec<u8>) {
        body["type"] = json!(kind);
        body["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        output.extend_from_slice(format!("event: {kind}\ndata: {body}\n\n").as_bytes());
    }
    pub fn start(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        self.event(
            "response.created",
            json!({"response":self.response}),
            &mut out,
        );
        out
    }
    fn text(&mut self, key: &str, text: &str, out: &mut Vec<u8>) {
        let index = if let Some(index) = self.slots.get(key) {
            *index
        } else {
            let index = self.response["output"].as_array().unwrap().len();
            let id = format!("msg_{}_{}", self.response["id"].as_str().unwrap(), index);
            let item = json!({"type":"message","id":id,"role":"assistant","status":"in_progress","content":[{"type":"output_text","text":"","annotations":[]}]});
            self.response["output"]
                .as_array_mut()
                .unwrap()
                .push(item.clone());
            self.slots.insert(key.into(), index);
            let mut started = item.clone();
            started["content"] = json!([]);
            self.event(
                "response.output_item.added",
                json!({"output_index":index,"item":started}),
                out,
            );
            self.event("response.content_part.added",json!({"output_index":index,"item_id":id,"content_index":0,"part":item["content"][0]}),out);
            index
        };
        let item = &mut self.response["output"][index];
        let mut accumulated = item["content"][0]["text"].as_str().unwrap().to_string();
        accumulated.push_str(text);
        item["content"][0]["text"] = json!(accumulated);
        let id = item["id"].clone();
        self.event(
            "response.output_text.delta",
            json!({"output_index":index,"item_id":id,"content_index":0,"delta":text}),
            out,
        );
    }
    fn tool(
        &mut self,
        key: &str,
        id: Option<&str>,
        name: Option<&str>,
        delta: &str,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let index = if let Some(index) = self.slots.get(key) {
            *index
        } else {
            let name = name
                .filter(|name| !name.is_empty())
                .ok_or("工具流首个片段缺少工具名称")?;
            let id = id
                .filter(|id| !id.is_empty())
                .ok_or("工具流首个片段缺少调用编号")?;
            let index = self.response["output"].as_array().unwrap().len();
            let custom = self.context.is_custom(name);
            let item = json!({"type":if custom {"custom_tool_call"}else{"function_call"}, "id":format!("fc_{}_{}",self.response["id"].as_str().unwrap(),index), "call_id":id,"name":name,"arguments":"","status":"in_progress"});
            self.response["output"]
                .as_array_mut()
                .unwrap()
                .push(item.clone());
            self.slots.insert(key.into(), index);
            // Custom tool arguments wrap free-form input in JSON: emit only after decoding at the end.
            if !custom {
                self.event(
                    "response.output_item.added",
                    json!({"output_index":index,"item":item}),
                    out,
                );
            }
            index
        };
        let item = &mut self.response["output"][index];
        let mut arguments = item["arguments"].as_str().unwrap().to_string();
        arguments.push_str(delta);
        item["arguments"] = json!(arguments);
        let item_id = item["id"].clone();
        if item["type"] == "function_call" && !delta.is_empty() {
            self.event(
                "response.function_call_arguments.delta",
                json!({"output_index":index,"item_id":item_id,"delta":delta}),
                out,
            );
        }
        Ok(())
    }
    pub fn push(&mut self, value: Value) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        if value.get("error").is_some() || value["type"] == "error" {
            return Err("上游在流式响应中返回错误，响应未完成".into());
        }
        if value["cswitch_done"] == true {
            self.terminal = true;
            return Ok(out);
        }
        if self.protocol == "openai_chat" {
            if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
                self.response["usage"] = gateway_transform::chat_usage(Some(usage));
            }
            for choice in value["choices"].as_array().into_iter().flatten() {
                if choice["index"].as_u64().unwrap_or(0) != 0 {
                    return Err("上游返回多个候选回答，当前路由仅接收单个候选".into());
                }
                if let Some(reason) = choice["finish_reason"].as_str() {
                    self.terminal = true;
                    self.incomplete = reason == "length";
                }
                let delta = &choice["delta"];
                if let Some(text) = delta["content"].as_str() {
                    self.text("text", text, &mut out);
                }
                for call in delta["tool_calls"].as_array().into_iter().flatten() {
                    let key = format!("tool-{}", call["index"].as_u64().ok_or("工具流缺少 index")?);
                    self.tool(
                        &key,
                        call["id"].as_str(),
                        call["function"]["name"].as_str(),
                        call["function"]["arguments"].as_str().unwrap_or(""),
                        &mut out,
                    )?;
                }
            }
        } else {
            let index = value["index"].as_u64().unwrap_or(0);
            match value["type"].as_str() {
                Some("message_start") => self.response["usage"] = value["message"]["usage"].clone(),
                Some("content_block_start") => {
                    let block = &value["content_block"];
                    match block["type"].as_str() {
                        Some("text") => self.text(
                            &format!("text-{index}"),
                            block["text"].as_str().unwrap_or(""),
                            &mut out,
                        ),
                        Some("tool_use") => self.tool(
                            &format!("tool-{index}"),
                            block["id"].as_str(),
                            block["name"].as_str(),
                            "",
                            &mut out,
                        )?,
                        Some("thinking" | "redacted_thinking") => {}
                        _ => return Err("上游返回了尚未支持的内容块类型".into()),
                    }
                }
                Some("content_block_delta") => match value["delta"]["type"].as_str() {
                    Some("text_delta") => self.text(
                        &format!("text-{index}"),
                        value["delta"]["text"].as_str().ok_or("文本流缺少 text")?,
                        &mut out,
                    ),
                    Some("input_json_delta") => self.tool(
                        &format!("tool-{index}"),
                        None,
                        None,
                        value["delta"]["partial_json"]
                            .as_str()
                            .ok_or("工具流缺少 partial_json")?,
                        &mut out,
                    )?,
                    Some("thinking_delta" | "signature_delta") => {}
                    _ => return Err("上游返回了尚未支持的增量类型".into()),
                },
                Some("message_delta") => {
                    self.incomplete = value["delta"]["stop_reason"] == "max_tokens";
                    if let Some(usage) = value["usage"].as_object() {
                        if !self.response["usage"].is_object() {
                            self.response["usage"] = json!({});
                        }
                        for (key, val) in usage {
                            self.response["usage"][key] = val.clone();
                        }
                    }
                }
                Some("message_stop") => self.terminal = true,
                _ => {}
            }
        }
        Ok(out)
    }
    pub fn finish(&mut self) -> Result<Vec<u8>, String> {
        if !self.terminal {
            return Err("上游响应未发送结束事件，已停止处理截断内容".into());
        }
        if self.response["output"].as_array().unwrap().is_empty() {
            return Err("上游没有返回文本或工具调用".into());
        }
        let mut out = Vec::new();
        for index in 0..self.response["output"].as_array().unwrap().len() {
            let mut item = self.response["output"][index].clone();
            item["status"] = json!("completed");
            match item["type"].as_str() {
                Some("message") => {
                    self.event("response.output_text.done",json!({"output_index":index,"item_id":item["id"],"content_index":0,"text":item["content"][0]["text"]}),&mut out);
                    self.event("response.content_part.done",json!({"output_index":index,"item_id":item["id"],"content_index":0,"part":item["content"][0]}),&mut out);
                }
                Some("custom_tool_call") => {
                    let args: Value = serde_json::from_str(item["arguments"].as_str().unwrap())
                        .map_err(|_| "自定义工具参数 JSON 不完整")?;
                    item["input"] = json!(args["input"].as_str().ok_or("自定义工具缺少 input")?);
                    item.as_object_mut().unwrap().remove("arguments");
                    let mut started = item.clone();
                    started["input"] = json!("");
                    started["status"] = json!("in_progress");
                    self.event(
                        "response.output_item.added",
                        json!({"output_index":index,"item":started}),
                        &mut out,
                    );
                    self.event(
                        "response.custom_tool_call_input.delta",
                        json!({"output_index":index,"item_id":item["id"],"delta":item["input"]}),
                        &mut out,
                    );
                    self.event(
                        "response.custom_tool_call_input.done",
                        json!({"output_index":index,"item_id":item["id"],"input":item["input"]}),
                        &mut out,
                    );
                }
                _ => {
                    if item["arguments"] == "" {
                        item["arguments"] = json!("{}");
                    }
                    if !self.incomplete {
                        serde_json::from_str::<Value>(item["arguments"].as_str().unwrap())
                            .map_err(|_| "工具参数 JSON 不完整")?;
                    }
                    self.event("response.function_call_arguments.done",json!({"output_index":index,"item_id":item["id"],"arguments":item["arguments"]}),&mut out);
                }
            }
            self.event(
                "response.output_item.done",
                json!({"output_index":index,"item":item}),
                &mut out,
            );
            self.response["output"][index] = item;
        }
        if self.protocol == "anthropic_messages" {
            self.response["usage"] =
                gateway_transform::anthropic_usage(Some(&self.response["usage"]));
        }
        self.response["status"] = json!(if self.incomplete {
            "incomplete"
        } else {
            "completed"
        });
        if self.incomplete {
            self.response["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        let kind = if self.incomplete {
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.event(kind, json!({"response":self.response}), &mut out);
        Ok(out)
    }
    pub fn failed(&mut self, reason: &str) -> Vec<u8> {
        self.response["status"] = json!("failed");
        self.response["error"] = json!({"code":"upstream_stream_error","message":reason});
        let mut out = Vec::new();
        self.event(
            "response.failed",
            json!({"response":self.response}),
            &mut out,
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decoder_preserves_split_utf8_and_multiline_json() {
        let mut decoder = Decoder::default();
        let data = "data: {\"text\":\ndata: \"你好\"}\r\n\r\n".as_bytes();
        let mut events = Vec::new();
        for byte in data {
            events.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(events, vec![json!({"text":"你好"})]);
        decoder.finish().unwrap();
    }
    #[test]
    fn rejects_corrupt_or_truncated_events() {
        assert!(Decoder::default().push(b"data: {broken}\n\n").is_err());
        let mut decoder = Decoder::default();
        decoder.push(b"data: {\"partial\":").unwrap();
        assert!(decoder.finish().is_err());
    }
    #[test]
    fn emits_text_before_stream_completion() {
        let mut stream = Converter::new(
            "openai_chat",
            json!("fixture"),
            ConversionContext::default(),
        );
        let out = stream
            .push(json!({"choices":[{"index":0,"delta":{"content":"first"}}]}))
            .unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("response.output_text.delta")
        );
        assert!(stream.finish().is_err());
        stream
            .push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}))
            .unwrap();
        assert!(
            String::from_utf8(stream.finish().unwrap())
                .unwrap()
                .contains("response.completed")
        );
    }
}
