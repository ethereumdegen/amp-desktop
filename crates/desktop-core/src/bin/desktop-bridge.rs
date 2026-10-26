//! Explicit development-only transport for exercising the real core in a browser.
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use desktop_core::sms::SmsConnector;
use desktop_core::Desktop;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

#[derive(Deserialize)]
struct Request {
    method: String,
    #[serde(default)]
    args: Value,
}
struct Bridge {
    desktop: Arc<Desktop>,
    sms: Arc<SmsConnector>,
    token: String,
}
fn authenticated(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    if value.len() != token.len() {
        return false;
    }
    value
        .bytes()
        .zip(token.bytes())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}
async fn request(
    State(bridge): State<Arc<Bridge>>,
    headers: HeaderMap,
    Json(request): Json<Request>,
) -> (StatusCode, Json<Value>) {
    if !authenticated(&headers, &bridge.token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(
                json!({"error":"Developer bridge authentication required. Enter the per-run token printed by bun run bridge."}),
            ),
        );
    }
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ![
        "127.0.0.1:4318",
        "localhost:4318",
        "127.0.0.1:1420",
        "localhost:1420",
    ]
    .contains(&host)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"Invalid development bridge host"})),
        );
    }
    if let Some(origin) = headers.get("origin") {
        if !matches!(
            origin.to_str().unwrap_or(""),
            "http://127.0.0.1:1420" | "http://localhost:1420"
        ) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"Only the local development UI may access this bridge"})),
            );
        }
    }
    let result = if request.method.starts_with("sms_") {
        bridge.sms.request(&request.method, request.args).await
    } else {
        bridge.desktop.request(&request.method, request.args).await
    };
    match result {
        Ok(data) => (StatusCode::OK, Json(json!({"data":data}))),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{error:#}")})),
        ),
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let directory = std::env::var_os("OMP_DESKTOP_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME is required"))
                .join("Library/Application Support/com.ompdesktop.development")
        });
    let token = std::env::var("OMP_DESKTOP_BRIDGE_TOKEN")
        .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    anyhow::ensure!(
        token.len() >= 32,
        "Development bridge token must be at least 32 characters"
    );
    let app = Desktop::open(directory.clone()).await?;
    let sms = SmsConnector::open(app.clone(), directory.clone()).await?;
    let bridge = Arc::new(Bridge {
        desktop: app.clone(),
        sms: sms.clone(),
        token: token.clone(),
    });
    let router = Router::new()
        .route("/api/request", post(request))
        .route(
            "/api/health",
            get(|| async { Json(json!({"status":"ready","transport":"development-only"})) }),
        )
        .layer(DefaultBodyLimit::max(24 * 1024 * 1024))
        .with_state(bridge);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:4318").await?;
    println!(
        "AMP Desktop development bridge ready at http://127.0.0.1:4318 (data: {})",
        directory.display()
    );
    println!("Developer bridge token (paste into the browser UI): {token}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
        })
        .await?;
    sms.shutdown().await?;
    app.shutdown_all().await?;
    Ok(())
}
