//! Desktop-owned bot metadata and authoritative live state. Engines own transcripts.
pub mod control_cli;
pub mod local_control;
pub mod remote;
mod sessions;
pub mod sms;

use anyhow::{anyhow, bail, Context, Result};
use native_agent::{NativeClient, NativeOptions, ProviderConfig};
use omp_control::{Installation, LaunchOptions, RpcClient};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelChoice {
    pub provider: String,
    pub id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BotRecord {
    id: String,
    name: String,
    cwd: String,
    instructions: String,
    engine: String,
    native_provider: Option<String>,
    profile: Option<String>,
    model: Option<ModelChoice>,
    thinking: Option<String>,
    session_file: Option<String>,
    #[serde(default)]
    sessions: Vec<sessions::SavedSession>,
    #[serde(default)]
    session_directory: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotView {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub instructions: String,
    pub engine: String,
    pub native_provider: Option<String>,
    pub profile: Option<String>,
    pub model: Option<ModelChoice>,
    pub thinking: Option<String>,
    pub session_file: Option<String>,
    pub status: String,
    pub is_running: bool,
    pub error: Option<String>,
    pub engine_version: Option<String>,
    pub pid: Option<u32>,
    pub messages: Vec<Value>,
    pub models: Vec<Value>,
    pub thinking_levels: Vec<String>,
    pub dialogs: Vec<Value>,
    pub state: Option<Value>,
    pub revision: u64,
    pub capabilities: Vec<String>,
}
impl BotView {
    fn from_record(record: &BotRecord) -> Self {
        Self {
            id: record.id.clone(),
            name: record.name.clone(),
            cwd: record.cwd.clone(),
            instructions: record.instructions.clone(),
            engine: record.engine.clone(),
            native_provider: record.native_provider.clone(),
            profile: record.profile.clone(),
            model: record.model.clone(),
            thinking: record.thinking.clone(),
            session_file: record.session_file.clone(),
            status: "stopped".into(),
            is_running: false,
            error: None,
            engine_version: None,
            pid: None,
            messages: vec![],
            models: vec![],
            thinking_levels: vec![],
            dialogs: vec![],
            state: None,
            revision: 0,
            capabilities: if record.engine == "omp" {
                vec![
                    "OMP tools",
                    "Extensions",
                    "Subagents",
                    "LSP",
                    "Profiles",
                    "Approvals",
                    "Images",
                ]
                .into_iter()
                .map(str::to_string)
                .collect()
            } else {
                vec![
                    "Read",
                    "Glob",
                    "Grep",
                    "Write",
                    "Edit",
                    "Bash",
                    "Approvals",
                    "Images",
                    "Persistent history",
                ]
                .into_iter()
                .map(str::to_string)
                .collect()
            },
        }
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineView {
    pub status: String,
    pub path: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
    pub configured_path: Option<String>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    #[serde(flatten)]
    pub config: ProviderConfig,
    pub has_api_key: bool,
    pub auth_method: String,
    pub oauth_available: bool,
}
impl ProviderView {
    fn new(config: ProviderConfig) -> Self {
        let has_api_key = native_agent::has_api_key(&config);
        let oauth_available = native_agent::oauth::oauth_available(&config);
        let auth_method = native_agent::oauth::auth_method(&config)
            .unwrap_or(if has_api_key { "apiKey" } else { "none" })
            .to_owned();
        Self {
            config,
            has_api_key,
            auth_method,
            oauth_available,
        }
    }
}
#[derive(Serialize)]
pub struct NativeView {
    pub providers: Vec<ProviderView>,
    pub error: Option<String>,
}
#[derive(Serialize)]
pub struct Snapshot {
    pub engine: EngineView,
    pub native: NativeView,
    pub bots: Vec<BotView>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedState {
    version: u32,
    configured_path: Option<String>,
    providers: Vec<ProviderConfig>,
    bots: Vec<BotRecord>,
}

#[derive(Clone)]
enum Runtime {
    Omp(RpcClient),
    Native(NativeClient),
}
impl Runtime {
    async fn request(&self, command: Value) -> Result<Value> {
        match self {
            Self::Omp(c) => c.request(command).await,
            Self::Native(c) => c.request(command).await,
        }
    }
    async fn respond(&self, response: Value) -> Result<()> {
        match self {
            Self::Omp(c) => c.respond(response).await,
            Self::Native(c) => c.respond(response).await,
        }
    }
    async fn shutdown(&self) -> Result<()> {
        match self {
            Self::Omp(c) => c.shutdown().await,
            Self::Native(c) => c.shutdown().await,
        }
    }
    fn subscribe(&self) -> broadcast::Receiver<Value> {
        match self {
            Self::Omp(c) => c.subscribe(),
            Self::Native(c) => c.subscribe(),
        }
    }
    fn pid(&self) -> Option<u32> {
        match self {
            Self::Omp(c) => c.pid(),
            Self::Native(_) => None,
        }
    }
    async fn history(&self) -> Result<Vec<Value>> {
        if let Self::Native(client) = self {
            return Ok(
                client.request(json!({"type":"get_messages"})).await?["messages"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        let mut cursors = std::collections::HashSet::new();
        let mut total = None;
        loop {
            let mut command = json!({"type":"get_messages_page","limit":256});
            if let Some(cursor) = &cursor {
                command["cursor"] = json!(cursor);
            }
            let page = self.request(command).await?;
            let count = page["totalMessages"]
                .as_u64()
                .ok_or_else(|| anyhow!("Invalid paginated transcript count"))?;
            if total.is_some_and(|old| old != count) {
                bail!("Transcript changed during loading; restart the bot to reload it");
            }
            total = Some(count);
            let batch = page["messages"]
                .as_array()
                .ok_or_else(|| anyhow!("Invalid paginated transcript"))?;
            messages.extend(batch.iter().cloned());
            match page["nextCursor"].as_str() {
                Some(next) => {
                    if batch.is_empty() || !cursors.insert(next.to_string()) {
                        bail!("Engine returned a non-advancing transcript cursor");
                    }
                    cursor = Some(next.to_string());
                }
                None => break,
            }
        }
        if Some(messages.len() as u64) != total {
            bail!("Engine returned an incomplete transcript");
        }
        Ok(messages)
    }
}
struct BotEntry {
    record: BotRecord,
    view: BotView,
    client: Option<Runtime>,
    generation: u64,
    operation: Arc<AsyncMutex<()>>,
    listener_cancel: Option<tokio::sync::oneshot::Sender<()>>,
}
struct State {
    engine: EngineView,
    providers: Vec<ProviderView>,
    model_discovery_errors: BTreeMap<String, String>,
    bots: BTreeMap<String, BotEntry>,
}
impl State {
    fn native_error(&self) -> Option<String> {
        (!self.model_discovery_errors.is_empty()).then(|| {
            self.model_discovery_errors.values().cloned().collect::<Vec<_>>().join("\n")
        })
    }
}

fn account_discovered_models(config: &ProviderConfig, discovered: Vec<String>) -> Vec<String> {
    let is_starter = native_agent::default_providers().iter().any(|default| {
        default.id == config.id && default.kind == config.kind
            && default.base_url == config.base_url && default.models == config.models
    }) || (config.id == "openai" && config.models == native_agent::default_openai_models(true));
    if is_starter { discovered } else { config.models.clone() }
}
pub struct Desktop {
    directory: PathBuf,
    state: Mutex<State>,
    persistence: AsyncMutex<()>,
    settings: AsyncMutex<()>,
    native_auth: native_agent::oauth::OAuthManager,
    provider_view: fn(ProviderConfig) -> ProviderView,
}
impl Desktop {
    pub async fn open(directory: PathBuf) -> Result<Arc<Self>> {
        Self::open_with_provider_view(directory, ProviderView::new).await
    }
    async fn open_with_provider_view(
        directory: PathBuf,
        provider_view: fn(ProviderConfig) -> ProviderView,
    ) -> Result<Arc<Self>> {
        tokio::fs::create_dir_all(&directory).await?;
        let path = directory.join("desktop.json");
        let saved = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let saved: SavedState = serde_json::from_slice(&bytes).with_context(|| {
                    format!(
                        "Cannot read {}. Metadata was not overwritten.",
                        path.display()
                    )
                })?;
                if saved.version != 1 {
                    bail!("Unsupported desktop metadata version {}", saved.version);
                }
                saved
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SavedState {
                version: 1,
                configured_path: None,
                providers: native_agent::default_providers(),
                bots: vec![],
            },
            Err(e) => return Err(e.into()),
        };
        let providers = saved
            .providers
            .into_iter()
            .map(|mut config| {
                native_agent::migrate_default_catalog(&mut config);
                provider_view(config)
            })
            .collect();
        let bots = saved
            .bots
            .into_iter()
            .map(|mut record| {
                sessions::adopt_legacy(&directory, &mut record);
                (
                    record.id.clone(),
                    BotEntry {
                        view: BotView::from_record(&record),
                        record,
                        client: None,
                        generation: 0,
                        operation: Arc::new(AsyncMutex::new(())),
                        listener_cancel: None,
                    },
                )
            })
            .collect();
        let app = Arc::new(Self {
            directory,
            state: Mutex::new(State {
                engine: EngineView {
                    status: "missing".into(),
                    path: None,
                    version: None,
                    error: None,
                    configured_path: saved.configured_path,
                },
                providers,
                model_discovery_errors: BTreeMap::new(),
                bots,
            }),
            persistence: AsyncMutex::new(()),
            settings: AsyncMutex::new(()),
            native_auth: native_agent::oauth::OAuthManager::default(),
            provider_view,
        });
        app.rescan().await;
        Ok(app)
    }
    pub fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap();
        Snapshot {
            engine: state.engine.clone(),
            native: NativeView {
                providers: state.providers.clone(),
                error: state.native_error(),
            },
            bots: state.bots.values().map(|b| b.view.clone()).collect(),
        }
    }
    pub fn has_running_bots(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .bots
            .values()
            .any(|b| b.client.is_some() || b.view.status == "starting")
    }
    pub fn has_active_work(&self) -> bool {
        self.state.lock().unwrap().bots.values().any(|b| {
            matches!(
                b.view.status.as_str(),
                "working" | "needs_input" | "starting"
            )
        })
    }
    async fn save(&self) -> Result<()> {
        let _guard = self.persistence.lock().await;
        let bytes = {
            let state = self.state.lock().unwrap();
            serde_json::to_vec_pretty(&SavedState {
                version: 1,
                configured_path: state.engine.configured_path.clone(),
                providers: state.providers.iter().map(|p| p.config.clone()).collect(),
                bots: state.bots.values().map(|b| b.record.clone()).collect(),
            })?
        };
        let temporary = self.directory.join("desktop.json.tmp");
        tokio::fs::write(&temporary, bytes).await?;
        tokio::fs::rename(temporary, self.directory.join("desktop.json")).await?;
        Ok(())
    }
    async fn rescan(&self) -> Option<Installation> {
        let configured = self.state.lock().unwrap().engine.configured_path.clone();
        match omp_control::discover(configured.as_deref()).await {
            Ok(installation) => {
                self.state.lock().unwrap().engine = EngineView {
                    status: "ready".into(),
                    path: Some(installation.path.to_string_lossy().into()),
                    version: Some(installation.version.clone()),
                    error: None,
                    configured_path: configured,
                };
                Some(installation)
            }
            Err(error) => {
                self.state.lock().unwrap().engine = EngineView {
                    status: if configured.is_some() {
                        "error"
                    } else {
                        "missing"
                    }
                    .into(),
                    path: None,
                    version: None,
                    error: Some(error.to_string()),
                    configured_path: configured,
                };
                None
            }
        }
    }
    fn gate(&self, id: &str) -> Result<Arc<AsyncMutex<()>>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .bots
            .get(id)
            .ok_or_else(|| anyhow!("Bot not found"))?
            .operation
            .clone())
    }
    fn runtime(&self, id: &str) -> Result<(Runtime, u64)> {
        let state = self.state.lock().unwrap();
        let bot = state.bots.get(id).ok_or_else(|| anyhow!("Bot not found"))?;
        Ok((
            bot.client
                .clone()
                .ok_or_else(|| anyhow!("Start this bot first"))?,
            bot.generation,
        ))
    }
    fn require_idle(&self, id: &str) -> Result<()> {
        let state = self.state.lock().unwrap();
        let bot = state.bots.get(id).ok_or_else(|| anyhow!("Bot not found"))?;
        if matches!(
            bot.view.status.as_str(),
            "working" | "needs_input" | "starting"
        ) || !bot.view.dialogs.is_empty() {
            bail!("Wait until this bot is settled before changing its model, thinking level, or conversation");
        }
        Ok(())
    }
    fn require_provider_stopped(&self, id: &str) -> Result<()> {
        let state = self.state.lock().unwrap();
        if state.bots.values().any(|b| {
            b.record.native_provider.as_deref() == Some(id)
                && (b.view.is_running || b.client.is_some() || b.view.status == "starting")
        }) {
            bail!("Stop bots using this provider before changing its settings, credentials, or model catalog");
        }
        Ok(())
    }
    fn provider_config(&self, id: &str) -> Result<ProviderConfig> {
        self.state
            .lock()
            .unwrap()
            .providers
            .iter()
            .find(|p| p.config.id == id)
            .map(|p| p.config.clone())
            .ok_or_else(|| anyhow!("Provider not found"))
    }
    fn refresh_provider_auth(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(provider) = state.providers.iter_mut().find(|p| p.config.id == id) {
            *provider = (self.provider_view)(provider.config.clone());
        }
    }
    fn account_catalog(&self, id: &str, connected: bool) {
        if id != "openai" {
            return;
        }
        let mut state = self.state.lock().unwrap();
        let Some(provider) = state.providers.iter_mut().find(|p| p.config.id == id) else {
            return;
        };
        let api = native_agent::default_openai_models(false);
        let codex = native_agent::default_openai_models(true);
        if connected && provider.config.models == api {
            provider.config.models = codex;
        } else if !connected && provider.config.models == codex {
            provider.config.models = api;
        }
    }
    async fn discover_account_models(&self, id: &str) -> Result<()> {
        let config = self.provider_config(id)?;
        let discovered = native_agent::list_models(&config).await;
        self.apply_account_discovery(&config, discovered);
        Ok(())
    }
    fn apply_account_discovery(&self, config: &ProviderConfig, discovered: Result<Vec<String>>) {
        let id = config.id.as_str();
        let mut state = self.state.lock().unwrap();
        match discovered {
            Ok(models) => {
                if let Some(provider) = state.providers.iter_mut().find(|p| p.config.id == id) {
                    provider.config.models = account_discovered_models(config, models);
                }
                state.model_discovery_errors.remove(id);
            }
            Err(error) => {
                state.model_discovery_errors.insert(id.to_owned(), format!(
                    "{} account connected, but live model discovery failed: {error}. The current model list was retained. Retry Refresh model list in provider settings.",
                    config.name,
                ));
            }
        }
    }
    fn apply_state(bot: &mut BotEntry, state: Value) {
        if let Some(model) = state
            .get("model")
            .and_then(|m| serde_json::from_value::<ModelChoice>(m.clone()).ok())
        {
            bot.view.model = Some(model.clone());
            bot.record.model = Some(model);
        }
        if let Some(thinking) = state.get("thinkingLevel").and_then(Value::as_str) {
            bot.view.thinking = Some(thinking.into());
            bot.record.thinking = Some(thinking.into());
        }
        if let Some(session) = state.get("sessionFile").and_then(Value::as_str) {
            bot.view.session_file = Some(session.into());
            // OMP can reserve a filename before its first durable message.
            if Path::new(session).is_file() {
                bot.record.session_file = Some(session.into());
                let title = sessions::conversation_title(&bot.view.messages);
                sessions::remember_current(&mut bot.record, title);
            }
        }
        bot.view.status = if !bot.view.dialogs.is_empty() {
            "needs_input"
        } else if state["isStreaming"].as_bool().unwrap_or(false)
            || !state["isSettled"].as_bool().unwrap_or(true)
        {
            "working"
        } else {
            "idle"
        }
        .into();
        bot.view.state = Some(state);
        bot.view.revision += 1;
    }
    async fn refresh_state(&self, id: &str, generation: u64, client: &Runtime) -> Result<()> {
        let data = client.request(json!({"type":"get_state"})).await?;
        let mut state = self.state.lock().unwrap();
        if let Some(bot) = state
            .bots
            .get_mut(id)
            .filter(|b| b.generation == generation && b.client.is_some())
        {
            Self::apply_state(bot, data);
        }
        Ok(())
    }
    async fn start_locked(self: &Arc<Self>, id: &str) -> Result<()> {
        let (record, generation, provider) = {
            let _settings = self.settings.lock().await;
            let mut state = self.state.lock().unwrap();
            let provider = state
                .bots
                .get(id)
                .and_then(|b| b.record.native_provider.as_ref())
                .and_then(|p| state.providers.iter().find(|c| &c.config.id == p))
                .map(|p| p.config.clone());
            let bot = state
                .bots
                .get_mut(id)
                .ok_or_else(|| anyhow!("Bot not found"))?;
            if bot.client.is_some() {
                if bot.view.status == "error" {
                    bail!("Stop this bot before restarting its engine");
                }
                return Ok(());
            }
            bot.generation += 1;
            bot.view.status = "starting".into();
            bot.view.error = None;
            bot.view.revision += 1;
            (bot.record.clone(), bot.generation, provider)
        };
        let result: Result<(Runtime,String)> = async {
            let cwd = PathBuf::from(&record.cwd);
            if !cwd.is_dir() { bail!("Workspace directory does not exist: {}", cwd.display()); }
            if let Some(session) = &record.session_file { if !Path::new(session).is_file() { bail!("Saved session is missing: {session}. No replacement session was created."); } }
            if record.engine == "omp" {
                let installation = self.rescan().await.ok_or_else(|| anyhow!("OMP is required for this bot. Open Engine settings to install it or select its executable."))?;
                let extra_args = record.session_directory.as_ref().filter(|_| record.session_file.is_none()).map(|directory| vec!["--session-dir".into(), directory.clone()]).unwrap_or_default();
                let options = LaunchOptions { cwd, session: record.session_file.clone(), model: record.model.as_ref().map(|m| format!("{}/{}",m.provider,m.id)), thinking: record.thinking.clone(), instructions: (!record.instructions.is_empty()).then_some(record.instructions.clone()), profile: record.profile.clone(), extra_args };
                let client = RpcClient::spawn(&installation, options).await?;
                Ok((Runtime::Omp(client), installation.version))
            } else {
                let config = provider.ok_or_else(|| anyhow!("Select a configured native provider"))?;
                let model = record.model.as_ref().map(|m| m.id.clone()).or_else(|| config.models.first().cloned()).ok_or_else(|| anyhow!("Add a model in Native provider settings before starting this bot"))?;
                let session_path = record.session_file.as_ref().map(PathBuf::from)
                    .or_else(|| record.session_directory.as_ref().map(|directory| PathBuf::from(directory).join("session.json")))
                    .unwrap_or_else(|| self.directory.join("native-sessions").join(format!("{}.json",record.id)));
                let client = NativeClient::spawn(config, NativeOptions { cwd, session_path, model, instructions: record.instructions.clone(), thinking: record.thinking.clone() }).await?;
                Ok((Runtime::Native(client), format!("native-agent {}", env!("CARGO_PKG_VERSION"))))
            }
        }.await;
        let (client, version) = match result {
            Ok(result) => result,
            Err(error) => {
                self.bot_error(id, generation, &error);
                return Err(error);
            }
        };
        let receiver = client.subscribe();
        {
            let mut state = self.state.lock().unwrap();
            let bot = state
                .bots
                .get_mut(id)
                .ok_or_else(|| anyhow!("Bot removed while starting"))?;
            bot.client = Some(client.clone());
            bot.view.is_running = true;
            bot.view.engine_version = Some(version);
            bot.view.pid = client.pid();
            bot.view.dialogs.clear();
        }
        let init: Result<()> = async {
            client
                .request(json!({"type":"set_ask_dialog","enabled":true}))
                .await?;
            let models = client
                .request(json!({"type":"get_available_models"}))
                .await?;
            let levels = client
                .request(json!({"type":"get_available_thinking_levels"}))
                .await?;
            let history = client.history().await?;
            {
                let mut state = self.state.lock().unwrap();
                let bot = state.bots.get_mut(id).unwrap();
                bot.view.models = models["models"].as_array().cloned().unwrap_or_default();
                bot.view.thinking_levels = levels["levels"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                bot.view.messages = history;
                for (index, message) in bot.view.messages.iter_mut().enumerate() {
                    if let Some(obj) = message.as_object_mut() {
                        obj.insert(
                            "_key".into(),
                            json!(format!("history-{generation}-{index}")),
                        );
                    }
                }
            }
            self.refresh_state(id, generation, &client).await?;
            self.save().await?;
            Ok(())
        }
        .await;
        if let Err(error) = init {
            let _ = client.shutdown().await;
            {
                let mut state = self.state.lock().unwrap();
                if let Some(bot) = state.bots.get_mut(id) {
                    bot.client = None;
                    bot.view.is_running = false;
                    bot.view.pid = None;
                }
            }
            self.bot_error(id, generation, &error);
            return Err(error);
        }
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        self.state
            .lock()
            .unwrap()
            .bots
            .get_mut(id)
            .unwrap()
            .listener_cancel = Some(cancel);
        let app = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            app.consume_events(id, generation, client, receiver, cancelled)
                .await;
        });
        Ok(())
    }
    fn bot_error(&self, id: &str, generation: u64, error: &anyhow::Error) {
        if let Some(bot) = self
            .state
            .lock()
            .unwrap()
            .bots
            .get_mut(id)
            .filter(|b| b.generation == generation)
        {
            bot.view.error = Some(error.to_string());
            bot.view.status = "error".into();
            bot.view.revision += 1;
        }
    }
    async fn consume_events(
        self: Arc<Self>,
        id: String,
        generation: u64,
        client: Runtime,
        mut receiver: broadcast::Receiver<Value>,
        mut cancelled: tokio::sync::oneshot::Receiver<()>,
    ) {
        loop {
            let received = tokio::select! { _=&mut cancelled=>break, event=receiver.recv()=>event };
            let event = match received {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Final engine history is authoritative after a slow consumer; never silently drop text.
                    match client.request(json!({"type":"get_messages"})).await {
                        Ok(history) => {
                            let mut state = self.state.lock().unwrap();
                            if let Some(bot) = state
                                .bots
                                .get_mut(&id)
                                .filter(|b| b.generation == generation)
                            {
                                bot.view.messages =
                                    history["messages"].as_array().cloned().unwrap_or_default();
                                for (i, m) in bot.view.messages.iter_mut().enumerate() {
                                    if let Some(o) = m.as_object_mut() {
                                        o.insert(
                                            "_key".into(),
                                            json!(format!("recovered-{generation}-{i}")),
                                        );
                                    }
                                }
                                bot.view.revision += 1;
                            }
                        }
                        Err(error) => {
                            self.bot_error(&id, generation, &error);
                            break;
                        }
                    }
                    continue;
                }
            };
            let kind = event["type"].as_str().unwrap_or("");
            {
                let mut state = self.state.lock().unwrap();
                let Some(bot) = state
                    .bots
                    .get_mut(&id)
                    .filter(|b| b.generation == generation && b.client.is_some())
                else {
                    break;
                };
                match kind {
                    "message_start" | "message_update" | "message_end" => {
                        if let Some(mut message) =
                            event.get("message").filter(|v| v.is_object()).cloned()
                        {
                            let key = format!(
                                "{generation}-{}",
                                event["messageId"].as_str().unwrap_or("message")
                            );
                            message
                                .as_object_mut()
                                .unwrap()
                                .insert("_key".into(), json!(key));
                            if let Some(existing) =
                                bot.view.messages.iter_mut().find(|m| m["_key"] == key)
                            {
                                *existing = message;
                            } else {
                                bot.view.messages.push(message);
                            }
                        }
                        if kind != "message_end" && bot.view.dialogs.is_empty() {
                            bot.view.status = "working".into();
                        }
                    }
                    "agent_start" => {
                        bot.view.status = "working".into();
                        bot.view.error = None;
                    }
                    "extension_ui_request" => match event["method"].as_str().unwrap_or("") {
                        "cancel" => {
                            let target = &event["targetId"];
                            bot.view.dialogs.retain(|d| &d["id"] != target);
                        }
                        "select" | "confirm" | "input" | "editor" | "ask" | "open_url" => {
                            if !bot.view.dialogs.iter().any(|d| d["id"] == event["id"]) {
                                bot.view.dialogs.push(event.clone());
                            }
                            bot.view.status = "needs_input".into();
                        }
                        "notify" => {
                            if event["notifyType"] == "error" {
                                bot.view.error = event["message"].as_str().map(str::to_string);
                            }
                        }
                        _ => {}
                    },
                    "prompt_result" => {
                        if event["status"] == "error" {
                            bot.view.error = event["error"]["message"]
                                .as_str()
                                .map(str::to_string)
                                .or_else(|| Some("Agent turn failed".into()));
                        }
                    }
                    "process_exit" => {
                        bot.client = None;
                        bot.view.is_running = false;
                        bot.view.pid = None;
                        bot.view.status = "error".into();
                        bot.view.error=Some(format!("Engine process exited ({}). Start to resume; the previous prompt will not be replayed.",event["code"]));
                    }
                    "transport_error" => {
                        bot.client = None;
                        bot.view.is_running = false;
                        bot.view.pid = None;
                        bot.view.status = "error".into();
                        bot.view.error = Some(
                            event["message"]
                                .as_str()
                                .unwrap_or("Engine transport failed")
                                .into(),
                        );
                    }
                    _ => {}
                }
                bot.view.revision += 1;
            }
            if matches!(
                kind,
                "session_settled"
                    | "agent_end"
                    | "prompt_result"
                    | "model_changed"
                    | "thinking_level_changed"
                    | "queue_update"
            ) {
                if let Err(error) = self.refresh_state(&id, generation, &client).await {
                    self.bot_error(&id, generation, &error);
                }
                if let Err(error) = self.save().await {
                    self.bot_error(&id, generation, &error);
                }
            }
            if kind == "transport_error" {
                let _ = client.shutdown().await;
                break;
            }
            if kind == "process_exit" {
                break;
            }
        }
    }
    async fn stop_locked(&self, id: &str) -> Result<()> {
        self.stop_runtime_locked(id, true).await
    }
    async fn stop_runtime_locked(&self, id: &str, abort: bool) -> Result<()> {
        let client = {
            let mut state = self.state.lock().unwrap();
            let bot = state
                .bots
                .get_mut(id)
                .ok_or_else(|| anyhow!("Bot not found"))?;
            bot.generation += 1;
            if let Some(cancel) = bot.listener_cancel.take() {
                let _ = cancel.send(());
            }
            bot.client.take()
        };
        let (result, history, retry) = if let Some(client) = client {
            let before = if matches!(&client, Runtime::Omp(_)) {
                Some(
                    tokio::time::timeout(std::time::Duration::from_secs(12), async {
                        if abort {
                            client.request(json!({"type":"abort_and_restore_queue"})).await?;
                        }
                        loop {
                            let state = client.request(json!({"type":"get_state"})).await?;
                            if state["isStreaming"] != true && state["isCompacting"] != true {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                        }
                        client.history().await
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r),
                )
            } else {
                None
            };
            let result = client.shutdown().await;
            let history = match before {
                Some(history) => Some(history),
                None if result.is_ok() => Some(client.history().await),
                None => None,
            };
            let retry = if result.is_err() { Some(client) } else { None };
            (result, history, retry)
        } else {
            (Ok(()), None, None)
        };
        {
            let mut state = self.state.lock().unwrap();
            let bot = state.bots.get_mut(id).unwrap();
            bot.client = retry;
            bot.view.is_running = bot.client.is_some();
            bot.view.status = if result.is_ok() { "stopped" } else { "error" }.into();
            bot.view.error = result.as_ref().err().map(ToString::to_string);
            if let Some(history) = history {
                match history {
                    Ok(mut messages) => {
                        for (i, m) in messages.iter_mut().enumerate() {
                            if let Some(o) = m.as_object_mut() {
                                o.insert(
                                    "_key".into(),
                                    json!(format!("stopped-{}-{i}", bot.generation)),
                                );
                            }
                        }
                        bot.view.messages = messages;
                    }
                    Err(error) => {
                        bot.view.error = Some(format!(
                            "Engine stopped; final transcript could not be reloaded: {error}"
                        ))
                    }
                }
            }
            bot.view.pid = bot.client.as_ref().and_then(Runtime::pid);
            bot.view.dialogs.clear();
            bot.view.revision += 1;
        }
        self.save().await?;
        result
    }
    pub async fn shutdown_all(self: &Arc<Self>) -> Result<()> {
        let ids: Vec<String> = self.state.lock().unwrap().bots.keys().cloned().collect();
        let mut jobs = tokio::task::JoinSet::new();
        for id in ids {
            let app = self.clone();
            jobs.spawn(async move {
                let gate = app.gate(&id)?;
                let _guard = gate.lock().await;
                app.stop_locked(&id).await
            });
        }
        let mut errors = vec![];
        while let Some(result) = jobs.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => errors.push(e.to_string()),
                Err(e) => errors.push(e.to_string()),
            }
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        Ok(())
    }
    pub async fn request(self: &Arc<Self>, method: &str, args: Value) -> Result<Value> {
        match method {
            // Provider connection state changes through explicit account/key
            // commands. Snapshot polling must never perform credential I/O.
            "bootstrap" => {}
            "configure_engine" => {
                let _guard = self.settings.lock().await;
                self.state.lock().unwrap().engine.configured_path = args["path"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                self.rescan().await;
                self.save().await?;
            }
            "create_bot" => {
                let _guard = self.settings.lock().await;
                let name = required(&args, "name")?.trim();
                if name.is_empty() || name.len() > 120 {
                    bail!("Bot name must be 1–120 characters");
                }
                let cwd = workspace(required(&args, "cwd")?)?;
                let engine = required(&args, "engine")?;
                if engine != "omp" && engine != "native" {
                    bail!("Select OMP or native engine");
                }
                let native_provider = if engine == "native" {
                    Some(required(&args, "nativeProvider")?.to_string())
                } else {
                    None
                };
                if let Some(provider) = &native_provider {
                    if !self
                        .state
                        .lock()
                        .unwrap()
                        .providers
                        .iter()
                        .any(|p| &p.config.id == provider)
                    {
                        bail!("Native provider not found");
                    }
                }
                let record = BotRecord {
                    id: Uuid::new_v4().to_string(),
                    name: name.into(),
                    cwd,
                    instructions: args["instructions"].as_str().unwrap_or("").into(),
                    engine: engine.into(),
                    native_provider,
                    profile: args["profile"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    model: None,
                    thinking: None,
                    session_file: None,
                    sessions: vec![],
                    session_directory: None,
                };
                {
                    let mut state = self.state.lock().unwrap();
                    state.bots.insert(
                        record.id.clone(),
                        BotEntry {
                            view: BotView::from_record(&record),
                            record,
                            client: None,
                            generation: 0,
                            operation: Arc::new(AsyncMutex::new(())),
                            listener_cancel: None,
                        },
                    );
                }
                self.save().await?;
            }
            "update_bot" => {
                let id = required(&args, "id")?;
                let gate = self.gate(id)?;
                let _guard = gate.lock().await;
                let name = required(&args, "name")?.trim();
                if name.is_empty() || name.len() > 120 {
                    bail!("Bot name must be 1–120 characters");
                }
                let cwd = workspace(required(&args, "cwd")?)?;
                {
                    let mut state = self.state.lock().unwrap();
                    let bot = state.bots.get_mut(id).unwrap();
                    if bot.client.is_some() {
                        bail!("Stop this bot before editing its configuration");
                    }
                    if bot.record.session_file.is_some() && cwd != bot.record.cwd {
                        bail!("Create a new bot to use another workspace with a different conversation");
                    }
                    let profile = args["profile"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    if bot.record.session_file.is_some() && profile != bot.record.profile {
                        bail!("Create a new bot to change the profile of an existing conversation");
                    }
                    bot.record.name = name.into();
                    bot.record.cwd = cwd;
                    bot.record.instructions = args["instructions"].as_str().unwrap_or("").into();
                    bot.record.profile = profile;
                    bot.view.name = bot.record.name.clone();
                    bot.view.cwd = bot.record.cwd.clone();
                    bot.view.instructions = bot.record.instructions.clone();
                    bot.view.profile = bot.record.profile.clone();
                    bot.view.revision += 1;
                }
                self.save().await?;
            }
            "delete_bot" => {
                let id = required(&args, "id")?;
                let gate = self.gate(id)?;
                let _guard = gate.lock().await;
                self.stop_locked(id).await?;
                self.state.lock().unwrap().bots.remove(id);
                self.save().await?;
            }
            "start_bot" => {
                let id = required(&args, "id")?;
                let gate = self.gate(id)?;
                let _guard = gate.lock().await;
                self.start_locked(id).await?;
            }
            "stop_bot" => {
                let id = required(&args, "id")?;
                let gate = self.gate(id)?;
                let _guard = gate.lock().await;
                self.stop_locked(id).await?;
            }
            "list_sessions" => {
                return self.list_sessions(required(&args, "id")?).await;
            }
            "new_session" | "resume_session" => {
                let id = required(&args, "id")?;
                let selected = if method == "resume_session" { Some(required(&args, "sessionId")?) } else { None };
                let gate = self.gate(id)?;
                let _guard = gate.try_lock().map_err(|_| anyhow!("This bot is busy; wait before changing conversations"))?;
                self.switch_session_locked(id, selected).await?;
            }
            "send_message" => {
                let id = required(&args, "id")?;
                let text = args["message"].as_str().unwrap_or("");
                if text.trim().is_empty() && args["images"].as_array().is_none_or(Vec::is_empty) {
                    bail!("Enter a message or attach an image");
                }
                let behavior = args["behavior"].as_str().unwrap_or("followUp");
                if !matches!(behavior, "steer" | "followUp") {
                    bail!("Invalid message queue behavior");
                }
                let gate = self.gate(id)?;
                let _guard = gate.lock().await;
                self.start_locked(id).await?;
                let (client, generation) = self.runtime(id)?;
                let mut command =
                    json!({"type":"prompt","message":text,"streamingBehavior":behavior});
                if let Some(images) = args.get("images") {
                    command["images"] = images.clone();
                }
                if let Err(error) = client.request(command).await {
                    self.bot_error(id, generation, &error);
                    return Err(error);
                }
                self.refresh_state(id, generation, &client).await?;
                self.save().await?;
            }
            "abort_bot" => {
                let id = required(&args, "id")?;
                let (client, generation) = self.runtime(id)?;
                client.request(json!({"type":"abort"})).await?;
                self.refresh_state(id, generation, &client).await?;
            }
            "set_model" | "set_thinking" => {
                let id = required(&args, "id")?;
                let gate = self.gate(id)?;
                let _guard = gate.lock().await;
                self.start_locked(id).await?;
                self.require_idle(id)?;
                let (client, generation) = self.runtime(id)?;
                let command = if method == "set_model" {
                    json!({"type":"set_model","provider":required(&args,"provider")?,"modelId":required(&args,"modelId")?})
                } else {
                    json!({"type":"set_thinking_level","level":required(&args,"level")?})
                };
                client.request(command).await?;
                self.refresh_state(id, generation, &client).await?;
                let levels = client
                    .request(json!({"type":"get_available_thinking_levels"}))
                    .await?;
                {
                    let mut state = self.state.lock().unwrap();
                    if let Some(bot) = state.bots.get_mut(id) {
                        bot.view.thinking_levels = levels["levels"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                    }
                }
                self.save().await?;
            }
            "respond_dialog" => {
                let id = required(&args, "id")?;
                let request_id = required(&args, "requestId")?;
                {
                    let state = self.state.lock().unwrap();
                    let bot = state.bots.get(id).ok_or_else(|| anyhow!("Bot not found"))?;
                    if !bot.view.dialogs.iter().any(|d| d["id"] == request_id) {
                        bail!("This request is no longer pending");
                    }
                }
                let (client, generation) = self.runtime(id)?;
                let response = args["response"]
                    .as_object()
                    .ok_or_else(|| anyhow!("Dialog response must be an object"))?;
                let mut payload = serde_json::Map::new();
                for key in ["value", "confirmed", "cancelled", "answers"] {
                    if let Some(value) = response.get(key) {
                        payload.insert(key.into(), value.clone());
                    }
                }
                payload.insert("type".into(), json!("extension_ui_response"));
                payload.insert("id".into(), json!(request_id));
                client.respond(Value::Object(payload)).await?;
                {
                    let mut state = self.state.lock().unwrap();
                    if let Some(bot) = state
                        .bots
                        .get_mut(id)
                        .filter(|b| b.generation == generation)
                    {
                        bot.view.dialogs.retain(|d| d["id"] != request_id);
                        bot.view.revision += 1;
                    }
                }
                self.refresh_state(id, generation, &client).await?;
            }
            "get_bot_info" => {
                let id = required(&args, "id")?;
                if let Ok((client, generation)) = self.runtime(id) {
                    let stats = client.request(json!({"type":"get_session_stats"})).await?;
                    let subagents = client.request(json!({"type":"get_subagents"})).await?;
                    self.refresh_state(id, generation, &client).await?;
                    let state = self
                        .state
                        .lock()
                        .unwrap()
                        .bots
                        .get(id)
                        .and_then(|b| b.view.state.clone());
                    return Ok(
                        json!({"stats":stats,"subagents":subagents["subagents"],"state":state}),
                    );
                }
                let state = self.state.lock().unwrap();
                let bot = state.bots.get(id).ok_or_else(|| anyhow!("Bot not found"))?;
                return Ok(json!({"stats":null,"subagents":[],"state":bot.view.state}));
            }
            "start_native_login" => {
                let _guard = self.settings.lock().await;
                let id = required(&args, "providerId")?;
                self.require_provider_stopped(id)?;
                let config = self.provider_config(id)?;
                return Ok(serde_json::to_value(
                    self.native_auth.begin(&config).await?,
                )?);
            }
            "native_login_status" | "complete_native_login" => {
                let login = required(&args, "loginId")?;
                if method == "complete_native_login" {
                    {
                        let _guard = self.settings.lock().await;
                        let id = self.native_auth.provider_id(login)?;
                        self.require_provider_stopped(&id)?;
                    }
                    // Exchange stages credentials only; cancellation must not wait
                    // behind the settings lock for the OAuth server.
                    self.native_auth
                        .submit_code(login, required(&args, "code")?)
                        .await?;
                }
                let _guard = self.settings.lock().await;
                let id = self.native_auth.provider_id(login)?;
                let config = self.provider_config(&id)?;
                self.require_provider_stopped(&id)?;
                let was_connected = self.native_auth.status(login)?.status == "connected";
                let status = self.native_auth.complete(login, &config).await?;
                if status.status == "connected" && !was_connected {
                    self.account_catalog(&id, true);
                    self.refresh_provider_auth(&id);
                    self.discover_account_models(&id).await?;
                    self.save().await?;
                }
                if method == "native_login_status" {
                    return Ok(serde_json::to_value(status)?);
                }
                if status.status == "error" {
                    bail!(
                        "{}",
                        status
                            .error
                            .unwrap_or_else(|| "Account sign-in failed".into())
                    );
                }
            }
            "cancel_native_login" => {
                let _guard = self.settings.lock().await;
                self.native_auth.cancel(required(&args, "loginId")?)?;
            }
            "logout_native_provider" => {
                let _guard = self.settings.lock().await;
                let id = required(&args, "providerId")?;
                self.require_provider_stopped(id)?;
                let config = self.provider_config(id)?;
                self.native_auth.cancel_provider(id)?;
                native_agent::oauth::logout(&config).await?;
                self.account_catalog(id, false);
                self.refresh_provider_auth(id);
                self.state.lock().unwrap().model_discovery_errors.remove(id);
                self.save().await?;
            }
            "configure_native_provider" => {
                let _guard = self.settings.lock().await;
                let mut config: ProviderConfig = serde_json::from_value(args.clone())?;
                validate_provider(&config)?;
                native_agent::migrate_default_catalog(&mut config);
                self.require_provider_stopped(&config.id)?;
                self.native_auth.cancel_provider(&config.id)?;
                if let Some(key) = args["apiKey"].as_str().filter(|s| !s.is_empty()) {
                    let id = config.id.clone();
                    let key = key.to_string();
                    tokio::task::spawn_blocking(move || native_agent::store_api_key(&id, &key))
                        .await??;
                    // Saving a key is the explicit alternative to account inference.
                    if native_agent::oauth::oauth_available(&config) {
                        native_agent::oauth::logout(&config).await?;
                    }
                    if config.id == "openai"
                        && config.models == native_agent::default_openai_models(true)
                    {
                        config.models = native_agent::default_openai_models(false);
                    }
                }
                let provider_view = (self.provider_view)(config.clone());
                {
                    let mut state = self.state.lock().unwrap();
                    state.model_discovery_errors.remove(&config.id);
                    if let Some(provider) = state
                        .providers
                        .iter_mut()
                        .find(|p| p.config.id == config.id)
                    {
                        *provider = provider_view;
                    } else {
                        state.providers.push(provider_view);
                    }
                }
                self.save().await?;
            }
            "remove_native_key" => {
                let _guard = self.settings.lock().await;
                let id = required(&args, "id")?.to_string();
                self.require_provider_stopped(&id)?;
                self.native_auth.cancel_provider(&id)?;
                let key_id = id.clone();
                tokio::task::spawn_blocking(move || native_agent::remove_api_key(&key_id))
                    .await??;
                self.refresh_provider_auth(&id);
            }
            "refresh_native_models" => {
                let id = required(&args, "providerId")?;
                let _guard = self.settings.lock().await;
                {
                    let state = self.state.lock().unwrap();
                    if state.bots.values().any(|b| {
                        b.record.native_provider.as_deref() == Some(id)
                            && matches!(
                                b.view.status.as_str(),
                                "working" | "needs_input" | "starting"
                            )
                    }) {
                        bail!(
                            "Wait until bots using this provider are idle before refreshing models"
                        );
                    }
                }
                let config = self
                    .state
                    .lock()
                    .unwrap()
                    .providers
                    .iter()
                    .find(|p| p.config.id == id)
                    .map(|p| p.config.clone())
                    .ok_or_else(|| anyhow!("Provider not found"))?;
                let models = native_agent::list_models(&config).await?;
                let clients: Vec<NativeClient> = self
                    .state
                    .lock()
                    .unwrap()
                    .bots
                    .values()
                    .filter(|b| b.record.native_provider.as_deref() == Some(id))
                    .filter_map(|b| match &b.client {
                        Some(Runtime::Native(c)) => Some(c.clone()),
                        _ => None,
                    })
                    .collect();
                for client in clients {
                    client.update_models(models.clone()).await?;
                }
                {
                    let mut state = self.state.lock().unwrap();
                    let provider = state
                        .providers
                        .iter_mut()
                        .find(|p| p.config.id == id)
                        .unwrap();
                    provider.config.models = models.clone();
                    state.model_discovery_errors.remove(id);
                    for bot in state.bots.values_mut().filter(|b| {
                        b.record.engine == "native"
                            && b.record.native_provider.as_deref() == Some(id)
                    }) {
                        bot.view.models = models
                            .iter()
                            .map(|model| json!({"provider":id,"id":model,"name":model}))
                            .collect();
                        bot.view.revision += 1;
                    }
                }
                self.save().await?;
            }
            "shutdown_all" => self.shutdown_all().await?,
            _ => bail!("Unknown desktop command: {method}"),
        }
        let state = self.state.lock().unwrap();
        let mut snapshot = serde_json::Map::new();
        snapshot.insert("engine".into(), serde_json::to_value(&state.engine)?);
        snapshot.insert(
            "native".into(),
            json!({"providers":&state.providers,"error":state.native_error()}),
        );
        snapshot.insert(
            "bots".into(),
            Value::Array(
                state
                    .bots
                    .values()
                    .map(|b| serde_json::to_value(&b.view))
                    .collect::<serde_json::Result<Vec<_>>>()?,
            ),
        );
        Ok(Value::Object(snapshot))
    }
}
fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key].as_str().ok_or_else(|| anyhow!("Missing {key}"))
}
fn workspace(input: &str) -> Result<String> {
    let expanded = if input == "~" || input.starts_with("~/") {
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is unavailable"))?)
            .join(input.strip_prefix("~/").unwrap_or(""))
    } else {
        PathBuf::from(input)
    };
    let path = expanded
        .canonicalize()
        .with_context(|| format!("Workspace not found: {}", expanded.display()))?;
    if !path.is_dir() {
        bail!("Workspace must be a directory");
    }
    Ok(path.to_string_lossy().into())
}
fn validate_provider(config: &ProviderConfig) -> Result<()> {
    native_agent::validate_config(config)?;
    if config.name.trim().is_empty() {
        bail!("Provider name is required");
    }
    if config.models.iter().any(|m| m.trim().is_empty()) {
        bail!("Model IDs cannot be empty");
    }
    Ok(())
}

#[cfg(test)]
fn offline_provider_view(config: ProviderConfig) -> ProviderView {
    ProviderView {
        oauth_available: native_agent::oauth::oauth_available(&config),
        config,
        has_api_key: false,
        auth_method: "none".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_discovery_replaces_starters_but_keeps_custom_catalogs() {
        let mut config = native_agent::default_providers().remove(0);
        config.models = native_agent::default_openai_models(true);
        let live = vec!["gpt-6.1-sol".to_owned(), "future-model-9".to_owned()];
        assert_eq!(account_discovered_models(&config, live.clone()), live);
        config.models = vec!["my-custom-model".into(), "gpt-6.1-sol".into()];
        assert_eq!(account_discovered_models(&config, live), config.models);
    }

    #[test]
    fn failed_account_discovery_keeps_connection_and_catalog_and_can_recover() {
        let dir = tempfile::tempdir().unwrap();
        // Discovery state transitions do not need the production credential store.
        let app = Arc::new(Desktop {
            directory: dir.path().to_path_buf(),
            state: Mutex::new(State {
                engine: EngineView {
                    status: "missing".into(), path: None, version: None,
                    error: None, configured_path: None,
                },
                providers: native_agent::default_providers().into_iter().map(|config| ProviderView {
                    config, has_api_key: false, auth_method: "none".into(), oauth_available: true,
                }).collect(),
                model_discovery_errors: BTreeMap::new(),
                bots: BTreeMap::new(),
            }),
            persistence: AsyncMutex::new(()),
            settings: AsyncMutex::new(()),
            native_auth: native_agent::oauth::OAuthManager::default(),
            provider_view: offline_provider_view,
        });
        let config = {
            let mut state = app.state.lock().unwrap();
            let provider = state.providers.iter_mut().find(|p| p.config.id == "openai").unwrap();
            provider.auth_method = "oauth".into();
            provider.config.models = native_agent::default_openai_models(true);
            provider.config.clone()
        };
        app.apply_account_discovery(&config, Err(anyhow!("catalog unavailable")));
        let snapshot = app.snapshot();
        let provider = snapshot.native.providers.iter().find(|p| p.config.id == "openai").unwrap();
        assert_eq!(provider.auth_method, "oauth");
        assert_eq!(provider.config.models, config.models);
        assert!(snapshot.native.error.is_some());
        let live = vec!["gpt-6.1-sol".into(), "future-model-9".into()];
        app.apply_account_discovery(&config, Ok(live.clone()));
        let snapshot = app.snapshot();
        let provider = snapshot.native.providers.iter().find(|p| p.config.id == "openai").unwrap();
        assert_eq!(provider.auth_method, "oauth");
        assert_eq!(provider.config.models, live);
        assert!(snapshot.native.error.is_none());
    }

    #[test]
    fn provider_urls_reject_cleartext_remote_and_embedded_secrets() {
        let mut p = native_agent::default_providers().remove(0);
        for base in [
            "http://example.com/v1",
            "https://user:secret@example.com/v1",
            "https://example.com/v1?key=secret",
        ] {
            p.base_url = base.into();
            assert!(validate_provider(&p).is_err());
        }
        p.base_url = "http://127.0.0.1:1234/v1".into();
        assert!(validate_provider(&p).is_ok());
    }
    #[tokio::test]
    async fn native_bot_metadata_survives_restart_without_omp() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let app = Desktop::open_with_provider_view(dir.path().to_path_buf(), offline_provider_view).await.unwrap();
        app.request(
            "create_bot",
            json!({"name":"Native","cwd":cwd.path(),"engine":"native","nativeProvider":"openai"}),
        )
        .await
        .unwrap();
        let original = app.snapshot().bots.remove(0);
        drop(app);
        let restored = Desktop::open_with_provider_view(dir.path().to_path_buf(), offline_provider_view).await.unwrap();
        let bot = restored.snapshot().bots.remove(0);
        assert_eq!(bot.id, original.id);
        assert_eq!(bot.engine, "native");
        assert_eq!(bot.status, "stopped");
        assert_eq!(bot.native_provider.as_deref(), Some("openai"));
    }
    #[tokio::test]
    async fn reserved_session_filename_is_not_resumed_until_durable() {
        let dir = tempfile::tempdir().unwrap();
        let app = Desktop::open_with_provider_view(dir.path().to_path_buf(), offline_provider_view).await.unwrap();
        app.request(
            "create_bot",
            json!({"name":"Fresh","cwd":dir.path(),"engine":"native","nativeProvider":"openai"}),
        )
        .await
        .unwrap();
        let id = app.snapshot().bots[0].id.clone();
        let session = dir.path().join("not-created-yet.json");
        {
            let mut state = app.state.lock().unwrap();
            let bot = state.bots.get_mut(&id).unwrap();
            Desktop::apply_state(bot, json!({"sessionFile":session,"isSettled":true}));
            assert!(bot.record.session_file.is_none());
            assert_eq!(bot.view.session_file.as_deref(), session.to_str());
        }
        tokio::fs::write(&session, "{}").await.unwrap();
        let mut state = app.state.lock().unwrap();
        let bot = state.bots.get_mut(&id).unwrap();
        Desktop::apply_state(bot, json!({"sessionFile":session,"isSettled":true}));
        assert_eq!(bot.record.session_file.as_deref(), session.to_str());
    }
    #[tokio::test]
    async fn provider_endpoint_cannot_change_under_a_live_native_session() {
        let dir = tempfile::tempdir().unwrap();
        let app = Desktop::open_with_provider_view(dir.path().to_path_buf(), offline_provider_view).await.unwrap();
        app.request(
            "create_bot",
            json!({"name":"Native","cwd":dir.path(),"engine":"native","nativeProvider":"openai"}),
        )
        .await
        .unwrap();
        let id = app.snapshot().bots[0].id.clone();
        app.request("start_bot", json!({"id":id})).await.unwrap();
        for command in ["start_native_login", "logout_native_provider"] {
            let error = app
                .request(command, json!({"providerId":"openai"}))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("Stop bots"));
        }
        let original = app
            .snapshot()
            .native
            .providers
            .into_iter()
            .find(|p| p.config.id == "openai")
            .unwrap()
            .config;
        let mut changed = original.clone();
        changed.base_url = "https://example.com/v1".into();
        let result = app
            .request(
                "configure_native_provider",
                serde_json::to_value(&changed).unwrap(),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            app.snapshot()
                .native
                .providers
                .into_iter()
                .find(|p| p.config.id == "openai")
                .unwrap()
                .config
                .base_url,
            original.base_url
        );
        app.request("stop_bot", json!({"id":id})).await.unwrap();
        app.request(
            "configure_native_provider",
            serde_json::to_value(&changed).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            app.snapshot()
                .native
                .providers
                .into_iter()
                .find(|p| p.config.id == "openai")
                .unwrap()
                .config
                .base_url,
            changed.base_url
        );
    }
    #[tokio::test]
    async fn invalid_engine_does_not_create_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let app = Desktop::open_with_provider_view(dir.path().to_path_buf(), offline_provider_view).await.unwrap();
        assert!(app
            .request(
                "create_bot",
                json!({"name":"Invalid","cwd":dir.path(),"engine":"unknown"})
            )
            .await
            .is_err());
        assert!(app.snapshot().bots.is_empty());
    }
}
