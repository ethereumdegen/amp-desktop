//! Real installed-engine probe. Runs paid inference using the selected engine
//! credentials, with all work/session/extension artifacts confined to scratch.
use anyhow::{bail, ensure, Context, Result};
use omp_control::{discover, Installation, LaunchOptions, RpcClient};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{sync::broadcast, time::timeout};
use uuid::Uuid;

const EXTENSION: &str = r#"
export default function (pi: any) {
  pi.registerCommand("desktop-spike-dialog", {
    description: "Scratch-only desktop transport UI and approval probe",
    handler: async (_args: string, ctx: any) => {
      const value = await ctx.ui.input("Desktop spike input", "Probe value");
      if (value !== "desktop-spike-value") throw new Error("Spike input round-trip failed");
      const selection = await ctx.ui.select("Desktop spike select", ["first", "second"]);
      if (selection !== "second") throw new Error("Spike select round-trip failed");
      const edited = await ctx.ui.editor("Desktop spike editor", "initial");
      if (edited !== "desktop-spike-edited") throw new Error("Spike editor round-trip failed");
      if (!await ctx.ui.confirm("Desktop spike approve", "Write approved.txt inside this temporary workspace?")) {
        throw new Error("Spike approval round-trip failed");
      }
      await Bun.write(ctx.cwd + "/approved.txt", "desktop-spike-value");
      if (await ctx.ui.confirm("Desktop spike deny", "This denied write must never run.")) {
        await Bun.write(ctx.cwd + "/denied.txt", "incorrect");
        throw new Error("Spike denial round-trip failed");
      }
      ctx.ui.notify("OMP_SPIKE_DIALOGS_OK", "info");
    }
  });
}
"#;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn options(scratch: &Path, model: Option<&str>, session: Option<String>) -> LaunchOptions {
    let mut extra_args = vec![
        "--no-tools".into(),
        "--no-skills".into(),
        "--no-rules".into(),
        "--no-extensions".into(),
        "--no-title".into(),
        "--no-lsp".into(),
        "--trusted-extension".into(),
        scratch
            .join("spike-extension.ts")
            .to_string_lossy()
            .into_owned(),
    ];
    if session.is_none() {
        extra_args.extend([
            "--session-dir".into(),
            scratch.join("sessions").to_string_lossy().into_owned(),
        ]);
    }
    LaunchOptions { cwd: scratch.to_owned(), session, model: model.map(str::to_owned),
        instructions: Some("This is a transport integration probe in a temporary workspace. Follow the user's exact output instructions. Do not invoke tools.".into()),
        extra_args, ..LaunchOptions::default() }
}

async fn event(receiver: &mut broadcast::Receiver<Value>) -> Result<Value> {
    let frame = receiver
        .recv()
        .await
        .context("OMP event stream closed or lagged during spike")?;
    match frame.get("type").and_then(Value::as_str) {
        Some("process_exit" | "transport_error") => bail!("OMP transport ended during spike"),
        Some("response") if frame.get("success") == Some(&Value::Bool(false)) => {
            bail!("OMP emitted a delayed command failure")
        }
        _ => Ok(frame),
    }
}

async fn dialogs(client: &RpcClient, scratch: &Path) -> Result<()> {
    let mut receiver = client.subscribe();
    let request = client.request(json!({"type":"prompt","message":"/desktop-spike-dialog"}));
    tokio::pin!(request);
    let mut accepted = false;
    let mut notified = false;
    let mut methods = HashSet::new();
    timeout(Duration::from_secs(60), async {
        while !accepted || !notified {
            tokio::select! {
                response = &mut request, if !accepted => { response?; accepted = true; }
                frame = event(&mut receiver) => {
                    let frame = frame?;
                    if frame["type"] != "extension_ui_request" { continue; }
                    let method = frame["method"].as_str().unwrap_or("");
                    if method == "notify" {
                        if frame["message"] == "OMP_SPIKE_DIALOGS_OK" { notified = true; }
                        continue;
                    }
                    let id = frame["id"].as_str().context("Dialog id missing")?;
                    let title = frame["title"].as_str().unwrap_or("");
                    let mut response = match (method, title) {
                        ("input", "Desktop spike input") => json!({"value":"desktop-spike-value"}),
                        ("select", "Desktop spike select") => json!({"value":"second"}),
                        ("editor", "Desktop spike editor") => json!({"value":"desktop-spike-edited"}),
                        ("confirm", "Desktop spike approve") => json!({"confirmed":true}),
                        ("confirm", "Desktop spike deny") => json!({"confirmed":false}),
                        _ => {
                            client.respond(json!({"type":"extension_ui_response","id":id,"cancelled":true})).await?;
                            bail!("Unexpected dialog; cancelled without approving");
                        }
                    };
                    response["type"] = json!("extension_ui_response");
                    response["id"] = json!(id);
                    client.respond(response).await?;
                    methods.insert(title.to_owned());
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await.context("Dialog probe timed out")??;
    ensure!(methods.len() == 5, "Not all dialog paths were exercised");
    ensure!(
        tokio::fs::read_to_string(scratch.join("approved.txt")).await? == "desktop-spike-value",
        "Approved write did not execute"
    );
    ensure!(
        !tokio::fs::try_exists(scratch.join("denied.txt")).await?,
        "Denied write executed"
    );
    println!("PASS dialogs: input/select/editor and explicit confirm allow/deny with real scratch filesystem effect");
    Ok(())
}

async fn messages(client: &RpcClient) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    let mut total = None;
    loop {
        let mut command = json!({"type":"get_messages_page","limit":256});
        if let Some(cursor) = &cursor {
            command["cursor"] = json!(cursor);
        }
        let page = client.request(command).await?;
        let count = page["totalMessages"]
            .as_u64()
            .context("Message page total missing")?;
        if let Some(total) = total {
            ensure!(total == count, "Message page total changed");
        }
        total = Some(count);
        let page_messages = page["messages"]
            .as_array()
            .context("Message page content missing")?;
        messages.extend(page_messages.iter().cloned());
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        match &cursor {
            Some(cursor) => {
                ensure!(seen.insert(cursor.clone()), "Message cursor repeated");
            }
            None => break,
        }
    }
    ensure!(
        messages.len() as u64 == total.unwrap_or(0),
        "Incomplete persisted message pagination"
    );
    Ok(messages)
}

fn assistant_has_marker(messages: &[Value], marker: &str) -> bool {
    messages.iter().any(|message| {
        message["role"] == "assistant"
            && match &message["content"] {
                Value::String(text) => text.contains(marker),
                Value::Array(blocks) => blocks.iter().any(|block| {
                    block["type"] == "text"
                        && block["text"]
                            .as_str()
                            .is_some_and(|text| text.contains(marker))
                }),
                _ => false,
            }
    })
}

async fn inference(client: &RpcClient, marker: &str) -> Result<()> {
    let mut receiver = client.subscribe();
    client.request(json!({"type":"prompt","message":format!("Reply with exactly {marker}. Nothing else.")})).await?;
    let mut updates = 0;
    let mut completed = false;
    let mut settled = false;
    timeout(Duration::from_secs(180), async {
        while !completed || !settled {
            let frame = event(&mut receiver).await?;
            match frame["type"].as_str() {
                Some("message_update") => { updates += 1; }
                Some("prompt_result") => {
                    ensure!(frame["status"] == "completed", "Real inference did not complete successfully (check installed OMP authentication/model)");
                    completed = true;
                }
                Some("session_settled") => { settled = true; }
                Some("extension_ui_request") if matches!(frame["method"].as_str(), Some("select"|"confirm"|"input"|"editor"|"ask"|"open_url")) => {
                    client.respond(json!({"type":"extension_ui_response","id":frame["id"],"cancelled":true})).await?;
                    bail!("Unexpected inference dialog: method={} title={}; cancelled", frame["method"], frame["title"]);
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await.context("Real inference timed out")??;
    ensure!(updates > 0, "No streamed message updates observed");
    ensure!(
        assistant_has_marker(&messages(client).await?, marker),
        "Assistant did not produce unique marker"
    );
    println!("PASS real inference: {updates} streamed updates, assistant unique marker, prompt_result=completed and session_settled");
    Ok(())
}

async fn cancel(client: &RpcClient) -> Result<()> {
    let mut receiver = client.subscribe();
    client.request(json!({"type":"prompt","message":"Generate 10000 numbered lines, one line per integer, including the square and cube. Start at 1, do not abbreviate or stop early."})).await?;
    timeout(Duration::from_secs(180), async {
        loop {
            let frame = event(&mut receiver).await?;
            if frame["type"] == "prompt_result" {
                bail!("Cancellation probe completed before first streaming text");
            }
            if frame["type"] == "message_update"
                && frame["assistantMessageEvent"]["type"] == "text_delta"
            {
                break;
            }
            if frame["type"] == "extension_ui_request"
                && matches!(
                    frame["method"].as_str(),
                    Some("select" | "confirm" | "input" | "editor" | "ask" | "open_url")
                )
            {
                client
                    .respond(
                        json!({"type":"extension_ui_response","id":frame["id"],"cancelled":true}),
                    )
                    .await?;
                bail!("Unexpected cancellation probe dialog; cancelled");
            }
        }
        client.request(json!({"type":"abort"})).await?;
        let mut aborted = false;
        let mut settled = false;
        while !aborted || !settled {
            let frame = event(&mut receiver).await?;
            if frame["type"] == "prompt_result" {
                ensure!(
                    frame["status"] == "aborted",
                    "Abort did not produce an aborted prompt result"
                );
                aborted = true;
            }
            if frame["type"] == "session_settled" {
                settled = true;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Streaming cancellation timed out")??;
    let state = client.request(json!({"type":"get_state"})).await?;
    ensure!(
        state["isStreaming"] == false && state["isSettled"] == true,
        "Session remained busy after cancellation"
    );
    println!("PASS cancellation: abort after real text delta, prompt_result=aborted, session_settled, idle state");
    Ok(())
}

async fn model_roundtrip(client: &RpcClient) -> Result<()> {
    let state = client.request(json!({"type":"get_state"})).await?;
    let original = &state["model"];
    let provider = original["provider"]
        .as_str()
        .context("Installed OMP has no selected model")?;
    let id = original["id"]
        .as_str()
        .context("Installed OMP selected model lacks id")?;
    let available = client
        .request(json!({"type":"get_available_models"}))
        .await?;
    let models = available["models"]
        .as_array()
        .context("OMP model list missing")?;
    ensure!(!models.is_empty(), "Installed OMP has no available models");
    let alternate = models
        .iter()
        .find(|model| model["provider"] != provider || model["id"] != id);
    if let Some(model) = alternate {
        let result = client
            .request(json!({"type":"set_model","provider":model["provider"],"modelId":model["id"]}))
            .await;
        let restored = client
            .request(json!({"type":"set_model","provider":provider,"modelId":id}))
            .await;
        result?;
        restored?;
        let state = client.request(json!({"type":"get_state"})).await?;
        ensure!(
            state["model"]["provider"] == provider && state["model"]["id"] == id,
            "Model restoration failed"
        );
        println!("PASS models: {} available; alternate selection and restoration scoped to scratch session", models.len());
    } else {
        client
            .request(json!({"type":"set_model","provider":provider,"modelId":id}))
            .await?;
        println!(
            "PASS models: one available; current selection accepted (alternate switch unavailable)"
        );
    }
    Ok(())
}

async fn run(installation: &Installation, scratch: &Path, model: Option<&str>) -> Result<()> {
    let marker = format!("OMP_DESKTOP_PROBE_{}", Uuid::new_v4().simple());
    let client = RpcClient::spawn(installation, options(scratch, model, None)).await?;
    let exercise = async {
        ensure!(
            messages(&client).await?.is_empty(),
            "Fresh scratch session unexpectedly contains prior messages"
        );
        dialogs(&client, scratch).await?;
        model_roundtrip(&client).await?;
        inference(&client, &marker).await?;
        cancel(&client).await?;
        let state = client.request(json!({"type":"get_state"})).await?;
        let session = state["sessionFile"]
            .as_str()
            .context("OMP did not expose persistent session file")?
            .to_owned();
        ensure!(
            Path::new(&session).starts_with(scratch),
            "OMP session escaped scratch directory"
        );
        ensure!(
            tokio::fs::metadata(&session).await?.len() > 0,
            "OMP did not persist session"
        );
        Ok::<_, anyhow::Error>(session)
    }
    .await;
    let stopped = client.shutdown().await;
    let session = exercise?;
    stopped?;
    ensure!(
        client.pid().is_none(),
        "OMP process remained alive after shutdown"
    );
    println!("PASS shutdown: original RPC child reaped");
    let resumed =
        RpcClient::spawn(installation, options(scratch, None, Some(session.clone()))).await?;
    let result = async {
        let state = resumed.request(json!({"type":"get_state"})).await?;
        ensure!(
            state["sessionFile"] == session,
            "Resume opened another session"
        );
        let messages = messages(&resumed).await?;
        // OMP intentionally excludes aborted assistant turns from resumed provider context.
        ensure!(
            assistant_has_marker(&messages, &marker),
            "Resume lost the completed assistant marker"
        );
        println!(
            "PASS persistence/resume: {} messages restored and original assistant marker present",
            messages.len()
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = resumed.shutdown().await;
    result?;
    stopped?;
    println!("PASS resumed shutdown: child reaped; scratch cleaned on exit");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut executable = None;
    let mut model = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--omp" => executable = Some(args.next().context("--omp requires an executable path")?),
            "--model" => model = Some(args.next().context("--model requires provider/model")?),
            _ => bail!("Usage: omp-spike [--omp /path/to/omp] [--model provider/model]"),
        }
    }
    let installation = discover(executable.as_deref()).await?;
    println!(
        "Installed OMP: {} ({})",
        installation.path.display(),
        installation.version
    );
    println!("This probe uses real inference and may incur provider charges. Only scratch launch disables tools/skills/rules and loads an exact trusted test extension; production launch preserves user policy.");
    let scratch =
        Scratch(std::env::temp_dir().join(format!("omp-desktop-spike-{}", Uuid::new_v4())));
    tokio::fs::create_dir_all(scratch.0.join("sessions")).await?;
    tokio::fs::write(scratch.0.join("spike-extension.ts"), EXTENSION).await?;
    run(&installation, &scratch.0, model.as_deref()).await
}
