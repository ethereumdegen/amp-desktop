//! Opt-in, outbound-only SMS transport. The journal precedes every irreversible side effect.
use super::*;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, sync::Notify, task::JoinHandle};
use url::Url;

mod network;
#[cfg(test)]
mod tests;

const KEYCHAIN_SERVICE: &str = "com.omp.desktop.sms";
const STALE_MS: i64 = 120_000;
const TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    enabled: bool,
    relay_url: String,
    pipestreamr_url: String,
    project_id: String,
    source_id: String,
    phone: String,
    bot_id: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            relay_url: String::new(),
            pipestreamr_url: String::new(),
            project_id: String::new(),
            source_id: String::new(),
            phone: String::new(),
            bot_id: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SmsMessage {
    pub id: String,
    pub project_id: String,
    pub source_id: String,
    pub from_id: String,
    pub body: String,
    pub received_at: i64,
    pub state: String,
    pub detail: Option<String>,
    pub claim_id: Option<String>,
    #[serde(default)]
    pub review: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsStatus {
    #[serde(flatten)]
    settings: Settings,
    running: bool,
    has_relay_token: bool,
    has_api_key: bool,
    error: Option<String>,
    messages: Vec<SmsMessage>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Pin {
    bot_id: String,
    directory: Option<String>,
    file: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Pending,
    Claiming,
    Submitting,
    ReplyReady,
    Sending,
    Terminal,
}
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    message: SmsMessage,
    phase: Phase,
    accepted: bool,
    reply: Option<String>,
    // Terminal intent remains durable until the relay acknowledges it.
    finish_pending: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    version: u32,
    credential_id: String,
    #[serde(default)]
    has_relay_token: bool,
    #[serde(default)]
    has_api_key: bool,
    settings: Settings,
    pin: Option<Pin>,
    entries: BTreeMap<String, Entry>,
}
impl Default for Saved {
    fn default() -> Self {
        Self {
            version: 1,
            credential_id: Uuid::new_v4().to_string(),
            has_relay_token: false,
            has_api_key: false,
            settings: Settings::default(),
            pin: None,
            entries: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Default)]
struct Secrets {
    relay_token: Option<String>,
    api_key: Option<String>,
}
trait CredentialStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, value: &str) -> Result<()>;
}
struct Keychain;
impl CredentialStore for Keychain {
    fn get(&self, account: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, account)
            .map_err(|_| anyhow!("SMS Keychain entry unavailable"))?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => bail!("SMS credentials could not be read from Keychain"),
        }
    }
    fn set(&self, account: &str, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, account)
            .map_err(|_| anyhow!("SMS Keychain entry unavailable"))?;
        if value.is_empty() {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(_) => bail!("SMS Keychain credential could not be removed"),
            }
        } else {
            entry
                .set_password(value)
                .map_err(|_| anyhow!("SMS credential could not be saved to Keychain"))
        }
    }
}
trait Clock: Send + Sync {
    fn now(&self) -> i64;
}
struct WallClock;
impl Clock for WallClock {
    fn now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64
    }
}
struct Inner {
    saved: Saved,
    secrets: Secrets,
    error: Option<String>,
    connected_at: Option<i64>,
    load_error: bool,
}

/// One connector per Desktop/data directory. Native IPC and authenticated development
/// bridge may dispatch `sms_*`; the production remote transport must reject the prefix.
pub struct SmsConnector {
    desktop: Arc<Desktop>,
    directory: PathBuf,
    inner: AsyncMutex<Inner>,
    credentials: Arc<dyn CredentialStore>,
    clock: Arc<dyn Clock>,
    network: network::Network,
    lifecycle: AsyncMutex<Option<JoinHandle<()>>>,
    operation: AsyncMutex<()>,
    enabled: AtomicBool,
    running: AtomicBool,
    suspended: AtomicBool,
    stopping: AtomicBool,
    wake: Notify,
    stop: Notify,
}

impl SmsConnector {
    pub async fn open(desktop: Arc<Desktop>, directory: PathBuf) -> Result<Arc<Self>> {
        Self::open_with(desktop, directory, Arc::new(Keychain), Arc::new(WallClock)).await
    }
    async fn open_with(
        desktop: Arc<Desktop>,
        directory: PathBuf,
        credentials: Arc<dyn CredentialStore>,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>> {
        let loaded = match tokio::fs::read(directory.join("sms.json")).await {
            Ok(bytes) => serde_json::from_slice::<Saved>(&bytes)
                .context("SMS settings/journal are unreadable; explicitly save settings to repair"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Saved::default()),
            Err(_) => Err(anyhow!(
                "SMS settings/journal could not be read; file was not overwritten"
            )),
        };
        let (mut saved, mut error, mut load_error) = match loaded {
            Ok(saved) if saved.version == 1 && Uuid::parse_str(&saved.credential_id).is_ok() => {
                (saved, None, false)
            }
            Ok(_) => (
                Saved::default(),
                Some("Unsupported SMS settings/journal; file was not overwritten".into()),
                true,
            ),
            Err(error) => (Saved::default(), Some(error.to_string()), true),
        };
        if !load_error {
            if let Err(validation) = validate_settings(&saved.settings) {
                error = Some(format!(
                    "SMS settings are invalid ({validation}); file was not overwritten"
                ));
                load_error = true;
            }
        }
        recover(&mut saved);
        let secrets = Secrets::default();
        let enabled = saved.settings.enabled;
        let app = Arc::new(Self {
            desktop,
            directory,
            inner: AsyncMutex::new(Inner {
                saved,
                secrets,
                error,
                connected_at: None,
                load_error,
            }),
            credentials,
            clock,
            network: network::Network::new()?,
            lifecycle: AsyncMutex::new(None),
            operation: AsyncMutex::new(()),
            enabled: AtomicBool::new(enabled),
            running: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            wake: Notify::new(),
            stop: Notify::new(),
        });
        if !load_error {
            // A first-time, disabled connector does not access Keychain or create files.
            let mut inner = app.inner.lock().await;
            if app.directory.join("sms.json").exists() {
                if let Err(error) = app.persist(&inner.saved).await {
                    inner.error = Some(error.to_string());
                    inner.load_error = true;
                }
            }
        }
        app.start().await;
        Ok(app)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub async fn request(self: &Arc<Self>, method: &str, args: Value) -> Result<Value> {
        match method {
            "sms_status" => self.status().await,
            "sms_configure" => {
                let _operation = self.operation.lock().await;
                let settings: Settings =
                    serde_json::from_value(args.clone()).context("Invalid SMS settings")?;
                validate_settings(&settings)?;
                if settings.enabled
                    && !self
                        .desktop
                        .state
                        .lock()
                        .unwrap()
                        .bots
                        .contains_key(&settings.bot_id)
                {
                    bail!("Select an existing desktop bot for SMS");
                }
                for name in ["relayToken", "apiKey"] {
                    if let Some(value) = args.get(name) {
                        let value = value
                            .as_str()
                            .ok_or_else(|| anyhow!("{name} must be a string"))?;
                        if value.len() > 8192 || value.chars().any(char::is_control) {
                            bail!("Invalid SMS credential");
                        }
                        if name == "relayToken" && !value.is_empty() && value.len() < 32 {
                            bail!("Relay bearer token must contain at least 32 characters");
                        }
                    }
                }
                self.suspend_worker().await?;
                let result = self.configure(settings, &args).await;
                self.start().await;
                result?;
                self.status().await
            }
            "sms_accept" | "sms_dismiss" => {
                let _operation = self.operation.lock().await;
                let id = required(&args, "id")?;
                let mut inner = self.inner.lock().await;
                if inner.load_error {
                    bail!("Repair SMS settings before reviewing messages");
                }
                let key = entry_key(&inner.saved.settings, id);
                if !inner
                    .saved
                    .entries
                    .get(&key)
                    .is_some_and(|entry| same_route(&entry.message, &inner.saved.settings))
                {
                    bail!("SMS message does not belong to the configured route");
                }
                let entry = inner
                    .saved
                    .entries
                    .get_mut(&key)
                    .ok_or_else(|| anyhow!("SMS message is no longer available"))?;
                if entry.phase != Phase::Pending || entry.message.state != "pending" {
                    bail!("Only pending messages may be accepted or dismissed; uncertain executions cannot be replayed");
                }
                if method == "sms_accept" {
                    entry.accepted = true;
                    entry.message.review = false;
                    entry.message.detail = Some(
                        "Accepted for this connection; waiting for the SMS bot to be idle".into(),
                    );
                } else {
                    terminal(entry, "dismissed", "Dismissed without submitting a prompt");
                }
                if let Err(error) = self.persist(&inner.saved).await {
                    inner.load_error = true;
                    inner.error = Some(error.to_string());
                    return Err(error);
                }
                drop(inner);
                self.wake.notify_one();
                self.status().await
            }
            _ => bail!("Unknown SMS method: {method}"),
        }
    }

    async fn configure(&self, settings: Settings, args: &Value) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let previous = &inner.saved.settings;
        let route_changed = previous.relay_url != settings.relay_url
            || previous.pipestreamr_url != settings.pipestreamr_url
            || previous.project_id != settings.project_id
            || previous.source_id != settings.source_id
            || previous.phone != settings.phone;
        if route_changed
            && inner.saved.entries.iter().any(|(key, entry)| {
                key.starts_with(&format!("{}|", previous.relay_url))
                    && (entry.finish_pending || entry.phase == Phase::ReplyReady)
            })
        {
            bail!("Reconcile pending SMS finalization before changing relay, PipeStreamr, project, source, or phone settings");
        }
        let id = inner.saved.credential_id.clone();
        let relay = args
            .get("relayToken")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let api = args
            .get("apiKey")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let store = self.credentials.clone();
        // Explicit configuration is the only credential mutation path. Blank means delete.
        let secrets = if !settings.enabled && relay.is_none() && api.is_none() {
            inner.secrets.clone()
        } else {
            tokio::task::spawn_blocking(move || {
                if let Some(value) = relay {
                    store.set(&format!("{id}:relayToken"), &value)?;
                }
                if let Some(value) = api {
                    store.set(&format!("{id}:apiKey"), &value)?;
                }
                read_secrets(store.as_ref(), &id)
            })
            .await
            .context("SMS credential operation interrupted")??
        };
        if settings.enabled && (secrets.relay_token.is_none() || secrets.api_key.is_none()) {
            inner.error = Some(
                "Save a relay bearer token and PipeStreamr API key before enabling SMS".into(),
            );
            bail!("Save a relay bearer token and PipeStreamr API key before enabling SMS");
        }
        if inner.saved.settings.bot_id != settings.bot_id {
            inner.saved.pin = None;
        }
        inner.saved.settings = settings;
        if inner.saved.settings.enabled
            || args.get("relayToken").is_some()
            || args.get("apiKey").is_some()
        {
            inner.saved.has_relay_token = secrets.relay_token.is_some();
            inner.saved.has_api_key = secrets.api_key.is_some();
        }
        inner.secrets = secrets;
        inner.error = None;
        inner.load_error = false;
        inner.connected_at = None;
        mark_review(&mut inner.saved);
        if let Err(error) = self.persist(&inner.saved).await {
            inner.load_error = true;
            inner.error = Some(error.to_string());
            return Err(error);
        }
        self.enabled
            .store(inner.saved.settings.enabled, Ordering::Release);
        Ok(())
    }

    async fn status(&self) -> Result<Value> {
        let inner = self.inner.lock().await;
        let prefix = format!("{}|", inner.saved.settings.relay_url);
        let mut messages: Vec<_> = inner
            .saved
            .entries
            .iter()
            .filter(|(key, entry)| {
                key.starts_with(&prefix) && same_route(&entry.message, &inner.saved.settings)
            })
            .map(|(_, entry)| entry.message.clone())
            .collect();
        messages.sort_by(|a, b| {
            let a_terminal = a.state != "pending" && a.state != "processing";
            let b_terminal = b.state != "pending" && b.state != "processing";
            a_terminal.cmp(&b_terminal).then_with(|| {
                if a_terminal {
                    b.received_at.cmp(&a.received_at)
                } else {
                    a.received_at.cmp(&b.received_at)
                }
            })
        });
        messages.truncate(120);
        Ok(serde_json::to_value(SmsStatus {
            settings: inner.saved.settings.clone(),
            running: self.running.load(Ordering::Acquire),
            has_relay_token: inner.saved.has_relay_token,
            has_api_key: inner.saved.has_api_key,
            error: inner.error.clone(),
            messages,
        })?)
    }

    pub async fn suspend(self: &Arc<Self>) -> Result<()> {
        let _operation = self.operation.lock().await;
        self.suspended.store(true, Ordering::Release);
        self.suspend_worker().await
    }
    pub async fn shutdown(self: &Arc<Self>) -> Result<()> {
        self.suspend().await
    }
    pub async fn resume(self: &Arc<Self>) -> Result<()> {
        let _operation = self.operation.lock().await;
        self.suspended.store(false, Ordering::Release);
        self.start().await;
        Ok(())
    }
    async fn suspend_worker(&self) -> Result<()> {
        let mut task = self.lifecycle.lock().await;
        self.stopping.store(true, Ordering::Release);
        if let Some(handle) = task.take() {
            self.stop.notify_one();
            handle.await.context("SMS worker could not be suspended")?;
        }
        let mut inner = self.inner.lock().await;
        inner.connected_at = None;
        mark_review(&mut inner.saved);
        if !inner.load_error && self.directory.join("sms.json").exists() {
            self.persist(&inner.saved).await?;
        }
        Ok(())
    }
    async fn start(self: &Arc<Self>) {
        let mut task = self.lifecycle.lock().await;
        if task.is_some() || self.suspended.load(Ordering::Acquire) {
            return;
        }
        let inner = self.inner.lock().await;
        if !inner.saved.settings.enabled
            || inner.load_error
            || validate_settings(&inner.saved.settings).is_err()
        {
            return;
        }
        drop(inner);
        let app = self.clone();
        self.stopping.store(false, Ordering::Release);
        self.running.store(true, Ordering::Release);
        *task = Some(tokio::spawn(async move {
            match app.load_worker_credentials().await {
                Ok(true) => {}
                Ok(false) => {
                    app.running.store(false, Ordering::Release);
                    return;
                }
                Err(error) => {
                    app.set_error(&error.to_string()).await;
                    app.running.store(false, Ordering::Release);
                    return;
                }
            }
            loop {
                if app.stopping.load(Ordering::Acquire) {
                    break;
                }
                // Never drop session startup or journal writes halfway through. Only
                // the settled-event wait is interruptible; network calls are bounded.
                let result = app.tick().await;
                if let Err(error) = result {
                    let mut inner = app.inner.lock().await;
                    inner.error = Some(error.to_string());
                    inner.connected_at = None;
                    recover(&mut inner.saved);
                    if let Err(error) = app.persist(&inner.saved).await {
                        inner.load_error = true;
                        inner.error = Some(error.to_string());
                    }
                }
                if app.stopping.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! { biased; _ = app.stop.notified() => {}, _ = app.wake.notified() => {}, _ = tokio::time::sleep(Duration::from_secs(3)) => {} }
            }
            let mut inner = app.inner.lock().await;
            recover(&mut inner.saved);
            inner.connected_at = None;
            if let Err(error) = app.persist(&inner.saved).await {
                inner.load_error = true;
                inner.error = Some(error.to_string());
            }
            app.running.store(false, Ordering::Release);
        }));
    }

    async fn load_worker_credentials(&self) -> Result<bool> {
        let id = {
            let mut inner = self.inner.lock().await;
            if inner.secrets.relay_token.is_some() && inner.secrets.api_key.is_some() {
                return Ok(true);
            }
            inner.error =
                Some("Reading SMS credentials; resolve Keychain permission if requested".into());
            inner.saved.credential_id.clone()
        };
        let store = self.credentials.clone();
        let mut pending = tokio::task::spawn_blocking(move || read_secrets(store.as_ref(), &id));
        // An OS permission prompt cannot be cancelled, but it must never hold
        // window creation, status inspection, or the connector's shutdown.
        let secrets = loop {
            tokio::select! {
                result = &mut pending => break result.context("SMS credential read interrupted")??,
                _ = self.stop.notified() => {
                    if self.stopping.load(Ordering::Acquire) { return Ok(false); }
                }
            }
        };
        if self.stopping.load(Ordering::Acquire) {
            return Ok(false);
        }
        let mut inner = self.inner.lock().await;
        inner.saved.has_relay_token = secrets.relay_token.is_some();
        inner.saved.has_api_key = secrets.api_key.is_some();
        inner.secrets = secrets;
        self.persist(&inner.saved).await?;
        if !inner.saved.has_relay_token || !inner.saved.has_api_key {
            bail!("SMS credentials are missing; save a relay token and PipeStreamr API key in SMS settings");
        }
        inner.error = None;
        Ok(true)
    }

    async fn persist(&self, saved: &Saved) -> Result<()> {
        tokio::fs::create_dir_all(&self.directory)
            .await
            .context("Cannot create SMS settings directory")?;
        let bytes = serde_json::to_vec(saved)?;
        let temporary = self.directory.join("sms.json.tmp");
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .await
            .context("Cannot write SMS journal; no prompt may be submitted")?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, self.directory.join("sms.json")).await?;
        // Rename durability matters: a lost pre-submit marker must never replay a prompt.
        #[cfg(unix)]
        {
            let directory = self.directory.clone();
            tokio::task::spawn_blocking(move || std::fs::File::open(directory)?.sync_all())
                .await??;
        }
        Ok(())
    }

    async fn tick(self: &Arc<Self>) -> Result<()> {
        let (settings, secrets, connected_at) = {
            let inner = self.inner.lock().await;
            if inner.load_error {
                bail!("SMS journal unavailable; explicitly repair settings before continuing");
            }
            (
                inner.saved.settings.clone(),
                inner.secrets.clone(),
                inner.connected_at,
            )
        };
        let messages = self.network.inbox(&settings, &secrets).await?;
        let now = self.clock.now();
        {
            let mut inner = self.inner.lock().await;
            let connected = connected_at.unwrap_or(now);
            if connected_at.is_none() {
                mark_review(&mut inner.saved);
            }
            merge_inbox(&mut inner.saved, messages, connected, now)?;
            self.persist(&inner.saved).await?;
            inner.connected_at = Some(connected);
            inner.error = None;
        }
        if self.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        self.flush_finishes(&settings, &secrets).await?;
        if self.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        let pinned = self.inner.lock().await.saved.pin.is_some();
        if pinned {
            self.check_pin(&settings.bot_id).await?;
        }
        // Only one prompt at a time, and only after an authoritative idle check.
        let candidate = {
            let inner = self.inner.lock().await;
            inner
                .saved
                .entries
                .iter()
                .filter(|(key, entry)| {
                    key.starts_with(&format!("{}|", settings.relay_url))
                        && same_route(&entry.message, &settings)
                        && entry.phase == Phase::Pending
                        && entry.message.state == "pending"
                        && !entry.message.review
                })
                .min_by_key(|(_, entry)| entry.message.received_at)
                .map(|(key, _)| key.clone())
        };
        let Some(key) = candidate else {
            return Ok(());
        };
        let gate = self.desktop.gate(&settings.bot_id)?;
        let Ok(_guard) = gate.try_lock() else {
            self.set_error("SMS bot is busy; waiting without claiming the message")
                .await;
            return Ok(());
        };
        if self.desktop.require_idle(&settings.bot_id).is_err() {
            self.set_error("SMS bot is busy or awaiting desktop approval; waiting without claiming the message").await;
            return Ok(());
        }
        self.prepare_session(&settings.bot_id).await?;
        self.desktop.start_locked(&settings.bot_id).await?;
        let (client, generation) = self.desktop.runtime(&settings.bot_id)?;
        let state = client.request(json!({"type":"get_state"})).await?;
        if runtime_busy(&state) {
            self.set_error("SMS bot is busy; waiting for all queued work and approvals")
                .await;
            return Ok(());
        }
        self.check_pin(&settings.bot_id).await?;
        let before = client.history().await?;
        let mut events = client.subscribe();
        if self.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        let message = {
            let mut inner = self.inner.lock().await;
            let entry = inner
                .saved
                .entries
                .get_mut(&key)
                .ok_or_else(|| anyhow!("SMS journal entry missing"))?;
            if entry.phase != Phase::Pending
                || entry.message.state != "pending"
                || entry.message.review
            {
                return Ok(());
            }
            // A queued fresh message can grow stale while another turn is running.
            let dispatch_now = self.clock.now();
            if !entry.accepted
                && (dispatch_now - entry.message.received_at > STALE_MS
                    || entry.message.received_at > dispatch_now + 30_000)
            {
                entry.message.review = true;
                entry.message.detail = Some(
                    "Message became stale while waiting; Accept or Dismiss in SMS settings".into(),
                );
                self.persist(&inner.saved).await?;
                return Ok(());
            }
            entry.phase = Phase::Claiming;
            entry.message.claim_id = Some(Uuid::new_v4().to_string());
            self.persist(&inner.saved).await?;
            inner.saved.entries[&key].message.clone()
        };
        // Ambiguous claim errors are never converted back to pending locally.
        if let Err(error) = self.network.claim(&settings, &secrets, &message).await {
            self.finish_local(
                &key,
                "unknown",
                "Relay claim outcome uncertain; no automatic prompt replay",
            )
            .await?;
            return Err(error);
        }
        {
            let mut inner = self.inner.lock().await;
            let entry = inner.saved.entries.get_mut(&key).unwrap();
            entry.phase = Phase::Submitting;
            entry.message.state = "processing".into();
            entry.message.detail =
                Some("Submitted once; approvals remain in the desktop bot".into());
            // This durable marker is deliberately before writing to the runtime.
            self.persist(&inner.saved).await?;
        }
        let outcome = tokio::time::timeout(TURN_TIMEOUT, async {
            if self.stopping.load(Ordering::Acquire) { bail!("SMS connector suspended before prompt submission"); }
            client.request(json!({"type":"prompt", "message":message.body, "streamingBehavior":"followUp"})).await?;
            // Admission is serialized, not the whole turn: Stop bot and session
            // administration must remain usable while a tool awaits approval.
            // Final history/pin checks withhold replies if other work intervenes.
            drop(_guard);
            let mut completed = false;
            loop {
                if self.stopping.load(Ordering::Acquire) { bail!("SMS connector suspended; bot work was not cancelled or approved"); }
                let event = tokio::select! {
                    biased;
                    _ = self.stop.notified() => { continue; }
                    event = events.recv() => event.context("Lost SMS runtime event correlation; execution outcome unknown")?,
                };
                match event["type"].as_str().unwrap_or("") {
                    "prompt_result" => {
                        if event["status"] != "completed" { bail!("SMS prompt did not complete; inspect the desktop transcript (no automatic replay)"); }
                        completed = true;
                    }
                    "extension_ui_request" if matches!(event["method"].as_str(), Some("select" | "confirm" | "input" | "editor" | "ask" | "open_url")) => {
                        self.set_error("SMS bot needs input: open its desktop conversation to review the approval; SMS never approves automatically").await;
                    }
                    "process_exit" | "transport_error" => bail!("SMS bot disconnected; execution outcome unknown, inspect its desktop transcript"),
                    "session_settled" if completed => break,
                    _ => {}
                }
            }
            self.check_pin(&settings.bot_id).await?;
            let state = client.request(json!({"type":"get_state"})).await?;
            if runtime_busy(&state) { bail!("SMS bot has unrelated pending work; reply was withheld"); }
            let after = client.history().await?;
            final_reply(&before, &after, &message.body)
        }).await;
        match outcome {
            Ok(Ok(reply)) => {
                let mut inner = self.inner.lock().await;
                let entry = inner.saved.entries.get_mut(&key).unwrap();
                entry.reply = Some(reply);
                entry.phase = Phase::ReplyReady;
                entry.message.detail =
                    Some("Final assistant text ready; SMS submission has not started".into());
                self.persist(&inner.saved).await?;
                drop(inner);
                self.send_reply(&settings, &secrets, &key).await?;
            }
            Ok(Err(_)) => {
                self.finish_local(&key, "unknown", "Prompt execution or final reply could not be correlated safely; inspect the desktop transcript. It will not be replayed.").await?;
            }
            Err(_) => {
                self.finish_local(&key, "unknown", "Timed out awaiting final reply; the bot/approvals may still be active. Inspect desktop; never automatically replay.").await?;
            }
        }
        if self.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        self.desktop
            .refresh_state(&settings.bot_id, generation, &client)
            .await?;
        self.desktop.save().await?;
        self.flush_finishes(&settings, &secrets).await
    }

    async fn prepare_session(&self, id: &str) -> Result<()> {
        let pinned = self.inner.lock().await.saved.pin.is_some();
        if pinned {
            return self.check_pin(id).await;
        }
        // Use the normal catalog cutover, preserving the user's prior conversation.
        self.desktop.switch_session_locked(id, None).await?;
        let pin = {
            let state = self.desktop.state.lock().unwrap();
            let bot = state
                .bots
                .get(id)
                .ok_or_else(|| anyhow!("SMS bot no longer exists"))?;
            Pin {
                bot_id: id.into(),
                directory: bot.record.session_directory.clone(),
                file: bot
                    .view
                    .session_file
                    .clone()
                    .or_else(|| bot.record.session_file.clone()),
            }
        };
        if pin.directory.is_none() && pin.file.is_none() {
            bail!("SMS dedicated conversation could not be pinned");
        }
        let mut inner = self.inner.lock().await;
        inner.saved.pin = Some(pin);
        self.persist(&inner.saved).await
    }
    async fn check_pin(&self, id: &str) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let pin = inner
            .saved
            .pin
            .as_mut()
            .ok_or_else(|| anyhow!("SMS conversation is not pinned"))?;
        let current = {
            let state = self.desktop.state.lock().unwrap();
            let bot = state
                .bots
                .get(id)
                .ok_or_else(|| anyhow!("SMS bot no longer exists"))?;
            (
                bot.record.session_directory.clone(),
                bot.view
                    .session_file
                    .clone()
                    .or_else(|| bot.record.session_file.clone()),
            )
        };
        if pin.bot_id != id
            || match pin.file.as_ref() {
                Some(file) => current.1.as_ref() != Some(file),
                None => current.0 != pin.directory,
            }
        {
            bail!("SMS paused: selected bot conversation differs from the pinned SMS conversation. Resume the SMS conversation in the desktop before continuing.");
        }
        if pin.file.is_none() && current.1.is_some() {
            pin.file = current.1;
            self.persist(&inner.saved).await?;
        }
        Ok(())
    }
    async fn set_error(&self, message: &str) {
        self.inner.lock().await.error = Some(message.into());
    }
    async fn finish_local(&self, key: &str, state: &str, detail: &str) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let entry = inner
            .saved
            .entries
            .get_mut(key)
            .ok_or_else(|| anyhow!("SMS journal entry missing"))?;
        terminal(entry, state, detail);
        self.persist(&inner.saved).await
    }
    async fn send_reply(&self, settings: &Settings, secrets: &Secrets, key: &str) -> Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        let (message, reply) = {
            let mut inner = self.inner.lock().await;
            let entry = inner.saved.entries.get_mut(key).unwrap();
            entry.phase = Phase::Sending;
            let reply = entry
                .reply
                .clone()
                .ok_or_else(|| anyhow!("Missing durable SMS reply"))?;
            let message = entry.message.clone();
            self.persist(&inner.saved).await?;
            (message, reply)
        };
        match self.network.send(settings, secrets, &message, &reply).await {
            Ok(receipt) => self.finish_local(key, "completed", &format!("PipeStreamr accepted SMS submission ({receipt}); not confirmation of handset delivery")).await,
            Err(error) => self.finish_local(key, "unknown", &format!("SMS submission outcome uncertain ({error}); reply-{}-1 retained. No automatic resend.", message.id)).await,
        }
    }
    async fn flush_finishes(&self, settings: &Settings, secrets: &Secrets) -> Result<()> {
        let pending: Vec<_> = {
            let inner = self.inner.lock().await;
            inner
                .saved
                .entries
                .iter()
                .filter(|(key, entry)| {
                    key.starts_with(&format!("{}|", settings.relay_url))
                        && same_route(&entry.message, settings)
                        && entry.finish_pending
                })
                .map(|(key, entry)| (key.clone(), entry.message.clone()))
                .collect()
        };
        for (key, message) in pending {
            if self.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            let acknowledged = self.network.finish(settings, secrets, &message).await?;
            let mut inner = self.inner.lock().await;
            let entry = inner.saved.entries.get_mut(&key).unwrap();
            entry.finish_pending = false;
            if !acknowledged {
                entry.message.detail = Some(format!("{} Relay finalization conflicted with another claim/state; retained locally without replay.", entry.message.detail.as_deref().unwrap_or("")));
            }
            self.persist(&inner.saved).await?;
        }
        // A crash before sending may resume only the persisted final reply, never inference.
        let ready: Vec<_> = {
            let inner = self.inner.lock().await;
            inner
                .saved
                .entries
                .iter()
                .filter(|(key, entry)| {
                    key.starts_with(&format!("{}|", settings.relay_url))
                        && same_route(&entry.message, settings)
                        && entry.phase == Phase::ReplyReady
                })
                .map(|(key, _)| key.clone())
                .collect()
        };
        for key in ready {
            self.send_reply(settings, secrets, &key).await?;
        }
        Ok(())
    }
}

fn read_secrets(store: &dyn CredentialStore, id: &str) -> Result<Secrets> {
    Ok(Secrets {
        relay_token: store
            .get(&format!("{id}:relayToken"))?
            .filter(|value| !value.is_empty()),
        api_key: store
            .get(&format!("{id}:apiKey"))?
            .filter(|value| !value.is_empty()),
    })
}
fn entry_key(settings: &Settings, id: &str) -> String {
    format!("{}|{id}", settings.relay_url)
}
fn same_route(message: &SmsMessage, settings: &Settings) -> bool {
    message.project_id == settings.project_id
        && message.source_id == settings.source_id
        && message.from_id == settings.phone
}
fn terminal(entry: &mut Entry, state: &str, detail: &str) {
    entry.phase = Phase::Terminal;
    entry.message.state = state.into();
    entry.message.detail = Some(detail.chars().take(2048).collect());
    entry.message.review = false;
    entry.finish_pending = true;
}
fn mark_review(saved: &mut Saved) {
    for entry in saved
        .entries
        .values_mut()
        .filter(|entry| entry.phase == Phase::Pending)
    {
        entry.message.review = true;
        entry.accepted = false;
        entry.message.detail = Some(
            "Arrived before this connection or while offline; Accept or Dismiss in SMS settings"
                .into(),
        );
    }
}
fn recover(saved: &mut Saved) {
    for entry in saved.entries.values_mut() {
        if matches!(
            entry.phase,
            Phase::Claiming | Phase::Submitting | Phase::Sending
        ) {
            terminal(entry, "unknown", "Connector stopped during a claim, prompt, or SMS submission; inspect desktop and PipeStreamr history. Never automatically replay.");
        }
    }
    mark_review(saved);
}
fn validate_settings(settings: &Settings) -> Result<()> {
    if !settings.relay_url.is_empty() {
        network::base_url(&settings.relay_url)?;
    }
    if !settings.pipestreamr_url.is_empty() {
        network::base_url(&settings.pipestreamr_url)?;
    }
    for (name, value) in [
        ("projectId", &settings.project_id),
        ("sourceId", &settings.source_id),
        ("botId", &settings.bot_id),
    ] {
        if (!value.is_empty() || settings.enabled) && Uuid::parse_str(value).is_err() {
            bail!("{name} must be a UUID");
        }
    }
    if (!settings.phone.is_empty() || settings.enabled) && !valid_phone(&settings.phone) {
        bail!("SMS phone must be verified E.164 (+ followed by 7–15 digits)");
    }
    if settings.enabled && (settings.relay_url.is_empty() || settings.pipestreamr_url.is_empty()) {
        bail!("Relay URL and PipeStreamr URL are required before enabling SMS");
    }
    Ok(())
}
fn valid_phone(phone: &str) -> bool {
    phone.strip_prefix('+').is_some_and(|digits| {
        (7..=15).contains(&digits.len())
            && !digits.starts_with('0')
            && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}
fn runtime_busy(state: &Value) -> bool {
    state["isStreaming"] == true
        || state["isCompacting"] == true
        || state["isSettled"] == false
        || state["hasPendingAsyncWork"] == true
        || state["queuedMessageCount"]
            .as_u64()
            .is_some_and(|count| count > 0)
        || ["steering", "followUp", "liveSteered"].iter().any(|kind| {
            state["queuedMessages"][*kind]
                .as_array()
                .is_some_and(|queue| !queue.is_empty())
        })
        || state["queuedMessages"]
            .as_array()
            .is_some_and(|queue| !queue.is_empty())
        || state["pendingMessages"]
            .as_array()
            .is_some_and(|queue| !queue.is_empty())
}
fn merge_inbox(
    saved: &mut Saved,
    messages: Vec<SmsMessage>,
    connected: i64,
    now: i64,
) -> Result<()> {
    for mut message in messages {
        if Uuid::parse_str(&message.id).is_err()
            || message.project_id != saved.settings.project_id
            || message.source_id != saved.settings.source_id
            || message.from_id != saved.settings.phone
            || message.body.len() > 16 * 1024
            || message
                .detail
                .as_ref()
                .is_some_and(|value| value.len() > 2048)
            || message.received_at < 0
            || !matches!(
                message.state.as_str(),
                "pending" | "processing" | "completed" | "failed" | "unknown" | "dismissed"
            )
            || message
                .claim_id
                .as_ref()
                .is_some_and(|id| Uuid::parse_str(id).is_err())
        {
            bail!(
                "Relay inbox message violates configured identity or schema; nothing was submitted"
            );
        }
        let key = entry_key(&saved.settings, &message.id);
        if let Some(entry) = saved.entries.get_mut(&key) {
            if entry.message.project_id != message.project_id
                || entry.message.source_id != message.source_id
                || entry.message.from_id != message.from_id
                || entry.message.body != message.body
                || entry.message.received_at != message.received_at
            {
                bail!("Relay changed an existing SMS event; connector paused");
            }
            if entry.phase == Phase::Pending && message.state != "pending" {
                entry.phase = Phase::Terminal;
                entry.message = message;
                entry.message.review = false;
            }
            // Local durable side-effect markers always outrank relay processing state.
            continue;
        }
        message.review = message.state == "pending"
            && (message.received_at <= connected
                || now - message.received_at > STALE_MS
                || message.received_at > now + 30_000);
        if message.review {
            message.detail = Some(
                "Message predates this connection or is stale; Accept or Dismiss in SMS settings"
                    .into(),
            );
        }
        let phase = if message.state == "pending" {
            Phase::Pending
        } else {
            Phase::Terminal
        };
        if message.state == "processing" {
            message.detail = Some("Previously claimed by another or interrupted connector; never automatically replayed".into());
        }
        saved.entries.insert(
            key,
            Entry {
                message,
                phase,
                accepted: false,
                reply: None,
                finish_pending: false,
            },
        );
    }
    Ok(())
}
fn final_reply(before: &[Value], after: &[Value], prompt: &str) -> Result<String> {
    if after.len() <= before.len() || !after.starts_with(before) {
        bail!("SMS history changed unexpectedly; reply withheld");
    }
    let turn = &after[before.len()..];
    let mut users = turn.iter().filter(|message| message["role"] == "user");
    if users
        .next()
        .is_none_or(|message| plain_text(message) != prompt)
        || users.next().is_some()
    {
        bail!("SMS turn contains unrelated desktop prompts; reply withheld");
    }
    let last = turn
        .last()
        .ok_or_else(|| anyhow!("SMS turn has no final message"))?;
    if last["role"] != "assistant"
        || matches!(
            last["stopReason"].as_str(),
            Some("error" | "aborted" | "toolUse")
        )
        || last["content"].as_array().is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| matches!(block["type"].as_str(), Some("toolCall" | "tool_use")))
        })
    {
        bail!("SMS turn has no final assistant text");
    }
    let text = plain_text(last);
    let text = text.trim();
    if text.is_empty() || text.contains('\0') {
        bail!("SMS final assistant text is empty or invalid");
    }
    const LIMIT: usize = 1200;
    let mut characters = text.chars();
    let mut result: String = characters.by_ref().take(LIMIT).collect();
    if characters.next().is_some() {
        const MARKER: &str = "\n[Truncated; see desktop]";
        let boundary = result
            .char_indices()
            .nth(LIMIT - MARKER.chars().count())
            .map(|(index, _)| index)
            .unwrap_or(result.len());
        result.truncate(boundary);
        result.push_str(MARKER);
    }
    Ok(result)
}
fn plain_text(message: &Value) -> String {
    if let Some(text) = message["content"].as_str() {
        return text.into();
    }
    let mut text = String::new();
    if let Some(blocks) = message["content"].as_array() {
        for block in blocks.iter().filter(|block| block["type"] == "text") {
            if let Some(value) = block["text"].as_str() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
            }
        }
    }
    text
}
