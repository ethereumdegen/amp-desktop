use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::{BTreeMap, BTreeSet}, sync::Mutex, time::Duration};
use tokio::sync::watch::Sender;

const KEYCHAIN_SERVICE: &str = "omp-desktop-native";
const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;
// Current IDs: https://developers.openai.com/api/docs/models
const OPENAI_MODELS: &[&str] = &["gpt-6.1-sol", "gpt-6-astra", "gpt-6-sol", "gpt-6-luna", "gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5-mini", "gpt-4.1", "gpt-4.1-mini"];
const CODEX_MODELS: &[&str] = &["gpt-6.1-sol", "gpt-6-astra", "gpt-6-sol", "gpt-6-luna", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];
const PREVIOUS_OPENAI_MODELS: &[&str] = &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5-mini", "gpt-4.1", "gpt-4.1-mini"];
const PREVIOUS_CODEX_MODELS: &[&str] = &["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"];
const ANTHROPIC_MODELS: &[&str] = &["claude-sonnet-5-5", "claude-opus-5-5", "claude-haiku-5-5", "claude-fable-5-1", "claude-sonnet-4-6", "claude-opus-4-6", "claude-haiku-4-5"];

pub fn default_openai_models(oauth: bool) -> Vec<String> {
    (if oauth { CODEX_MODELS } else { OPENAI_MODELS }).iter().map(|id| (*id).to_owned()).collect()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub api_key_env: Option<String>,
}

pub fn default_providers() -> Vec<ProviderConfig> {
    vec![
        ProviderConfig {
            id: "openai".into(),
            name: "OpenAI".into(),
            kind: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            models: default_openai_models(false),
            api_key_env: Some("OPENAI_API_KEY".into()),
        },
        ProviderConfig {
            id: "openrouter".into(),
            name: "OpenRouter".into(),
            kind: "openai".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            models: vec![],
            api_key_env: Some("OPENROUTER_API_KEY".into()),
        },
        ProviderConfig {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            kind: "anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            models: ANTHROPIC_MODELS.iter().map(|id| (*id).to_owned()).collect(),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
        },
    ]
}

/// Upgrade only exact shipped catalogs, retaining API/account mode and user edits.
pub fn migrate_default_catalog(config: &mut ProviderConfig) {
    let models = if config.id == "openai" && official_openai(config) {
        if config.models == PREVIOUS_CODEX_MODELS {
            CODEX_MODELS
        } else if config.models == PREVIOUS_OPENAI_MODELS
            || config.models == ["gpt-4.1", "gpt-4.1-mini"]
        {
            OPENAI_MODELS
        } else {
            return;
        }
    } else if config.id == "anthropic" && config.kind == "anthropic"
        && config.base_url == "https://api.anthropic.com/v1"
        && config.models == ["claude-sonnet-4-6", "claude-opus-4-6"]
    {
        ANTHROPIC_MODELS
    } else {
        return;
    };
    config.models = models.iter().map(|id| (*id).to_owned()).collect();
}

fn official_openai(config: &ProviderConfig) -> bool {
    config.kind == "openai" && config.base_url.trim_end_matches('/') == "https://api.openai.com/v1"
}

fn uses_openai_responses(config: &ProviderConfig, model: &str) -> bool {
    official_openai(config) && model.strip_prefix("gpt-")
        .and_then(|id| id.split(['.', '-']).next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 6)
}

fn openai_token_field(config: &ProviderConfig, model: &str) -> &'static str {
    if official_openai(config) && (model.starts_with("gpt-5") || model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4")) {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

fn openai_chat_model(id: &str) -> bool {
    (id.starts_with("gpt-") || id.starts_with("chatgpt-") || id.starts_with("o1") || id.starts_with("o3") || id.starts_with("o4"))
        && !["audio", "realtime", "transcribe", "tts", "image", "embedding", "search", "codex", "-pro", "deep-research", "instruct"].iter().any(|kind| id.contains(kind))
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("Provider id must contain 1–64 ASCII letters, digits, hyphens or underscores");
    }
    Ok(())
}

fn is_local(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

fn base_url(config: &ProviderConfig) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(&config.base_url)
        .map_err(|_| anyhow!("Provider base URL is invalid"))?;
    // Checking the authority also rejects explicitly empty user information (https://@host).
    let authority = config
        .base_url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""));
    if !url.username().is_empty()
        || url.password().is_some()
        || authority.is_some_and(|s| s.contains('@'))
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
        || !(url.scheme() == "https" || (url.scheme() == "http" && is_local(&url)))
    {
        bail!("Provider base URL must use HTTPS (or localhost HTTP), without credentials, query or fragment");
    }
    Ok(url)
}

pub fn validate_config(config: &ProviderConfig) -> Result<()> {
    validate_id(&config.id)?;
    if config.kind != "openai" && config.kind != "anthropic" {
        bail!("Provider kind must be openai or anthropic");
    }
    base_url(config)?;
    if let Some(name) = &config.api_key_env {
        let mut bytes = name.bytes();
        if !bytes
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            || !bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            bail!("API key environment variable must be a valid ASCII environment variable name");
        }
    }
    Ok(())
}

// Cache misses and backend failures as well as keys: status polls and tool rounds
// must not repeatedly ask the OS to unlock the same credential. Hold the lock
// through backend access so concurrent first reads and explicit writes serialize.
type SavedApiKey = std::result::Result<Option<String>, &'static str>;

#[derive(Default)]
struct ApiKeyCache {
    entries: Mutex<BTreeMap<String, SavedApiKey>>,
}

static API_KEY_CACHE: ApiKeyCache = ApiKeyCache {
    entries: Mutex::new(BTreeMap::new()),
};

impl ApiKeyCache {
    fn read(&self, id: &str, load: impl FnOnce() -> SavedApiKey) -> Result<Option<String>> {
        validate_id(id)?;
        let mut entries = self.entries.lock()
            .map_err(|_| anyhow!("Provider credential cache is unavailable"))?;
        let stored = match entries.get(id) {
            Some(stored) => stored.clone(),
            None => {
                let stored = load();
                entries.insert(id.to_owned(), stored.clone());
                stored
            }
        };
        stored.map_err(|message| anyhow!(message))
    }

    fn update(
        &self,
        id: &str,
        key: Option<&str>,
        persist: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        validate_id(id)?;
        let mut entries = self.entries.lock()
            .map_err(|_| anyhow!("Provider credential cache is unavailable"))?;
        // Publish only after persistence succeeds; a failed write must neither
        // advertise the new key nor forget a credential that is still saved.
        persist()?;
        entries.insert(id.to_owned(), Ok(key.map(str::to_owned)));
        Ok(())
    }

    fn api_key(
        &self,
        config: &ProviderConfig,
        environment: impl FnOnce(&str) -> std::result::Result<String, std::env::VarError>,
        load: impl FnOnce() -> SavedApiKey,
    ) -> Result<Option<String>> {
        if let Some(name) = &config.api_key_env {
            match environment(name) {
                Ok(value) if !value.trim().is_empty() => return Ok(Some(value)),
                Ok(_) | Err(std::env::VarError::NotPresent) => {}
                Err(_) => bail!("Provider API key environment variable is not valid Unicode"),
            }
        }
        match self.read(&config.id, load) {
            // Local compatible servers may intentionally run without authentication.
            // Their availability must not require an unlocked credential backend.
            Err(_) if is_local(&base_url(config)?) => Ok(None),
            result => result,
        }
    }
}

fn keychain_entry(id: &str) -> Result<keyring::Entry> {
    validate_id(id)?;
    keyring::Entry::new(KEYCHAIN_SERVICE, id)
        .map_err(|_| anyhow!("Could not access provider credential storage"))
}

pub fn store_api_key(id: &str, key: &str) -> Result<()> {
    if key.trim().is_empty() {
        bail!("API key must not be empty");
    }
    API_KEY_CACHE.update(id, Some(key), || {
        keychain_entry(id)?
            .set_password(key)
            .map_err(|_| anyhow!("Could not save provider API key"))
    })
}

pub fn remove_api_key(id: &str) -> Result<()> {
    API_KEY_CACHE.update(id, None, || match keychain_entry(id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(anyhow!("Could not remove provider API key")),
    })
}

fn api_key(config: &ProviderConfig) -> Result<Option<String>> {
    API_KEY_CACHE.api_key(config, |name| std::env::var(name), || {
        let entry = keychain_entry(&config.id)
            .map_err(|_| "Could not access provider credential storage")?;
        match entry.get_password() {
            Ok(key) if !key.trim().is_empty() => Ok(Some(key)),
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err("Could not read provider API key from credential storage"),
        }
    })
}

pub fn has_api_key(config: &ProviderConfig) -> bool {
    if validate_config(config).is_err() {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        if config.api_key_env.as_ref().is_some_and(|name| {
            std::env::var(name).is_ok_and(|key| !key.trim().is_empty())
        }) {
            return true;
        }
        crate::keychain::contains(KEYCHAIN_SERVICE, &config.id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        api_key(config).is_ok_and(|key| key.is_some())
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| anyhow!("Could not initialize provider HTTP client"))
}

fn endpoint(config: &ProviderConfig, path: &str) -> Result<reqwest::Url> {
    let mut url = base_url(config)?;
    url.set_path(&format!("{}/{}", url.path().trim_end_matches('/'), path));
    Ok(url)
}

fn authenticated(
    config: &ProviderConfig,
    request: reqwest::RequestBuilder,
    oauth: Option<&crate::oauth::OAuthCredential>,
) -> Result<reqwest::RequestBuilder> {
    if let Some(oauth) = oauth {
        if config.kind != "anthropic" || !crate::oauth::oauth_available(config) {
            bail!("Account credentials cannot be used with this provider endpoint");
        }
        return Ok(anthropic_oauth_headers(request, &oauth.access_token));
    }
    let key = api_key(config)?;
    if key.is_none() && !is_local(&base_url(config)?) {
        bail!("Provider credentials are missing; sign in or configure an API key");
    }
    let request = if config.kind == "anthropic" {
        request.header("anthropic-version", "2023-06-01")
    } else {
        request
    };
    match key {
        Some(key) if config.kind == "anthropic" => Ok(request.header("x-api-key", key)),
        Some(key) => Ok(request.bearer_auth(key)),
        None => Ok(request),
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let response = request
        .send()
        .await
        .map_err(|_| anyhow!("Provider request failed"))?;
    if !response.status().is_success() {
        // Never include upstream bodies, URLs, request headers, or chained reqwest errors.
        bail!("Provider returned HTTP {}", response.status().as_u16());
    }
    Ok(response)
}

pub async fn list_models(config: &ProviderConfig) -> Result<Vec<String>> {
    validate_config(config)?;
    tokio::time::timeout(Duration::from_secs(30), discover_models(config))
        .await
        .map_err(|_| anyhow!("Provider model discovery timed out after 30 seconds"))?
}

async fn discover_models(config: &ProviderConfig) -> Result<Vec<String>> {
    let oauth = crate::oauth::credentials(config).await?;
    if let Some(credential) = oauth.as_ref().filter(|_| config.kind == "openai") {
        return discover_codex_models(credential).await;
    }
    let http = client()?;
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut url = endpoint(config, "models")?;
        if config.kind == "anthropic" {
            url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(cursor) = &after {
                url.query_pairs_mut().append_pair("after_id", cursor);
            }
        }
        let response = send(authenticated(config, http.get(url), oauth.as_ref())?).await?;
        let body: Value = response
            .json()
            .await
            .map_err(|_| anyhow!("Provider model list is not valid JSON"))?;
        let data = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Provider model list has no data array"))?;
        for model in data {
            let id = model
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow!("Provider model list contains an invalid model id"))?;
            if !official_openai(config) || openai_chat_model(id) {
                models.push(id.to_owned());
            }
        }
        if config.kind != "anthropic" || body.get("has_more").and_then(Value::as_bool) != Some(true)
        {
            break;
        }
        let next = body
            .get("last_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("Provider model pagination is missing a cursor"))?;
        if after.as_deref() == Some(next) || data.is_empty() {
            bail!("Provider model pagination did not advance");
        }
        after = Some(next.to_owned());
    }
    models.sort();
    models.dedup();
    Ok(models)
}

async fn discover_codex_models(credential: &crate::oauth::OAuthCredential) -> Result<Vec<String>> {
    let mut request = client()?.get("https://chatgpt.com/backend-api/codex/models?client_version=0.159.0")
        .bearer_auth(&credential.access_token)
        .header("originator", "omp")
        .header("openai-beta", "responses=experimental")
        .header("version", "0.159.0");
    if let Some(id) = &credential.account_id {
        request = request.header("chatgpt-account-id", id);
    }
    let body: Value = send(request).await?.json().await
        .map_err(|_| anyhow!("Account model catalog is not valid JSON"))?;
    codex_model_ids(&body)
}

fn codex_model_ids(body: &Value) -> Result<Vec<String>> {
    let data = body["models"].as_array().or(body["data"].as_array())
        .ok_or_else(|| anyhow!("Account model catalog has no models array"))?;
    let mut seen = BTreeSet::new();
    let models: Vec<String> = data.iter()
        .filter(|m| !matches!(m["visibility"].as_str(), Some("hide" | "hidden")))
        .filter_map(|m| m["slug"].as_str().or(m["id"].as_str()))
        .filter(|id| !id.is_empty() && seen.insert(*id))
        .map(str::to_owned).collect();
    if models.is_empty() { bail!("Account model catalog has no available chat models"); }
    Ok(models)
}

fn anthropic_oauth_headers(request: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    request.bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14")
        .header("user-agent", "claude-cli/2.1.280 (external, cli)")
        .header("x-app", "cli")
        .header("anthropic-dangerous-direct-browser-access", "true")
        .header("x-stainless-arch", std::env::consts::ARCH)
        .header("x-stainless-os", if cfg!(target_os = "macos") { "MacOS" } else { "Linux" })
        .header("x-stainless-lang", "js")
        .header("x-stainless-package-version", "0.112.1")
        .header("x-stainless-runtime", "node")
        .header("x-stainless-runtime-version", "v26.3.0")
        .header("x-stainless-retry-count", "0")
        .header("x-stainless-timeout", "600")
}

fn prepare_anthropic_oauth(body: &mut Value, instructions: &str, messages: &[Value]) -> Result<()> {
    use sha2::{Digest, Sha256};
    let first = messages.iter().find(|m| m["role"] == "user")
        .and_then(|m| text_content(&m["content"]).ok()).unwrap_or_default();
    // Match the current upstream CLI fingerprint, including its UTF-16 indexing.
    let mut sample = [b'0' as u16; 3];
    for (index, code) in first.encode_utf16().take(21).enumerate() {
        if let Some(slot) = [4, 7, 20].iter().position(|&position| position == index) { sample[slot] = code; }
    }
    let sample: String = char::decode_utf16(sample).map(|c| c.unwrap_or('\u{fffd}')).collect();
    let digest = format!("{:x}", Sha256::digest(format!("59cf53e54c78{sample}2.1.280")));
    body["system"] = json!([
        {"type":"text","text":format!("x-anthropic-billing-header: cc_version=2.1.280.{}; cc_entrypoint=cli; cch=00000;", &digest[..3])},
        {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude.","cache_control":{"type":"ephemeral"}},
        {"type":"text","text":instructions}
    ]);
    // Upstream isolates custom tools from server-side builtins using a single '_'.
    if let Some(tools) = body["tools"].as_array_mut() {
        for tool in tools { tool["name"] = json!(format!("_{}", required_str(tool, "name")?)); }
    }
    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages {
            if let Some(blocks) = message["content"].as_array_mut() {
                for block in blocks.iter_mut().filter(|b| b["type"] == "tool_use") {
                    block["name"] = json!(format!("_{}", required_str(block, "name")?));
                }
            }
        }
    }
    Ok(())
}

fn decoded_anthropic_content(mut content: Vec<Value>, oauth: bool) -> Vec<Value> {
    if oauth {
        for block in content.iter_mut().filter(|b| b["type"] == "toolCall") {
            if let Some(name) = block["name"].as_str().and_then(|n| n.strip_prefix('_')) {
                block["name"] = json!(name);
            }
        }
    }
    content
}

fn text_content(content: &Value) -> Result<String> {
    if let Some(text) = content.as_str() {
        return Ok(text.to_owned());
    }
    let blocks = content
        .as_array()
        .ok_or_else(|| anyhow!("Message content must be text or blocks"))?;
    let mut text = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let part = required_str(block, "text")?;
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
            _ => bail!("Tool result contains unsupported content"),
        }
    }
    Ok(text)
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Provider message is missing a required string field"))
}

fn user_blocks(content: &Value, anthropic: bool) -> Result<Vec<Value>> {
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"type":"text","text":text})]);
    }
    let blocks = content
        .as_array()
        .ok_or_else(|| anyhow!("User content must be text or blocks"))?;
    blocks.iter().map(|block| {
        match required_str(block, "type")? {
            "text" => Ok(json!({"type":"text","text":required_str(block, "text")?})),
            "image" => {
                let data = required_str(block, "data")?;
                let mime = required_str(block, "mimeType")?;
                if anthropic {
                    Ok(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}))
                } else {
                    Ok(json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}}))
                }
            },
            _ => bail!("User content contains an unsupported block"),
        }
    }).collect()
}

fn openai_messages(instructions: &str, messages: &[Value]) -> Result<Vec<Value>> {
    let mut result = Vec::with_capacity(messages.len() + 1);
    let mut pending_tool_images = Vec::new();
    if !instructions.is_empty() {
        result.push(json!({"role":"system","content":instructions}));
    }
    for message in messages {
        let content = message
            .get("content")
            .ok_or_else(|| anyhow!("Message has no content"))?;
        // Chat Completions does not accept images in role=tool. Keep all tool
        // replies contiguous, then attach the actual images as a user multimodal
        // message before the next conversational message.
        if message["role"] != "toolResult" && !pending_tool_images.is_empty() {
            result.push(json!({"role":"user","content":std::mem::take(&mut pending_tool_images)}));
        }
        match required_str(message, "role")? {
            "user" => result.push(json!({"role":"user","content":user_blocks(content, false)?})),
            "toolResult" => {
                let blocks = user_blocks(content, false)?;
                let mut text = String::new();
                for block in blocks {
                    if block["type"] == "image_url" {
                        if pending_tool_images.is_empty() {
                            pending_tool_images.push(json!({"type":"text","text":"Actual images returned by the preceding tool results (design mockups, not existing-app screenshots unless explicitly stated by the tool):"}));
                        }
                        pending_tool_images.push(block);
                    } else {
                        if !text.is_empty() { text.push('\n'); }
                        text.push_str(required_str(&block, "text")?);
                    }
                }
                result.push(json!({"role":"tool","tool_call_id":required_str(message,"toolCallId")?,"content":text}));
            },
            "assistant" => {
                let mut text = String::new();
                let mut calls = Vec::new();
                if let Some(value) = content.as_str() {
                    text.push_str(value);
                } else {
                    let blocks = content
                        .as_array()
                        .ok_or_else(|| anyhow!("Assistant content must be text or blocks"))?;
                    for block in blocks {
                        match required_str(block, "type")? {
                            "text" => text.push_str(required_str(block, "text")?),
                            "thinking" | "redacted_thinking" => {}
                            "toolCall" => {
                                let arguments = block
                                    .get("arguments")
                                    .filter(|v| v.is_object())
                                    .ok_or_else(|| {
                                        anyhow!("Tool call arguments must be an object")
                                    })?;
                                calls.push(json!({"id":required_str(block,"id")?,"type":"function","function":{
                                    "name":required_str(block,"name")?,"arguments":arguments.to_string()
                                }}));
                            }
                            _ => bail!("Assistant content contains an unsupported block"),
                        }
                    }
                }
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut converted = json!({"role":"assistant","content":if text.is_empty() { Value::Null } else { Value::String(text) }});
                if !calls.is_empty() {
                    converted["tool_calls"] = Value::Array(calls);
                }
                result.push(converted);
            }
            _ => bail!("Conversation contains an unsupported message role"),
        }
    }
    if !pending_tool_images.is_empty() {
        result.push(json!({"role":"user","content":pending_tool_images}));
    }
    Ok(result)
}

fn anthropic_messages(messages: &[Value]) -> Result<Vec<Value>> {
    let mut result: Vec<Value> = Vec::with_capacity(messages.len());
    for message in messages {
        let content = message
            .get("content")
            .ok_or_else(|| anyhow!("Message has no content"))?;
        let (role, blocks) = match required_str(message, "role")? {
            "user" => ("user", user_blocks(content, true)?),
            "toolResult" => (
                "user",
                vec![json!({
                    "type":"tool_result","tool_use_id":required_str(message,"toolCallId")?,
                    "content":user_blocks(content, true)?,"is_error":message.get("isError").and_then(Value::as_bool).unwrap_or(false)
                })],
            ),
            "assistant" => {
                let mut converted = Vec::new();
                if let Some(text) = content.as_str() {
                    if !text.is_empty() {
                        converted.push(json!({"type":"text","text":text}));
                    }
                } else {
                    for block in content
                        .as_array()
                        .ok_or_else(|| anyhow!("Assistant content must be text or blocks"))?
                    {
                        match required_str(block, "type")? {
                            "text" => {
                                let text = required_str(block, "text")?;
                                if !text.is_empty() {
                                    converted.push(json!({"type":"text","text":text}));
                                }
                            }
                            "thinking" => {
                                if block["signature"].as_str().is_some_and(|s| !s.is_empty()) {
                                    converted.push(json!({"type":"thinking","thinking":required_str(block,"thinking")?,"signature":required_str(block,"signature")?}));
                                }
                            }
                            "redacted_thinking" => {
                                converted.push(json!({"type":"redacted_thinking","data":required_str(block,"data")?}));
                            }
                            "toolCall" => {
                                let input = block
                                    .get("arguments")
                                    .filter(|v| v.is_object())
                                    .ok_or_else(|| {
                                        anyhow!("Tool call arguments must be an object")
                                    })?;
                                converted
                                    .push(json!({"type":"tool_use","id":required_str(block,"id")?,
                                    "name":required_str(block,"name")?,"input":input}));
                            }
                            _ => bail!("Assistant content contains an unsupported block"),
                        }
                    }
                }
                if converted.is_empty() {
                    continue;
                }
                ("assistant", converted)
            }
            _ => bail!("Conversation contains an unsupported message role"),
        };
        // Anthropic tool results and subsequent queued user text belong in one user turn.
        if let Some(last) = result.last_mut().filter(|last| last["role"] == role) {
            last["content"]
                .as_array_mut()
                .ok_or_else(|| anyhow!("Invalid message assembly"))?
                .extend(blocks);
        } else {
            result.push(json!({"role":role,"content":blocks}));
        }
    }
    Ok(result)
}

fn anthropic_tools(tools: &[Value]) -> Result<Vec<Value>> {
    tools
        .iter()
        .map(|tool| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                bail!("Provider tool definition must be a function");
            }
            let function = tool
                .get("function")
                .ok_or_else(|| anyhow!("Missing tool function definition"))?;
            Ok(json!({"name":required_str(function,"name")?,
            "description":function.get("description").and_then(Value::as_str).unwrap_or(""),
            "input_schema":function.get("parameters").filter(|v| v.is_object())
                .ok_or_else(|| anyhow!("Missing tool parameter schema"))?}))
        })
        .collect()
}

struct SseEvent {
    name: String,
    data: String,
}

#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    event_name: String,
    data: String,
    has_data: bool,
    after_cr: bool,
    started: bool,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.finish_line(&mut events)?;
                self.after_cr = byte == b'\r';
            } else {
                if self.line.len() + self.data.len() >= MAX_SSE_EVENT_BYTES {
                    bail!("Provider stream event exceeds the size limit");
                }
                self.line.push(byte);
            }
        }
        Ok(events)
    }

    fn finish_line(&mut self, events: &mut Vec<SseEvent>) -> Result<()> {
        let mut line = std::str::from_utf8(&self.line)
            .map_err(|_| anyhow!("Provider stream is not valid UTF-8"))?;
        if !self.started {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.started = true;
        }
        if line.is_empty() {
            if self.has_data {
                self.data.pop(); // The last SSE data-field newline is not part of the event.
                events.push(SseEvent {
                    name: std::mem::take(&mut self.event_name),
                    data: std::mem::take(&mut self.data),
                });
                self.has_data = false;
            } else {
                self.event_name.clear();
            }
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => {
                    self.event_name.clear();
                    self.event_name.push_str(value);
                }
                "data" => {
                    if self.data.len() + value.len() + 1 > MAX_SSE_EVENT_BYTES {
                        bail!("Provider stream event exceeds the size limit");
                    }
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

#[derive(Default)]
struct ToolDelta {
    id: String,
    name: String,
    arguments: String,
}

impl ToolDelta {
    fn block(&self, final_block: bool) -> Result<Option<Value>> {
        if self.id.is_empty() || self.name.is_empty() {
            if final_block {
                bail!("Provider stream contains an incomplete tool call");
            }
            return Ok(None);
        }
        let arguments: Value = match serde_json::from_str(&self.arguments) {
            Ok(value) => value,
            Err(_) if !final_block => return Ok(None),
            Err(_) => bail!("Provider stream contains invalid tool arguments"),
        };
        if !arguments.is_object() {
            if final_block {
                bail!("Provider tool arguments must be an object");
            }
            return Ok(None);
        }
        Ok(Some(
            json!({"type":"toolCall","id":self.id,"name":self.name,"arguments":arguments}),
        ))
    }
}

#[derive(Default)]
struct OpenAiStream {
    text: String,
    tools: BTreeMap<u64, ToolDelta>,
    finished: bool,
    terminal: bool,
}

impl OpenAiStream {
    fn accept(&mut self, event: SseEvent) -> Result<bool> {
        if event.data.trim() == "[DONE]" {
            if !self.finished {
                bail!("Provider stream ended without a completion reason");
            }
            self.terminal = true;
            return Ok(false);
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| anyhow!("Provider stream contains invalid JSON"))?;
        if value.get("error").is_some() {
            bail!("Provider reported a streaming error");
        }
        let choices = value
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Provider stream contains no choices"))?;
        let mut changed = false;
        for choice in choices {
            if choice.get("index").and_then(Value::as_u64) != Some(0) {
                continue;
            }
            let delta = choice
                .get("delta")
                .ok_or_else(|| anyhow!("Provider stream contains no delta"))?;
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if self.finished {
                    bail!("Provider sent content after completion");
                }
                self.text.push_str(text);
                changed |= !text.is_empty();
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                if self.finished {
                    bail!("Provider sent tools after completion");
                }
                for call in calls {
                    let index = call
                        .get("index")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow!("Provider tool delta has no index"))?;
                    let tool = self.tools.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        tool.id.push_str(id);
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            tool.name.push_str(name);
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                            tool.arguments.push_str(arguments);
                        }
                    }
                    changed = true;
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if reason != "stop" && reason != "tool_calls" {
                    bail!("Provider response stopped before successful completion");
                }
                self.finished = true;
            }
        }
        Ok(changed)
    }

    fn snapshot(&self, final_block: bool) -> Result<Vec<Value>> {
        let mut blocks = Vec::with_capacity(self.tools.len() + 1);
        if !self.text.is_empty() {
            blocks.push(json!({"type":"text","text":self.text}));
        }
        for tool in self.tools.values() {
            if let Some(block) = tool.block(final_block)? {
                blocks.push(block);
            }
        }
        Ok(blocks)
    }
}

struct AnthropicBlock {
    value: Value,
    tool: Option<ToolDelta>,
    stopped: bool,
}

#[derive(Default)]
struct AnthropicStream {
    blocks: BTreeMap<u64, AnthropicBlock>,
    started: bool,
    finished: bool,
    terminal: bool,
}

impl AnthropicStream {
    fn accept(&mut self, event: SseEvent) -> Result<bool> {
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| anyhow!("Provider stream contains invalid JSON"))?;
        let kind = required_str(&value, "type")?;
        if !event.name.is_empty() && event.name != kind {
            bail!("Provider stream event type does not match its payload");
        }
        match kind {
            "ping" => Ok(false),
            "error" => bail!("Provider reported a streaming error"),
            "message_start" => {
                if self.started {
                    bail!("Provider sent duplicate message start");
                }
                self.started = true;
                Ok(false)
            }
            "content_block_start" => {
                if !self.started || self.finished {
                    bail!("Provider sent a block outside an active message");
                }
                let index = block_index(&value)?;
                let source = value
                    .get("content_block")
                    .ok_or_else(|| anyhow!("Provider stream has no content block"))?;
                let (block, tool) = match required_str(source, "type")? {
                    "text" => (
                        json!({"type":"text","text":required_str(source,"text")?}),
                        None,
                    ),
                    "thinking" => (
                        json!({"type":"thinking","thinking":required_str(source,"thinking")?,"signature":source["signature"].as_str().unwrap_or("")}),
                        None,
                    ),
                    "redacted_thinking" => (
                        json!({"type":"redacted_thinking","data":required_str(source,"data")?}),
                        None,
                    ),
                    "tool_use" => {
                        let input = source
                            .get("input")
                            .filter(|v| v.is_object())
                            .ok_or_else(|| anyhow!("Provider tool input must be an object"))?;
                        (
                            Value::Null,
                            Some(ToolDelta {
                                id: required_str(source, "id")?.into(),
                                name: required_str(source, "name")?.into(),
                                arguments: if input.as_object().is_some_and(|v| v.is_empty()) {
                                    String::new()
                                } else {
                                    input.to_string()
                                },
                            }),
                        )
                    }
                    _ => bail!("Provider sent an unsupported content block"),
                };
                if self
                    .blocks
                    .insert(
                        index,
                        AnthropicBlock {
                            value: block,
                            tool,
                            stopped: false,
                        },
                    )
                    .is_some()
                {
                    bail!("Provider sent a duplicate content block");
                }
                Ok(true)
            }
            "content_block_delta" => {
                let block = self
                    .blocks
                    .get_mut(&block_index(&value)?)
                    .ok_or_else(|| anyhow!("Provider delta references an unknown block"))?;
                if block.stopped {
                    bail!("Provider sent a delta after block completion");
                }
                let delta = value
                    .get("delta")
                    .ok_or_else(|| anyhow!("Provider stream has no block delta"))?;
                match required_str(delta, "type")? {
                    "text_delta" if block.value["type"] == "text" => {
                        append_string(&mut block.value, "text", required_str(delta, "text")?)?;
                    }
                    "thinking_delta" if block.value["type"] == "thinking" => {
                        append_string(
                            &mut block.value,
                            "thinking",
                            required_str(delta, "thinking")?,
                        )?;
                    }
                    "signature_delta" if block.value["type"] == "thinking" => {
                        append_string(&mut block.value, "signature", required_str(delta, "signature")?)?;
                    }
                    "input_json_delta" if block.tool.is_some() => {
                        if let Some(tool) = block.tool.as_mut() {
                            tool.arguments
                                .push_str(required_str(delta, "partial_json")?);
                        }
                    }
                    _ => bail!("Provider sent an unsupported block delta"),
                }
                Ok(true)
            }
            "content_block_stop" => {
                let block = self
                    .blocks
                    .get_mut(&block_index(&value)?)
                    .ok_or_else(|| anyhow!("Provider stopped an unknown block"))?;
                if block.stopped {
                    bail!("Provider stopped a block twice");
                }
                if let Some(tool) = block.tool.as_mut() {
                    if tool.arguments.is_empty() {
                        tool.arguments.push_str("{}");
                    }
                    tool.block(true)?;
                }
                block.stopped = true;
                Ok(true)
            }
            "message_delta" => {
                if !self.started || self.finished {
                    bail!("Provider sent an invalid message completion");
                }
                let reason = value
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Provider message completion has no stop reason"))?;
                if reason != "end_turn" && reason != "tool_use" && reason != "stop_sequence" {
                    bail!("Provider response stopped before successful completion");
                }
                self.finished = true;
                Ok(false)
            }
            "message_stop" => {
                if !self.finished || self.blocks.values().any(|block| !block.stopped) {
                    bail!("Provider message ended before all content completed");
                }
                self.terminal = true;
                Ok(false)
            }
            _ => bail!("Provider sent an unsupported stream event"),
        }
    }

    fn snapshot(&self, final_block: bool) -> Result<Vec<Value>> {
        let mut content = Vec::with_capacity(self.blocks.len());
        for block in self.blocks.values() {
            if let Some(tool) = &block.tool {
                if let Some(value) = tool.block(final_block)? {
                    content.push(value);
                }
            } else {
                content.push(block.value.clone());
            }
        }
        Ok(content)
    }
}

fn block_index(value: &Value) -> Result<u64> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("Provider block has no index"))
}

fn append_string(value: &mut Value, field: &str, text: &str) -> Result<()> {
    let existing = match value.get_mut(field) {
        Some(Value::String(existing)) => existing,
        _ => bail!("Provider content block has an invalid text field"),
    };
    existing.push_str(text);
    Ok(())
}

pub async fn generate(
    config: &ProviderConfig,
    model: &str,
    instructions: &str,
    messages: &[Value],
    tools: &[Value],
    updates: Sender<Vec<Value>>,
) -> Result<Vec<Value>> {
    validate_config(config)?;
    if model.is_empty() {
        bail!("Select a provider model before starting a turn");
    }
    let oauth = crate::oauth::credentials(config).await?;
    if let Some(credential) = oauth.as_ref().filter(|_| config.kind == "openai") {
        return crate::codex::generate(model, instructions, messages, tools, &credential.access_token, credential.account_id.as_deref(), updates).await;
    }
    if uses_openai_responses(config, model) {
        let key = api_key(config)?
            .ok_or_else(|| anyhow!("Provider credentials are missing; sign in or configure an API key"))?;
        return crate::codex::generate_api(model, instructions, messages, tools, &key, updates).await;
    }
    let anthropic = config.kind == "anthropic";
    let mut body = if anthropic {
        json!({"model":model,"system":instructions,"messages":anthropic_messages(messages)?,
            "max_tokens":8192,"stream":true})
    } else {
        json!({"model":model,"messages":openai_messages(instructions,messages)?,"stream":true})
    };
    if !anthropic {
        body[openai_token_field(config, model)] = json!(8192);
    }
    if !tools.is_empty() {
        body["tools"] = if anthropic {
            Value::Array(anthropic_tools(tools)?)
        } else {
            Value::Array(tools.to_vec())
        };
    }
    let path = if anthropic {
        "messages"
    } else {
        "chat/completions"
    };
    let mut url = endpoint(config, path)?;
    let oauth_anthropic = anthropic && oauth.is_some();
    if oauth_anthropic {
        url.set_query(Some("beta=true"));
        prepare_anthropic_oauth(&mut body, instructions, messages)?;
    }
    let mut payload = serde_json::to_string(&body).map_err(|_| anyhow!("Could not encode provider request"))?;
    if oauth_anthropic {
        let hash = xxhash_rust::xxh64::xxh64(payload.as_bytes(), 0x4d659218e32a3268) & 0xfffff;
        // Replace only the known first system block, never user-provided content.
        let marker = "\"text\":\"x-anthropic-billing-header:";
        let start = payload.find(marker).ok_or_else(|| anyhow!("Missing account request identity"))?;
        let offset = payload[start..].find("cch=00000").ok_or_else(|| anyhow!("Missing account request attestation"))? + start;
        payload.replace_range(offset..offset + 9, &format!("cch={hash:05x}"));
    }
    let response = send(authenticated(
        config,
        client()?.post(url).header("accept", "text/event-stream")
            .header("content-type", "application/json").body(payload),
        oauth.as_ref(),
    )?).await?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
    {
        bail!("Provider did not return an SSE stream");
    }
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut openai = OpenAiStream::default();
    let mut claude = AnthropicStream::default();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|_| anyhow!("Provider stream connection failed"))?;
        for event in decoder.push(&bytes)? {
            let changed = if anthropic {
                claude.accept(event)?
            } else {
                openai.accept(event)?
            };
            if changed {
                let snapshot = if anthropic {
                    decoded_anthropic_content(claude.snapshot(false)?, oauth_anthropic)
                } else {
                    openai.snapshot(false)?
                };
                updates.send_replace(snapshot);
            }
            if if anthropic {
                claude.terminal
            } else {
                openai.terminal
            } {
                let final_content = if anthropic {
                    decoded_anthropic_content(claude.snapshot(true)?, oauth_anthropic)
                } else {
                    openai.snapshot(true)?
                };
                updates.send_replace(final_content.clone());
                return Ok(final_content);
            }
        }
    }
    bail!("Provider stream ended without its terminal marker")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_api_keys_cache_parallel_and_repeated_success_absence_and_denial() {
        use std::sync::{atomic::{AtomicUsize, Ordering}, Barrier};

        for outcome in [Ok(Some("saved-key".to_owned())), Ok(None), Err("credential access denied")] {
            let cache = ApiKeyCache::default();
            let calls = AtomicUsize::new(0);
            let ready = Barrier::new(8);
            let expected = outcome.clone().map_err(str::to_owned);
            std::thread::scope(|scope| {
                for _ in 0..8 {
                    scope.spawn(|| {
                        ready.wait();
                        for _ in 0..4 {
                            let actual = cache.read("openai", || {
                                calls.fetch_add(1, Ordering::SeqCst);
                                outcome.clone()
                            });
                            assert_eq!(actual.map_err(|error| error.to_string()), expected);
                        }
                    });
                }
            });
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                cache.read("another-provider", || Ok(Some("other-key".into()))).unwrap(),
                Some("other-key".into()),
            );
        }
    }

    #[test]
    fn saved_api_key_updates_replace_denials_and_removal_forgets_old_keys() {
        let cache = ApiKeyCache::default();
        assert!(cache.read("openai", || Err("credential access denied")).is_err());
        assert!(cache.update("openai", Some("unsaved"), || Err(anyhow!("write denied"))).is_err());
        assert_eq!(
            cache.read("openai", || panic!("denial must stay cached")).unwrap_err().to_string(),
            "credential access denied",
        );

        cache.update("openai", Some("first-key"), || Ok(())).unwrap();
        assert_eq!(
            cache.read("openai", || panic!("explicit save must replace denial")).unwrap(),
            Some("first-key".into()),
        );
        assert!(cache.update("openai", Some("unsaved"), || Err(anyhow!("write denied"))).is_err());
        assert!(cache.update("openai", None, || Err(anyhow!("delete denied"))).is_err());
        assert_eq!(
            cache.read("openai", || panic!("failed mutations must preserve the saved key")).unwrap(),
            Some("first-key".into()),
        );

        cache.update("openai", Some("replacement"), || Ok(())).unwrap();
        assert_eq!(
            cache.read("openai", || panic!("replacement must be immediate")).unwrap(),
            Some("replacement".into()),
        );
        cache.update("openai", None, || Ok(())).unwrap();
        for _ in 0..3 {
            assert_eq!(cache.read("openai", || panic!("removal must cache absence")).unwrap(), None);
        }
        cache.update("openai", Some("reconnected"), || Ok(())).unwrap();
        assert_eq!(
            cache.read("openai", || panic!("save must replace cached absence")).unwrap(),
            Some("reconnected".into()),
        );
    }

    #[test]
    fn api_key_environment_precedes_cached_keys_and_denials_without_global_environment_changes() {
        let cache = ApiKeyCache::default();
        let config = default_providers().remove(0);
        let key = cache.api_key(&config, |name| {
            assert_eq!(name, "OPENAI_API_KEY");
            Ok("environment-key".into())
        }, || panic!("environment keys must not access credential storage")).unwrap();
        assert_eq!(key, Some("environment-key".into()));

        assert!(cache.api_key(&config, |_| Err(std::env::VarError::NotPresent),
            || Err("credential access denied")).is_err());
        assert_eq!(
            cache.api_key(&config, |_| Ok("new-environment-key".into()),
                || panic!("environment must override cached denial")).unwrap(),
            Some("new-environment-key".into()),
        );
        cache.update("openai", Some("saved-key"), || Ok(())).unwrap();
        for environment in [Ok(" \t".into()), Err(std::env::VarError::NotPresent)] {
            assert_eq!(
                cache.api_key(&config, |_| environment,
                    || panic!("empty or absent environment must reuse saved key")).unwrap(),
                Some("saved-key".into()),
            );
        }
        assert!(cache.api_key(&config,
            |_| Err(std::env::VarError::NotUnicode(std::ffi::OsString::from("invalid"))),
            || panic!("invalid environment must not read storage")).is_err());
        let mut no_environment = config.clone();
        no_environment.api_key_env = None;
        assert_eq!(
            cache.api_key(&no_environment, |_| panic!("no environment configured"),
                || panic!("saved key must stay cached")).unwrap(),
            Some("saved-key".into()),
        );
    }

    #[test]
    fn local_provider_remains_unauthenticated_when_cached_storage_is_unavailable() {
        let cache = ApiKeyCache::default();
        let mut config = default_providers().remove(0);
        config.api_key_env = None;
        config.base_url = "http://127.0.0.1:1234/v1".into();
        for _ in 0..3 {
            assert_eq!(cache.api_key(&config, |_| unreachable!(),
                || Err("credential access denied")).unwrap(), None);
        }
        config.base_url = "https://api.example.com/v1".into();
        assert!(cache.api_key(&config, |_| unreachable!(),
            || panic!("local fallback must retain the cached backend denial")).is_err());
        cache.update(&config.id, Some("saved-key"), || Ok(())).unwrap();
        config.base_url = "http://127.0.0.1:1234/v1".into();
        assert_eq!(cache.api_key(&config, |_| unreachable!(),
            || panic!("local providers still use explicitly saved keys")).unwrap(),
            Some("saved-key".into()));
    }

    fn event(value: Value) -> SseEvent {
        SseEvent {
            name: String::new(),
            data: value.to_string(),
        }
    }

    #[test]
    fn sse_survives_every_network_split_and_utf8_boundary() {
        let source = "\u{feff}: hello\r\nevent: content_block_delta\r\ndata: {\"text\":\"hé🌍\"}\r\n\r\ndata: [DONE]\n\n";
        for split in 0..=source.len() {
            let mut decoder = SseDecoder::default();
            let mut events = decoder.push(&source.as_bytes()[..split]).unwrap();
            events.extend(decoder.push(&source.as_bytes()[split..]).unwrap());
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].name, "content_block_delta");
            assert_eq!(events[0].data, "{\"text\":\"hé🌍\"}");
            assert_eq!(events[1].data, "[DONE]");
        }
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for byte in source.as_bytes() {
            events.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn sse_multiline_cr_only_comments_and_truncated_events() {
        let mut decoder = SseDecoder::default();
        let events = decoder
            .push(b": ping\revent: test\rdata: first\rdata:second\r\rdata: incomplete")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "test");
        assert_eq!(events[0].data, "first\nsecond");
        assert!(decoder.push(b"\n").unwrap().is_empty());
    }

    #[test]
    fn openai_assembles_interleaved_tool_deltas() {
        let mut stream = OpenAiStream::default();
        stream.accept(event(json!({"choices":[{"index":0,"delta":{"content":"Working","tool_calls":[
            {"index":1,"id":"call_b","type":"function","function":{"name":"gr","arguments":"{\"pattern\":"}},
            {"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":"{\"path\":\""}}
        ]},"finish_reason":null}]}))).unwrap();
        assert_eq!(
            stream.snapshot(false).unwrap(),
            vec![json!({"type":"text","text":"Working"})]
        );
        stream
            .accept(event(json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"a.txt\"}"}},
            {"index":1,"function":{"name":"ep","arguments":"\"hello\"}"}}
        ]},"finish_reason":null}]})))
            .unwrap();
        stream
            .accept(event(
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            ))
            .unwrap();
        assert!(!stream.terminal);
        stream
            .accept(SseEvent {
                name: String::new(),
                data: "[DONE]".into(),
            })
            .unwrap();
        assert!(stream.terminal);
        assert_eq!(
            stream.snapshot(true).unwrap(),
            vec![
                json!({"type":"text","text":"Working"}),
                json!({"type":"toolCall","id":"call_a","name":"read","arguments":{"path":"a.txt"}}),
                json!({"type":"toolCall","id":"call_b","name":"grep","arguments":{"pattern":"hello"}}),
            ]
        );
    }

    #[test]
    fn openai_rejects_truncation_and_invalid_tool_json() {
        let mut stream = OpenAiStream::default();
        assert!(stream
            .accept(SseEvent {
                name: String::new(),
                data: "[DONE]".into()
            })
            .is_err());
        stream
            .accept(event(json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call","function":{"name":"read","arguments":"{"}}
        ]},"finish_reason":"tool_calls"}]})))
            .unwrap();
        assert!(!stream.terminal);
        assert!(stream.snapshot(true).is_err());
        assert!(stream
            .accept(event(
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]})
            ))
            .is_err());
    }

    #[test]
    fn anthropic_assembles_text_and_tool_input() {
        let mut stream = AnthropicStream::default();
        for value in [
            json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Inspecting"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool_1","name":"read","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"src/main.rs\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}),
        ] {
            stream.accept(event(value)).unwrap();
        }
        assert!(!stream.terminal);
        stream
            .accept(event(json!({"type":"message_stop"})))
            .unwrap();
        assert!(stream.terminal);
        assert_eq!(
            stream.snapshot(true).unwrap(),
            vec![
                json!({"type":"text","text":"Inspecting"}),
                json!({"type":"toolCall","id":"tool_1","name":"read","arguments":{"path":"src/main.rs"}}),
            ]
        );
    }

    #[test]
    fn anthropic_requires_block_and_message_completion() {
        let mut stream = AnthropicStream::default();
        stream
            .accept(event(json!({"type":"message_start"})))
            .unwrap();
        stream.accept(event(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}}))).unwrap();
        assert!(stream
            .accept(event(json!({"type":"message_stop"})))
            .is_err());
        assert!(stream
            .accept(event(
                json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"}})
            ))
            .is_err());
    }

    #[test]
    fn history_preserves_tool_results_and_images_in_both_protocols() {
        let history = vec![
            json!({"role":"user","content":[{"type":"text","text":"Look"},{"type":"image","data":"aW1n","mimeType":"image/png"}]}),
            json!({"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"a"}}]}),
            json!({"role":"toolResult","toolCallId":"call_1","toolName":"read","content":[{"type":"text","text":"contents"}],"isError":false}),
        ];
        let openai = openai_messages("system", &history).unwrap();
        assert_eq!(
            openai[1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aW1n"
        );
        assert_eq!(
            openai[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert_eq!(openai[3]["role"], "tool");
        assert_eq!(openai[3]["tool_call_id"], "call_1");
        let anthropic = anthropic_messages(&history).unwrap();
        assert_eq!(
            anthropic[0]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(anthropic[1]["content"][0]["type"], "tool_use");
        assert_eq!(anthropic[2]["content"][0]["tool_use_id"], "call_1");
    }

    #[test]
    fn tool_images_continue_in_chat_and_anthropic_without_orphaning_calls() {
        let history = vec![
            json!({"role":"assistant","content":[
                {"type":"toolCall","id":"preview","name":"render_preview","arguments":{"html":"<p>mockup</p>"}},
                {"type":"toolCall","id":"read","name":"read","arguments":{"path":"README.md"}}
            ]}),
            json!({"role":"toolResult","toolCallId":"preview","content":[
                {"type":"text","text":"Design mockup rendered in Chrome"},
                {"type":"image","mimeType":"image/png","data":"aW1n"}
            ],"isError":false}),
            json!({"role":"toolResult","toolCallId":"read","content":[{"type":"text","text":"project facts"}],"isError":false}),
            json!({"role":"assistant","content":[{"type":"text","text":"Here is the proposed design."}]}),
        ];
        let chat = openai_messages("", &history).unwrap();
        assert_eq!(chat[1]["role"], "tool");
        assert_eq!(chat[2]["role"], "tool");
        assert_eq!(chat[3]["role"], "user");
        assert_eq!(chat[3]["content"][1]["image_url"]["url"], "data:image/png;base64,aW1n");
        assert_eq!(chat[4]["role"], "assistant");
        assert_eq!(chat[1]["content"], "Design mockup rendered in Chrome");
        let anthropic = anthropic_messages(&history).unwrap();
        assert_eq!(anthropic[1]["content"][0]["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(anthropic[1]["content"][0]["content"][0]["text"], "Design mockup rendered in Chrome");
        assert_eq!(anthropic[1]["content"][1]["tool_use_id"], "read");
        assert_eq!(anthropic[2]["role"], "assistant");
    }

    #[test]
    fn empty_interrupted_assistants_are_not_sent_to_providers() {
        let history = vec![
            json!({"role":"user","content":"First"}),
            json!({"role":"assistant","content":[]}),
            json!({"role":"assistant","content":""}),
            json!({"role":"assistant","content":[{"type":"text","text":""}]}),
            json!({"role":"assistant","content":[{"type":"thinking","thinking":"not replayed"}]}),
            json!({"role":"user","content":"Continue"}),
        ];
        let openai = openai_messages("", &history).unwrap();
        assert_eq!(openai.len(), 2);
        assert!(openai.iter().all(|message| message["role"] == "user"));
        let anthropic = anthropic_messages(&history).unwrap();
        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn configuration_rejects_unsafe_urls_ids_and_env_names() {
        let mut config = default_providers().remove(0);
        for url in [
            "http://api.example.com/v1",
            "https://user:secret@example.com/v1",
            "https://@example.com/v1",
            "https://example.com/v1?key=secret",
            "https://example.com/v1#secret",
            "file:///tmp/models",
        ] {
            config.base_url = url.into();
            assert!(validate_config(&config).is_err());
        }
        for url in [
            "https://example.com/v1",
            "http://localhost:1234/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:1234/v1",
        ] {
            config.base_url = url.into();
            assert!(validate_config(&config).is_ok());
        }
        config.id = "../bad".into();
        assert!(validate_config(&config).is_err());
        config.id = "a".repeat(65);
        assert!(validate_config(&config).is_err());
        config.id = "safe-id_1".into();
        config.api_key_env = Some("BAD=NAME".into());
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn stream_updates_coalesce_without_retaining_queued_snapshots() {
        let (updates, mut receiver) = tokio::sync::watch::channel(Vec::new());
        for index in 0..10_000 {
            updates.send_replace(vec![json!({"type":"text","text":index.to_string()})]);
        }
        assert_eq!(
            *receiver.borrow_and_update(),
            vec![json!({"type":"text","text":"9999"})]
        );
        assert!(!receiver.has_changed().unwrap());
    }

    #[test]
    fn official_catalog_excludes_nonchat_models_without_restricting_custom_ids() {
        for id in ["text-embedding-3-large", "whisper-1", "gpt-image-1", "gpt-4o-audio-preview", "gpt-4o-realtime-preview", "gpt-5.2-codex", "gpt-5.5-pro"] {
            assert!(!openai_chat_model(id), "{id}");
        }
        for id in ["gpt-6.1-sol", "gpt-6-astra", "gpt-7.2-future", "gpt-5.5", "gpt-5.4-mini", "gpt-4.1", "o3", "o4-mini"] {
            assert!(openai_chat_model(id), "{id}");
        }
        let mut custom = default_providers().remove(0);
        custom.base_url = "http://127.0.0.1:1234/v1".into();
        assert!(!official_openai(&custom));
        custom.models = vec!["my-arbitrary-compatible-model".into()];
        assert!(validate_config(&custom).is_ok());
    }

    #[test]
    fn migration_preserves_custom_catalogs_and_provider_endpoints() {
        let mut legacy = default_providers().remove(0);
        legacy.models = vec!["gpt-4.1".into(), "gpt-4.1-mini".into()];
        migrate_default_catalog(&mut legacy);
        assert_eq!(legacy.models, default_providers().remove(0).models);
        legacy.models.push("my-model".into());
        let custom = legacy.models.clone();
        migrate_default_catalog(&mut legacy);
        assert_eq!(legacy.models, custom);
        legacy.base_url = "https://example.com/v1".into();
        legacy.models = vec!["gpt-4.1".into(), "gpt-4.1-mini".into()];
        migrate_default_catalog(&mut legacy);
        assert_eq!(legacy.models.len(), 2);
    }

    #[test]
    fn migration_upgrades_both_previous_modes_without_replacing_custom_lists() {
        let mut config = default_providers().remove(0);
        for (previous, oauth) in [(PREVIOUS_OPENAI_MODELS, false), (PREVIOUS_CODEX_MODELS, true)] {
            config.models = previous.iter().map(|id| (*id).to_owned()).collect();
            migrate_default_catalog(&mut config);
            assert_eq!(config.models, default_openai_models(oauth));
            assert!(config.models.iter().any(|id| id == "gpt-6.1-sol"));
            config.models = previous.iter().map(|id| (*id).to_owned()).collect();
            config.models.push("custom-model".into());
            let custom = config.models.clone();
            migrate_default_catalog(&mut config);
            assert_eq!(config.models, custom);
        }
        config.models = PREVIOUS_CODEX_MODELS.iter().map(|id| (*id).to_owned()).collect();
        config.base_url = "https://compatible.example/v1".into();
        let custom = config.models.clone();
        migrate_default_catalog(&mut config);
        assert_eq!(config.models, custom);
    }

    #[test]
    fn account_discovery_preserves_future_ids_and_server_priority() {
        let models = codex_model_ids(&json!({"models":[
            {"slug":"gpt-6.1-sol","visibility":"list"},
            {"slug":"future-model-9","visibility":"list"},
            {"slug":"gpt-reserve","visibility":"hide"},
            {"id":"another-new-model"},
            {"slug":"future-model-9"},
            {"slug":"hidden-model","visibility":"hidden"}
        ]})).unwrap();
        assert_eq!(models, ["gpt-6.1-sol", "future-model-9", "another-new-model"]);
        assert!(codex_model_ids(&json!({"models":[{"slug":"hidden","visibility":"hide"}]})).is_err());
    }

    #[test]
    fn gpt6_and_future_official_models_use_responses_without_affecting_compatible_servers() {
        let mut config = default_providers().remove(0);
        for model in ["gpt-6.1-sol", "gpt-6-astra", "gpt-7.2-future", "gpt-10"] {
            assert!(uses_openai_responses(&config, model));
        }
        assert!(!uses_openai_responses(&config, "gpt-5.5"));
        config.base_url = "http://127.0.0.1:1234/v1".into();
        assert!(!uses_openai_responses(&config, "gpt-6.1-sol"));
    }

    #[test]
    fn anthropic_signed_and_redacted_thinking_survives_tool_continuation() {
        let history = vec![
            json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"reasoning","signature":"signed-by-provider"},
                {"type":"redacted_thinking","data":"opaque"},
                {"type":"toolCall","id":"tool_1","name":"read","arguments":{"path":"a"}}]}),
            json!({"role":"toolResult","toolCallId":"tool_1","content":"result"})
        ];
        let wire = anthropic_messages(&history).unwrap();
        assert_eq!(wire[0]["content"][0]["signature"], "signed-by-provider");
        assert_eq!(wire[0]["content"][1], json!({"type":"redacted_thinking","data":"opaque"}));
        let mut body = json!({"messages":wire,"tools":[{"name":"read","input_schema":{}}]});
        prepare_anthropic_oauth(&mut body, "Native instructions", &history).unwrap();
        assert_eq!(body["tools"][0]["name"], "_read");
        assert_eq!(body["messages"][0]["content"][2]["name"], "_read");
        assert_eq!(body["system"][1]["text"], "You are Claude Code, Anthropic's official CLI for Claude.");
        assert_eq!(decoded_anthropic_content(vec![json!({"type":"toolCall","name":"_read"})], true)[0]["name"], "read");
    }

    #[test]
    fn modern_official_chat_models_use_completion_token_limit() {
        let mut config = default_providers().remove(0);
        for model in ["gpt-5.5", "gpt-5.4-mini", "o3", "o4-mini"] {
            assert_eq!(openai_token_field(&config, model), "max_completion_tokens");
        }
        assert_eq!(openai_token_field(&config, "gpt-4.1"), "max_tokens");
        config.base_url = "http://127.0.0.1:1234/v1".into();
        assert_eq!(openai_token_field(&config, "gpt-5-custom"), "max_tokens");
    }

    #[test]
    fn provider_futures_are_send_without_polling() {
        fn assert_send<T: Send>(_: T) {}
        let config = default_providers().remove(0);
        let (updates, _receiver) = tokio::sync::watch::channel(Vec::new());
        assert_send(generate(&config, "model", "", &[], &[], updates));
        assert_send(list_models(&config));
    }
}
