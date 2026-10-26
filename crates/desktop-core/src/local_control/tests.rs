use super::*;
use crate::{remote::Credentials, Desktop};
use serde_json::json;
use std::{
    collections::BTreeMap,
    os::unix::fs::{symlink, PermissionsExt},
    sync::{Condvar, Mutex as StdMutex},
};
use tokio::sync::{Mutex as AsyncMutex, Notify};

#[derive(Default)]
struct MemoryCredentials {
    entries: StdMutex<BTreeMap<String, String>>,
    next_gate: StdMutex<Option<Arc<Gate>>>,
}

impl MemoryCredentials {
    fn before_io(&self) {
        let gate = self.next_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.store(true, Ordering::Release);
            gate.notification.notify_one();
            let mut released = gate.released.lock().unwrap();
            while !*released {
                released = gate.wake.wait(released).unwrap();
            }
        }
    }

    fn block_next(&self) -> Release {
        let gate = Arc::new(Gate::default());
        *self.next_gate.lock().unwrap() = Some(gate.clone());
        Release(gate)
    }
}

impl Credentials for MemoryCredentials {
    fn read(&self, instance: &str) -> Result<Option<String>> {
        self.before_io();
        Ok(self.entries.lock().unwrap().get(instance).cloned())
    }

    fn write(&self, instance: &str, token: &str) -> Result<()> {
        self.before_io();
        self.entries
            .lock()
            .unwrap()
            .insert(instance.into(), token.into());
        Ok(())
    }
}

#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    notification: Notify,
    released: StdMutex<bool>,
    wake: Condvar,
}

// Always release blocking credential IO, including if a test assertion panics.
struct Release(Arc<Gate>);
impl Release {
    async fn entered(&self) {
        timeout(Duration::from_secs(2), async {
            loop {
                let notification = self.0.notification.notified();
                if self.0.entered.load(Ordering::Acquire) {
                    return;
                }
                notification.await;
            }
        })
        .await
        .expect("credential operation was not dispatched");
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    remote: Arc<RemoteAccess>,
    credentials: Arc<MemoryCredentials>,
    closing: Arc<AtomicBool>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        // Desktop::open probes provider Keychain entries. Construct inert state
        // instead; all credential operations here use only the injected store.
        let desktop = Arc::new(Desktop {
            directory: directory.path().to_path_buf(),
            state: StdMutex::new(crate::State {
                engine: crate::EngineView {
                    status: "missing".into(),
                    path: None,
                    version: None,
                    error: None,
                    configured_path: None,
                },
                providers: vec![],
                model_discovery_errors: BTreeMap::new(),
                bots: BTreeMap::new(),
            }),
            persistence: AsyncMutex::new(()),
            settings: AsyncMutex::new(()),
            native_auth: native_agent::oauth::OAuthManager::default(),
            provider_view: crate::offline_provider_view,
        });
        let credentials = Arc::new(MemoryCredentials::default());
        let remote = RemoteAccess::open_with_credentials(
            desktop,
            directory.path().to_path_buf(),
            Arc::new(|_| None),
            credentials.clone(),
        )
        .await
        .unwrap();
        let closing = remote.closing_flag();
        Self {
            directory,
            remote,
            credentials,
            closing,
        }
    }

    async fn start(&self) -> Arc<LocalControl> {
        LocalControl::start(
            self.directory.path().to_path_buf(),
            self.remote.clone(),
            self.closing.clone(),
        )
        .await
        .unwrap()
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::locate(self.directory.path(), true).unwrap()
    }

    async fn call(&self, method: &str, args: Value) -> Value {
        request(self.directory.path(), method, args).await.unwrap()
    }

    async fn finish(&self, server: Arc<LocalControl>) {
        self.remote.shutdown().await.unwrap();
        server.shutdown().await;
        self.remove_lock();
    }

    fn remove_lock(&self) {
        let endpoint = self.endpoint();
        // Tests own unique canonical data directories; after server shutdown no
        // process can use these generated locks. Production never removes locks.
        fs::remove_file(
            endpoint
                .socket
                .with_file_name(OsStr::from_bytes(endpoint.lock_name.as_bytes())),
        )
        .unwrap();
    }

    async fn wire(&self, bytes: &[u8], finish_write: bool) -> Value {
        let endpoint = self.endpoint();
        let mut stream = UnixStream::connect(&endpoint.socket).await.unwrap();
        let _ = stream.write_all(bytes).await;
        if finish_write {
            stream.shutdown().await.unwrap();
        }
        let frame = timeout(Duration::from_secs(2), read_frame(&mut stream))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_slice(&frame).unwrap()
    }
}

#[tokio::test]
async fn socket_uses_same_controller_and_keeps_secrets_out_of_status_and_settings() {
    let fixture = Fixture::new().await;
    let server = fixture.start().await;
    let status = fixture
        .call(
            "remote_configure",
            json!({"enabled":false,"port":23456,"tailnetUrl":"https://owner.example.ts.net"}),
        )
        .await;
    assert_eq!(status["port"], 23456);
    assert_eq!(
        fixture
            .remote
            .request("remote_status", Value::Null)
            .await
            .unwrap(),
        status
    );
    let first = fixture.call("remote_token", Value::Null).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        fixture
            .remote
            .request("remote_token", Value::Null)
            .await
            .unwrap()["token"],
        first
    );
    let second = fixture.call("remote_rotate_token", Value::Null).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first, second);
    assert_eq!(
        fixture
            .remote
            .request("remote_token", Value::Null)
            .await
            .unwrap()["token"],
        second
    );
    let status = fixture.call("remote_status", Value::Null).await.to_string();
    let settings = fs::read_to_string(fixture.directory.path().join("remote.json")).unwrap();
    for token in [&first, &second] {
        assert!(!status.contains(token));
        assert!(!settings.contains(token));
    }
    fixture.finish(server).await;
}

#[tokio::test]
async fn owner_permissions_and_kernel_peer_credentials_are_enforced() {
    let fixture = Fixture::new().await;
    let server = fixture.start().await;
    let endpoint = fixture.endpoint();
    assert_eq!(
        endpoint.directory.metadata().unwrap().mode() & 0o7777,
        0o700
    );
    assert_eq!(
        fs::symlink_metadata(&endpoint.socket).unwrap().mode() & 0o7777,
        0o600
    );
    assert_eq!(
        endpoint
            .open_lock(false)
            .unwrap()
            .metadata()
            .unwrap()
            .mode()
            & 0o7777,
        0o600
    );
    let stream = UnixStream::connect(&endpoint.socket).await.unwrap();
    verify_peer(&stream).unwrap();
    verify_uid(effective_uid()).unwrap();
    assert!(verify_uid(effective_uid().wrapping_add(1)).is_err());
    drop(stream);
    fixture.finish(server).await;
}

#[tokio::test]
async fn framing_methods_and_json_identity_claims_cannot_expand_authority() {
    let fixture = Fixture::new().await;
    let server = fixture.start().await;
    let cases: &[&[u8]] = &[
        b"{\"method\":\"bootstrap\",\"args\":null}\n",
        b"{\"method\":\"remote_status\",\"args\":null,\"uid\":0}\n",
        b"{\"method\":\"remote_status\"}\n",
        b"{\"method\":\"remote_status\",\"args\":null}garbage\n",
        b"secret-from-malformed-json\n",
        b"{\"method\":\"remote_configure\",\"args\":{\"secret-from-malformed-json\":true}}\n",
    ];
    for bytes in cases {
        let result = fixture.wire(bytes, false).await;
        assert!(
            result.get("error").is_some(),
            "unexpected response: {result}"
        );
        assert!(!result.to_string().contains("secret-from-malformed-json"));
    }
    let partial = fixture
        .wire(b"{\"method\":\"remote_status\",\"args\":null}", true)
        .await;
    assert!(partial["error"].is_string());
    let oversized = fixture.wire(&vec![b'x'; FRAME_LIMIT + 1], false).await;
    assert!(oversized["error"].is_string());
    assert!(request(fixture.directory.path(), "shutdown", Value::Null)
        .await
        .is_err());
    assert!(request(
        fixture.directory.path(),
        "remote_configure",
        json!({"tailnetUrl":"x".repeat(FRAME_LIMIT)})
    )
    .await
    .is_err());
    assert_eq!(
        fixture.call("remote_status", Value::Null).await["enabled"],
        false
    );
    fixture.finish(server).await;
}

#[tokio::test]
async fn closing_is_checked_after_partial_frame_and_failed_quit_can_recover() {
    let fixture = Fixture::new().await;
    let server = fixture.start().await;
    let mut stream = UnixStream::connect(&fixture.endpoint().socket)
        .await
        .unwrap();
    stream
        .write_all(b"{\"method\":\"remote_configure\",\"args\":{\"enabled\":false,\"port\":23457}}")
        .await
        .unwrap();
    fixture.closing.store(true, Ordering::Release);
    stream.write_all(b"\n").await.unwrap();
    let response: Value = serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
    assert!(response["error"].is_string());
    assert_ne!(
        fixture
            .remote
            .request("remote_status", Value::Null)
            .await
            .unwrap()["port"],
        23457
    );
    fixture.closing.store(false, Ordering::Release);
    assert_eq!(
        fixture
            .call("remote_configure", json!({"enabled":false,"port":23457}))
            .await["port"],
        23457
    );
    fixture.finish(server).await;
}

#[tokio::test]
async fn accepted_rotation_survives_cli_disconnect() {
    let fixture = Fixture::new().await;
    let server = fixture.start().await;
    let first = fixture.call("remote_token", Value::Null).await;
    let release = fixture.credentials.block_next();
    let mut stream = UnixStream::connect(&fixture.endpoint().socket)
        .await
        .unwrap();
    stream
        .write_all(b"{\"method\":\"remote_rotate_token\",\"args\":null}\n")
        .await
        .unwrap();
    release.entered().await;
    drop(stream);
    drop(release);
    let current = timeout(Duration::from_secs(2), async {
        loop {
            let current = fixture
                .remote
                .request("remote_token", Value::Null)
                .await
                .unwrap();
            if current["token"] != first["token"] {
                break current;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_ne!(first["token"], current["token"]);
    assert_eq!(fixture.call("remote_token", Value::Null).await, current);
    fixture.finish(server).await;
}

#[tokio::test]
async fn shutdown_cancels_transport_without_waiting_for_keychain_or_partial_clients() {
    let fixture = Fixture::new().await;
    let server = fixture.start().await;
    let endpoint = fixture.endpoint();
    let mut partial = UnixStream::connect(&endpoint.socket).await.unwrap();
    partial.write_all(b"{").await.unwrap();
    let release = fixture.credentials.block_next();
    let directory = fixture.directory.path().to_path_buf();
    let pending =
        tokio::spawn(async move { request(&directory, "remote_token", Value::Null).await });
    release.entered().await;
    assert_eq!(
        fixture.call("remote_status", Value::Null).await["enabled"],
        false
    );
    assert_eq!(
        fixture
            .call("remote_configure", json!({"enabled":false}))
            .await["enabled"],
        false
    );
    // Match terminal composition order; terminal remote state also invalidates
    // any late result from the still-running blocking credential operation.
    fixture.remote.shutdown().await.unwrap();
    timeout(Duration::from_secs(1), server.shutdown())
        .await
        .unwrap();
    assert!(!endpoint.socket.exists());
    assert!(endpoint.open_lock(false).is_ok());
    assert!(pending.await.unwrap().is_err());
    drop(release);
    let mut byte = [0u8];
    let read = timeout(Duration::from_secs(1), partial.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(read, Ok(0) | Err(_)));
    server.shutdown().await;
    fixture.remove_lock();
}

#[tokio::test]
async fn local_shutdown_preserves_controller_and_releases_lifetime_lock() {
    let fixture = Fixture::new().await;
    let first = fixture.start().await;
    fixture
        .call("remote_configure", json!({"enabled":false,"port":23458}))
        .await;
    first.shutdown().await;
    assert_eq!(
        fixture
            .remote
            .request("remote_status", Value::Null)
            .await
            .unwrap()["port"],
        23458
    );
    let second = fixture.start().await;
    assert_eq!(
        fixture.call("remote_status", Value::Null).await["port"],
        23458
    );
    fixture.finish(second).await;
}

#[tokio::test]
async fn drop_stops_transport_and_releases_endpoint_without_an_async_shutdown() {
    let fixture = Fixture::new().await;
    let server = fixture.start().await;
    let endpoint = fixture.endpoint();
    let mut partial = UnixStream::connect(&endpoint.socket).await.unwrap();
    partial.write_all(b"{").await.unwrap();
    drop(server);
    // Drop schedules cancellation; the runtime turn performs descriptor/lease
    // destruction without requiring a caller to await an OS credential prompt.
    timeout(Duration::from_secs(1), async {
        while endpoint.socket.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let replacement = fixture.start().await;
    fixture.finish(replacement).await;
}

#[tokio::test]
async fn exclusive_lock_prevents_live_cleanup_and_stale_socket_is_reclaimed() {
    let fixture = Fixture::new().await;
    let endpoint = fixture.endpoint();
    let stale = std::os::unix::net::UnixListener::bind(&endpoint.socket).unwrap();
    fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    let server = fixture.start().await;
    let identity = endpoint.socket_metadata().unwrap().unwrap().identity;
    assert!(LocalControl::start(
        fixture.directory.path().to_path_buf(),
        fixture.remote.clone(),
        fixture.closing.clone()
    )
    .await
    .is_err());
    assert!(identity == endpoint.socket_metadata().unwrap().unwrap().identity);
    assert_eq!(
        fixture.call("remote_status", Value::Null).await["running"],
        false
    );
    fixture.finish(server).await;
}

#[tokio::test]
async fn precreated_socket_symlinks_and_regular_files_are_never_deleted_or_followed() {
    for symlink_entry in [false, true] {
        let fixture = Fixture::new().await;
        let endpoint = fixture.endpoint();
        let target = fixture.directory.path().join("untouched");
        fs::write(&target, b"untouched").unwrap();
        if symlink_entry {
            symlink(&target, &endpoint.socket).unwrap();
        } else {
            fs::write(&endpoint.socket, b"unrelated entry").unwrap();
            fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(LocalControl::start(
            fixture.directory.path().to_path_buf(),
            fixture.remote.clone(),
            fixture.closing.clone()
        )
        .await
        .is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        assert!(fs::symlink_metadata(&endpoint.socket).is_ok());
        assert!(
            request(fixture.directory.path(), "remote_status", Value::Null)
                .await
                .is_err()
        );
        fs::remove_file(&endpoint.socket).unwrap();
        let recovered = fixture.start().await;
        assert_eq!(
            fixture.call("remote_status", Value::Null).await["enabled"],
            false
        );
        fixture.finish(recovered).await;
    }
}

#[tokio::test]
async fn lock_symlink_or_unsafe_mode_is_rejected_without_repair() {
    for symlink_entry in [false, true] {
        let fixture = Fixture::new().await;
        let endpoint = fixture.endpoint();
        let lock = endpoint
            .socket
            .with_file_name(OsStr::from_bytes(endpoint.lock_name.as_bytes()));
        let target = fixture.directory.path().join("untouched");
        fs::write(&target, b"untouched").unwrap();
        if symlink_entry {
            symlink(&target, &lock).unwrap();
        } else {
            fs::write(&lock, b"not a protected lock").unwrap();
            fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(LocalControl::start(
            fixture.directory.path().to_path_buf(),
            fixture.remote.clone(),
            fixture.closing.clone()
        )
        .await
        .is_err());
        assert!(endpoint.open_lock(false).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        if !symlink_entry {
            assert_eq!(fs::metadata(&lock).unwrap().mode() & 0o777, 0o644);
        }
        fs::remove_file(lock).unwrap();
        fixture.remote.shutdown().await.unwrap();
    }
}

#[test]
fn unsafe_owner_directories_are_rejected_and_client_lookup_is_read_only() {
    #[cfg(target_os = "macos")]
    let temporary = "/private/tmp";
    #[cfg(not(target_os = "macos"))]
    let temporary = "/tmp";
    let root = tempfile::Builder::new()
        .prefix("omp-owner-")
        .tempdir_in(temporary)
        .unwrap();
    let data = root.path().join("data");
    fs::create_dir(&data).unwrap();
    let owner = root.path().join("owner");
    assert!(Endpoint::at(&data, &owner, false).is_err());
    assert!(!owner.exists());
    fs::create_dir(&owner).unwrap();
    fs::set_permissions(&owner, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Endpoint::at(&data, &owner, true).is_err());
    assert_eq!(fs::metadata(&owner).unwrap().mode() & 0o777, 0o755);
    fs::remove_dir(&owner).unwrap();
    symlink(&data, &owner).unwrap();
    assert!(Endpoint::at(&data, &owner, true).is_err());
    assert!(Endpoint::at(&data, &owner, false).is_err());
}

#[tokio::test]
async fn canonical_aliases_match_but_distinct_data_directories_are_isolated() {
    let one = Fixture::new().await;
    let two = Fixture::new().await;
    let first = one.start().await;
    let second = two.start().await;
    let alias_root = tempfile::tempdir().unwrap();
    let alias = alias_root.path().join("alias");
    symlink(one.directory.path(), &alias).unwrap();
    assert_eq!(
        Endpoint::locate(&alias, false).unwrap().socket,
        one.endpoint().socket
    );
    assert_ne!(one.endpoint().socket, two.endpoint().socket);
    assert!(one.endpoint().socket.as_os_str().as_bytes().len() < 104);
    request(
        &alias,
        "remote_configure",
        json!({"enabled":false,"port":23459}),
    )
    .await
    .unwrap();
    assert_eq!(one.call("remote_status", Value::Null).await["port"], 23459);
    assert_ne!(two.call("remote_status", Value::Null).await["port"], 23459);
    one.finish(first).await;
    two.finish(second).await;
}

#[tokio::test]
async fn nonexistent_data_directory_is_not_created() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("not-created");
    assert!(request(&absent, "remote_status", Value::Null)
        .await
        .is_err());
    assert!(!absent.exists());
}

#[tokio::test]
async fn client_rejects_unsafe_live_socket_permissions_without_changing_them() {
    let fixture = Fixture::new().await;
    let server = fixture.start().await;
    let endpoint = fixture.endpoint();
    fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        request(fixture.directory.path(), "remote_status", Value::Null)
            .await
            .is_err()
    );
    assert_eq!(
        fs::metadata(&endpoint.socket).unwrap().mode() & 0o777,
        0o644
    );
    fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fixture.call("remote_status", Value::Null).await["enabled"],
        false
    );
    fixture.finish(server).await;
}

#[tokio::test]
async fn shutdown_never_unlinks_a_replacement_unrelated_entry() {
    let fixture = Fixture::new().await;
    let server = fixture.start().await;
    let endpoint = fixture.endpoint();
    fs::remove_file(&endpoint.socket).unwrap();
    fs::write(&endpoint.socket, b"replacement must survive").unwrap();
    server.shutdown().await;
    assert_eq!(
        fs::read(&endpoint.socket).unwrap(),
        b"replacement must survive"
    );
    fs::remove_file(&endpoint.socket).unwrap();
    fixture.remote.shutdown().await.unwrap();
    fixture.remove_lock();
}

#[tokio::test]
async fn hardlinked_locks_and_noncanonical_closing_flags_are_rejected() {
    let fixture = Fixture::new().await;
    assert!(LocalControl::start(
        fixture.directory.path().to_path_buf(),
        fixture.remote.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .is_err());
    let endpoint = fixture.endpoint();
    let lock = endpoint
        .socket
        .with_file_name(OsStr::from_bytes(endpoint.lock_name.as_bytes()));
    let target = fixture.directory.path().join("lock-target");
    fs::write(&target, b"untouched").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&target, &lock).unwrap();
    assert!(LocalControl::start(
        fixture.directory.path().to_path_buf(),
        fixture.remote.clone(),
        fixture.closing.clone(),
    )
    .await
    .is_err());
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
    fs::remove_file(lock).unwrap();
    fixture.remote.shutdown().await.unwrap();
}

#[tokio::test]
async fn very_long_data_directory_uses_a_short_working_hashed_socket() {
    let fixture = Fixture::new().await;
    let mut long = fixture.directory.path().to_path_buf();
    for _ in 0..8 {
        long.push("long-application-support-directory");
    }
    fs::create_dir_all(&long).unwrap();
    assert!(long.as_os_str().as_bytes().len() > 104);
    let endpoint = Endpoint::locate(&long, true).unwrap();
    assert!(endpoint.socket.as_os_str().as_bytes().len() < 104);
    let server = LocalControl::start(
        long.clone(),
        fixture.remote.clone(),
        fixture.closing.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        request(&long, "remote_status", Value::Null).await.unwrap()["enabled"],
        false
    );
    fixture.remote.shutdown().await.unwrap();
    server.shutdown().await;
    fs::remove_file(
        endpoint
            .socket
            .with_file_name(OsStr::from_bytes(endpoint.lock_name.as_bytes())),
    )
    .unwrap();
}
