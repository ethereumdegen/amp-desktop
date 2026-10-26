//! Scripted loopback protocol evidence only: these tests do not claim real-model inference.
use native_agent::{NativeClient, NativeOptions, ProviderConfig};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

async fn fixture(
    replies: Vec<String>,
) -> (
    ProviderConfig,
    mpsc::UnboundedReceiver<Value>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for body in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![];
            let split = loop {
                let mut buf = [0u8; 1024];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
                assert!(bytes.len() < 65536);
            };
            let headers = String::from_utf8_lossy(&bytes[..split]);
            assert!(headers.starts_with("POST /v1/chat/completions "));
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .map(|(_, v)| v.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < split + length {
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
            }
            tx.send(serde_json::from_slice(&bytes[split..split + length]).unwrap())
                .unwrap();
            let headers=format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
            socket.write_all(headers.as_bytes()).await.unwrap();
            // Fragment SSE fields and UTF-8 byte sequences at actual transport boundaries.
            for chunk in body.as_bytes().chunks(7) {
                socket.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
            socket.shutdown().await.unwrap();
        }
    });
    (
        ProviderConfig {
            id: "protocol-fixture".into(),
            name: "Protocol fixture".into(),
            kind: "openai".into(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            models: vec!["fixture-model".into()],
            api_key_env: None,
        },
        rx,
        server,
    )
}
fn event(value: Value) -> String {
    format!("data: {}\r\n\r\n", value)
}
fn tool_response(name: &str, args: Value) -> String {
    format!(
        "{}{}data: [DONE]\r\n\r\n",
        event(
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":null}]})
        ),
        event(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}))
    )
}
fn final_response(text: &str) -> String {
    format!(
        "{}{}data: [DONE]\n\n",
        event(json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]})),
        event(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}))
    )
}
async fn settled(events: &mut tokio::sync::broadcast::Receiver<Value>) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut status = None;
        loop {
            let event = events.recv().await.unwrap();
            if event["type"] == "prompt_result" {
                assert_ne!(event["status"], "error", "{event}");
                assert!(event.get("result").is_none());
                status = event["status"].as_str().map(str::to_owned);
            }
            if event["type"] == "session_settled" {
                return status.expect("prompt result precedes settlement");
            }
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn actual_read_tool_result_is_sent_in_next_provider_request() {
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("marker.txt"), "protocol-marker-π")
        .await
        .unwrap();
    let (config, mut requests, server) = fixture(vec![
        tool_response("read", json!({"path":"marker.txt"})),
        final_response("Read completed π"),
    ])
    .await;
    let client = NativeClient::spawn(
        config,
        NativeOptions {
            cwd: dir.path().into(),
            session_path: dir.path().join("session.json"),
            model: "fixture-model".into(),
            instructions: "Preserved system instructions".into(),
            thinking: None,
        },
    )
    .await
    .unwrap();
    let mut events = client.subscribe();
    client
        .request(json!({"type":"prompt","message":"Inspect marker.txt"}))
        .await
        .unwrap();
    assert_eq!(settled(&mut events).await, "completed");
    requests.recv().await.unwrap();
    let second = requests.recv().await.unwrap();
    let tool = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    assert!(tool["content"]
        .as_str()
        .unwrap()
        .contains("protocol-marker-π"));
    assert_eq!(tool["tool_call_id"], "call_1");
    let history = client
        .request(json!({"type":"get_messages"}))
        .await
        .unwrap();
    assert!(history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "toolResult" && m["isError"] == false));
    client.shutdown().await.unwrap();
    server.await.unwrap();
}
#[tokio::test]
async fn abort_at_approval_never_writes_and_persists_paired_failed_result() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _requests, server) = fixture(vec![tool_response(
        "write",
        json!({"path":"must-not-exist.txt","content":"unapproved"}),
    )])
    .await;
    let options = || NativeOptions {
        cwd: dir.path().into(),
        session_path: dir.path().join("session.json"),
        model: "fixture-model".into(),
        instructions: String::new(),
        thinking: None,
    };
    let client = NativeClient::spawn(config.clone(), options())
        .await
        .unwrap();
    let mut events = client.subscribe();
    client
        .request(json!({"type":"prompt","message":"Write file"}))
        .await
        .unwrap();
    let approval = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = events.recv().await.unwrap();
            if e["type"] == "extension_ui_request" {
                break e;
            }
        }
    })
    .await
    .unwrap();
    assert!(approval["message"]
        .as_str()
        .unwrap()
        .contains("must-not-exist.txt"));
    client.request(json!({"type":"abort"})).await.unwrap();
    let cancellation = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event["type"] == "extension_ui_request" && event["method"] == "cancel" {
                break event;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(cancellation["targetId"], approval["id"]);
    assert_eq!(settled(&mut events).await, "aborted");
    assert!(!dir.path().join("must-not-exist.txt").exists());
    let before = client
        .request(json!({"type":"get_messages"}))
        .await
        .unwrap();
    let failed = before["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "toolResult")
        .unwrap();
    assert_eq!(failed["toolCallId"], "call_1");
    assert_eq!(failed["isError"], true);
    client.shutdown().await.unwrap();
    let restored = NativeClient::spawn(config, options()).await.unwrap();
    assert_eq!(
        before,
        restored
            .request(json!({"type":"get_messages"}))
            .await
            .unwrap()
    );
    restored.shutdown().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn denied_approval_is_a_tool_error_and_model_can_continue() {
    let dir = tempfile::tempdir().unwrap();
    let (config, mut requests, server) = fixture(vec![
        tool_response("write", json!({"path":"denied.txt","content":"unapproved"})),
        final_response("The user declined the change."),
    ])
    .await;
    let client = NativeClient::spawn(
        config,
        NativeOptions {
            cwd: dir.path().into(),
            session_path: dir.path().join("session.json"),
            model: "fixture-model".into(),
            instructions: String::new(),
            thinking: None,
        },
    )
    .await
    .unwrap();
    let mut events = client.subscribe();
    client
        .request(json!({"type":"prompt","message":"Write denied.txt"}))
        .await
        .unwrap();
    let approval = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = events.recv().await.unwrap();
            if e["type"] == "extension_ui_request" {
                break e;
            }
        }
    })
    .await
    .unwrap();
    client
        .respond(json!({"type":"extension_ui_response","id":approval["id"],"confirmed":false}))
        .await
        .unwrap();
    assert_eq!(settled(&mut events).await, "completed");
    requests.recv().await.unwrap();
    let continuation = requests.recv().await.unwrap();
    let _tool = continuation["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    assert!(!dir.path().join("denied.txt").exists());
    let history = client
        .request(json!({"type":"get_messages"}))
        .await
        .unwrap();
    assert!(history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "toolResult" && m["isError"] == true));
    client.shutdown().await.unwrap();
    server.await.unwrap();
}
