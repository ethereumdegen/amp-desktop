use crate::{fixtures::snapshot, png::save_previews, privacy::TracePolicy};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use native_agent::{NativeClient, NativeOptions, ProviderConfig};
use serde_json::{json, Value};
use spice_framework::{AgentConfig, AgentOutput, AgentUnderTest, SpiceError, ToolCall, Turn};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const TOOLS: &[&str] = &[
    "read",
    "glob",
    "grep",
    "write",
    "edit",
    "bash",
    "render_preview",
];
const MAX_EVENT_BYTES: usize = 24 * 1024 * 1024;

pub struct NativeAdapter {
    pub provider: ProviderConfig,
    pub model: String,
    pub output_dir: PathBuf,
    pub timeout: Duration,
}

// Runner cancellation must not leave a provider request/browser/tool running.
// Normal completion awaits shutdown explicitly; this is the outer-timeout guard.
struct ShutdownGuard(Option<NativeClient>);
impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            tokio::spawn(async move {
                let _ = client.shutdown().await;
            });
        }
    }
}

fn text(message: &Value) -> String {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block["type"] == "text" {
                block["text"].as_str()
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Capture {
    turns: Vec<Turn>,
    started: HashMap<String, Instant>,
    approvals: Vec<Value>,
    error: Option<String>,
    final_text: String,
    updates: usize,
}
impl Capture {
    fn new() -> Self {
        Self {
            turns: vec![],
            started: HashMap::new(),
            approvals: vec![],
            error: None,
            final_text: String::new(),
            updates: 0,
        }
    }
    fn event(&mut self, event: &Value) -> Result<()> {
        match event["type"].as_str().unwrap_or("") {
            "message_start" if event["message"]["role"] == "assistant" => {
                if let Some(id) = event["messageId"].as_str() {
                    self.started.insert(id.into(), Instant::now());
                }
            }
            "message_update" => self.updates += 1,
            "message_end" if event["message"]["role"] == "assistant" => {
                let message = &event["message"];
                let calls = message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|block| block["type"] == "toolCall")
                    .map(|block| -> Result<ToolCall> {
                        Ok(ToolCall {
                            id: block["id"]
                                .as_str()
                                .context("native toolCall missing id")?
                                .into(),
                            name: block["name"]
                                .as_str()
                                .context("native toolCall missing name")?
                                .into(),
                            arguments: block["arguments"].clone(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let output = text(message);
                if calls.is_empty() && !output.is_empty() {
                    self.final_text = output.clone();
                }
                let duration = event["messageId"]
                    .as_str()
                    .and_then(|id| self.started.remove(id))
                    .map(|start| start.elapsed())
                    .unwrap_or_default();
                self.turns.push(Turn {
                    index: self.turns.len(),
                    output_text: Some(output),
                    tool_calls: calls,
                    tool_results: vec![],
                    stop_reason: message["stopReason"].as_str().map(str::to_owned),
                    duration,
                });
            }
            "tool_execution_end" => {
                let id = event["toolCallId"]
                    .as_str()
                    .context("native tool result missing call id")?;
                let turn = self
                    .turns
                    .iter_mut()
                    .rev()
                    .find(|turn| turn.tool_calls.iter().any(|call| call.id == id))
                    .context("native result has no captured assistant tool call")?;
                // Preserve the actual message, including blocks, call id, errors,
                // and image bytes. No inferred or synthetic tool successes.
                turn.tool_results.push(event["result"].clone());
            }
            "prompt_result" => {
                if event["status"] != "completed" {
                    self.error = Some(
                        event["error"]["message"]
                            .as_str()
                            .or_else(|| event["error"].as_str())
                            .unwrap_or("native prompt did not complete")
                            .into(),
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl NativeAdapter {
    async fn execute(&self, user_message: &str, config: &AgentConfig) -> Result<AgentOutput> {
        let start = Instant::now();
        let id = config.data["case_id"].as_str().context("missing case_id")?;
        if !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("invalid case_id");
        }
        let cwd = PathBuf::from(config.data["cwd"].as_str().context("missing fixture cwd")?);
        let protected = PathBuf::from(
            config.data["protected_root"]
                .as_str()
                .context("missing protected_root")?,
        );
        let before = snapshot(&protected)?;
        let case_dir = self.output_dir.join("native").join(id);
        fs::create_dir_all(&case_dir)?;
        let session_directory = tempfile::Builder::new()
            .prefix("omp-spice-session-")
            .tempdir()?;
        let session_path = session_directory.path().join("session.json");
        let real = config.data["real_workspace"].as_bool().unwrap_or(false);
        let mut privacy = TracePolicy::new(real, &cwd, &protected);
        let options = || NativeOptions {
            cwd: cwd.clone(),
            session_path: session_path.clone(),
            model: self.model.clone(),
            instructions: String::new(),
            thinking: Some("off".into()),
        };
        let client = NativeClient::spawn(self.provider.clone(), options()).await?;
        let mut guard = ShutdownGuard(Some(client.clone()));
        let mut receiver = client.subscribe();
        let mut raw = File::create(case_dir.join("events.jsonl"))?;
        let mut total_bytes = 0;
        let mut capture = Capture::new();
        let collect = async {
            client
                .request(json!({"type":"prompt", "message":user_message}))
                .await?;
            loop {
                let original = receiver
                    .recv()
                    .await
                    .context("native event stream lost; trace is incomplete")?;
                let event = privacy.event(&original);
                if let Some(reason) = &privacy.breach {
                    capture.error = Some(reason.clone());
                    client.request(json!({"type":"abort"})).await?;
                }
                let mut line = serde_json::to_vec(&event)?;
                line.push(b'\n');
                total_bytes += line.len();
                if total_bytes > MAX_EVENT_BYTES {
                    bail!("native raw trace exceeds 24 MiB safety limit");
                }
                raw.write_all(&line)?;
                capture.event(&event)?;
                if event["type"] == "extension_ui_request" && event["method"] == "confirm" {
                    let response = json!({"type":"extension_ui_response", "id":event["id"], "confirmed":false});
                    // No string matching, command allowlists, or implicit shell
                    // permission: every approval is denied in every scenario.
                    client.respond(response.clone()).await?;
                    let request = if privacy.breach.is_some() {
                        json!({"type":"extension_ui_request","id":event["id"],"method":"confirm","message":"[REDACTED after privacy abort]"})
                    } else {
                        event.clone()
                    };
                    capture
                        .approvals
                        .push(json!({"request":request, "response":response, "policy":"deny-all"}));
                }
                if event["type"] == "session_settled" {
                    break;
                }
                if privacy.breach.is_some() {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let run_result = tokio::time::timeout(self.timeout, collect).await;
        match run_result {
            Err(_) => {
                capture.error = Some(format!(
                    "native live run exceeded {:?}; cancelled without retry",
                    self.timeout
                ))
            }
            Ok(Err(error)) => capture.error = Some(error.to_string()),
            Ok(Ok(())) => {}
        }
        // Shutdown cancels the real provider and any browser process before the
        // filesystem comparison, including on timeout and event-stream errors.
        let stopped = client.shutdown().await;
        guard.0 = None;
        if let Err(error) = stopped {
            capture.error = Some(format!("native shutdown failed: {error}"));
        }
        raw.flush()?;
        fs::write(
            case_dir.join("approvals.json"),
            serde_json::to_vec_pretty(&capture.approvals)?,
        )?;
        let messages = client.request(json!({"type":"get_messages"})).await?["messages"].clone();
        let disk: Value = serde_json::from_slice(&fs::read(&session_path)?)?;
        let mut persistence_ok = disk["messages"] == messages;
        // Reopen through the actual runtime, not just a JSON decoder. No prompt
        // is submitted, so this does not make another provider request.
        let restored = NativeClient::spawn(self.provider.clone(), options()).await?;
        let mut restored_guard = ShutdownGuard(Some(restored.clone()));
        let restored_messages =
            restored.request(json!({"type":"get_messages"})).await?["messages"].clone();
        persistence_ok &= restored_messages == messages;
        restored.shutdown().await?;
        restored_guard.0 = None;
        let safe_session = privacy.session(&disk);
        if privacy.breach.is_none() {
            fs::write(
                case_dir.join("session.json"),
                serde_json::to_vec_pretty(&safe_session)?,
            )?;
        }
        let unchanged = snapshot(&protected)? == before;
        if !persistence_ok {
            capture.error =
                Some("persisted/reopened native messages differ from real runtime history".into());
        }
        if !unchanged {
            capture.error =
                Some("protected source paths or bytes changed during deny-all live run".into());
        }
        if let Some(reason) = &privacy.breach {
            capture.error = Some(reason.clone());
        }
        let mut output = AgentOutput {
            final_text: capture.final_text,
            turns: capture.turns,
            duration: start.elapsed(),
            error: capture.error,
            ..Default::default()
        };
        output.tools_called = output
            .turns
            .iter()
            .flat_map(|turn| turn.tool_calls.iter().map(|call| call.name.clone()))
            .collect();
        // Artifacts are decoded only from successful actual render_preview
        // result blocks. Validation failures stay visible in the Spice report.
        let preview_result = save_previews(&output, &case_dir);
        let (preview_artifacts, preview_error) = match preview_result {
            Ok(paths) => (paths, None),
            Err(error) => {
                let error = error.to_string();
                output.error = Some(error.clone());
                (vec![], Some(error))
            }
        };
        fs::write(
            case_dir.join("evidence.json"),
            serde_json::to_vec_pretty(&json!({
                "case_id":id, "cwd":cwd, "protected_root":protected,
                "source_unchanged":unchanged, "source_snapshot_entries":before.len(),
                "snapshot_excludes":["node_modules", ".git", "target", "dist", "build", ".next", ".cache", "coverage", "credential-like paths"],
                "trace_redaction":if real { "unrelated sibling discovery entries omitted; privacy-abort bodies omitted" } else { "none" },
                "privacy_abort":privacy.breach,
                "persistence_reopened_equal":persistence_ok, "streaming_updates":capture.updates,
                "approvals_denied":capture.approvals.len(), "preview_artifacts":preview_artifacts,
                "preview_error":preview_error, "error":output.error,
                "usage":"Native exposes estimated context, not billed tokens/cost; usage is intentionally unknown"
            }))?,
        )?;
        Ok(output)
    }
}

#[async_trait]
impl AgentUnderTest for NativeAdapter {
    async fn run(
        &self,
        user_message: &str,
        config: &AgentConfig,
    ) -> Result<AgentOutput, SpiceError> {
        self.execute(user_message, config)
            .await
            .map_err(|error| SpiceError::AgentError(format!("native live adapter: {error:#}")))
    }
    fn available_tools(&self, _: &AgentConfig) -> Vec<String> {
        TOOLS.iter().map(|name| (*name).into()).collect()
    }
    fn name(&self) -> &str {
        "NativeClient real streaming, deny-all approvals"
    }
}

pub fn result_text(result: &Value) -> String {
    text(result)
}
pub fn successful_results(output: &AgentOutput, name: &str) -> Vec<(ToolCall, Value)> {
    output
        .turns
        .iter()
        .flat_map(|turn| {
            turn.tool_calls
                .iter()
                .filter(move |call| call.name == name)
                .filter_map(move |call| {
                    turn.tool_results
                        .iter()
                        .find(|result| {
                            result["toolCallId"] == call.id && result["isError"] == false
                        })
                        .map(|result| (call.clone(), result.clone()))
                })
        })
        .collect()
}
pub fn read_evidence(output: &AgentOutput, suffix: &str, marker: &str) -> Result<(), String> {
    let found = successful_results(output, "read")
        .iter()
        .any(|(call, result)| {
            let path = call.arguments["path"].as_str().unwrap_or("");
            Path::new(path).ends_with(suffix) && result_text(result).contains(marker)
        });
    if found {
        Ok(())
    } else {
        Err(format!(
            "no successful actual read of {suffix} with expected source content {marker:?}"
        ))
    }
}
pub fn denied_evidence(output: &AgentOutput) -> Result<(), String> {
    let denial = output
        .turns
        .iter()
        .flat_map(|turn| turn.tool_results.iter())
        .any(|result| {
            matches!(result["toolName"].as_str(), Some("write" | "edit"))
                && result["isError"] == true
                && result_text(result).contains("denied")
        });
    if denial {
        Ok(())
    } else {
        Err("no real write/edit tool result recorded user-denied approval".into())
    }
}
