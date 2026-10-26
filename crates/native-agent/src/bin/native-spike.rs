//! Real-provider probe. Requires configured environment credentials (or saved keychain key).
//! NATIVE_SPIKE_PROVIDER=openai|openrouter|anthropic, NATIVE_SPIKE_MODEL=<id>.
use anyhow::{bail, Context, Result};
use native_agent::{default_providers, has_api_key, list_models, NativeClient, NativeOptions};
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let id = std::env::var("NATIVE_SPIKE_PROVIDER").unwrap_or_else(|_| "openai".into());
    let mut config = default_providers()
        .into_iter()
        .find(|p| p.id == id)
        .context("Unknown NATIVE_SPIKE_PROVIDER")?;
    if !has_api_key(&config) {
        bail!("Native real-provider spike requires a keychain key or the selected provider's API-key environment variable; no inference was performed");
    }
    let model = std::env::var("NATIVE_SPIKE_MODEL")
        .ok()
        .or_else(|| config.models.first().cloned())
        .context("Set NATIVE_SPIKE_MODEL for this provider")?;
    if !config.models.contains(&model) {
        config.models.push(model.clone());
    }
    let models = list_models(&config).await?;
    println!("Real provider model discovery: {} IDs", models.len());
    let scratch = std::env::temp_dir().join(format!("omp-native-spike-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&scratch).await?;
    let outcome = exercise(config, model, &scratch).await;
    let cleanup = tokio::fs::remove_dir_all(&scratch).await;
    outcome?;
    cleanup?;
    Ok(())
}
async fn exercise(
    config: native_agent::ProviderConfig,
    model: String,
    scratch: &std::path::Path,
) -> Result<()> {
    let marker = format!("NATIVE_REAL_{}", Uuid::new_v4());
    tokio::fs::write(scratch.join("native-marker.txt"), &marker).await?;
    let options = || {
        NativeOptions{cwd:scratch.into(),session_path:scratch.join("session.json"),model:model.clone(),instructions:"Use read when asked to inspect a file. Do not write files or execute shell commands in this probe.".into(),thinking:None}
    };
    let client = NativeClient::spawn(config.clone(), options()).await?;
    let mut events = client.subscribe();
    client.request(json!({"type":"prompt","message":"Use the read tool to read native-marker.txt, then reply with exactly the file's contents. Do not guess the contents."})).await?;
    let observed = tokio::time::timeout(Duration::from_secs(180), async {
        let mut read = false;
        loop {
            let event = events.recv().await?;
            if event["type"] == "extension_ui_request" && event["method"] != "cancel" {
                client
                    .respond(
                        json!({"type":"extension_ui_response","id":event["id"],"confirmed":false}),
                    )
                    .await?;
            }
            if event["type"] == "message_end"
                && event["message"]["role"] == "toolResult"
                && event["message"]["toolName"] == "read"
                && event["message"]["isError"] == false
            {
                read = true;
            }
            if event["type"] == "prompt_result" && event["status"] == "error" {
                bail!(
                    "Real provider inference failed: {}",
                    event["error"]["message"]
                        .as_str()
                        .unwrap_or("native runtime error")
                );
            }
            if event["type"] == "session_settled" {
                return Ok::<_, anyhow::Error>(read);
            }
        }
    })
    .await;
    let read = match observed {
        Ok(result) => result?,
        Err(_) => {
            client.shutdown().await?;
            bail!("Real-provider spike timed out");
        }
    };
    let messages = client.request(json!({"type":"get_messages"})).await?;
    let final_text = messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .map(|m| m["content"].to_string())
        .unwrap_or_default();
    client.shutdown().await?;
    if !read || !final_text.contains(&marker) {
        bail!("Provider did not complete the required actual read-tool loop and marker response");
    }
    let resumed = NativeClient::spawn(config, options()).await?;
    let restored = resumed.request(json!({"type":"get_messages"})).await?;
    resumed.shutdown().await?;
    if restored != messages {
        bail!("Native session history did not survive restart");
    }
    println!("Real streamed inference, actual read-tool continuation, and persistent session restart passed ({})",model);
    Ok(())
}
