use super::*;
use std::{collections::BTreeMap, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct MemoryCredentials {
    entries: Mutex<BTreeMap<String, String>>,
    unavailable: AtomicBool,
}
impl Credentials for MemoryCredentials {
    fn read(&self, instance: &str) -> Result<Option<String>> {
        anyhow::ensure!(
            !self.unavailable.load(Ordering::Relaxed),
            "Credential storage unavailable"
        );
        Ok(self.entries.lock().unwrap().get(instance).cloned())
    }
    fn write(&self, instance: &str, token: &str) -> Result<()> {
        anyhow::ensure!(
            !self.unavailable.load(Ordering::Relaxed),
            "Credential storage unavailable"
        );
        self.entries
            .lock()
            .unwrap()
            .insert(instance.into(), token.into());
        Ok(())
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    desktop: Arc<Desktop>,
    remote: Arc<RemoteAccess>,
    credentials: Arc<MemoryCredentials>,
    port: u16,
    token: String,
    asset_requests: Arc<Mutex<Vec<String>>>,
}

// Construct only inert core state: Desktop::open probes provider Keychain entries.
// No test in this module calls it or accesses the production credential backend.
async fn desktop(directory: &std::path::Path) -> Arc<Desktop> {
    tokio::fs::create_dir_all(directory).await.unwrap();
    Arc::new(Desktop {
        directory: directory.to_path_buf(),
        state: Mutex::new(crate::State {
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
    })
}

async fn fixture(tailnet: Option<&str>) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let desktop = desktop(&directory.path().join("core")).await;
    let credentials = Arc::new(MemoryCredentials::default());
    let asset_requests = Arc::new(Mutex::new(vec![]));
    let seen = asset_requests.clone();
    let assets: AssetLoader = Arc::new(move |path| {
        seen.lock().unwrap().push(path.into());
        let (bytes, mime) = match path {
            "index.html" => (b"<html>OMP Remote</html>".to_vec(), "text/html"),
            "assets/app.js" => (b"console.log('remote')".to_vec(), "text/javascript"),
            "assets/large.js" => (vec![b'x'; 8 * 1024 * 1024], "text/javascript"),
            _ => return None,
        };
        Some(RemoteAsset {
            bytes,
            mime_type: mime.into(),
        })
    });
    let remote = RemoteAccess::open_with_credentials(
        desktop.clone(),
        directory.path().to_path_buf(),
        assets,
        credentials.clone(),
    )
    .await
    .unwrap();
    // Keep the actual listener reserved all the way into launch. Parallel tests
    // cannot race between choosing an ephemeral port and binding it.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    remote
        .request(
            "remote_configure",
            json!({"port":port,"tailnetUrl":tailnet}),
        )
        .await
        .unwrap();
    let token = remote.request("remote_token", Value::Null).await.unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    {
        let mut controller = remote.controller.lock().await;
        controller.settings.enabled = true;
        save_settings(directory.path(), &controller.settings)
            .await
            .unwrap();
        remote.launch(&mut controller, listener, token.clone());
    }
    Fixture {
        directory,
        desktop,
        remote,
        credentials,
        port,
        token,
        asset_requests,
    }
}

struct Http {
    status: u16,
    headers: String,
    body: Vec<u8>,
}
impl Http {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn read_response(mut stream: tokio::net::TcpStream) -> Http {
    let mut bytes = vec![];
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let boundary = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers = String::from_utf8(bytes[..boundary].to_vec()).unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    Http {
        status,
        headers: headers.to_ascii_lowercase(),
        body: bytes[boundary + 4..].to_vec(),
    }
}

async fn raw(port: u16, request: String) -> Http {
    let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    read_response(stream).await
}

fn wire(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
    extra: &str,
) -> String {
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{auth}{extra}Content-Length: {}\r\n\r\n{body}", body.len())
}

async fn api(f: &Fixture, token: Option<&str>, method: &str, args: Value, extra: &str) -> Http {
    raw(
        f.port,
        wire(
            f.port,
            "POST",
            "/api/request",
            token,
            &json!({"method":method,"args":args}).to_string(),
            extra,
        ),
    )
    .await
}

async fn create_bot(f: &Fixture) -> String {
    let result = api(
        f,
        Some(&f.token),
        "create_bot",
        json!({
            "name":"Shared", "cwd":f.directory.path(), "engine":"omp",
        }),
        "",
    )
    .await;
    assert_eq!(result.status, 200, "{}", result.text());
    result.json()["data"]["bots"][0]["id"]
        .as_str()
        .unwrap()
        .into()
}

async fn live_native_bot(f: &Fixture) -> (String, native_agent::NativeClient) {
    let provider = native_agent::default_providers().remove(0);
    f.desktop
        .state
        .lock()
        .unwrap()
        .providers
        .push(crate::ProviderView {
            config: provider.clone(),
            has_api_key: false,
            auth_method: "none".into(),
            oauth_available: true,
        });
    f.desktop.request("create_bot", json!({
        "name":"Live native", "cwd":f.directory.path(), "engine":"native", "nativeProvider":provider.id,
    })).await.unwrap();
    // Spawning and local model/state commands do not invoke a provider or its
    // credential lookup. This is a real live runtime, not a fake dispatcher.
    let client = native_agent::NativeClient::spawn(
        provider,
        native_agent::NativeOptions {
            cwd: f.directory.path().to_path_buf(),
            session_path: f.directory.path().join("live-session.json"),
            model: "gpt-4.1".into(),
            instructions: String::new(),
            thinking: None,
        },
    )
    .await
    .unwrap();
    let runtime_state = client.request(json!({"type":"get_state"})).await.unwrap();
    let id = f.desktop.snapshot().bots[0].id.clone();
    {
        let mut state = f.desktop.state.lock().unwrap();
        let bot = state.bots.get_mut(&id).unwrap();
        bot.client = Some(crate::Runtime::Native(client.clone()));
        bot.generation = 7;
        bot.view.is_running = true;
        Desktop::apply_state(bot, runtime_state);
    }
    (id, client)
}

#[tokio::test]
async fn off_by_default_and_per_directory_credentials_are_persistent_but_never_in_settings() {
    let store = Arc::new(MemoryCredentials::default());
    let mut identities = vec![];
    let mut tokens = vec![];
    for _ in 0..2 {
        let dir = tempfile::tempdir().unwrap();
        let core = desktop(&dir.path().join("core")).await;
        let remote = RemoteAccess::open_with_credentials(
            core.clone(),
            dir.path().to_path_buf(),
            Arc::new(|_| None),
            store.clone(),
        )
        .await
        .unwrap();
        let status = remote.request("remote_status", Value::Null).await.unwrap();
        assert_eq!(status["enabled"], false);
        assert_eq!(status["running"], false);
        assert_eq!(status["port"], DEFAULT_PORT);
        assert_eq!(status["error"], Value::Null);
        assert!(!status.to_string().contains("token"));
        identities.push(status["instanceId"].as_str().unwrap().to_string());
        let token = remote.request("remote_token", Value::Null).await.unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(valid_token(&token));
        let saved = tokio::fs::read_to_string(dir.path().join("remote.json"))
            .await
            .unwrap();
        assert!(!saved.contains(&token));
        assert!(!saved.contains("token"));
        remote.shutdown().await.unwrap();
        let reopened = RemoteAccess::open_with_credentials(
            core,
            dir.path().to_path_buf(),
            Arc::new(|_| None),
            store.clone(),
        )
        .await
        .unwrap();
        assert!(!reopened.is_running());
        assert_eq!(
            reopened.request("remote_token", Value::Null).await.unwrap()["token"],
            token
        );
        assert_eq!(
            reopened
                .request("remote_status", Value::Null)
                .await
                .unwrap()["instanceId"],
            identities.last().unwrap().as_str()
        );
        reopened.shutdown().await.unwrap();
        tokens.push(token);
    }
    assert_ne!(identities[0], identities[1]);
    assert_ne!(tokens[0], tokens[1]);
    assert_eq!(store.entries.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn http_dispatch_and_local_dispatch_share_the_same_bot_and_revision() {
    let f = fixture(None).await;
    let id = create_bot(&f).await;
    assert_eq!(f.desktop.snapshot().bots[0].id, id);
    f.desktop
        .request(
            "update_bot",
            json!({
                "id":id,"name":"Local change","cwd":f.directory.path(),"instructions":"Host-owned",
            }),
        )
        .await
        .unwrap();
    let response = api(&f, Some(&f.token), "bootstrap", Value::Null, "").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["data"]["bots"][0]["name"], "Local change");
    assert_eq!(
        response.json()["data"]["bots"][0]["revision"],
        f.desktop.snapshot().bots[0].revision
    );
    let response = api(
        &f,
        Some(&f.token),
        "update_bot",
        json!({
            "id":id,"name":"Remote change","cwd":f.directory.path(),"instructions":"Same host",
        }),
        "",
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(f.desktop.snapshot().bots[0].name, "Remote change");
    f.remote.shutdown().await.unwrap();
}

#[tokio::test]
async fn authorization_precedes_body_reading_and_bad_origins_never_mutate() {
    let f = fixture(None).await;
    let response = raw(f.port, format!(
        "POST /api/request HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: 999999999\r\n\r\n", f.port,
    )).await;
    assert_eq!(response.status, 401); // No body was sent at all.
    for (token, extra, expected) in [
        (None, "Tailscale-User-Login: owner@example.com\r\n", 401),
        (Some("invalid"), "", 401),
        (
            Some(f.token.as_str()),
            "Origin: https://attacker.example\r\n",
            403,
        ),
        (Some(f.token.as_str()), "Origin: null\r\n", 403),
        (
            Some(f.token.as_str()),
            "Origin: http://localhost:1\r\n",
            403,
        ),
        (
            Some(f.token.as_str()),
            "Origin: http://localhost\r\nOrigin: http://localhost\r\n",
            403,
        ),
    ] {
        let response = api(
            &f,
            token,
            "create_bot",
            json!({
                "name":"Forbidden","cwd":f.directory.path(),"engine":"omp",
            }),
            extra,
        )
        .await;
        assert_eq!(response.status, expected);
        assert!(!response.text().contains(&f.token));
    }
    let body = json!({"method":"bootstrap"}).to_string();
    let response = raw(f.port, format!(
        "POST /api/request HTTP/1.1\r\nHost: attacker.example:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        f.port, f.token, body.len(),
    )).await;
    assert_eq!(response.status, 403);
    assert!(f.desktop.snapshot().bots.is_empty());
    let malformed = raw(
        f.port,
        wire(
            f.port,
            "POST",
            "/api/request",
            Some(&f.token),
            "not JSON",
            "",
        ),
    )
    .await;
    assert_eq!(malformed.status, 400);
    f.remote.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_tailnet_origin_supports_preserved_and_rewritten_proxy_hosts() {
    let f = fixture(Some("https://host.example.ts.net:443/")).await;
    let status = f
        .remote
        .request("remote_status", Value::Null)
        .await
        .unwrap();
    assert_eq!(status["tailnetUrl"], "https://host.example.ts.net");
    let body = json!({"method":"bootstrap"}).to_string();
    for (host, origin, expected) in [
        (
            format!("127.0.0.1:{}", f.port),
            Some("https://host.example.ts.net"),
            200,
        ),
        (
            format!("localhost:{}", f.port),
            Some("https://host.example.ts.net:443"),
            200,
        ),
        (
            "host.example.ts.net".into(),
            Some("https://host.example.ts.net"),
            200,
        ),
        ("host.example.ts.net:443".into(), None, 200),
        (
            "host.example.ts.net:8443".into(),
            Some("https://host.example.ts.net"),
            403,
        ),
        (
            "host.example.ts.net.attacker.test".into(),
            Some("https://host.example.ts.net"),
            403,
        ),
        (
            "host.example.ts.net".into(),
            Some("https://other.example.ts.net"),
            403,
        ),
        (
            "host.example.ts.net".into(),
            Some("https://host.example.ts.net/path"),
            403,
        ),
    ] {
        let origin = origin
            .map(|value| format!("Origin: {value}\r\n"))
            .unwrap_or_default();
        let response = raw(f.port, format!(
            "POST /api/request HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\n{origin}Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
            f.token, body.len(),
        )).await;
        assert_eq!(response.status, expected, "Host {host}");
    }
    let local = api(
        &f,
        Some(&f.token),
        "bootstrap",
        Value::Null,
        &format!("Origin: http://localhost:{}\r\n", f.port),
    )
    .await;
    assert_eq!(local.status, 200);
    for url in [
        "http://host.example.ts.net",
        "https://example.com",
        "https://host.example.ts.net.attacker.test",
        "https://user:password@host.example.ts.net",
        "https://@host.example.ts.net",
        "https://host.example.ts.net/secret",
        "https://host.example.ts.net/../",
        "https://host.example.ts.net?token=secret",
        "https://host.example.ts.net#secret",
    ] {
        assert!(
            f.remote
                .request("remote_configure", json!({"tailnetUrl":url}))
                .await
                .is_err(),
            "{url}"
        );
    }
    f.remote.shutdown().await.unwrap();
}

#[tokio::test]
async fn only_safe_bundled_assets_are_served_with_security_headers_and_minimal_health() {
    let f = fixture(None).await;
    tokio::fs::write(f.directory.path().join("private.txt"), "private host data")
        .await
        .unwrap();
    let response = raw(f.port, wire(f.port, "GET", "/", None, "", "")).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.text(), "<html>OMP Remote</html>");
    for header in [
        "content-type: text/html",
        "cache-control: no-store",
        "content-security-policy:",
        "frame-ancestors 'none'",
        "x-frame-options: deny",
        "x-content-type-options: nosniff",
        "referrer-policy: no-referrer",
    ] {
        assert!(response.headers.contains(header), "{header}");
    }
    let response = raw(f.port, wire(f.port, "GET", "/assets/app.js", None, "", "")).await;
    assert_eq!(response.status, 200);
    assert!(response.headers.contains("content-type: text/javascript"));
    let seen = f.asset_requests.lock().unwrap().len();
    for path in [
        "/../private.txt",
        "/assets/../../private.txt",
        "/%2e%2e/private.txt",
        "/%252e%252e/private.txt",
        "/assets%2f..%2fprivate.txt",
        "/assets\\private.txt",
        "/.env",
        "//private.txt",
    ] {
        let response = raw(f.port, wire(f.port, "GET", path, None, "", "")).await;
        assert!(
            matches!(response.status, 400 | 404),
            "{path}: {}",
            response.status
        );
        assert!(!response.text().contains("private host data"));
    }
    assert_eq!(f.asset_requests.lock().unwrap().len(), seen);
    let response = raw(f.port, wire(f.port, "GET", "/private.txt", None, "", "")).await;
    assert_eq!(response.status, 404);
    let health = raw(f.port, wire(f.port, "GET", "/api/health", None, "", "")).await;
    assert_eq!(
        health.json(),
        json!({"status":"ready","transport":"remote"})
    );
    assert!(!health.text().contains(&f.token));
    assert!(health.headers.contains("cache-control: no-store"));
    assert!(!health.headers.contains("access-control-allow-origin"));
    f.remote.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_only_controls_cannot_be_called_remotely_and_rotation_revokes_the_old_token() {
    let f = fixture(None).await;
    let before = f
        .remote
        .request("remote_status", Value::Null)
        .await
        .unwrap();
    for method in [
        "remote_status",
        "remote_configure",
        "remote_token",
        "remote_rotate_token",
        "sms_status",
        "sms_configure",
        "sms_accept",
        "sms_dismiss",
    ] {
        let response = api(&f, Some(&f.token), method, json!({"enabled":false}), "").await;
        assert_eq!(response.status, 403);
        assert!(!response.text().contains(&f.token));
    }
    assert_eq!(
        f.remote
            .request("remote_status", Value::Null)
            .await
            .unwrap(),
        before
    );
    let rotated = f
        .remote
        .request("remote_rotate_token", Value::Null)
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(rotated, f.token);
    assert_eq!(
        api(&f, Some(&f.token), "bootstrap", Value::Null, "")
            .await
            .status,
        401
    );
    assert_eq!(
        api(&f, Some(&rotated), "bootstrap", Value::Null, "")
            .await
            .status,
        200
    );
    let disabled = f
        .remote
        .request("remote_configure", json!({"enabled":false}))
        .await
        .unwrap();
    assert_eq!(disabled["running"], false);
    assert_eq!(disabled["enabled"], false);
    assert!(
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, f.port))
            .await
            .is_err()
    );
    assert_eq!(
        f.remote.request("remote_token", Value::Null).await.unwrap()["token"],
        rotated
    );
    let enabled = f
        .remote
        .request("remote_configure", json!({"enabled":true}))
        .await
        .unwrap();
    assert_eq!(enabled["running"], true);
    assert_eq!(
        api(&f, Some(&rotated), "bootstrap", Value::Null, "")
            .await
            .status,
        200
    );
    f.remote.shutdown().await.unwrap();
}

#[tokio::test]
async fn normal_shutdown_restores_opt_in_and_token_without_recreating_bot_state() {
    let f = fixture(None).await;
    let (id, client) = live_native_bot(&f).await;
    let session = client.request(json!({"type":"get_state"})).await.unwrap()["sessionId"].clone();
    let changed = api(
        &f,
        Some(&f.token),
        "set_model",
        json!({
            "id":id, "provider":"openai", "modelId":"remote-selected-model",
        }),
        "",
    )
    .await;
    assert_eq!(changed.status, 200, "{}", changed.text());
    assert_eq!(
        client.request(json!({"type":"get_state"})).await.unwrap()["model"]["id"],
        "remote-selected-model"
    );
    let revision = f.desktop.snapshot().bots[0].revision;
    f.remote.shutdown().await.unwrap();
    let saved: Value = serde_json::from_slice(
        &tokio::fs::read(f.directory.path().join("remote.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(saved["enabled"], true);
    let reopened = RemoteAccess::open_with_credentials(
        f.desktop.clone(),
        f.directory.path().to_path_buf(),
        f.remote.assets.clone(),
        f.credentials.clone(),
    )
    .await
    .unwrap();
    wait_for_restoration(&reopened).await;
    let reconnect = api(&f, Some(&f.token), "bootstrap", Value::Null, "").await;
    assert_eq!(reconnect.status, 200);
    assert_eq!(reconnect.json()["data"]["bots"][0]["id"], id);
    assert_eq!(reconnect.json()["data"]["bots"][0]["revision"], revision);
    assert_eq!(f.desktop.snapshot().bots.len(), 1);
    assert!(f.desktop.has_running_bots());
    assert_eq!(f.desktop.state.lock().unwrap().bots[&id].generation, 7);
    assert_eq!(
        client.request(json!({"type":"get_state"})).await.unwrap()["sessionId"],
        session
    );
    let changed = api(
        &f,
        Some(&f.token),
        "set_model",
        json!({
            "id":id, "provider":"openai", "modelId":"after-reconnect",
        }),
        "",
    )
    .await;
    assert_eq!(changed.status, 200);
    assert_eq!(
        client.request(json!({"type":"get_state"})).await.unwrap()["model"]["id"],
        "after-reconnect"
    );
    assert_eq!(
        reopened.request("remote_token", Value::Null).await.unwrap()["token"],
        f.token
    );
    reopened.shutdown().await.unwrap();
    f.desktop.shutdown_all().await.unwrap();
}

async fn wait_for_dispatch(state: &Arc<ServerState>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.policy.lock().unwrap().in_flight == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn disconnected_accepted_mutations_are_drained_before_disable_returns() {
    let f = fixture(None).await;
    let id = create_bot(&f).await;
    let gate = f.desktop.gate(&id).unwrap();
    let held = gate.lock().await;
    let state = f
        .remote
        .controller
        .lock()
        .await
        .server
        .as_ref()
        .unwrap()
        .state
        .clone();
    let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, f.port))
        .await
        .unwrap();
    let body = json!({"method":"update_bot","args":{
        "id":id,"name":"Drained change","cwd":f.directory.path(),"instructions":"",
    }})
    .to_string();
    stream
        .write_all(wire(f.port, "POST", "/api/request", Some(&f.token), &body, "").as_bytes())
        .await
        .unwrap();
    wait_for_dispatch(&state).await;
    drop(stream);
    let remote = f.remote.clone();
    let disabling = tokio::spawn(async move {
        remote
            .request("remote_configure", json!({"enabled":false}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.policy.lock().unwrap().accepting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!disabling.is_finished());
    assert_eq!(f.desktop.snapshot().bots[0].name, "Shared");
    drop(held);
    let status = tokio::time::timeout(Duration::from_secs(5), disabling)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(status["running"], false);
    assert_eq!(f.desktop.snapshot().bots[0].name, "Drained change");
    assert_eq!(state.policy.lock().unwrap().in_flight, 0);
    f.remote.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_cancels_partial_uploads_without_waiting_for_the_sender() {
    let f = fixture(None).await;
    let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, f.port))
        .await
        .unwrap();
    stream.write_all(format!(
        "POST /api/request HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\nExpect: 100-continue\r\nContent-Length: 1024\r\n\r\n",
        f.port, f.token,
    ).as_bytes()).await.unwrap();
    // Hyper emits 100 Continue only when the authenticated handler polls the
    // body. Establish that the upload is in progress before stopping the host.
    let interim = tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = vec![];
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(stream.read_u8().await.unwrap());
        }
        String::from_utf8(bytes).unwrap()
    })
    .await
    .unwrap();
    assert!(interim.starts_with("HTTP/1.1 100 "));
    stream.write_all(b"{").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), f.remote.shutdown())
        .await
        .unwrap()
        .unwrap();
    // The partial body never became an accepted dispatch.
    assert!(f.desktop.snapshot().bots.is_empty());
    assert!(!f.remote.is_running());
}

#[tokio::test]
async fn bind_and_credential_failures_are_visible_without_fallback_or_local_core_failure() {
    let dir = tempfile::tempdir().unwrap();
    let core = desktop(&dir.path().join("core")).await;
    let store = Arc::new(MemoryCredentials::default());
    let remote = RemoteAccess::open_with_credentials(
        core.clone(),
        dir.path().to_path_buf(),
        Arc::new(|_| None),
        store.clone(),
    )
    .await
    .unwrap();
    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = occupied.local_addr().unwrap().port();
    let status = remote
        .request("remote_configure", json!({"enabled":true,"port":port}))
        .await
        .unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["running"], false);
    assert_eq!(status["port"], port);
    assert!(status["error"].as_str().unwrap().contains("Could not bind"));
    assert!(core.request("bootstrap", Value::Null).await.is_ok());
    drop(occupied);
    assert_eq!(
        remote
            .request("remote_configure", json!({"enabled":true}))
            .await
            .unwrap()["running"],
        true
    );
    remote.shutdown().await.unwrap();
    store.unavailable.store(true, Ordering::Relaxed);
    let reopened = RemoteAccess::open_with_credentials(
        core.clone(),
        dir.path().to_path_buf(),
        Arc::new(|_| None),
        store,
    )
    .await
    .unwrap();
    let status = wait_for_restoration(&reopened).await;
    assert_eq!(status["enabled"], true);
    assert_eq!(status["running"], false);
    assert!(core.request("bootstrap", Value::Null).await.is_ok());
    assert!(tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .is_err());
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_settings_remain_visible_and_are_not_silently_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("remote.json"), b"not JSON")
        .await
        .unwrap();
    let core = desktop(&dir.path().join("core")).await;
    let remote = RemoteAccess::open_with_credentials(
        core,
        dir.path().to_path_buf(),
        Arc::new(|_| None),
        Arc::new(MemoryCredentials::default()),
    )
    .await
    .unwrap();
    assert!(!remote.is_running());
    assert!(remote.request("remote_status", Value::Null).await.unwrap()["error"].is_string());
    assert!(remote
        .request("remote_configure", json!({"enabled":true}))
        .await
        .is_err());
    assert_eq!(
        tokio::fs::read(dir.path().join("remote.json"))
            .await
            .unwrap(),
        b"not JSON"
    );
    remote.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_the_controller_closes_its_listener_but_keeps_the_shared_runtime_alive() {
    let f = fixture(None).await;
    let (_, client) = live_native_bot(&f).await;
    let running = f.remote.running.clone();
    drop(f.remote);
    tokio::time::timeout(Duration::from_secs(5), async {
        while running.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, f.port))
            .await
            .is_err()
    );
    assert!(f.desktop.has_running_bots());
    assert!(client.request(json!({"type":"get_state"})).await.is_ok());
    f.desktop.shutdown_all().await.unwrap();
}

#[tokio::test]
async fn stalled_response_reader_cannot_hold_remote_shutdown_open() {
    let f = fixture(None).await;
    let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, f.port))
        .await
        .unwrap();
    stream
        .write_all(wire(f.port, "GET", "/assets/large.js", None, "", "").as_bytes())
        .await
        .unwrap();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), stream.peek(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        1
    );
    tokio::time::timeout(Duration::from_secs(3), f.remote.shutdown())
        .await
        .expect("A reader that stops consuming assets must not block host shutdown")
        .unwrap();
    assert!(!f.remote.is_running());
    assert!(
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, f.port))
            .await
            .is_err()
    );
    let mut buffered = Vec::new();
    if let Err(error) =
        tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buffered))
            .await
            .expect("Shutdown must close the existing transport, not just its listener")
    {
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    }
}

#[tokio::test]
async fn suspended_quit_attempt_can_restore_remote_control_of_the_same_runtime() {
    let f = fixture(None).await;
    let (_, client) = live_native_bot(&f).await;
    let session = client.request(json!({"type":"get_state"})).await.unwrap()["sessionId"].clone();
    f.remote.suspend().await.unwrap();
    let suspended = f
        .remote
        .request("remote_status", Value::Null)
        .await
        .unwrap();
    assert_eq!(suspended["enabled"], true);
    assert_eq!(suspended["running"], false);
    let restored = f
        .remote
        .request("remote_configure", json!({"enabled":true}))
        .await
        .unwrap();
    assert_eq!(restored["running"], true);
    let snapshot = api(&f, Some(&f.token), "bootstrap", Value::Null, "").await;
    assert_eq!(snapshot.status, 200);
    assert_eq!(
        snapshot.json()["data"]["bots"][0]["state"]["sessionId"],
        session
    );
    assert_eq!(snapshot.json()["data"]["bots"][0]["isRunning"], true);
    f.remote.shutdown().await.unwrap();
    f.desktop.shutdown_all().await.unwrap();
}

async fn wait_for_restoration(remote: &RemoteAccess) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = remote.request("remote_status", Value::Null).await.unwrap();
            if status["running"] == true || status["error"].is_string() {
                return status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Saved remote access must start or report a visible error")
}

#[tokio::test]
async fn credential_permission_does_not_block_local_desktop_startup() {
    struct PendingCredentials {
        store: Arc<MemoryCredentials>,
        entered: Notify,
        permission: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Credentials for PendingCredentials {
        fn read(&self, instance: &str) -> Result<Option<String>> {
            self.entered.notify_one();
            self.permission
                .lock()
                .unwrap()
                .recv()
                .context("Credential permission cancelled")?;
            self.store.read(instance)
        }
        fn write(&self, instance: &str, token: &str) -> Result<()> {
            self.store.write(instance, token)
        }
    }

    let f = fixture(None).await;
    f.remote.shutdown().await.unwrap();
    let (permission, pending) = std::sync::mpsc::channel();
    let credentials = Arc::new(PendingCredentials {
        store: f.credentials.clone(),
        entered: Notify::new(),
        permission: Mutex::new(pending),
    });
    let restored = tokio::time::timeout(
        Duration::from_secs(5),
        RemoteAccess::open_with_credentials(
            f.desktop.clone(),
            f.directory.path().to_path_buf(),
            f.remote.assets.clone(),
            credentials.clone(),
        ),
    )
    .await
    .expect("A pending Keychain permission must not hold up the host window")
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), credentials.entered.notified())
        .await
        .unwrap();
    let local = f
        .desktop
        .request(
            "create_bot",
            json!({
                "name":"Local during permission", "cwd":f.directory.path(), "engine":"omp",
            }),
        )
        .await
        .unwrap();
    let id = local["bots"][0]["id"].clone();
    assert_eq!(local["bots"][0]["name"], "Local during permission");
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        restored.request("remote_status", Value::Null),
    )
    .await
    .expect("Status must remain available while Keychain permission is pending")
    .unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["running"], false);
    tokio::time::timeout(Duration::from_secs(5), restored.suspend())
        .await
        .expect("Quit staging must not wait for optional Keychain permission")
        .unwrap();
    let disabled = tokio::time::timeout(
        Duration::from_secs(5),
        restored.request("remote_configure", json!({"enabled":false})),
    )
    .await
    .expect("Disable must remain available while Keychain permission is pending")
    .unwrap();
    assert_eq!(disabled["enabled"], false);
    tokio::time::timeout(Duration::from_secs(5), restored.shutdown())
        .await
        .expect("Shutdown must not wait for optional Keychain permission")
        .unwrap();
    permission.send(()).unwrap();
    let reopened = RemoteAccess::open_with_credentials(
        f.desktop.clone(),
        f.directory.path().to_path_buf(),
        f.remote.assets.clone(),
        f.credentials.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        reopened
            .request("remote_configure", json!({"enabled":true}))
            .await
            .unwrap()["running"],
        true
    );
    let connected = api(&f, Some(&f.token), "bootstrap", Value::Null, "").await;
    assert_eq!(connected.status, 200);
    assert_eq!(connected.json()["data"]["bots"][0]["id"], id);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn queued_owner_configuration_cannot_resume_a_quitting_host() {
    let f = fixture(None).await;
    f.remote.suspend().await.unwrap();
    let controller = f.remote.controller.lock().await;
    let remote = f.remote.clone();
    let (queued, ready) = tokio::sync::oneshot::channel();
    let configure = tokio::spawn(async move {
        queued.send(()).unwrap();
        remote
            .request("remote_configure", json!({"enabled":true}))
            .await
    });
    ready.await.unwrap();
    f.remote.closing_flag().store(true, Ordering::Release);
    drop(controller);
    assert!(configure.await.unwrap().is_err());
    assert!(!f.remote.is_running());
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", f.port))
        .await
        .is_err());
    f.remote.shutdown().await.unwrap();
}

#[tokio::test]
async fn credentials_resolving_during_quit_cannot_publish_a_listener() {
    let f = fixture(None).await;
    let epoch = {
        let mut controller = f.remote.controller.lock().await;
        f.remote.stop(&mut controller).await.unwrap();
        controller.token = None;
        controller.configuration_epoch
    };
    let credentials = f.remote.credential_io.lock().await;
    let remote = f.remote.clone();
    let (queued, ready) = tokio::sync::oneshot::channel();
    let restore = tokio::spawn(async move {
        queued.send(()).unwrap();
        remote.restore_listener(epoch).await;
    });
    ready.await.unwrap();
    f.remote.closing_flag().store(true, Ordering::Release);
    drop(credentials);
    restore.await.unwrap();
    assert!(!f.remote.is_running());
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", f.port))
        .await
        .is_err());
    f.remote.shutdown().await.unwrap();
}
