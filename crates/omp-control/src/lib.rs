//! Direct, policy-preserving control of the user's installed OMP executable.
mod discovery;
pub mod frame;
pub use discovery::{discover, Installation};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{broadcast, mpsc, oneshot, watch},
    time::timeout,
};
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
pub struct LaunchOptions {
    pub cwd: PathBuf,
    pub session: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub instructions: Option<String>,
    pub profile: Option<String>,
    pub extra_args: Vec<String>,
}

#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<Inner>,
}
struct Pending {
    command: String,
    result: oneshot::Sender<Result<Value>>,
}
struct Inner {
    pid: u32,
    alive: AtomicBool,
    closing: AtomicBool,
    protocol_v2: AtomicBool,
    pending: Mutex<HashMap<String, Pending>>,
    events: broadcast::Sender<Value>,
    writer: mpsc::Sender<Write>,
    signals: mpsc::UnboundedSender<i32>,
    exit: watch::Receiver<bool>,
    shutdown_lock: tokio::sync::Mutex<()>,
}
enum Write {
    Frame(Vec<u8>, oneshot::Sender<Result<()>>),
    Close(oneshot::Sender<()>),
}

impl Inner {
    fn fail_pending(&self, message: &str) {
        let requests = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, request) in requests {
            let _ = request
                .result
                .send(Err(anyhow::anyhow!(message.to_owned())));
        }
    }
    fn transport_failure(&self, message: &str) {
        self.closing.store(true, Ordering::Release);
        self.fail_pending(message);
        let _ = self
            .events
            .send(json!({"type":"transport_error","message":message}));
        let _ = self.signals.send(libc::SIGKILL);
    }
    fn route(&self, frame: Value) {
        if frame.get("type").and_then(Value::as_str) != Some("response") {
            let _ = self.events.send(frame);
            return;
        }
        let request = frame
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| self.pending.lock().unwrap().remove(id));
        if let Some(request) = request {
            let result =
                if frame.get("command").and_then(Value::as_str) != Some(request.command.as_str()) {
                    Err(anyhow::anyhow!(
                        "OMP response command does not match request"
                    ))
                } else if frame.get("success").and_then(Value::as_bool) == Some(true) {
                    Ok(frame.get("data").cloned().unwrap_or(Value::Null))
                } else {
                    let error = frame
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("OMP command failed");
                    Err(anyhow::anyhow!(error.to_owned()))
                };
            let _ = request.result.send(result);
        } else if frame.get("success").and_then(Value::as_bool) == Some(false) {
            // OMP may emit a late same-id error after prompt admission. Preserve it
            // for the event consumer rather than silently losing the failure.
            let _ = self.events.send(frame);
        }
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        if self.alive.load(Ordering::Acquire) {
            signal_group(self.pid, libc::SIGKILL);
        }
    }
}

// A request future can be cancelled at any await point without leaking its sender.
struct PendingGuard {
    inner: Weak<Inner>,
    id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.pending.lock().unwrap().remove(&self.id);
        }
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) {
    // Child is its own process-group leader; never signal the desktop's group.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}
#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: i32) {}

fn fresh_session_dir() -> Result<PathBuf> {
    let root = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME").context("HOME is required for OMP session storage")?)
            .join("Library/Application Support/omp-desktop")
    } else if let Some(root) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(root).join("omp-desktop")
    } else {
        PathBuf::from(std::env::var_os("HOME").context("HOME is required for OMP session storage")?)
            .join(".local/share/omp-desktop")
    };
    Ok(root.join("omp-sessions").join(Uuid::new_v4().to_string()))
}

impl RpcClient {
    pub async fn spawn(installation: &Installation, options: LaunchOptions) -> Result<Self> {
        let cwd = if options.cwd.as_os_str().is_empty() {
            std::env::current_dir()?
        } else {
            options.cwd.clone()
        };
        let cwd = tokio::fs::canonicalize(cwd)
            .await
            .context("OMP workspace is unavailable")?;
        if !tokio::fs::metadata(&cwd).await?.is_dir() {
            bail!("OMP workspace must be a directory");
        }
        let mut command = Command::new(&installation.path);
        command
            .args(["--mode", "rpc", "--cwd"])
            .arg(&cwd)
            .current_dir(&cwd)
            .env("PATH", &installation.environment_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A desktop launched from an agent tool must not inherit that tool's
        // session identity or privileged local tool bridge.
        for key in [
            "PI_SESSION_FILE",
            "PI_SESSION_ID",
            "PI_ARTIFACTS_DIR",
            "PI_TOOL_BRIDGE_URL",
            "PI_TOOL_BRIDGE_TOKEN",
            "PI_TOOL_BRIDGE_SESSION",
            "PI_EVAL_LOCAL_ROOTS",
            "PI_CODING_AGENT_SESSION_DIR",
        ] {
            command.env_remove(key);
        }
        #[cfg(unix)]
        command.process_group(0);
        if let Some(session) = &options.session {
            command.arg("--resume").arg(session);
        } else if !options
            .extra_args
            .iter()
            .any(|arg| arg == "--session-dir" || arg.starts_with("--session-dir="))
        {
            // Explicit session-dir bypasses OMP autoResume. Never use --continue or
            // overwrite global settings; engine-owned files survive desktop deletion.
            let directory = fresh_session_dir()?;
            tokio::fs::create_dir_all(&directory)
                .await
                .context("Cannot create isolated OMP session directory")?;
            command.arg("--session-dir").arg(directory);
        }
        for (flag, value) in [
            ("--model", &options.model),
            ("--thinking", &options.thinking),
            ("--append-system-prompt", &options.instructions),
            ("--profile", &options.profile),
        ] {
            if let Some(value) = value {
                command.arg(flag).arg(value);
            }
        }
        command.args(&options.extra_args);
        let mut child = command.spawn().context("Cannot start installed OMP")?;
        let pid = child.id().context("OMP did not provide a process id")?;
        let stdin = child.stdin.take().context("OMP stdin is unavailable")?;
        let stdout = child.stdout.take().context("OMP stdout is unavailable")?;
        let mut stderr = child.stderr.take().context("OMP stderr is unavailable")?;
        let (events, _) = broadcast::channel(2048);
        let (writer, mut writes) = mpsc::channel(64);
        let (signals, mut signal_requests) = mpsc::unbounded_channel();
        let (exit_sender, exit) = watch::channel(false);
        let (ready_sender, ready) = oneshot::channel();
        let inner = Arc::new(Inner {
            pid,
            alive: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            protocol_v2: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            events,
            writer,
            signals,
            exit,
            shutdown_lock: tokio::sync::Mutex::new(()),
        });

        // Only bounded scratch bytes are retained; stderr is never echoed into UI
        // errors because extension/provider output can contain credentials.
        tokio::spawn(async move {
            let mut buffer = [0u8; 8192];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(write) = writes.recv().await {
                match write {
                    Write::Frame(bytes, result) => {
                        let outcome = timeout(Duration::from_secs(15), async {
                            stdin.write_all(&bytes).await?;
                            stdin.flush().await
                        })
                        .await;
                        let result_value = match outcome {
                            Ok(Ok(())) => Ok(()),
                            _ => Err(anyhow::anyhow!("Cannot write OMP RPC input")),
                        };
                        let failed = result_value.is_err();
                        let _ = result.send(result_value);
                        if failed {
                            if let Some(inner) = weak.upgrade() {
                                inner.transport_failure("Cannot write OMP RPC input");
                            }
                            break;
                        }
                    }
                    Write::Close(result) => {
                        let _ = stdin.shutdown().await;
                        let _ = result.send(());
                        break;
                    }
                }
            }
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            let mut decoder = frame::FrameDecoder::default();
            let mut ready_sender = Some(ready_sender);
            let outcome: Result<()> = async {
                while let Some(frame) = frame::read_json_line(&mut reader, &mut line).await? {
                    if frame.get("type").and_then(Value::as_str) == Some("ready") {
                        if let Some(sender) = ready_sender.take() {
                            let _ = sender.send(frame);
                            continue;
                        }
                        bail!("OMP sent duplicate ready frame");
                    }
                    let Some(inner) = weak.upgrade() else {
                        return Ok(());
                    };
                    if frame.get("type").and_then(Value::as_str) == Some("rpc_chunk")
                        && !inner.protocol_v2.load(Ordering::Acquire)
                    {
                        bail!("OMP sent chunks before protocol negotiation");
                    }
                    if let Some(frame) = decoder.push(frame)? {
                        inner.route(frame);
                    }
                }
                decoder.finish()?;
                Ok(())
            }
            .await;
            if let Some(inner) = weak.upgrade() {
                if let Err(error) = outcome {
                    inner.transport_failure(&error.to_string());
                } else if !inner.closing.load(Ordering::Acquire)
                    && inner.alive.load(Ordering::Acquire)
                {
                    inner.transport_failure("OMP output stream closed unexpectedly");
                }
            }
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            let status = loop {
                tokio::select! {
                    status = child.wait() => break status,
                    signal = signal_requests.recv() => {
                        match signal {
                            Some(signal) => { signal_group(pid, signal); #[cfg(not(unix))] { let _ = child.start_kill(); } }
                            None => { let _ = child.start_kill(); break child.wait().await; }
                        }
                    }
                }
            };
            signal_group(pid, libc::SIGKILL);
            if let Some(inner) = weak.upgrade() {
                inner.alive.store(false, Ordering::Release);
                inner.fail_pending("OMP process exited");
                let code = status.ok().and_then(|status| status.code());
                let _ = inner
                    .events
                    .send(json!({"type":"process_exit","code":code}));
            }
            let _ = exit_sender.send(true);
        });
        let client = Self { inner };
        let startup = async {
            let ready = timeout(Duration::from_secs(45), ready)
                .await
                .context("Timed out waiting for OMP ready")?
                .context("OMP exited before ready")?;
            let versions = ready
                .get("supportedProtocolVersions")
                .and_then(Value::as_array);
            if !versions
                .is_some_and(|versions| versions.iter().any(|version| version.as_u64() == Some(2)))
                || ready.get("maxFrameBytes").and_then(Value::as_u64)
                    != Some(frame::MAX_FRAME_BYTES as u64)
                || ready
                    .get("maxReassembledFrameBytes")
                    .and_then(Value::as_u64)
                    != Some(frame::MAX_REASSEMBLED_BYTES as u64)
            {
                bail!("Installed OMP does not support compatible JSONL protocol v2; update OMP");
            }
            client.inner.protocol_v2.store(true, Ordering::Release);
            let negotiated = client
                .request(json!({"type":"negotiate_protocol","protocolVersion":2}))
                .await?;
            if negotiated.get("protocolVersion").and_then(Value::as_u64) != Some(2) {
                bail!("OMP protocol v2 negotiation failed");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = startup {
            let _ = client.shutdown().await;
            return Err(error);
        }
        Ok(client)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.inner.events.subscribe()
    }
    pub fn pid(&self) -> Option<u32> {
        self.inner
            .alive
            .load(Ordering::Acquire)
            .then_some(self.inner.pid)
    }

    pub async fn request(&self, command: Value) -> Result<Value> {
        self.request_inner(command, false).await
    }
    async fn request_inner(&self, mut command: Value, during_shutdown: bool) -> Result<Value> {
        if !self.inner.alive.load(Ordering::Acquire)
            || (!during_shutdown && self.inner.closing.load(Ordering::Acquire))
        {
            bail!("OMP transport is closed");
        }
        let object = command
            .as_object_mut()
            .context("OMP command must be an object")?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .context("OMP command requires a type")?
            .to_owned();
        let id = Uuid::new_v4().to_string();
        object.insert("id".to_owned(), Value::String(id.clone()));
        let bytes = encode_command(&command)?;
        let (sender, result) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(
            id.clone(),
            Pending {
                command: kind,
                result: sender,
            },
        );
        let _guard = PendingGuard {
            inner: Arc::downgrade(&self.inner),
            id,
        };
        // Exit can race insertion, so recheck after registering the waiter.
        if !self.inner.alive.load(Ordering::Acquire) {
            bail!("OMP process exited");
        }
        timeout(Duration::from_secs(120), async {
            self.write(bytes).await?;
            result.await.context("OMP response channel closed")?
        })
        .await
        .context("OMP request timed out; the command was not replayed")?
    }

    pub async fn respond(&self, response: Value) -> Result<()> {
        if response.get("type").and_then(Value::as_str) != Some("extension_ui_response")
            || response.get("id").and_then(Value::as_str).is_none()
        {
            bail!("Expected extension_ui_response with an id");
        }
        if self.inner.closing.load(Ordering::Acquire) {
            bail!("OMP transport is closed");
        }
        self.write(encode_command(&response)?).await
    }
    async fn write(&self, bytes: Vec<u8>) -> Result<()> {
        timeout(Duration::from_secs(30), async {
            let (sender, result) = oneshot::channel();
            self.inner
                .writer
                .send(Write::Frame(bytes, sender))
                .await
                .context("OMP input is closed")?;
            result.await.context("OMP writer stopped")?
        })
        .await
        .context("OMP input write timed out; the command was not replayed")?
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _lock = self.inner.shutdown_lock.lock().await;
        if *self.inner.exit.borrow() {
            return Ok(());
        }
        self.inner.closing.store(true, Ordering::Release);
        // Abort is a side-channel command in OMP and releases tool/dialog waits.
        let _ = timeout(
            Duration::from_secs(2),
            self.request_inner(json!({"type":"abort"}), true),
        )
        .await;
        let _ = timeout(Duration::from_secs(2), async {
            let (sender, result) = oneshot::channel();
            self.inner.writer.send(Write::Close(sender)).await?;
            result.await.context("OMP writer already closed")?;
            Ok::<_, anyhow::Error>(())
        })
        .await;
        if self.wait_exit(Duration::from_secs(3)).await {
            return Ok(());
        }
        let _ = self.inner.signals.send(libc::SIGTERM);
        if self.wait_exit(Duration::from_secs(2)).await {
            return Ok(());
        }
        let _ = self.inner.signals.send(libc::SIGKILL);
        if !self.wait_exit(Duration::from_secs(3)).await {
            bail!("OMP process did not exit after process-group termination");
        }
        Ok(())
    }
    async fn wait_exit(&self, duration: Duration) -> bool {
        let mut exit = self.inner.exit.clone();
        if *exit.borrow() {
            return true;
        }
        timeout(duration, async {
            while !*exit.borrow_and_update() {
                if exit.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok()
    }
}

fn encode_command(command: &Value) -> Result<Vec<u8>> {
    // The server consumes full JSONL commands (not rpc_chunk input); allow image
    // attachments up to the logical-frame ceiling, while bounding host allocation.
    let mut bytes = serde_json::to_vec(command).context("Cannot serialize OMP command")?;
    if bytes.len() >= frame::MAX_REASSEMBLED_BYTES {
        bail!("OMP command exceeds input limit");
    }
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inner() -> Arc<Inner> {
        let (events, _) = broadcast::channel(16);
        let (writer, _) = mpsc::channel(1);
        let (signals, _) = mpsc::unbounded_channel();
        let (_, exit) = watch::channel(true);
        Arc::new(Inner {
            pid: 0,
            alive: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            protocol_v2: AtomicBool::new(true),
            pending: Mutex::new(HashMap::new()),
            events,
            writer,
            signals,
            exit,
            shutdown_lock: tokio::sync::Mutex::new(()),
        })
    }
    #[tokio::test]
    async fn correlates_out_of_order_responses_and_rejects_wrong_command() {
        let inner = inner();
        let (a, ar) = oneshot::channel();
        let (b, br) = oneshot::channel();
        inner.pending.lock().unwrap().insert(
            "a".into(),
            Pending {
                command: "get_state".into(),
                result: a,
            },
        );
        inner.pending.lock().unwrap().insert(
            "b".into(),
            Pending {
                command: "get_messages".into(),
                result: b,
            },
        );
        inner.route(json!({"type":"response","id":"b","command":"get_messages","success":true,"data":{"messages":[]}}));
        inner.route(json!({"type":"response","id":"a","command":"wrong","success":true}));
        assert!(ar.await.unwrap().is_err());
        assert_eq!(br.await.unwrap().unwrap(), json!({"messages":[]}));
        assert!(inner.pending.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn cancelled_waiter_is_removed_and_exit_releases_other_waiters() {
        let inner = inner();
        let (sender, _receiver) = oneshot::channel();
        inner.pending.lock().unwrap().insert(
            "cancelled".into(),
            Pending {
                command: "prompt".into(),
                result: sender,
            },
        );
        drop(PendingGuard {
            inner: Arc::downgrade(&inner),
            id: "cancelled".into(),
        });
        assert!(inner.pending.lock().unwrap().is_empty());
        let (sender, receiver) = oneshot::channel();
        inner.pending.lock().unwrap().insert(
            "exit".into(),
            Pending {
                command: "prompt".into(),
                result: sender,
            },
        );
        inner.fail_pending("OMP process exited");
        assert!(receiver.await.unwrap().is_err());
    }
    #[tokio::test]
    async fn emits_events_and_delayed_prompt_errors_without_confusing_acceptance() {
        let inner = inner();
        let mut events = inner.events.subscribe();
        inner.route(json!({"type":"message_update","messageId":"stable"}));
        inner.route(json!({"type":"response","id":"late","command":"prompt","success":false,"error":"late failure"}));
        assert_eq!(events.recv().await.unwrap()["type"], "message_update");
        assert_eq!(events.recv().await.unwrap()["id"], "late");
    }
}
