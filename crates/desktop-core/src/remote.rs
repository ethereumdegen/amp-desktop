//! Opt-in, loopback-only transport for the desktop instance supplied by the host.
use crate::Desktop;
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch, Mutex as AsyncMutex, Notify},
    task::{JoinHandle, JoinSet},
};
use url::Url;
use uuid::Uuid;

const DEFAULT_PORT: u16 = 14501;
const BODY_LIMIT: usize = 24 * 1024 * 1024;
const KEYCHAIN_SERVICE: &str = "omp-desktop-remote";

pub struct RemoteAsset {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}
pub type AssetLoader = Arc<dyn Fn(&str) -> Option<RemoteAsset> + Send + Sync>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Settings {
    version: u32,
    enabled: bool,
    port: u16,
    tailnet_url: Option<String>,
    instance_id: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            enabled: false,
            port: DEFAULT_PORT,
            tailnet_url: None,
            instance_id: Uuid::new_v4().to_string(),
        }
    }
}
impl Settings {
    fn validate(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.version == 1,
            "Unsupported remote access settings version"
        );
        anyhow::ensure!(
            self.port != 0,
            "Remote access port must be between 1 and 65535"
        );
        Uuid::parse_str(&self.instance_id).context("Invalid remote access instance identifier")?;
        self.tailnet_url = self
            .tailnet_url
            .as_deref()
            .map(tailnet_origin)
            .transpose()?;
        Ok(())
    }

    fn patched(&self, args: Value) -> Result<Self> {
        let patch = args
            .as_object()
            .context("Remote settings must be an object")?;
        anyhow::ensure!(
            patch
                .keys()
                .all(|key| matches!(key.as_str(), "enabled" | "port" | "tailnetUrl")),
            "Unknown remote access setting"
        );
        let mut next = self.clone();
        if let Some(enabled) = patch.get("enabled") {
            next.enabled = enabled
                .as_bool()
                .context("Remote enabled must be a boolean")?;
        }
        if let Some(port) = patch.get("port") {
            next.port = port
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n != 0)
                .context("Remote access port must be between 1 and 65535")?;
        }
        if let Some(url) = patch.get("tailnetUrl") {
            next.tailnet_url = if url.is_null() {
                None
            } else {
                Some(tailnet_origin(
                    url.as_str()
                        .context("Tailnet URL must be a string or null")?,
                )?)
            };
        }
        Ok(next)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteStatus {
    enabled: bool,
    running: bool,
    local_url: String,
    tailnet_url: Option<String>,
    port: u16,
    error: Option<String>,
    serve_command: String,
    instance_id: String,
}

// The only credential boundary. Tests substitute an isolated in-memory store;
// production never falls back to files, environment variables, or weak tokens.
pub(crate) trait Credentials: Send + Sync {
    fn read(&self, instance: &str) -> Result<Option<String>>;
    fn write(&self, instance: &str, token: &str) -> Result<()>;
}
struct Keychain;
impl Credentials for Keychain {
    fn read(&self, instance: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, instance)
            .map_err(|_| anyhow!("Could not access remote access credential storage"))?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => bail!("Could not read the remote access token from Keychain"),
        }
    }
    fn write(&self, instance: &str, token: &str) -> Result<()> {
        keyring::Entry::new(KEYCHAIN_SERVICE, instance)
            .map_err(|_| anyhow!("Could not access remote access credential storage"))?
            .set_password(token)
            .map_err(|_| anyhow!("Could not save the remote access token in Keychain"))
    }
}

struct Controller {
    settings: Settings,
    token: Option<String>,
    error: Option<String>,
    settings_error: bool,
    closed: bool,
    suspended: bool,
    configuration_epoch: u64,
    server: Option<Running>,
}

impl Controller {
    fn require_open(&self) -> Result<()> {
        anyhow::ensure!(!self.closed, "Remote access has shut down");
        anyhow::ensure!(
            !self.settings_error,
            "Saved remote access settings must be repaired before changing remote access"
        );
        Ok(())
    }

    fn can_start(&self, epoch: u64) -> bool {
        !self.closed
            && !self.suspended
            && !self.settings_error
            && self.settings.enabled
            && self.configuration_epoch == epoch
            && self.server.is_none()
    }
}

pub struct RemoteAccess {
    desktop: Arc<Desktop>,
    directory: PathBuf,
    assets: AssetLoader,
    credentials: Arc<dyn Credentials>,
    credential_io: AsyncMutex<()>,
    closing: Arc<AtomicBool>,
    controller: AsyncMutex<Controller>,
    running: Arc<AtomicBool>,
    server_error: Arc<Mutex<Option<String>>>,
}

impl RemoteAccess {
    pub async fn open(
        desktop: Arc<Desktop>,
        directory: PathBuf,
        assets: AssetLoader,
    ) -> Result<Arc<Self>> {
        Self::open_with_credentials(desktop, directory, assets, Arc::new(Keychain)).await
    }

    pub(crate) async fn open_with_credentials(
        desktop: Arc<Desktop>,
        directory: PathBuf,
        assets: AssetLoader,
        credentials: Arc<dyn Credentials>,
    ) -> Result<Arc<Self>> {
        tokio::fs::create_dir_all(&directory).await?;
        let saved = match tokio::fs::read(directory.join("remote.json")).await {
            Ok(bytes) => serde_json::from_slice::<Settings>(&bytes)
                .context("Cannot read saved remote access settings")
                .and_then(|mut saved| {
                    saved.validate()?;
                    Ok(saved)
                }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let settings = Settings::default();
                save_settings(&directory, &settings).await.map(|_| settings)
            }
            Err(_) => Err(anyhow!("Cannot read saved remote access settings")),
        };
        let (settings, error, settings_error) = match saved {
            Ok(settings) => (settings, None, false),
            Err(error) => (Settings::default(), Some(error.to_string()), true),
        };
        let remote = Arc::new(Self {
            desktop,
            directory,
            assets,
            credentials,
            credential_io: AsyncMutex::new(()),
            closing: Arc::new(AtomicBool::new(false)),
            controller: AsyncMutex::new(Controller {
                settings,
                token: None,
                error,
                settings_error,
                closed: false,
                suspended: false,
                configuration_epoch: 0,
                server: None,
            }),
            running: Arc::new(AtomicBool::new(false)),
            server_error: Arc::new(Mutex::new(None)),
        });
        let restore = {
            let controller = remote.controller.lock().await;
            (controller.settings.enabled && !controller.settings_error)
                .then_some(controller.configuration_epoch)
        };
        if let Some(epoch) = restore {
            // Keychain may require native permission. Neither window startup nor
            // lifecycle controls may wait for that optional credential operation.
            let pending = Arc::downgrade(&remote);
            tokio::spawn(async move {
                if let Some(remote) = pending.upgrade() {
                    remote.restore_listener(epoch).await;
                }
            });
        }
        Ok(remote)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn closing_flag(&self) -> Arc<AtomicBool> {
        self.closing.clone()
    }

    fn require_administration(&self, controller: &Controller) -> Result<()> {
        controller.require_open()?;
        anyhow::ensure!(
            !self.closing.load(Ordering::Acquire),
            "The desktop is shutting down"
        );
        Ok(())
    }

    /// Local desktop/owner IPC only. HTTP dispatches directly to Desktop instead.
    pub async fn request(&self, method: &str, args: Value) -> Result<Value> {
        match method {
            "remote_status" => {
                let controller = self.controller.lock().await;
                self.status(&controller)
            }
            "remote_configure" => {
                let epoch = {
                    let mut controller = self.controller.lock().await;
                    self.require_administration(&controller)?;
                    let next = controller.settings.patched(args)?;
                    save_settings(&self.directory, &next).await?;
                    let restart = next.port != controller.settings.port
                        || next.tailnet_url != controller.settings.tailnet_url;
                    controller.settings = next;
                    controller.suspended = false;
                    controller.configuration_epoch += 1;
                    controller.error = None;
                    if restart || !controller.settings.enabled || !self.is_running() {
                        self.stop(&mut controller).await?;
                    }
                    let epoch = controller.configuration_epoch;
                    if !controller.can_start(epoch) {
                        return self.status(&controller);
                    }
                    epoch
                };
                self.restore_listener(epoch).await;
                let controller = self.controller.lock().await;
                self.status(&controller)
            }
            "remote_token" => {
                self.ensure_token(None).await?;
                let controller = self.controller.lock().await;
                self.require_administration(&controller)?;
                let token = controller
                    .token
                    .as_deref()
                    .context("Remote access token unavailable")?;
                Ok(json!({"token":token}))
            }
            "remote_rotate_token" => {
                {
                    let controller = self.controller.lock().await;
                    self.require_administration(&controller)?;
                }
                let _credential_io = self.credential_io.lock().await;
                let instance = {
                    let controller = self.controller.lock().await;
                    self.require_administration(&controller)?;
                    controller.settings.instance_id.clone()
                };
                let store = self.credentials.clone();
                let token = new_token();
                let token = tokio::task::spawn_blocking(move || -> Result<String> {
                    store.write(&instance, &token)?;
                    Ok(token)
                })
                .await
                .context("Remote credential storage task failed")??;
                let mut controller = self.controller.lock().await;
                controller.require_open()?;
                controller.token = Some(token.clone());
                if let Some(server) = &controller.server {
                    server.state.policy.lock().unwrap().token = token.clone();
                }
                Ok(json!({"token":token}))
            }
            _ => bail!("Unknown local remote access method"),
        }
    }

    /// Quiesces networking for a quit attempt without poisoning recovery if a
    /// later engine shutdown fails. Local settings can explicitly restart it.
    pub async fn suspend(&self) -> Result<()> {
        let mut controller = self.controller.lock().await;
        anyhow::ensure!(!controller.closed, "Remote access has shut down");
        controller.suspended = true;
        controller.configuration_epoch += 1;
        self.stop(&mut controller).await
    }

    /// Stops accepting requests and drains accepted dispatches; saved opt-in is unchanged.
    pub async fn shutdown(&self) -> Result<()> {
        let mut controller = self.controller.lock().await;
        controller.closed = true;
        controller.configuration_epoch += 1;
        self.stop(&mut controller).await
    }

    fn status(&self, controller: &Controller) -> Result<Value> {
        let settings = &controller.settings;
        let https_port = settings
            .tailnet_url
            .as_deref()
            .and_then(parse_origin)
            .and_then(|origin| origin.port_or_known_default())
            .unwrap_or(8443);
        Ok(serde_json::to_value(RemoteStatus {
            enabled: settings.enabled,
            running: self.is_running(),
            local_url: format!("http://127.0.0.1:{}", settings.port),
            tailnet_url: settings.tailnet_url.clone(),
            port: settings.port,
            error: controller
                .error
                .clone()
                .or_else(|| self.server_error.lock().unwrap().clone()),
            serve_command: format!(
                "tailscale serve --bg --https={https_port} http://127.0.0.1:{}",
                settings.port
            ),
            instance_id: settings.instance_id.clone(),
        })?)
    }

    async fn ensure_token(&self, purpose: Option<u64>) -> Result<()> {
        {
            let controller = self.controller.lock().await;
            self.require_administration(&controller)?;
            if controller.token.is_some() {
                return Ok(());
            }
        }
        // Serialize credential reads/writes separately from listener lifecycle.
        // A Keychain prompt must never own the controller lock.
        let _credential_io = self.credential_io.lock().await;
        let instance = {
            let controller = self.controller.lock().await;
            self.require_administration(&controller)?;
            if controller.token.is_some() {
                return Ok(());
            }
            if let Some(epoch) = purpose {
                anyhow::ensure!(controller.can_start(epoch), "Remote startup was cancelled");
            }
            controller.settings.instance_id.clone()
        };
        let store = self.credentials.clone();
        let read_instance = instance.clone();
        let stored = tokio::task::spawn_blocking(move || store.read(&read_instance))
            .await
            .context("Remote credential storage task failed")??;
        let token = if let Some(token) = stored {
            anyhow::ensure!(
                valid_token(&token),
                "Invalid remote access credential; rotate the token locally to repair it"
            );
            token
        } else {
            {
                let controller = self.controller.lock().await;
                self.require_administration(&controller)?;
                if let Some(epoch) = purpose {
                    anyhow::ensure!(controller.can_start(epoch), "Remote startup was cancelled");
                }
            }
            let token = new_token();
            let store = self.credentials.clone();
            tokio::task::spawn_blocking(move || -> Result<String> {
                store.write(&instance, &token)?;
                Ok(token)
            })
            .await
            .context("Remote credential storage task failed")??
        };
        let mut controller = self.controller.lock().await;
        controller.require_open()?;
        controller.token = Some(token);
        Ok(())
    }

    async fn restore_listener(&self, epoch: u64) {
        {
            let controller = self.controller.lock().await;
            if !controller.can_start(epoch) || self.closing.load(Ordering::Acquire) {
                return;
            }
        }
        let credentials = self.ensure_token(Some(epoch)).await;
        let mut controller = self.controller.lock().await;
        if !controller.can_start(epoch) || self.closing.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = credentials {
            if controller.token.is_none() {
                controller.error = Some(error.to_string());
                return;
            }
        }
        // Rotation may have completed while this task was reacquiring state.
        // Always publish the current credential, never an earlier read result.
        let Some(token) = controller.token.clone() else {
            controller.error = Some("Remote access token unavailable".into());
            return;
        };
        if let Err(error) = self.start(&mut controller, token).await {
            controller.error = Some(error.to_string());
        }
    }

    async fn start(&self, controller: &mut Controller, token: String) -> Result<()> {
        self.require_administration(controller)?;
        let port = controller.settings.port;
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .await.map_err(|_| anyhow!("Could not bind remote access to 127.0.0.1:{port}; the port may already be in use"))?;
        self.require_administration(controller)?;
        self.launch(controller, listener, token);
        Ok(())
    }

    fn launch(&self, controller: &mut Controller, listener: TcpListener, token: String) {
        let (stopping, _) = watch::channel(false);
        let state = Arc::new(ServerState {
            desktop: self.desktop.clone(),
            assets: self.assets.clone(),
            policy: Mutex::new(Policy {
                accepting: true,
                token,
                port: controller.settings.port,
                tailnet_url: controller
                    .settings
                    .tailnet_url
                    .as_deref()
                    .and_then(parse_origin),
                in_flight: 0,
            }),
            drained: Notify::new(),
            stopping,
        });
        let router = Router::new().fallback(handle).with_state(state.clone());
        let (stop, stopped) = oneshot::channel();
        self.running.store(true, Ordering::Release);
        *self.server_error.lock().unwrap() = None;
        let running = self.running.clone();
        let error = self.server_error.clone();
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let mut stopped = stopped;
            let mut connections = JoinSet::new();
            let result = loop {
                tokio::select! {
                    _ = &mut stopped => break Ok(()),
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let service = TowerToHyperService::new(router.clone());
                            connections.spawn(async move {
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            });
                        }
                        Err(error) => break Err(error),
                    },
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            };
            drop(listener);
            task_state.stop_accepting();
            // Accepted mutations own independent tasks. Finish them before
            // closing sockets, but never wait for a peer to consume a response.
            task_state.drain().await;
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            running.store(false, Ordering::Release);
            if result.is_err() {
                *error.lock().unwrap() = Some("Remote access listener stopped unexpectedly".into());
            }
            result
        });
        controller.server = Some(Running {
            state,
            stop: Some(stop),
            task: Some(task),
        });
    }

    async fn stop(&self, controller: &mut Controller) -> Result<()> {
        if let Some(mut server) = controller.server.take() {
            server.state.stop_accepting();
            self.running.store(false, Ordering::Release);
            if let Some(stop) = server.stop.take() {
                let _ = stop.send(());
            }
            server.state.drain().await;
            if let Some(task) = server.task.take() {
                task.await
                    .context("Remote access listener task failed")?
                    .context("Remote access listener failed")?;
            }
        }
        Ok(())
    }
}

async fn save_settings(directory: &std::path::Path, settings: &Settings) -> Result<()> {
    let temporary = directory.join("remote.json.tmp");
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(settings)?)
        .await
        .context("Could not save remote access settings")?;
    tokio::fs::rename(temporary, directory.join("remote.json"))
        .await
        .context("Could not save remote access settings")?;
    Ok(())
}

fn new_token() -> String {
    // UUID v4 uses OS cryptographic randomness; two UUIDs provide 244 random bits.
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

struct Running {
    state: Arc<ServerState>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.state.stop_accepting();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // Do not abort: accepted operations must survive transport-owner drops.
    }
}
struct Policy {
    accepting: bool,
    token: String,
    port: u16,
    tailnet_url: Option<Url>,
    in_flight: usize,
}
struct ServerState {
    desktop: Arc<Desktop>,
    assets: AssetLoader,
    policy: Mutex<Policy>,
    drained: Notify,
    stopping: watch::Sender<bool>,
}
impl ServerState {
    fn stop_accepting(&self) {
        self.policy.lock().unwrap().accepting = false;
        self.stopping.send_replace(true);
    }
    async fn drain(&self) {
        loop {
            let notified = self.drained.notified();
            // Register before checking, so a finishing dispatch cannot be missed.
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.policy.lock().unwrap().in_flight == 0 {
                return;
            }
            notified.await;
        }
    }
    fn admit(self: &Arc<Self>, headers: &HeaderMap) -> Result<Dispatch, Response> {
        let mut policy = self.policy.lock().unwrap();
        validate_request(&policy, headers, true)?;
        policy.in_flight += 1;
        Ok(Dispatch(self.clone()))
    }
}
struct Dispatch(Arc<ServerState>);
impl Drop for Dispatch {
    fn drop(&mut self) {
        let mut policy = self.0.policy.lock().unwrap();
        policy.in_flight -= 1;
        if policy.in_flight == 0 {
            self.0.drained.notify_waiters();
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    method: String,
    #[serde(default)]
    args: Value,
}

async fn handle(State(state): State<Arc<ServerState>>, request: Request) -> Response {
    let api_request = request.uri().path() == "/api/request";
    let checked = {
        let policy = state.policy.lock().unwrap();
        validate_request(&policy, request.headers(), api_request)
    };
    if let Err(response) = checked {
        return response;
    }
    let path = request.uri().path();
    if path == "/api/health" && request.method() == Method::GET && request.uri().query().is_none() {
        return json_response(
            StatusCode::OK,
            json!({"status":"ready","transport":"remote"}),
        );
    }
    if api_request {
        if request.method() != Method::POST {
            return failure(
                StatusCode::METHOD_NOT_ALLOWED,
                "Use POST for remote requests",
            );
        }
        if request.uri().query().is_some() {
            return failure(
                StatusCode::BAD_REQUEST,
                "Remote API URLs must not contain a query",
            );
        }
        let (parts, body) = request.into_parts();
        let mut stopping = state.stopping.subscribe();
        if *stopping.borrow() {
            return failure(StatusCode::SERVICE_UNAVAILABLE, "Remote access is stopping");
        }
        // Authorization and origin checks above intentionally precede even reading
        // the body. Stop cancels partial bodies, not accepted Desktop dispatches.
        let bytes = tokio::select! {
            _ = stopping.changed() => return failure(StatusCode::SERVICE_UNAVAILABLE, "Remote access is stopping"),
            bytes = to_bytes(body, BODY_LIMIT) => match bytes {
                Ok(bytes) => bytes,
                Err(_) => return failure(StatusCode::PAYLOAD_TOO_LARGE, "Remote request body could not be read within the 24 MiB limit"),
            },
        };
        let envelope = match serde_json::from_slice::<Envelope>(&bytes) {
            Ok(envelope) => envelope,
            Err(_) => return failure(StatusCode::BAD_REQUEST, "Invalid remote request JSON"),
        };
        if envelope.method.starts_with("sms_") || matches!(
            envelope.method.as_str(),
            "remote_status" | "remote_configure" | "remote_token" | "remote_rotate_token"
        ) {
            return failure(
                StatusCode::FORBIDDEN,
                "Connection administration is available only in the local desktop app",
            );
        }
        // Recheck after body reading: rotation or disable may have happened while
        // the sender was uploading. Admission and revocation share the same lock.
        let dispatch = match state.admit(&parts.headers) {
            Ok(dispatch) => dispatch,
            Err(response) => return response,
        };
        let desktop = state.desktop.clone();
        // A disconnected browser must not cancel a partially applied mutation.
        let task = tokio::spawn(async move {
            let _dispatch = dispatch;
            desktop.request(&envelope.method, envelope.args).await
        });
        return match task.await {
            Ok(Ok(data)) => json_response(StatusCode::OK, json!({"data":data})),
            Ok(Err(error)) => failure(StatusCode::BAD_REQUEST, &error.to_string()),
            Err(_) => failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Remote request dispatch failed",
            ),
        };
    }
    if path.starts_with("/api/") {
        return failure(StatusCode::NOT_FOUND, "Remote API endpoint not found");
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return failure(StatusCode::METHOD_NOT_ALLOWED, "Use GET for remote assets");
    }
    let Some(asset_path) = asset_path(path) else {
        return failure(StatusCode::BAD_REQUEST, "Invalid remote asset path");
    };
    let Some(asset) = (state.assets)(asset_path) else {
        return failure(StatusCode::NOT_FOUND, "Remote asset not found");
    };
    let length = asset.bytes.len();
    let mut response = if request.method() == Method::HEAD {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(asset.bytes))
    };
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&asset.mime_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).unwrap(),
    );
    secure(response)
}

fn validate_request(
    policy: &Policy,
    headers: &HeaderMap,
    authenticate: bool,
) -> Result<(), Response> {
    if !policy.accepting {
        return Err(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Remote access is stopping",
        ));
    }
    if !safe_host_origin(headers, policy.port, policy.tailnet_url.as_ref()) {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "Remote request Host or Origin is not allowed",
        ));
    }
    if authenticate && !authenticated(headers, &policy.token) {
        return Err(failure(
            StatusCode::UNAUTHORIZED,
            "Remote access token required or invalid",
        ));
    }
    Ok(())
}
fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}
fn authenticated(headers: &HeaderMap, token: &str) -> bool {
    one_header(headers, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|supplied| bool::from(supplied.as_bytes().ct_eq(token.as_bytes())))
}

fn tailnet_origin(value: &str) -> Result<String> {
    let url = parse_origin(value).context("Tailnet URL must be an exact HTTPS *.ts.net origin without credentials, path, query, or fragment")?;
    anyhow::ensure!(
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| { host.ends_with(".ts.net") && host.len() > ".ts.net".len() }),
        "Tailnet URL must be an exact HTTPS *.ts.net origin"
    );
    Ok(url.origin().ascii_serialization())
}
fn parse_origin(value: &str) -> Option<Url> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace() || byte == b'\\')
    {
        return None;
    }
    // Inspect the original authority/path before URL normalization can erase
    // dot segments or empty userinfo (for example /../ or https://@host).
    let (scheme, rest) = value.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let authority = match rest.split_once('/') {
        Some((authority, "")) => authority,
        Some(_) => return None,
        None => rest,
    };
    if authority.is_empty()
        || authority
            .bytes()
            .any(|byte| matches!(byte, b'@' | b'%' | b'?' | b'#'))
    {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host_str().is_none()
    {
        return None;
    }
    Some(url)
}
fn loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "[::1]") || host.eq_ignore_ascii_case("localhost")
}
fn safe_host_origin(headers: &HeaderMap, port: u16, tailnet: Option<&Url>) -> bool {
    let Some(host) = one_header(headers, "host") else {
        return false;
    };
    // Authority parsing excludes paths, schemes, whitespace and ambiguous userinfo.
    if host.contains('@') || host.contains('%') {
        return false;
    }
    let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    let has_port = if host.starts_with('[') {
        host.find(']').is_some_and(|end| end + 1 < host.len())
    } else {
        host.contains(':')
    };
    if has_port && authority.port_u16().is_none() {
        return false;
    }
    let hostname = authority.host();
    let local = loopback(hostname) && authority.port_u16().unwrap_or(80) == port;
    let external = tailnet.is_some_and(|url| {
        url.host_str()
            .is_some_and(|expected| expected.eq_ignore_ascii_case(hostname))
            && authority.port_u16().unwrap_or(443) == url.port_or_known_default().unwrap_or(443)
    });
    if !local && !external {
        return false;
    }
    if !headers.contains_key("origin") {
        return true;
    }
    let Some(origin) = one_header(headers, "origin").and_then(parse_origin) else {
        return false;
    };
    if tailnet.is_some_and(|configured| {
        configured.scheme() == origin.scheme()
            && configured.host_str() == origin.host_str()
            && configured.port_or_known_default() == origin.port_or_known_default()
    }) {
        return true;
    }
    local
        && origin.scheme() == "http"
        && origin.host_str().is_some_and(loopback)
        && origin.port_or_known_default() == Some(port)
}

fn asset_path(path: &str) -> Option<&str> {
    if path == "/" {
        return Some("index.html");
    }
    let relative = path.strip_prefix('/')?;
    // Bundled files have exact ASCII paths. Reject all percent encoding rather
    // than normalize it (including double-encoded separators/dot segments).
    if relative.is_empty()
        || relative
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.'))
        || !relative
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some(relative)
}
fn failure(status: StatusCode, message: &str) -> Response {
    json_response(status, json!({"error":message}))
}
fn json_response(status: StatusCode, data: Value) -> Response {
    secure((status, Json(data)).into_response())
}
fn secure(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    response
}

#[cfg(test)]
mod tests;
