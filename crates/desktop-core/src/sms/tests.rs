use super::*;
use std::sync::atomic::{AtomicI64, AtomicUsize};

#[derive(Default)]
struct MemoryCredentials {
    values: Mutex<BTreeMap<String, String>>,
    reads: AtomicUsize,
}
impl CredentialStore for MemoryCredentials {
    fn get(&self, account: &str) -> Result<Option<String>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.values.lock().unwrap().get(account).cloned())
    }
    fn set(&self, account: &str, value: &str) -> Result<()> {
        let mut values = self.values.lock().unwrap();
        if value.is_empty() {
            values.remove(account);
        } else {
            values.insert(account.into(), value.into());
        }
        Ok(())
    }
}
struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}
fn configured() -> Saved {
    Saved {
        settings: Settings {
            enabled: false,
            relay_url: "http://127.0.0.1:8765".into(),
            pipestreamr_url: "http://127.0.0.1:8766".into(),
            project_id: Uuid::new_v4().to_string(),
            source_id: Uuid::new_v4().to_string(),
            phone: "+14155550123".into(),
            bot_id: Uuid::new_v4().to_string(),
        },
        ..Saved::default()
    }
}
fn inbound(saved: &Saved, received_at: i64) -> SmsMessage {
    SmsMessage {
        id: Uuid::new_v4().to_string(),
        project_id: saved.settings.project_id.clone(),
        source_id: saved.settings.source_id.clone(),
        from_id: saved.settings.phone.clone(),
        body: "What should I do next?".into(),
        received_at,
        state: "pending".into(),
        detail: None,
        claim_id: None,
        review: false,
    }
}
fn insert(saved: &mut Saved, message: SmsMessage, phase: Phase) -> String {
    let key = entry_key(&saved.settings, &message.id);
    saved.entries.insert(
        key.clone(),
        Entry {
            message,
            phase,
            accepted: false,
            reply: None,
            finish_pending: false,
        },
    );
    key
}

#[test]
fn preconnection_stale_and_future_messages_require_review() {
    let mut saved = configured();
    let old = inbound(&saved, 999_999);
    let fresh = inbound(&saved, 1_000_001);
    let future = inbound(&saved, 1_031_000);
    merge_inbox(
        &mut saved,
        vec![old.clone(), fresh.clone(), future.clone()],
        1_000_000,
        1_000_002,
    )
    .unwrap();
    assert!(
        saved.entries[&entry_key(&saved.settings, &old.id)]
            .message
            .review
    );
    assert!(
        !saved.entries[&entry_key(&saved.settings, &fresh.id)]
            .message
            .review
    );
    assert!(
        saved.entries[&entry_key(&saved.settings, &future.id)]
            .message
            .review
    );
    let mut stale = configured();
    let message = inbound(&stale, 1_000_001);
    merge_inbox(&mut stale, vec![message.clone()], 1_000_000, 1_200_000).unwrap();
    assert!(
        stale.entries[&entry_key(&stale.settings, &message.id)]
            .message
            .review
    );
}

#[test]
fn duplicate_refresh_keeps_review_and_terminal_tombstone() {
    let mut saved = configured();
    let message = inbound(&saved, 900);
    merge_inbox(&mut saved, vec![message.clone()], 1000, 1000).unwrap();
    let key = entry_key(&saved.settings, &message.id);
    merge_inbox(&mut saved, vec![message.clone()], 0, 1000).unwrap();
    assert!(saved.entries[&key].message.review);
    let entry = saved.entries.get_mut(&key).unwrap();
    entry.accepted = true;
    entry.message.review = false;
    merge_inbox(&mut saved, vec![message.clone()], 0, 1000).unwrap();
    assert!(saved.entries[&key].accepted);
    terminal(
        saved.entries.get_mut(&key).unwrap(),
        "unknown",
        "Submission interrupted",
    );
    merge_inbox(&mut saved, vec![message], 0, 1000).unwrap();
    assert_eq!(saved.entries[&key].phase, Phase::Terminal);
    assert_eq!(saved.entries[&key].message.state, "unknown");
}

#[test]
fn reconnect_revokes_unsubmitted_acceptance_and_crash_never_replays() {
    let mut saved = configured();
    for phase in [
        Phase::Pending,
        Phase::Claiming,
        Phase::Submitting,
        Phase::ReplyReady,
        Phase::Sending,
    ] {
        let message = inbound(&saved, 1000);
        let key = insert(&mut saved, message, phase);
        let entry = saved.entries.get_mut(&key).unwrap();
        entry.accepted = true;
        entry.reply = Some("Durable final response".into());
    }
    recover(&mut saved);
    for entry in saved.entries.values() {
        match entry.phase {
            Phase::Pending => {
                assert!(entry.message.review);
                assert!(!entry.accepted);
            }
            Phase::ReplyReady => assert_eq!(entry.reply.as_deref(), Some("Durable final response")),
            Phase::Terminal => {
                assert_eq!(entry.message.state, "unknown");
                assert!(entry.finish_pending);
            }
            _ => panic!("Uncertain side effect may not resume automatically"),
        }
    }
}

#[test]
fn changed_duplicate_and_wrong_routing_are_rejected() {
    let mut saved = configured();
    let message = inbound(&saved, 1000);
    merge_inbox(&mut saved, vec![message.clone()], 0, 1000).unwrap();
    let mut changed = message.clone();
    changed.body = "Different payload".into();
    assert!(merge_inbox(&mut saved, vec![changed], 0, 1000).is_err());
    let mut wrong = inbound(&saved, 1000);
    wrong.from_id = "+14155559999".into();
    assert!(merge_inbox(&mut saved, vec![wrong], 0, 1000).is_err());
}

#[test]
fn final_text_excludes_thinking_tools_and_unrelated_turns() {
    let user = json!({"role":"user","content":[{"type":"text","text":"SMS prompt"}]});
    let assistant = json!({"role":"assistant","content":[{"type":"thinking","thinking":"PRIVATE"},{"type":"text","text":"Final answer"}],"stopReason":"stop"});
    assert_eq!(
        final_reply(
            &[],
            &[
                user.clone(),
                json!({"role":"toolResult","content":"SECRET"}),
                assistant.clone()
            ],
            "SMS prompt"
        )
        .unwrap(),
        "Final answer"
    );
    assert!(final_reply(
        &[],
        &[
            user.clone(),
            json!({"role":"user","content":"Manual desktop prompt"}),
            assistant.clone()
        ],
        "SMS prompt"
    )
    .is_err());
    assert!(final_reply(&[], &[user.clone(), json!({"role":"assistant","content":[{"type":"text","text":"Not final"},{"type":"toolCall","name":"bash"}]})], "SMS prompt").is_err());
    assert!(final_reply(
        &[assistant.clone()],
        &[user.clone(), assistant.clone()],
        "SMS prompt"
    )
    .is_err());
    let long = json!({"role":"assistant","content":"🦀".repeat(2000)});
    let text = final_reply(&[], &[user, long], "SMS prompt").unwrap();
    assert_eq!(text.chars().count(), 1200);
    assert!(text.ends_with("[Truncated; see desktop]"));
}

#[test]
fn endpoint_validation_rejects_credential_leaks_and_public_http() {
    for url in [
        "https://relay.example",
        "https://relay.example/base/",
        "http://127.0.0.1:8765",
        "http://[::1]:8765",
        "http://localhost:8765",
    ] {
        assert!(network::base_url(url).is_ok(), "{url}");
    }
    for url in [
        "http://relay.example",
        "https://user:secret@relay.example",
        "https://relay.example?token=secret",
        "https://relay.example#secret",
        "file:///tmp/relay",
        "http://127.0.0.1.evil.example",
        "https://relay.example/a%2fb",
    ] {
        assert!(network::base_url(url).is_err(), "{url}");
    }
}

#[tokio::test]
async fn disabled_status_is_keychain_free_and_secrets_never_persist() {
    let directory = tempfile::tempdir().unwrap();
    let desktop = Desktop::open_with_provider_view(directory.path().into(), offline_provider_view)
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentials::default());
    let clock = Arc::new(TestClock(AtomicI64::new(1_000_000)));
    let connector = SmsConnector::open_with(
        desktop.clone(),
        directory.path().into(),
        credentials.clone(),
        clock.clone(),
    )
    .await
    .unwrap();
    connector.request("sms_status", json!({})).await.unwrap();
    assert_eq!(credentials.reads.load(Ordering::Relaxed), 0);
    let token = "isolated-test-relay-token-at-least-32-chars";
    let key = "isolated-test-pipestreamr-key";
    let status = connector
        .request(
            "sms_configure",
            json!({"enabled":false,"relayToken":token,"apiKey":key}),
        )
        .await
        .unwrap();
    assert_eq!(status["hasRelayToken"], true);
    assert_eq!(status["hasApiKey"], true);
    assert!(status.get("relayToken").is_none());
    let saved = tokio::fs::read(directory.path().join("sms.json"))
        .await
        .unwrap();
    let text = String::from_utf8(saved).unwrap();
    assert!(!text.contains(token));
    assert!(!text.contains(key));
    let reads = credentials.reads.load(Ordering::Relaxed);
    connector.shutdown().await.unwrap();
    let reopened =
        SmsConnector::open_with(desktop, directory.path().into(), credentials.clone(), clock)
            .await
            .unwrap();
    let status = reopened.request("sms_status", json!({})).await.unwrap();
    assert_eq!(status["hasRelayToken"], true);
    assert_eq!(credentials.reads.load(Ordering::Relaxed), reads);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_metadata_is_visible_and_not_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let desktop = Desktop::open_with_provider_view(directory.path().into(), offline_provider_view)
        .await
        .unwrap();
    let bytes = b"{broken SMS journal";
    tokio::fs::write(directory.path().join("sms.json"), bytes)
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentials::default());
    let connector = SmsConnector::open_with(
        desktop,
        directory.path().into(),
        credentials.clone(),
        Arc::new(TestClock(AtomicI64::new(1))),
    )
    .await
    .unwrap();
    let status = connector.request("sms_status", json!({})).await.unwrap();
    assert!(status["error"].as_str().is_some());
    assert!(!connector.is_enabled());
    assert_eq!(credentials.reads.load(Ordering::Relaxed), 0);
    connector.shutdown().await.unwrap();
    assert_eq!(
        tokio::fs::read(directory.path().join("sms.json"))
            .await
            .unwrap(),
        bytes
    );
}

#[tokio::test]
async fn explicit_dismiss_is_durable_and_unknown_cannot_be_accepted() {
    let directory = tempfile::tempdir().unwrap();
    let desktop = Desktop::open_with_provider_view(directory.path().into(), offline_provider_view)
        .await
        .unwrap();
    let connector = SmsConnector::open_with(
        desktop,
        directory.path().into(),
        Arc::new(MemoryCredentials::default()),
        Arc::new(TestClock(AtomicI64::new(1))),
    )
    .await
    .unwrap();
    let message = {
        let mut inner = connector.inner.lock().await;
        inner.saved = configured();
        let mut message = inbound(&inner.saved, 0);
        message.review = true;
        insert(&mut inner.saved, message.clone(), Phase::Pending);
        message
    };
    let status = connector
        .request("sms_dismiss", json!({"id":message.id}))
        .await
        .unwrap();
    assert_eq!(status["messages"][0]["state"], "dismissed");
    assert!(connector
        .request("sms_accept", json!({"id":message.id}))
        .await
        .is_err());
    let saved: Saved = serde_json::from_slice(
        &tokio::fs::read(directory.path().join("sms.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(saved.entries.values().next().unwrap().finish_pending);
    connector.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_preserves_unknown_and_requires_new_acceptance_for_pending_work() {
    let directory = tempfile::tempdir().unwrap();
    let desktop = Desktop::open_with_provider_view(directory.path().into(), offline_provider_view)
        .await
        .unwrap();
    let mut saved = configured();
    let uncertain = inbound(&saved, 100);
    let uncertain_key = insert(&mut saved, uncertain.clone(), Phase::Submitting);
    saved
        .entries
        .get_mut(&uncertain_key)
        .unwrap()
        .message
        .claim_id = Some(Uuid::new_v4().to_string());
    let queued = inbound(&saved, 200);
    let queued_key = insert(&mut saved, queued.clone(), Phase::Pending);
    saved.entries.get_mut(&queued_key).unwrap().accepted = true;
    tokio::fs::write(
        directory.path().join("sms.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .await
    .unwrap();
    let connector = SmsConnector::open_with(
        desktop,
        directory.path().into(),
        Arc::new(MemoryCredentials::default()),
        Arc::new(TestClock(AtomicI64::new(1000))),
    )
    .await
    .unwrap();
    let status = connector.request("sms_status", json!({})).await.unwrap();
    let messages = status["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .find(|message| message["id"] == uncertain.id)
            .unwrap()["state"],
        "unknown"
    );
    assert_eq!(
        messages
            .iter()
            .find(|message| message["id"] == queued.id)
            .unwrap()["review"],
        true
    );
    assert!(connector
        .request("sms_accept", json!({"id":uncertain.id}))
        .await
        .is_err());
    connector
        .request("sms_accept", json!({"id":queued.id}))
        .await
        .unwrap();
    connector.suspend().await.unwrap();
    assert!(
        connector.inner.lock().await.saved.entries[&queued_key]
            .message
            .review
    );
    connector.resume().await.unwrap();
    assert!(!connector.running.load(Ordering::Acquire));
    connector.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_keychain_permission_does_not_block_startup_status_or_shutdown() {
    struct BlockingCredentials {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }
    impl CredentialStore for BlockingCredentials {
        fn get(&self, _: &str) -> Result<Option<String>> {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            if let Some(release) = self.release.lock().unwrap().take() {
                let _ = release.recv();
            }
            Ok(None)
        }
        fn set(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let desktop = Desktop::open_with_provider_view(directory.path().into(), offline_provider_view)
        .await
        .unwrap();
    let mut saved = configured();
    saved.settings.enabled = true;
    tokio::fs::write(
        directory.path().join("sms.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .await
    .unwrap();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let credentials = Arc::new(BlockingCredentials {
        entered: Mutex::new(Some(entered)),
        release: Mutex::new(Some(blocked)),
    });
    let path = directory.path().to_owned();
    let mut opening = tokio::spawn(SmsConnector::open_with(
        desktop,
        path,
        credentials,
        Arc::new(WallClock),
    ));
    waiting.await.unwrap();
    let opened = tokio::time::timeout(Duration::from_millis(200), &mut opening).await;
    let responsive = opened.is_ok();
    let mut status_responsive = false;
    let mut shutdown_responsive = false;
    if let Ok(Ok(Ok(connector))) = opened {
        status_responsive = tokio::time::timeout(
            Duration::from_millis(200),
            connector.request("sms_status", json!({})),
        )
        .await
        .is_ok();
        shutdown_responsive =
            tokio::time::timeout(Duration::from_millis(200), connector.shutdown())
                .await
                .is_ok();
    }
    let _ = release.send(());
    if !responsive {
        if let Ok(Ok(connector)) = opening.await {
            connector.shutdown().await.unwrap();
        }
    }
    assert!(
        responsive,
        "Keychain permission must not hold desktop window creation"
    );
    assert!(
        status_responsive,
        "Status must remain usable during a Keychain prompt"
    );
    assert!(
        shutdown_responsive,
        "Quit must not wait for Keychain permission"
    );
}

#[tokio::test]
async fn outbound_transport_uses_persisted_identity_and_stable_key() {
    use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
    type Capture = Arc<Mutex<Vec<(String, Value)>>>;
    async fn send(
        State(capture): State<Capture>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        capture
            .lock()
            .unwrap()
            .push((headers["idempotency-key"].to_str().unwrap().into(), body));
        Json(json!({"id":Uuid::new_v4().to_string(),"sid":"SM-test-accepted","status":"queued"}))
    }
    let capture: Capture = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/api/v1/messages/send", post(send))
        .with_state(capture.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut saved = configured();
    saved.settings.pipestreamr_url = format!("http://{address}");
    let message = inbound(&saved, 1000);
    let secrets = Secrets {
        relay_token: Some("relay".into()),
        api_key: Some("api".into()),
    };
    network::Network::new()
        .unwrap()
        .send(&saved.settings, &secrets, &message, "Final reply")
        .await
        .unwrap();
    let captured = capture.lock().unwrap();
    assert_eq!(captured[0].0, format!("reply-{}-1", message.id));
    assert_eq!(
        captured[0].1,
        json!({"integration_id":message.source_id,"project_id":message.project_id,"to":message.from_id,"body":"Final reply"})
    );
    server.abort();
}
