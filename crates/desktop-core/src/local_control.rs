//! Owner-only administration of the already-running desktop controller.
//!
//! The trust boundary is the effective Unix UID: every process running as the
//! desktop owner can administer remote access. Neither JSON identity claims nor
//! network credentials grant access to this socket.
use crate::remote::RemoteAccess;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    ffi::{CString, OsStr},
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{oneshot, Mutex},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

const FRAME_LIMIT: usize = 16 * 1024;
const CONNECTION_LIMIT: usize = 32;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const APP_GUIDANCE: &str =
    "Launch AMP Desktop first, and use the same user and data directory as the running desktop";
const OUTCOME_GUIDANCE: &str = "The command may already have changed remote access. No command was automatically retried; inspect remote status before deciding whether to retry";

/// The GUI and owner CLI must use this same application identifier and override.
/// Resolving this path never creates directories (in particular, CLI help is inert).
pub fn data_directory(application_identifier: &str) -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("OMP_DESKTOP_DATA_DIR") {
        return Ok(PathBuf::from(directory));
    }
    Ok(dirs::data_dir()
        .context("Could not locate the application data directory")?
        .join(application_identifier))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    method: String,
    args: Value,
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum Reply {
    Data { data: Value },
    Error { error: String },
}

fn permitted(method: &str) -> bool {
    matches!(
        method,
        "remote_status" | "remote_configure" | "remote_token" | "remote_rotate_token"
    )
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory-safety preconditions.
    unsafe { libc::geteuid() }
}

fn verify_peer(stream: &UnixStream) -> Result<()> {
    let uid = stream
        .peer_cred()
        .context("Could not verify owner socket peer credentials")?
        .uid();
    verify_uid(uid)
}

fn verify_uid(uid: u32) -> Result<()> {
    anyhow::ensure!(
        uid == effective_uid(),
        "Owner socket peer belongs to a different user"
    );
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

struct Entry {
    identity: Identity,
    uid: u32,
    mode: u32,
}

impl Entry {
    fn is_socket(&self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFSOCK as u32
    }
}

struct Endpoint {
    directory: File,
    socket: PathBuf,
    socket_name: CString,
    lock_name: CString,
}

impl Endpoint {
    fn locate(data_directory: &Path, create: bool) -> Result<Self> {
        #[cfg(target_os = "macos")]
        let temporary = Path::new("/private/tmp");
        #[cfg(not(target_os = "macos"))]
        let temporary = Path::new("/tmp");
        let owner = temporary.join(format!("omp-desktop-owner-{}", effective_uid()));
        Self::at(data_directory, &owner, create)
    }

    fn at(data_directory: &Path, owner: &Path, create: bool) -> Result<Self> {
        // Canonicalization also unifies macOS /var and /private/var aliases. The
        // client deliberately does not create a missing app-data directory.
        let canonical = fs::canonicalize(data_directory).with_context(|| {
            format!("Could not resolve the AMP Desktop data directory. {APP_GUIDANCE}")
        })?;
        anyhow::ensure!(
            canonical.is_dir(),
            "AMP Desktop data directory is not a directory. {APP_GUIDANCE}"
        );
        let digest = Sha256::digest(canonical.as_os_str().as_bytes());
        let mut key = String::with_capacity(32);
        use std::fmt::Write;
        for byte in &digest[..16] {
            write!(&mut key, "{byte:02x}").expect("writing to a String cannot fail");
        }
        if create {
            match fs::DirBuilder::new().mode(0o700).create(owner) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).context("Could not create the owner socket directory")
                }
            }
        }
        // O_NOFOLLOW rejects a pre-created directory symlink, including on the
        // read-only client path. Never repair a pre-existing unsafe directory.
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(owner)
            .with_context(|| {
                format!("Could not open the protected owner socket directory. {APP_GUIDANCE}")
            })?;
        let metadata = directory.metadata()?;
        anyhow::ensure!(
            metadata.is_dir()
                && metadata.uid() == effective_uid()
                && metadata.mode() & 0o7777 == 0o700,
            "Unsafe owner socket directory: expected an owner-owned directory with mode 0700"
        );
        let socket_name = CString::new(format!("{key}.sock"))?;
        let lock_name = CString::new(format!("{key}.lock"))?;
        let socket = owner.join(OsStr::from_bytes(socket_name.as_bytes()));
        // Leave room for the NUL byte in Darwin's 104-byte sockaddr_un.sun_path.
        anyhow::ensure!(
            socket.as_os_str().as_bytes().len() < 104,
            "Owner socket path is too long"
        );
        Ok(Self {
            directory,
            socket,
            socket_name,
            lock_name,
        })
    }

    fn open_lock(&self, create: bool) -> Result<File> {
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if create {
                libc::O_RDWR | libc::O_CREAT
            } else {
                libc::O_RDONLY
            };
        // SAFETY: directory and the generated NUL-terminated basename are valid;
        // mode is used only for O_CREAT. The resulting descriptor is uniquely owned.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                self.lock_name.as_ptr(),
                flags,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error())
                .context("Could not safely open the owner socket lock");
        }
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        anyhow::ensure!(metadata.is_file() && metadata.uid() == effective_uid()
            && metadata.mode() & 0o7777 == 0o600 && metadata.nlink() == 1,
            "Unsafe owner socket lock: expected an owner-owned regular file with mode 0600 and one link");
        let entry = self
            .metadata(&self.lock_name)?
            .context("Owner socket lock disappeared")?;
        anyhow::ensure!(
            Identity::of(&metadata) == entry.identity,
            "Owner socket lock changed while opening it"
        );
        Ok(file)
    }

    fn metadata(&self, name: &CString) -> Result<Option<Entry>> {
        // Opening /dev/fd paths would follow symlinks. fstatat instead inspects
        // the basename relative to the already verified directory descriptor.
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: stat points to writable storage; name and directory are valid.
        let result = unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error).context("Could not inspect the owner socket entry");
        }
        // SAFETY: successful fstatat initialized the structure.
        let stat = unsafe { stat.assume_init() };
        Ok(Some(Entry {
            identity: Identity {
                device: stat.st_dev as u64,
                inode: stat.st_ino as u64,
            },
            uid: stat.st_uid,
            mode: stat.st_mode as u32,
        }))
    }

    fn socket_metadata(&self) -> Result<Option<Entry>> {
        let metadata = self.metadata(&self.socket_name)?;
        if let Some(metadata) = &metadata {
            anyhow::ensure!(
                metadata.is_socket()
                    && metadata.uid == effective_uid()
                    && metadata.mode & 0o7777 == 0o600,
                "Unsafe owner socket: expected an owner-owned Unix socket with mode 0600"
            );
        }
        Ok(metadata)
    }

    fn unlink_socket(&self, identity: Identity) {
        // Never unlink a replacement file or an endpoint whose identity we did
        // not record while owning the exclusive lifetime lock.
        if let Ok(Some(metadata)) = self.metadata(&self.socket_name) {
            if metadata.is_socket()
                && metadata.uid == effective_uid()
                && identity == metadata.identity
            {
                // SAFETY: the descriptor and generated basename are valid. unlinkat
                // removes only the entry itself and never follows a final symlink.
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), self.socket_name.as_ptr(), 0);
                }
            }
        }
    }
}

struct Lease {
    endpoint: Endpoint,
    // The lock file is intentionally retained on disk: deleting it would allow
    // two processes to flock different inodes under the same generated name.
    _lock: File,
    socket: Option<Identity>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(identity) = self.socket {
            self.endpoint.unlink_socket(identity);
        }
        // _lock drops only after cleanup, preserving exclusive unlink ownership.
    }
}

struct Running {
    task: JoinHandle<()>,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        // Drop cannot await. Cancelling the supervisor also drops its JoinSet,
        // which aborts every transport/administration future, not blocking IO.
        self.task.abort();
    }
}

pub struct LocalControl {
    running: Mutex<Option<Running>>,
    stopped: Arc<AtomicBool>,
}

impl LocalControl {
    pub async fn start(
        directory: PathBuf,
        remote: Arc<RemoteAccess>,
        closing: Arc<AtomicBool>,
    ) -> Result<Arc<Self>> {
        anyhow::ensure!(
            Arc::ptr_eq(&closing, &remote.closing_flag()),
            "Owner socket must share the desktop controller's closing flag"
        );
        let endpoint = Endpoint::locate(&directory, true)?;
        let lock = endpoint.open_lock(true)?;
        // SAFETY: flock operates on the live file descriptor and has no pointer arguments.
        let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            return Err(io::Error::last_os_error())
                .context("Another AMP Desktop already owns this data directory's owner socket");
        }
        let mut lease = Lease {
            endpoint,
            _lock: lock,
            socket: None,
        };
        if let Some(stale) = lease.endpoint.socket_metadata()? {
            // A live owner cannot hold this same lock. Only now is stale cleanup safe.
            lease.endpoint.unlink_socket(stale.identity);
        }
        let listener = UnixListener::bind(&lease.endpoint.socket)
            .context("Could not bind the owner socket")?;
        let metadata = lease
            .endpoint
            .metadata(&lease.endpoint.socket_name)?
            .context("New owner socket disappeared")?;
        anyhow::ensure!(
            metadata.is_socket() && metadata.uid == effective_uid(),
            "New owner socket has unsafe ownership"
        );
        lease.socket = Some(metadata.identity);
        // The enclosing 0700 directory protects the short interval before chmod.
        // AT_SYMLINK_NOFOLLOW prevents following a replaced final component.
        // SAFETY: the verified directory descriptor and generated basename are
        // valid; the no-follow flag excludes a replaced final symlink.
        let chmod = unsafe {
            libc::fchmodat(
                lease.endpoint.directory.as_raw_fd(),
                lease.endpoint.socket_name.as_ptr(),
                0o600,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if chmod != 0 {
            return Err(io::Error::last_os_error())
                .context("Could not protect the owner socket with mode 0600");
        }
        lease.endpoint.socket_metadata()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_task = stopped.clone();
        let (stop, stop_rx) = oneshot::channel();
        let task = tokio::spawn(serve(
            listener,
            lease,
            remote,
            closing,
            stopped_task,
            stop_rx,
        ));
        Ok(Arc::new(Self {
            running: Mutex::new(Some(Running {
                task,
                stop: Some(stop),
            })),
            stopped,
        }))
    }

    /// Stops local transport only. It never waits for a Keychain permission prompt
    /// or changes remote enablement, and can be called more than once.
    pub async fn shutdown(&self) {
        let mut running = self.running.lock().await;
        self.stopped.store(true, Ordering::Release);
        if let Some(mut active) = running.take() {
            if let Some(stop) = active.stop.take() {
                let _ = stop.send(());
            }
            let _ = (&mut active.task).await;
        }
    }
}

impl Drop for LocalControl {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // Runtime cancellation drops the listener/lease and all child resources.
        drop(self.running.get_mut().take());
    }
}

async fn serve(
    listener: UnixListener,
    _lease: Lease,
    remote: Arc<RemoteAccess>,
    closing: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break; };
                if stopped.load(Ordering::Acquire) || connections.len() >= CONNECTION_LIMIT || verify_peer(&stream).is_err() {
                    continue;
                }
                connections.spawn(connection(stream, remote.clone(), closing.clone(), stopped.clone()));
            }
        }
    }
    drop(listener);
    connections.abort_all();
    // Aborting an async credential waiter does not join its blocking Keychain IO.
    while connections.join_next().await.is_some() {}
    drop(_lease);
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let count = stream
            .read(&mut chunk)
            .await
            .context("Could not read owner socket frame")?;
        anyhow::ensure!(
            count != 0,
            "Incomplete owner socket frame: expected a JSON line"
        );
        anyhow::ensure!(
            frame.len() + count <= FRAME_LIMIT,
            "Owner socket frame is too large"
        );
        if let Some(newline) = chunk[..count].iter().position(|byte| *byte == b'\n') {
            anyhow::ensure!(
                newline + 1 == count,
                "Expected exactly one owner socket JSON line"
            );
            frame.extend_from_slice(&chunk[..newline]);
            return Ok(frame);
        }
        frame.extend_from_slice(&chunk[..count]);
    }
}

async fn connection(
    mut stream: UnixStream,
    remote: Arc<RemoteAccess>,
    closing: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
) {
    let result = async {
        let frame = timeout(FRAME_TIMEOUT, read_frame(&mut stream))
            .await
            .map_err(|_| anyhow!("Timed out waiting for an owner socket JSON line"))??;
        // Parser errors are deliberately not echoed: a malformed input may contain
        // secrets, and no diagnostics need any portion of the received payload.
        let command: Command = serde_json::from_slice(&frame)
            .map_err(|_| anyhow!("Invalid owner socket request envelope"))?;
        anyhow::ensure!(
            permitted(&command.method),
            "Unknown local remote access method"
        );
        // This is the dispatch gate, not merely an accept-time check: a client
        // waiting to finish its frame cannot undo quit's remote suspension.
        anyhow::ensure!(
            !closing.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire),
            "AMP Desktop is closing; owner administration is temporarily unavailable"
        );
        // Do not select on EOF/disconnection here: accepted mutations must finish
        // even if the owner CLI exits before reading their response.
        remote.request(&command.method, command.args).await
    }
    .await;
    let response = match result {
        Ok(data) => Reply::Data { data },
        Err(error) => Reply::Error {
            error: error.to_string(),
        },
    };
    if let Ok(mut bytes) = serde_json::to_vec(&response) {
        if bytes.len() >= FRAME_LIMIT {
            bytes.clear();
            bytes.extend_from_slice(b"{\"error\":\"Owner socket response is too large\"}");
        }
        bytes.push(b'\n');
        let _ = timeout(FRAME_TIMEOUT, stream.write_all(&bytes)).await;
    }
}

/// Sends one explicitly requested owner command to an existing desktop. Never
/// launches an application or retries a command whose outcome might be unknown.
pub async fn request(directory: &Path, method: &str, args: Value) -> Result<Value> {
    anyhow::ensure!(permitted(method), "Unknown local remote access method");
    #[derive(serde::Serialize)]
    struct Outgoing<'a> {
        method: &'a str,
        args: Value,
    }
    let mut bytes = serde_json::to_vec(&Outgoing { method, args })
        .context("Could not encode the owner command")?;
    anyhow::ensure!(
        bytes.len() < FRAME_LIMIT,
        "Owner socket request is too large"
    );
    bytes.push(b'\n');
    let endpoint = Endpoint::locate(directory, false)?;
    endpoint
        .open_lock(false)
        .with_context(|| format!("Owner socket is not available. {APP_GUIDANCE}"))?;
    anyhow::ensure!(
        endpoint.socket_metadata()?.is_some(),
        "Owner socket is not available. {APP_GUIDANCE}"
    );
    let mut stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(&endpoint.socket))
        .await
        .with_context(|| format!("Timed out connecting to the owner socket. {APP_GUIDANCE}"))?
        .with_context(|| format!("Could not connect to the owner socket. {APP_GUIDANCE}"))?;
    verify_peer(&stream)?;
    timeout(FRAME_TIMEOUT, stream.write_all(&bytes))
        .await
        .map_err(|_| anyhow!("Timed out sending the owner command. {OUTCOME_GUIDANCE}"))?
        .with_context(|| format!("Could not send the owner command. {OUTCOME_GUIDANCE}"))?;
    let frame = timeout(RESPONSE_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| anyhow!("Timed out waiting for the owner command. {OUTCOME_GUIDANCE}"))?
        .with_context(|| format!("Could not read the owner command result. {OUTCOME_GUIDANCE}"))?;
    let response: Value = serde_json::from_slice(&frame)
        .map_err(|_| anyhow!("Invalid owner socket response. {OUTCOME_GUIDANCE}"))?;
    let Value::Object(mut object) = response else {
        bail!("Invalid owner socket response. {OUTCOME_GUIDANCE}");
    };
    if object.len() == 1 {
        if let Some(data) = object.remove("data") {
            return Ok(data);
        }
        if let Some(error) = object.get("error").and_then(Value::as_str) {
            bail!("{error}. No command was automatically retried; inspect remote status before retrying");
        }
    }
    bail!("Invalid owner socket response. {OUTCOME_GUIDANCE}")
}

#[cfg(test)]
mod tests;
