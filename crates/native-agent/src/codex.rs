//! ChatGPT OAuth uses the Codex Responses backend, never API chat completions.
//! Official API-key Responses requests share the same transcript and stream parser.
//! Wire contract: can1357/oh-my-pi packages/catalog/src/wire/codex.ts and
//! packages/ai/src/providers/openai-codex-responses.ts (also checked in bundled omp).
use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::watch::Sender;

const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const API_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const CLIENT_VERSION: &str = "0.159.0";
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_ITEMS: u64 = 4096;

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Codex data is missing a required string field"))
}

fn call_id(value: &str) -> Result<&str> {
    // Upstream transcripts may combine call_id and response item_id. Only the
    // former belongs in a function_call/function_call_output pair.
    let id = value.split(['|', '\n']).next().unwrap_or("");
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        bail!("Codex tool call identifier is invalid");
    }
    Ok(id)
}

fn input_content(content: &Value) -> Result<Vec<Value>> {
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"type":"input_text","text":text})]);
    }
    content.as_array().ok_or_else(|| anyhow!("Codex content must be text or blocks"))?
        .iter().map(|block| match string(block, "type")? {
            "text" => Ok(json!({"type":"input_text","text":string(block,"text")?})),
            "image" => {
                let mime = string(block, "mimeType")?;
                if !matches!(mime, "image/png" | "image/jpeg" | "image/webp" | "image/gif") {
                    bail!("Codex image format is unsupported");
                }
                Ok(json!({"type":"input_image","image_url":format!("data:{mime};base64,{}",string(block,"data")?),"detail":"auto"}))
            }
            _ => bail!("Codex content contains an unsupported block"),
        }).collect()
}

fn input_messages(messages: &[Value]) -> Result<Vec<Value>> {
    let mut input = Vec::with_capacity(messages.len());
    for message in messages {
        let content = message
            .get("content")
            .ok_or_else(|| anyhow!("Codex message has no content"))?;
        match string(message, "role")? {
            "user" => input.push(json!({"role":"user","content":input_content(content)?})),
            "toolResult" => {
                let blocks = input_content(content)?;
                let has_images = blocks.iter().any(|block| block["type"] == "input_image");
                let output = if has_images {
                    Value::Array(blocks)
                } else {
                    Value::String(
                        blocks
                            .iter()
                            .filter_map(|block| block["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                };
                input.push(json!({"type":"function_call_output","call_id":call_id(string(message,"toolCallId")?)?,"output":output}));
            }
            "assistant" => {
                if let Some(text) = content.as_str() {
                    if !text.is_empty() {
                        input.push(json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}));
                    }
                    continue;
                }
                for block in content
                    .as_array()
                    .ok_or_else(|| anyhow!("Codex assistant content must be text or blocks"))?
                {
                    match string(block, "type")? {
                        "text" => {
                            let text = string(block, "text")?;
                            if !text.is_empty() {
                                input.push(json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}));
                            }
                        }
                        // Native reasoning signatures are not part of this transcript format.
                        "thinking" => {}
                        "toolCall" => {
                            let arguments = block
                                .get("arguments")
                                .filter(|v| v.is_object())
                                .ok_or_else(|| anyhow!("Codex tool arguments must be an object"))?;
                            input.push(json!({"type":"function_call","call_id":call_id(string(block,"id")?)?,
                                "name":string(block,"name")?,"arguments":arguments.to_string()}));
                        }
                        _ => bail!("Codex assistant content contains an unsupported block"),
                    }
                }
            }
            _ => bail!("Codex conversation contains an unsupported message role"),
        }
    }
    Ok(input)
}

fn request_body(
    model: &str,
    instructions: &str,
    messages: &[Value],
    tools: &[Value],
) -> Result<Value> {
    if model.trim().is_empty() {
        bail!("Select a Codex model before starting a turn");
    }
    let mut converted_tools = Vec::with_capacity(tools.len());
    for tool in tools {
        if tool["type"] != "function" {
            bail!("Codex tool definition must be a function");
        }
        let function = tool
            .get("function")
            .ok_or_else(|| anyhow!("Codex tool definition is missing its function"))?;
        let parameters = function
            .get("parameters")
            .filter(|v| v.is_object())
            .ok_or_else(|| anyhow!("Codex tool parameter schema must be an object"))?;
        // Native tools do not declare the all-required schema required by strict
        // mode. Explicit false prevents the Responses default from changing it.
        converted_tools.push(json!({"type":"function","name":string(function,"name")?,
            "description":function.get("description").and_then(Value::as_str).unwrap_or(""),
            "parameters":parameters,"strict":false}));
    }
    // Codex refuses sampling controls and caller-supplied output-token caps.
    Ok(
        json!({"model":model,"instructions":instructions,"input":input_messages(messages)?,
        "tools":converted_tools,"store":false,"stream":true}),
    )
}

fn request(
    client: &reqwest::Client,
    body: &Value,
    token: &str,
    account_id: Option<&str>,
) -> Result<reqwest::Request> {
    if token.is_empty() {
        bail!("Sign in to OpenAI Codex before starting a turn");
    }
    let mut request = client
        .post(ENDPOINT)
        .bearer_auth(token)
        .header("accept", "text/event-stream")
        .header("openai-beta", "responses=experimental")
        .header("originator", "omp")
        .header("version", CLIENT_VERSION)
        .header(
            "x-codex-routing-hint",
            format!("model={}", string(body, "model")?),
        )
        .json(body);
    if let Some(id) = account_id.filter(|id| !id.is_empty()) {
        request = request.header("chatgpt-account-id", id);
    }
    request
        .build()
        .map_err(|_| anyhow!("Could not construct the Codex request"))
}

fn api_request(
    client: &reqwest::Client,
    body: &Value,
    api_key: &str,
) -> Result<reqwest::Request> {
    if api_key.is_empty() {
        bail!("Configure an OpenAI API key before starting a turn");
    }
    // API-key Responses traffic must not inherit Codex beta, account or routing headers.
    client
        .post(API_ENDPOINT)
        .bearer_auth(api_key)
        .header("accept", "text/event-stream")
        .json(body)
        .build()
        .map_err(|_| anyhow!("Could not construct the OpenAI Responses request"))
}

struct Event {
    name: String,
    data: String,
}

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    name: String,
    data: String,
    has_data: bool,
    after_cr: bool,
    started: bool,
    framed: bool,
    event_bytes: usize,
}

impl Decoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.event_bytes += 1;
            if self.event_bytes > MAX_EVENT_BYTES {
                bail!("Codex stream event exceeds the size limit");
            }
            if byte == b'\r' || byte == b'\n' {
                self.finish_line(&mut events)?;
                self.after_cr = byte == b'\r';
            } else {
                self.line.push(byte);
            }
        }
        Ok(events)
    }

    fn finish_line(&mut self, events: &mut Vec<Event>) -> Result<()> {
        let mut line = std::str::from_utf8(&self.line)
            .map_err(|_| anyhow!("Codex stream is not valid UTF-8"))?;
        if !self.started {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.started = true;
        }
        if line.is_empty() {
            if self.has_data {
                self.data.pop();
                events.push(Event {
                    name: std::mem::take(&mut self.name),
                    data: std::mem::take(&mut self.data),
                });
                self.has_data = false;
            } else {
                self.name.clear();
            }
            self.event_bytes = 0;
        } else if line.starts_with(':') {
            self.framed = true;
        } else {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if matches!(field, "event" | "data" | "id" | "retry") {
                self.framed = true;
            } else if !self.framed {
                bail!("Expected a Responses SSE body; received a different format (check sign-in or network proxy)");
            }
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => {
                    self.name.clear();
                    self.name.push_str(value);
                }
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                    self.has_data = true;
                }
                _ => {}
            }
        }
        self.line.clear();
        Ok(())
    }
}

enum Item {
    Message(BTreeMap<u64, String>),
    Function {
        id: String,
        name: String,
        arguments: String,
    },
    Reasoning,
}

#[derive(Default)]
struct ResponsesStream {
    items: BTreeMap<u64, Item>,
    output_bytes: usize,
    terminal: bool,
}

fn index(value: &Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .filter(|v| *v < MAX_ITEMS)
        .ok_or_else(|| anyhow!("Codex stream contains an invalid item index"))
}

fn completed_text(item: &Value) -> Result<BTreeMap<u64, String>> {
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Codex message content is missing"))?;
    if content.len() as u64 >= MAX_ITEMS {
        bail!("Codex message contains too many content parts");
    }
    let mut parts = BTreeMap::new();
    for (index, part) in content.iter().enumerate() {
        let text = match string(part, "type")? {
            "output_text" => string(part, "text")?,
            "refusal" => string(part, "refusal")?,
            _ => bail!("Codex returned unsupported message content"),
        };
        parts.insert(index as u64, text.to_owned());
    }
    Ok(parts)
}

impl ResponsesStream {
    fn count_output(&mut self, bytes: usize) -> Result<()> {
        self.output_bytes = self.output_bytes.saturating_add(bytes);
        if self.output_bytes > MAX_OUTPUT_BYTES {
            bail!("Codex response exceeds the size limit");
        }
        Ok(())
    }

    fn set_item(&mut self, position: u64, value: &Value, completed: bool) -> Result<()> {
        if position >= MAX_ITEMS {
            bail!("Codex response contains too many output items");
        }
        if value
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| {
                matches!(s, "incomplete" | "failed" | "cancelled")
                    || (completed && s != "completed")
            })
        {
            bail!("Codex returned an incomplete output item");
        }
        let item = match string(value, "type")? {
            "message" => Item::Message(completed_text(value)?),
            "function_call" => Item::Function {
                id: call_id(string(value, "call_id")?)?.to_owned(),
                name: string(value, "name")?.to_owned(),
                arguments: string(value, "arguments")?.to_owned(),
            },
            "reasoning" => Item::Reasoning,
            _ => bail!("Codex returned an unsupported output item"),
        };
        self.items.insert(position, item);
        Ok(())
    }

    fn accept(&mut self, event: Event) -> Result<bool> {
        if event.data == "[DONE]" {
            bail!("Codex stream ended without a Responses completion event");
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| anyhow!("Codex stream contains invalid JSON"))?;
        if !value.is_object() {
            bail!("Codex stream event must be an object");
        }
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(&event.name);
        if kind.is_empty() || (!event.name.is_empty() && event.name != kind) {
            bail!("Codex stream event type is invalid");
        }
        match kind {
            "error" | "response.failed" => bail!("Codex could not complete the response"),
            "response.incomplete" => bail!("Codex response was incomplete"),
            "response.completed" | "response.done" => {
                self.count_output(event.data.len())?;
                let response = value
                    .get("response")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| anyhow!("Codex completion is missing its response"))?;
                if response.get("status").and_then(Value::as_str) != Some("completed")
                    || response.get("error").is_some_and(|v| !v.is_null())
                    || response
                        .get("incomplete_details")
                        .is_some_and(|v| !v.is_null())
                {
                    bail!("Codex response did not complete successfully");
                }
                let output = response
                    .get("output")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("Codex completion is missing its output array"))?;
                for (position, item) in output.iter().enumerate() {
                    self.set_item(position as u64, item, true)?;
                }
                if output.len() != self.items.len() {
                    bail!("Codex completion does not match streamed output");
                }
                self.snapshot(true)?;
                self.terminal = true;
                Ok(true)
            }
            "response.output_item.added" | "response.output_item.done" => {
                self.count_output(event.data.len())?;
                self.set_item(
                    index(&value, "output_index")?,
                    value
                        .get("item")
                        .ok_or_else(|| anyhow!("Codex output item is missing"))?,
                    kind == "response.output_item.done",
                )?;
                Ok(true)
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let delta = string(&value, "delta")?;
                self.count_output(event.data.len())?;
                let position = index(&value, "output_index")?;
                let part = index(&value, "content_index")?;
                let item = self
                    .items
                    .entry(position)
                    .or_insert_with(|| Item::Message(BTreeMap::new()));
                let Item::Message(parts) = item else {
                    bail!("Codex text delta targets a non-message item");
                };
                parts.entry(part).or_default().push_str(delta);
                Ok(!delta.is_empty())
            }
            "response.output_text.done" | "response.refusal.done" => {
                let field = if kind == "response.refusal.done" {
                    "refusal"
                } else {
                    "text"
                };
                let text = string(&value, field)?;
                self.count_output(event.data.len())?;
                let item = self
                    .items
                    .entry(index(&value, "output_index")?)
                    .or_insert_with(|| Item::Message(BTreeMap::new()));
                let Item::Message(parts) = item else {
                    bail!("Codex text completion targets a non-message item");
                };
                parts.insert(index(&value, "content_index")?, text.to_owned());
                Ok(true)
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let done = kind.ends_with(".done");
                let text = string(&value, if done { "arguments" } else { "delta" })?;
                self.count_output(event.data.len())?;
                let Some(Item::Function { arguments, .. }) =
                    self.items.get_mut(&index(&value, "output_index")?)
                else {
                    bail!("Codex tool delta has no matching function call");
                };
                if done {
                    arguments.clear();
                }
                arguments.push_str(text);
                Ok(true)
            }
            // Lifecycle, content-part, reasoning, rate-limit and usage events do
            // not change this adapter's visible text/function transcript.
            _ => Ok(false),
        }
    }

    fn snapshot(&self, final_content: bool) -> Result<Vec<Value>> {
        let mut content = Vec::new();
        for item in self.items.values() {
            match item {
                Item::Message(parts) => {
                    for text in parts.values().filter(|text| !text.is_empty()) {
                        content.push(json!({"type":"text","text":text}));
                    }
                }
                Item::Function {
                    id,
                    name,
                    arguments,
                } => {
                    let parsed = serde_json::from_str::<Value>(arguments)
                        .ok()
                        .filter(Value::is_object);
                    match parsed {
                        Some(arguments) if !name.is_empty() => content.push(
                            json!({"type":"toolCall","id":id,"name":name,"arguments":arguments}),
                        ),
                        _ if final_content => {
                            bail!("Codex returned incomplete or invalid tool arguments")
                        }
                        _ => {}
                    }
                }
                Item::Reasoning => {}
            }
        }
        Ok(content)
    }
}

pub async fn generate(
    model: &str,
    instructions: &str,
    messages: &[Value],
    tools: &[Value],
    access_token: &str,
    account_id: Option<&str>,
    updates: Sender<Vec<Value>>,
) -> Result<Vec<Value>> {
    let client = http_client()?;
    let body = request_body(model, instructions, messages, tools)?;
    execute(&client, request(&client, &body, access_token, account_id)?, updates).await
}

pub async fn generate_api(
    model: &str,
    instructions: &str,
    messages: &[Value],
    tools: &[Value],
    api_key: &str,
    updates: Sender<Vec<Value>>,
) -> Result<Vec<Value>> {
    let client = http_client()?;
    let body = request_body(model, instructions, messages, tools)?;
    execute(&client, api_request(&client, &body, api_key)?, updates).await
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|_| anyhow!("Could not create the Responses HTTP client"))
}

async fn execute(
    client: &reqwest::Client,
    request: reqwest::Request,
    updates: Sender<Vec<Value>>,
) -> Result<Vec<Value>> {
    let response = client
        .execute(request)
        .await
        .map_err(|_| anyhow!("Responses connection failed (check network access)"))?;
    consume_response(response, updates).await
}

async fn consume_response(
    response: reqwest::Response,
    updates: Sender<Vec<Value>>,
) -> Result<Vec<Value>> {
    if !response.status().is_success() {
        // Never include remote bodies, header values, tokens, or reqwest errors.
        bail!("Codex request failed (HTTP {})", response.status().as_u16());
    }
    // Codex's upstream transport parses the body, not its MIME label. Some valid
    // streams arrive with missing/misleading Content-Type headers. The decoder
    // requires SSE framing and the parser requires a complete Responses schema;
    // HTML, raw JSON (including error envelopes) and truncated streams still fail.
    // stream:true officially promises SSE, so do not reinterpret JSON as success.
    let mut bytes = response.bytes_stream();
    let mut decoder = Decoder::default();
    let mut stream = ResponsesStream::default();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|_| anyhow!("Codex stream connection failed"))?;
        for event in decoder.push(&chunk)? {
            if stream.accept(event)? {
                let snapshot = stream.snapshot(stream.terminal)?;
                if stream.terminal {
                    updates.send_replace(snapshot.clone());
                    return Ok(snapshot);
                }
                updates.send_replace(snapshot);
            }
        }
    }
    bail!("Codex stream ended without a Responses completion event")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(value: Value) -> Event {
        Event {
            name: String::new(),
            data: value.to_string(),
        }
    }

    #[test]
    fn responses_request_converts_images_tools_and_results() {
        let messages = vec![
            json!({"role":"user","content":[{"type":"text","text":"inspect"},{"type":"image","mimeType":"image/png","data":"aGVsbG8="}]}),
            json!({"role":"assistant","content":[{"type":"text","text":"Working"},{"type":"thinking","thinking":"not replayed"},{"type":"toolCall","id":"call_1|fc_1","name":"read","arguments":{"path":"a"}}]}),
            json!({"role":"toolResult","toolCallId":"call_1|fc_1","content":[{"type":"text","text":"contents"}]}),
        ];
        let tools = vec![
            json!({"type":"function","function":{"name":"read","description":"Read file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}),
        ];
        let body = request_body("gpt-5-codex", "Be helpful", &messages, &tools).unwrap();
        assert_eq!(body["instructions"], "Be helpful");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("messages").is_none());
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,aGVsbG8="
        );
        assert_eq!(body["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(
            body["input"][2],
            json!({"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"a\"}"})
        );
        assert_eq!(
            body["input"][3],
            json!({"type":"function_call_output","call_id":"call_1","output":"contents"})
        );
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn tool_result_images_use_responses_content_blocks() {
        let input = input_messages(&[json!({"role":"toolResult","toolCallId":"call_a","content":[
            {"type":"text","text":"screenshot"},{"type":"image","mimeType":"image/jpeg","data":"abc"}
        ]})]).unwrap();
        assert_eq!(input[0]["output"][0]["type"], "input_text");
        assert_eq!(input[0]["output"][1]["type"], "input_image");
    }

    #[test]
    fn oauth_request_targets_only_official_codex_responses() {
        let client = reqwest::Client::new();
        let body = request_body("gpt-5-codex", "", &[], &[]).unwrap();
        let request =
            request(&client, &body, "synthetic-token", Some("synthetic-account")).unwrap();
        assert_eq!(request.url().as_str(), ENDPOINT);
        assert_eq!(request.headers()["authorization"], "Bearer synthetic-token");
        assert_eq!(request.headers()["chatgpt-account-id"], "synthetic-account");
        assert_eq!(request.headers()["openai-beta"], "responses=experimental");
        assert_eq!(request.headers()["version"], CLIENT_VERSION);
    }

    #[test]
    fn sse_handles_all_splits_unicode_multiline_and_crlf() {
        let source = "\u{feff}: ping\r\nevent: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\r\ndata: \"delta\":\"hé🌍\"}\r\n\r\n";
        for split in 0..=source.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder.push(&source.as_bytes()[..split]).unwrap();
            events.extend(decoder.push(&source.as_bytes()[split..]).unwrap());
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].name, "response.output_text.delta");
            let value: Value = serde_json::from_str(&events[0].data).unwrap();
            assert_eq!(value["delta"], "hé🌍");
        }
        let mut decoder = Decoder::default();
        let mut count = 0;
        for byte in source.bytes() {
            count += decoder.push(&[byte]).unwrap().len();
        }
        assert_eq!(count, 1);
        assert!(Decoder::default()
            .push(b"data: truncated\n")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn decoder_bounds_metadata_lines_and_rejects_invalid_utf8() {
        assert!(Decoder::default()
            .push(&vec![b'x'; MAX_EVENT_BYTES + 1])
            .is_err());
        let mut decoder = Decoder::default();
        decoder.push(b"data: x\n").unwrap();
        assert!(decoder.push(&vec![b'x'; MAX_EVENT_BYTES]).is_err());
        assert!(Decoder::default().push(b"data: \xff\n\n").is_err());
    }

    #[test]
    fn interleaved_tools_and_text_finalize_without_duplication() {
        let mut stream = ResponsesStream::default();
        stream.accept(event(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","content":[]}}))).unwrap();
        stream.accept(event(json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call_b","name":"grep","arguments":""}}))).unwrap();
        stream.accept(event(json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_a","name":"read","arguments":""}}))).unwrap();
        stream.accept(event(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Hello"}))).unwrap();
        stream.accept(event(json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":"}))).unwrap();
        assert_eq!(
            stream.snapshot(false).unwrap(),
            vec![json!({"type":"text","text":"Hello"})]
        );
        stream.accept(event(json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"pattern\":\"x\"}"}))).unwrap();
        stream.accept(event(json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"a\"}"}))).unwrap();
        stream.accept(event(json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":"Hello"}))).unwrap();
        stream.accept(event(json!({"type":"response.function_call_arguments.done","output_index":1,"arguments":"{\"path\":\"a\"}"}))).unwrap();
        assert!(!stream.terminal);
        stream
            .accept(event(
                json!({"type":"response.completed","response":{"status":"completed","output":[
                    {"type":"message","content":[{"type":"output_text","text":"Hello"}]},
                    {"type":"function_call","call_id":"call_a","name":"read","arguments":"{\"path\":\"a\"}"},
                    {"type":"function_call","call_id":"call_b","name":"grep","arguments":"{\"pattern\":\"x\"}"}
                ]}}),
            ))
            .unwrap();
        assert!(stream.terminal);
        assert_eq!(
            stream.snapshot(true).unwrap(),
            vec![
                json!({"type":"text","text":"Hello"}),
                json!({"type":"toolCall","id":"call_a","name":"read","arguments":{"path":"a"}}),
                json!({"type":"toolCall","id":"call_b","name":"grep","arguments":{"pattern":"x"}})
            ]
        );
    }

    #[test]
    fn terminal_output_can_supply_full_items_and_refusals() {
        let mut stream = ResponsesStream::default();
        stream
            .accept(event(
                json!({"type":"response.done","response":{"status":"completed","output":[
                    {"type":"reasoning"},
                    {"type":"message","content":[{"type":"refusal","refusal":"Cannot do that"}]},
                    {"type":"function_call","call_id":"call_x","name":"read","arguments":"{}"}
                ]}}),
            ))
            .unwrap();
        assert_eq!(
            stream.snapshot(true).unwrap(),
            vec![
                json!({"type":"text","text":"Cannot do that"}),
                json!({"type":"toolCall","id":"call_x","name":"read","arguments":{}})
            ]
        );
    }

    #[test]
    fn failures_and_malformed_tools_never_complete_or_leak_remote_errors() {
        for value in [
            json!({"type":"error","error":{"message":"synthetic-secret"}}),
            json!({"type":"response.failed","response":{"error":{"message":"synthetic-secret"}}}),
            json!({"type":"response.incomplete","response":{}}),
            json!({"type":"response.completed","response":{"status":"incomplete"}}),
            json!({"type":"response.completed","response":{"status":"failed","error":{"message":"synthetic-secret"}}}),
            json!({"type":"response.completed","response":{"output":[]}}),
            json!({"type":"response.completed","response":{"status":"completed"}}),
            json!({"type":"response.completed","response":{"status":"completed","output":[],"incomplete_details":{"reason":"max_output_tokens"}}}),
            json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","status":"in_progress","content":[]}]}}),
        ] {
            let mut stream = ResponsesStream::default();
            let error = stream.accept(event(value)).unwrap_err().to_string();
            assert!(!error.contains("synthetic-secret"));
            assert!(!stream.terminal);
        }
        for arguments in ["{", "[]", "null"] {
            let mut stream = ResponsesStream::default();
            stream.accept(event(json!({"type":"response.output_item.added","output_index":0,"item":{
                "type":"function_call","call_id":"call_a","name":"read","arguments":arguments
            }}))).unwrap();
            assert!(stream
                .accept(event(
                    json!({"type":"response.completed","response":{"status":"completed","output":[{
                        "type":"function_call","call_id":"call_a","name":"read","arguments":arguments
                    }]}})
                ))
                .is_err());
            assert!(!stream.terminal);
        }
        assert!(ResponsesStream::default()
            .accept(Event {
                name: String::new(),
                data: "[DONE]".into()
            })
            .is_err());
        assert!(ResponsesStream::default().accept(event(json!({"type":"response.output_text.delta","output_index":MAX_ITEMS,"content_index":0,"delta":"x"}))).is_err());
    }

    async fn http_response(
        status: u16,
        content_type: Option<&str>,
        body: String,
    ) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mime = content_type
            .map(|value| format!("Content-Type: {value}\r\n"))
            .unwrap_or_default();
        let header = format!(
            "HTTP/1.1 {status} Test\r\n{mime}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                request.push(byte[0]);
                assert!(request.len() < 16384);
            }
            socket.write_all(header.as_bytes()).await.unwrap();
            for chunk in body.as_bytes().chunks(7) {
                // An HTTP-error consumer may stop without reading the remote body.
                if socket.write_all(chunk).await.is_err() {
                    return;
                }
            }
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/responses"))
            .send()
            .await
            .unwrap()
    }

    fn sse(values: &[Value]) -> String {
        values
            .iter()
            .map(|value| format!("event: {}\r\ndata: {value}\r\n\r\n", value["type"].as_str().unwrap()))
            .collect()
    }

    #[tokio::test]
    async fn consumer_accepts_schema_valid_sse_despite_missing_or_misleading_mime() {
        let body = sse(&[
            json!({"type":"response.created","response":{"status":"in_progress"}}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hé🌍"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{
                "type":"function_call","status":"in_progress","call_id":"call_a","name":"read","arguments":""
            }}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":"}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"a\"}"}),
            json!({"type":"response.completed","response":{"status":"completed","error":null,"output":[
                {"type":"message","status":"completed","content":[{"type":"output_text","text":"hé🌍"}]},
                {"type":"function_call","status":"completed","call_id":"call_a","name":"read","arguments":"{\"path\":\"a\"}"}
            ]}}),
        ]);
        let expected = vec![
            json!({"type":"text","text":"hé🌍"}),
            json!({"type":"toolCall","id":"call_a","name":"read","arguments":{"path":"a"}}),
        ];
        for mime in [None, Some("application/json"), Some("text/html"), Some("application/octet-stream"), Some("text/event-stream; charset=utf-8")] {
            let response = http_response(200, mime, body.clone()).await;
            let (updates, receiver) = tokio::sync::watch::channel(Vec::new());
            assert_eq!(consume_response(response, updates).await.unwrap(), expected);
            assert_eq!(*receiver.borrow(), expected);
        }
    }

    #[tokio::test]
    async fn consumer_rejects_non_sse_error_pages_without_leaking_remote_details() {
        for body in [
            "<html><body>synthetic-secret sign-in required</body></html>".to_owned(),
            json!({"error":{"message":"synthetic-secret"}}).to_string(),
            // stream:true has no documented raw-JSON completion fallback.
            json!({"object":"response","status":"completed","output":[]}).to_string(),
        ] {
            for mime in [None, Some("text/html"), Some("application/json"), Some("text/event-stream")] {
                let response = http_response(200, mime, body.clone()).await;
                let (updates, receiver) = tokio::sync::watch::channel(Vec::new());
                let error = consume_response(response, updates).await.unwrap_err().to_string();
                assert!(!error.contains("synthetic-secret"));
                assert!(receiver.borrow().is_empty());
            }
        }
        let response = http_response(401, Some("application/json"), "synthetic-secret".into()).await;
        let (updates, _) = tokio::sync::watch::channel(Vec::new());
        let error = consume_response(response, updates).await.unwrap_err().to_string();
        assert!(error.contains("HTTP 401"));
        assert!(!error.contains("synthetic-secret"));
    }

    #[tokio::test]
    async fn consumer_never_promotes_truncated_failed_or_invalid_tool_streams_to_success() {
        let delta = json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"partial"});
        for body in [
            String::new(),
            sse(&[delta.clone()]),
            format!("{}data: {{\"type\":\"response.completed\"", sse(&[delta.clone()])),
            sse(&[json!({"type":"error","error":{"message":"synthetic-secret"}})]),
            sse(&[delta.clone(), json!({"type":"response.incomplete","response":{"status":"incomplete"}})]),
            sse(&[json!({"type":"response.completed","response":{"status":"completed","output":[{
                "type":"function_call","status":"completed","call_id":"call_a","name":"read","arguments":"{"
            }]}})]),
            sse(&[json!({"type":"response.completed","response":{"status":"completed","output":[],"error":{"message":"synthetic-secret"}}})]),
        ] {
            let response = http_response(200, None, body).await;
            let (updates, _) = tokio::sync::watch::channel(Vec::new());
            let error = consume_response(response, updates).await.unwrap_err().to_string();
            assert!(!error.contains("synthetic-secret"));
        }
    }
}
