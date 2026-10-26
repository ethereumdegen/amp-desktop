use regex::Regex;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

pub fn sensitive_component(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || name == ".git"
        || name == ".npmrc"
        || name == ".netrc"
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || [
            "credential",
            "secret",
            "token",
            "private-key",
            "private_key",
            "apikey",
            "api-key",
            "api_key",
        ]
        .iter()
        .any(|part| name.contains(part))
        || matches!(
            name.as_str(),
            "key.pem" | "key.json" | "auth.json" | "keys.json"
        )
}
fn normalized(cwd: &Path, input: &str) -> PathBuf {
    let path = if input == "~" || input.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join(input.strip_prefix("~/").unwrap_or(""))
    } else if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        cwd.join(input)
    };
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub struct TracePolicy {
    real: bool,
    cwd: PathBuf,
    project: PathBuf,
    discovery: HashSet<String>,
    omitted: HashSet<String>,
    omitted_pattern: Option<Regex>,
    pub breach: Option<String>,
}
impl TracePolicy {
    pub fn new(real: bool, cwd: &Path, project: &Path) -> Self {
        Self {
            real,
            cwd: cwd.to_owned(),
            project: project.to_owned(),
            discovery: HashSet::new(),
            omitted: HashSet::new(),
            omitted_pattern: None,
            breach: None,
        }
    }
    fn call(&mut self, id: &str, name: &str, args: &Value) {
        if !self.real {
            return;
        }
        let input = args["path"].as_str().unwrap_or(".");
        let lexical = normalized(&self.cwd, input);
        let path = std::fs::canonicalize(&lexical).unwrap_or(lexical);
        if matches!(name, "read" | "grep" | "write" | "edit") {
            let secret = path
                .components()
                .any(|part| sensitive_component(&part.as_os_str().to_string_lossy()));
            let root_doc = path == self.cwd.join("AGENTS.md") || path == self.cwd.join("README.md");
            if secret || (!path.starts_with(&self.project) && path != self.cwd && !root_doc) {
                self.breach = Some(format!("aborted real read-only case: sensitive/out-of-project {name} path {input:?}; result bodies omitted"));
            }
            if name == "grep" && !path.starts_with(&self.project) {
                self.breach = Some("aborted real case: grep outside the supplied project may disclose unrelated sources; result bodies omitted".into());
            }
        }
        if (name == "read" && path == self.cwd)
            || (name == "glob" && !path.starts_with(&self.project))
        {
            self.discovery.insert(id.into());
        }
    }
    fn remember_omitted(&mut self, path: &str) {
        let path = path.trim_start_matches("./");
        let name = path.split('/').next().unwrap_or("");
        if !name.is_empty()
            && !matches!(name, "harbor-tasks" | "README.md" | "AGENTS.md")
            && self.omitted.insert(name.into())
        {
            self.omitted_pattern = None;
        }
    }

    fn refresh_omitted_pattern(&mut self) {
        if self.omitted_pattern.is_none() && !self.omitted.is_empty() {
            let mut names: Vec<_> = self.omitted.iter().collect();
            names.sort_by_key(|name| std::cmp::Reverse(name.len()));
            self.omitted_pattern = Some(
                Regex::new(
                    &names
                        .into_iter()
                        .map(|name| regex::escape(name))
                        .collect::<Vec<_>>()
                        .join("|"),
                )
                .expect("escaped literal alternation is valid"),
            );
        }
    }

    fn redact_strings(&self, value: &mut Value) {
        let Some(pattern) = &self.omitted_pattern else {
            return;
        };
        match value {
            Value::String(text) => {
                let word = |ch: char| ch.is_alphanumeric() || matches!(ch, '_' | '-');
                let safe = pattern.replace_all(text, |captures: &regex::Captures<'_>| {
                    let found = captures.get(0).unwrap();
                    let before = text[..found.start()].chars().next_back();
                    let after = text[found.end()..].chars().next();
                    if !before.is_some_and(word) && !after.is_some_and(word) {
                        "[REDACTED sibling]".to_owned()
                    } else {
                        found.as_str().to_owned()
                    }
                });
                if let std::borrow::Cow::Owned(safe) = safe {
                    *text = safe;
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.redact_strings(item);
                }
            }
            Value::Object(object) => {
                // Keep protocol identities and encoded image bytes intact.
                for (key, item) in object {
                    if key == "path"
                        && item.as_str().is_some_and(|path| {
                            normalized(&self.cwd, path).starts_with(&self.project)
                        })
                    {
                        continue;
                    }
                    if !matches!(
                        key.as_str(),
                        "type"
                            | "role"
                            | "name"
                            | "id"
                            | "toolName"
                            | "toolCallId"
                            | "messageId"
                            | "stopReason"
                            | "status"
                            | "mimeType"
                            | "data"
                    ) {
                        self.redact_strings(item);
                    }
                }
            }
            _ => {}
        }
    }
    fn message(&mut self, message: &mut Value) {
        if message["role"] == "assistant" {
            if let Some(blocks) = message["content"].as_array() {
                for block in blocks {
                    if block["type"] == "toolCall" {
                        self.call(
                            block["id"].as_str().unwrap_or(""),
                            block["name"].as_str().unwrap_or(""),
                            &block["arguments"],
                        );
                    }
                }
            }
        }
        if message["role"] != "toolResult" {
            return;
        }
        if message["toolName"] == "grep" {
            // A scoped grep can still match a credential file. Inspect only
            // result path metadata before retaining any returned text bodies.
            let sensitive_match = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|block| block["text"].as_str())
                .filter_map(|body| serde_json::from_str::<Value>(body).ok())
                .any(|result| {
                    result["matches"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|item| {
                            Path::new(item["path"].as_str().unwrap_or(""))
                                .components()
                                .any(|part| {
                                    sensitive_component(&part.as_os_str().to_string_lossy())
                                })
                        })
                });
            if sensitive_match {
                self.breach = Some("aborted real case: grep matched a credential-like path; all result bodies omitted".into());
            }
        }
        if self.breach.is_some() {
            message["content"] = json!([{"type":"text","text":"[REDACTED: real-workspace privacy boundary triggered; no tool-result body retained]"}]);
            message["spiceTraceRedaction"] = json!("privacy-abort");
        } else if self
            .discovery
            .contains(message["toolCallId"].as_str().unwrap_or(""))
        {
            if let Some(blocks) = message["content"].as_array_mut() {
                for block in blocks {
                    if block["type"] != "text" {
                        continue;
                    }
                    let Ok(mut listing) =
                        serde_json::from_str::<Value>(block["text"].as_str().unwrap_or(""))
                    else {
                        block["text"] =
                            json!("[REDACTED: workspace discovery body was not structured JSON]");
                        continue;
                    };
                    if let Some(entries) = listing["entries"].as_array_mut() {
                        entries.retain(|entry| {
                            let keep = entry["name"] == "harbor-tasks";
                            if !keep {
                                self.remember_omitted(entry["name"].as_str().unwrap_or(""));
                            }
                            keep
                        });
                    }
                    if let Some(matches) = listing["matches"].as_array_mut() {
                        matches.retain(|entry| {
                            let path = entry.as_str().unwrap_or("").trim_start_matches("./");
                            let keep = path == "harbor-tasks" || path.starts_with("harbor-tasks/");
                            if !keep {
                                self.remember_omitted(path);
                            }
                            keep
                        });
                    }
                    if let Some(object) = listing.as_object_mut() {
                        // Pruning paths can also enumerate unrelated projects.
                        if object.remove("prunedDirectories").is_some() {
                            object.insert("spicePruningPathsOmitted".into(), json!(true));
                        }
                    }
                    listing["spiceTraceRedaction"] = json!("Unrelated sibling entries omitted; retained entries are unchanged actual tool output, not a complete workspace listing");
                    block["text"] = json!(listing.to_string());
                }
            }
            message["spiceTraceRedaction"] = json!("workspace-discovery-siblings-omitted");
        }
    }
    pub fn event(&mut self, event: &Value) -> Value {
        if !self.real {
            return event.clone();
        }
        let mut safe = event.clone();
        if safe["type"] == "tool_execution_start" {
            self.call(
                safe["toolCallId"].as_str().unwrap_or(""),
                safe["toolName"].as_str().unwrap_or(""),
                &safe["args"],
            );
        }
        if safe.get("message").is_some() {
            self.message(&mut safe["message"]);
        }
        if safe.get("result").is_some() {
            self.message(&mut safe["result"]);
        }
        if self.breach.is_some() {
            // After a privacy abort even assistant prose/error messages could
            // repeat sensitive data. Keep only type/ids/tool names and explicit
            // redaction markers; the failed run never reaches the judge.
            if safe["message"]["role"] == "assistant" {
                safe["message"]["content"] =
                    json!([{"type":"text","text":"[REDACTED after privacy abort]"}]);
            }
            if safe.get("error").is_some() {
                safe["error"] = json!({"message":"[REDACTED after privacy abort]"});
            }
            safe["spiceTraceRedaction"] = json!("privacy-abort");
        }
        self.refresh_omitted_pattern();
        self.redact_strings(&mut safe);
        safe
    }
    pub fn session(&mut self, session: &Value) -> Value {
        let mut safe = session.clone();
        if !self.real {
            return safe;
        }
        if let Some(messages) = safe["messages"].as_array_mut() {
            for message in messages {
                self.message(message);
            }
        }
        self.refresh_omitted_pattern();
        self.redact_strings(&mut safe);
        safe["spiceTraceRedaction"] = json!("Workspace discovery siblings omitted; original session verified in isolated temporary storage and not copied verbatim");
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_credentials_without_reading_them() {
        for name in [
            ".env",
            ".env.local",
            "credentials.json",
            "access_token",
            "id_rsa",
            ".git",
        ] {
            assert!(sensitive_component(name));
        }
        for name in [
            "README.md",
            "AGENTS.md",
            "package.json",
            "App.tsx",
            "styles.css",
        ] {
            assert!(!sensitive_component(name));
        }
    }
    #[test]
    fn omits_unrelated_discovery_and_sensitive_bodies() {
        let mut policy = TracePolicy::new(
            true,
            Path::new("/workspace"),
            Path::new("/workspace/harbor-tasks"),
        );
        policy.event(&json!({"type":"tool_execution_start","toolCallId":"listing","toolName":"read","args":{"path":"."}}));
        let event = policy.event(&json!({"type":"tool_execution_end","result":{"role":"toolResult","toolCallId":"listing","toolName":"read","content":[{"type":"text","text":json!({"entries":[{"name":"harbor-tasks","type":"directory"},{"name":"unrelated-private-project","type":"directory"}],"prunedDirectories":["unrelated-private-project/node_modules"]}).to_string()}]}}));
        let serialized = event.to_string();
        assert!(serialized.contains("harbor-tasks"));
        assert!(!serialized.contains("unrelated-private-project"));
        let assistant = json!({"role":"assistant","content":[{"type":"text","text":"Found harbor-tasks beside unrelated-private-project."},{"type":"toolCall","id":"call","name":"render_preview","arguments":{"html":"<p>unrelated-private-project</p>"}}]});
        let event = policy.event(&json!({"type":"message_end","message":assistant}));
        assert!(!event.to_string().contains("unrelated-private-project"));
        assert_eq!(event["message"]["content"][1]["name"], "render_preview");
        let session = policy.session(&json!({"messages":[assistant]}));
        assert!(!session.to_string().contains("unrelated-private-project"));
        policy.event(&json!({"type":"tool_execution_start","toolCallId":"secret","toolName":"read","args":{"path":"harbor-tasks/.env.local"}}));
        let event = policy.event(&json!({"type":"tool_execution_end","result":{"role":"toolResult","toolCallId":"secret","toolName":"read","content":[{"type":"text","text":"DO_NOT_RETAIN_REAL_SECRET"}]}}));
        assert!(policy.breach.is_some());
        assert!(!event.to_string().contains("DO_NOT_RETAIN_REAL_SECRET"));
    }
}
