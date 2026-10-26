//! Desktop-owned conversation catalog. Engine files remain authoritative and are never copied.
use super::*;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SavedSession {
    id: String,
    session_file: String,
    engine: String,
    cwd: String,
    native_provider: Option<String>,
    profile: Option<String>,
    #[serde(default)]
    model: Option<ModelChoice>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionSummary {
    id: String,
    title: String,
    session_file: String,
    modified_at: Option<u64>,
    current: bool,
}

pub(super) fn remember_current(record: &mut BotRecord, title: Option<String>) {
    let Some(path) = record.session_file.as_ref() else {
        return;
    };
    if let Some(session) = record
        .sessions
        .iter_mut()
        .find(|session| &session.session_file == path)
    {
        session.model = record.model.clone();
        session.thinking = record.thinking.clone();
        if title.is_some() {
            session.title = title;
        }
        return;
    }
    record.sessions.push(SavedSession {
        id: Uuid::new_v4().to_string(),
        session_file: path.clone(),
        engine: record.engine.clone(),
        cwd: record.cwd.clone(),
        native_provider: record.native_provider.clone(),
        profile: record.profile.clone(),
        model: record.model.clone(),
        thinking: record.thinking.clone(),
        title,
    });
}

pub(super) fn adopt_legacy(directory: &Path, record: &mut BotRecord) {
    if record.engine == "native"
        && record.session_file.is_none()
        && record.session_directory.is_none()
    {
        let legacy = directory
            .join("native-sessions")
            .join(format!("{}.json", record.id));
        if legacy.is_file() {
            record.session_file = Some(legacy.to_string_lossy().into_owned());
        }
    }
    if record
        .session_file
        .as_ref()
        .is_some_and(|path| Path::new(path).is_file())
    {
        remember_current(record, None);
    }
}

fn compatible(record: &BotRecord, session: &SavedSession) -> bool {
    session.engine == record.engine
        && session.cwd == record.cwd
        && session.native_provider == record.native_provider
        && session.profile == record.profile
}

fn message_title(message: &Value) -> Option<String> {
    if message["role"] != "user" {
        return None;
    }
    let text = message["content"].as_str().or_else(|| {
        message["content"]
            .as_array()?
            .iter()
            .find_map(|block| block["text"].as_str())
    })?;
    let mut title = String::new();
    let mut whitespace = false;
    for character in text.trim().chars().take(120) {
        if character.is_whitespace() {
            whitespace = true;
        } else {
            if whitespace && !title.is_empty() {
                title.push(' ');
            }
            title.push(character);
            whitespace = false;
        }
    }
    (!title.is_empty()).then_some(title)
}

pub(super) fn conversation_title(messages: &[Value]) -> Option<String> {
    messages.iter().find_map(message_title)
}

async fn omp_header(record: &BotRecord, session: &SavedSession) -> Result<Value> {
    let file = tokio::fs::File::open(&session.session_file).await?;
    let mut lines = BufReader::new(file).lines();
    let mut header: Value = serde_json::from_str(
        &lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("Empty OMP session file"))?,
    )?;
    if header["type"] == "title" {
        if header["v"] != 1
            || !header["title"].is_string()
            || !header["updatedAt"].is_string()
            || !header["pad"].is_string()
        {
            bail!("Invalid OMP title record");
        }
        let title: String = header["title"]
            .as_str()
            .unwrap()
            .chars()
            .take(120)
            .collect();
        header = serde_json::from_str(
            &lines
                .next_line()
                .await?
                .ok_or_else(|| anyhow!("Missing OMP session header"))?,
        )?;
        if !title.trim().is_empty() {
            header["title"] = json!(title);
        }
    }
    if header["type"] != "session" || header["id"].as_str().is_none() {
        bail!("Invalid OMP conversation");
    }
    let cwd = header["cwd"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing OMP workspace"))?;
    if tokio::fs::canonicalize(cwd).await? != tokio::fs::canonicalize(&record.cwd).await? {
        bail!("Conversation belongs to a different workspace");
    }
    Ok(header)
}

async fn inspect(record: &BotRecord, session: &SavedSession) -> Result<()> {
    if !compatible(record, session) {
        bail!("Conversation is not compatible with this bot");
    }
    if !tokio::fs::metadata(&session.session_file).await?.is_file() {
        bail!("Saved conversation is not a file");
    }
    if record.engine == "omp" {
        omp_header(record, session).await?;
    }
    // The native loader validates version/provider and repairs interrupted turns
    // in memory. Do not decode image-bearing histories twice merely to select one.
    Ok(())
}

fn runtime_busy(state: &Value) -> bool {
    state["isStreaming"] == true
        || state["isCompacting"] == true
        || state["isSettled"] == false
        || state["hasPendingAsyncWork"] == true
        || state["queuedMessageCount"]
            .as_u64()
            .is_some_and(|count| count > 0)
        || ["steering", "followUp", "liveSteered"].iter().any(|kind| {
            state["queuedMessages"][*kind]
                .as_array()
                .is_some_and(|queue| !queue.is_empty())
        })
        || state["queuedMessages"]
            .as_array()
            .is_some_and(|queue| !queue.is_empty())
        || state["steeringMessages"]
            .as_array()
            .is_some_and(|queue| !queue.is_empty())
        || state["followUpMessages"]
            .as_array()
            .is_some_and(|queue| !queue.is_empty())
}

impl Desktop {
    pub(super) async fn list_sessions(&self, id: &str) -> Result<Value> {
        let gate = self.gate(id)?;
        let _guard = gate.lock().await;
        let record = {
            let mut state = self.state.lock().unwrap();
            let bot = state
                .bots
                .get_mut(id)
                .ok_or_else(|| anyhow!("Bot not found"))?;
            adopt_legacy(&self.directory, &mut bot.record);
            if bot.client.is_none() {
                bot.view.session_file = bot.record.session_file.clone();
            }
            bot.record.clone()
        };
        // Persist opaque IDs before exposing them, including legacy adoption.
        self.save().await?;
        let mut sessions = Vec::new();
        for session in &record.sessions {
            if !compatible(&record, session) {
                continue;
            }
            let Ok(metadata) = tokio::fs::metadata(&session.session_file).await else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let mut title = session.title.clone();
            if record.engine == "omp" {
                let Ok(header) = omp_header(&record, session).await else {
                    continue;
                };
                if let Some(engine_title) = header["title"]
                    .as_str()
                    .filter(|title| !title.trim().is_empty())
                {
                    title = Some(engine_title.chars().take(120).collect());
                }
            }
            let title = title.unwrap_or_else(|| {
                Path::new(&session.session_file)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .chars()
                    .take(120)
                    .collect()
            });
            let modified_at = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64);
            sessions.push(SessionSummary {
                id: session.id.clone(),
                title,
                session_file: session.session_file.clone(),
                modified_at,
                current: record.session_file.as_ref() == Some(&session.session_file),
            });
        }
        sessions.sort_by(|a, b| {
            b.modified_at
                .cmp(&a.modified_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(json!({"sessions": sessions}))
    }

    pub(super) async fn switch_session_locked(
        self: &Arc<Self>,
        id: &str,
        selected: Option<&str>,
    ) -> Result<()> {
        self.require_idle(id)?;
        if let Ok((client, _)) = self.runtime(id) {
            let state = client.request(json!({"type":"get_state"})).await?;
            if runtime_busy(&state) {
                bail!("Wait until this bot is settled before changing conversations");
            }
            {
                let mut all = self.state.lock().unwrap();
                let bot = all.bots.get_mut(id).unwrap();
                Self::apply_state(bot, state);
            }
            self.require_idle(id)?;
        }
        let (old_record, old_view, was_running) = {
            let mut state = self.state.lock().unwrap();
            let bot = state
                .bots
                .get_mut(id)
                .ok_or_else(|| anyhow!("Bot not found"))?;
            adopt_legacy(&self.directory, &mut bot.record);
            if bot.client.is_none() {
                bot.view.session_file = bot.record.session_file.clone();
            }
            (bot.record.clone(), bot.view.clone(), bot.client.is_some())
        };
        let target = match selected {
            Some(selected) => {
                let session = old_record
                    .sessions
                    .iter()
                    .find(|session| session.id == selected)
                    .ok_or_else(|| {
                        anyhow!("Select a saved conversation from this bot's session list")
                    })?;
                inspect(&old_record, session).await?;
                if old_record.session_file.as_ref() == Some(&session.session_file) {
                    self.start_locked(id).await?;
                    return Ok(());
                }
                Some(session.clone())
            }
            None => None,
        };
        let fresh_directory = if selected.is_none() {
            let parent = self.directory.join("sessions").join(id);
            tokio::fs::create_dir_all(&parent).await?;
            let directory = parent.join(Uuid::new_v4().to_string());
            // Reserve exclusively before stopping the previous runtime.
            tokio::fs::create_dir(&directory).await?;
            Some(directory.to_string_lossy().into_owned())
        } else {
            None
        };
        self.save().await?;
        self.stop_runtime_locked(id, false).await?;
        {
            let mut state = self.state.lock().unwrap();
            let bot = state.bots.get_mut(id).unwrap();
            bot.record.session_file = target.as_ref().map(|session| session.session_file.clone());
            if let Some(session) = &target {
                bot.record.model = session.model.clone();
                bot.record.thinking = session.thinking.clone();
            }
            bot.record.session_directory = fresh_directory;
            bot.view = BotView::from_record(&bot.record);
            bot.view.revision = old_view.revision + 1;
        }
        let transition = async {
            self.save().await?;
            self.start_locked(id).await
        }
        .await;
        if let Err(error) = transition {
            // No engine command has submitted a prompt. Restore the old selection even
            // when the replacement could not start or its metadata could not be saved.
            let stop_error = self.stop_runtime_locked(id, false).await.err();
            {
                let mut state = self.state.lock().unwrap();
                let bot = state.bots.get_mut(id).unwrap();
                bot.record = old_record;
                bot.view = old_view;
                bot.view.is_running = bot.client.is_some();
                bot.view.pid = bot.client.as_ref().and_then(Runtime::pid);
                bot.view.status = if bot.client.is_some() {
                    "error"
                } else {
                    "stopped"
                }
                .into();
            }
            let restore_save = self.save().await.err();
            let restart_error = if was_running && stop_error.is_none() {
                self.start_locked(id).await.err()
            } else {
                None
            };
            let mut message = format!(
                "Conversation switch failed: {error}. The previous conversation remains selected."
            );
            for recovery in [stop_error, restore_save, restart_error]
                .into_iter()
                .flatten()
            {
                message.push_str(&format!(" Recovery: {recovery}."));
            }
            bail!("{message}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn native_bot(root: &Path, cwd: &Path) -> (Arc<Desktop>, String) {
        let app = Desktop::open_with_provider_view(root.to_path_buf(), offline_provider_view)
            .await
            .unwrap();
        app.request(
            "create_bot",
            json!({"name":"Sessions","cwd":cwd,"engine":"native","nativeProvider":"openai"}),
        )
        .await
        .unwrap();
        let id = app.snapshot().bots[0].id.clone();
        (app, id)
    }

    async fn legacy(root: &Path, id: &str) -> (PathBuf, Vec<u8>) {
        let path = root.join("native-sessions").join(format!("{id}.json"));
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let bytes = serde_json::to_vec_pretty(&json!({
            "version":1,"sessionId":Uuid::new_v4().to_string(),"providerId":"openai",
            "model":"saved-model","thinking":"off","messages":[
                {"role":"user","content":[{"type":"text","text":"Saved conversation"}]},
                {"role":"assistant","content":[{"type":"text","text":"Already answered"}]}
            ]
        }))
        .unwrap();
        tokio::fs::write(&path, &bytes).await.unwrap();
        (path, bytes)
    }

    #[tokio::test]
    async fn new_resume_preserves_legacy_bytes_and_transcript_without_replay() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (app, id) = native_bot(root.path(), cwd.path()).await;
        let (old_path, old_bytes) = legacy(root.path(), &id).await;
        let listed = app
            .request("list_sessions", json!({"id":id}))
            .await
            .unwrap();
        let old_id = listed["sessions"][0]["id"].as_str().unwrap().to_owned();
        assert_eq!(listed["sessions"][0]["current"], true);
        app.request("start_bot", json!({"id":id})).await.unwrap();
        app.request("new_session", json!({"id":id})).await.unwrap();
        let fresh = app.snapshot().bots.remove(0);
        assert_eq!(fresh.id, id);
        assert!(fresh.messages.is_empty());
        assert_eq!(fresh.model.as_ref().unwrap().id, "saved-model");
        assert_ne!(fresh.session_file.as_deref(), old_path.to_str());
        assert_eq!(tokio::fs::read(&old_path).await.unwrap(), old_bytes);
        let fresh_path = fresh.session_file.unwrap();
        let fresh_bytes = tokio::fs::read(&fresh_path).await.unwrap();
        app.request("resume_session", json!({"id":id,"sessionId":old_id}))
            .await
            .unwrap();
        let resumed = app.snapshot().bots.remove(0);
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(
            resumed.messages[1]["content"][0]["text"],
            "Already answered"
        );
        assert_eq!(resumed.session_file.as_deref(), old_path.to_str());
        assert_eq!(tokio::fs::read(&old_path).await.unwrap(), old_bytes);
        assert_eq!(tokio::fs::read(&fresh_path).await.unwrap(), fresh_bytes);
        app.shutdown_all().await.unwrap();
        drop(app);
        let restored =
            Desktop::open_with_provider_view(root.path().to_path_buf(), offline_provider_view)
                .await
                .unwrap();
        let listed = restored
            .request("list_sessions", json!({"id":id}))
            .await
            .unwrap();
        assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);
        assert!(listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["id"] == old_id && session["current"] == true));
    }

    #[tokio::test]
    async fn mutation_rejects_work_approvals_and_contended_gate_without_changing_selection() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (app, id) = native_bot(root.path(), cwd.path()).await;
        let (path, bytes) = legacy(root.path(), &id).await;
        app.request("list_sessions", json!({"id":id}))
            .await
            .unwrap();
        for status in ["working", "needs_input", "starting"] {
            app.state
                .lock()
                .unwrap()
                .bots
                .get_mut(&id)
                .unwrap()
                .view
                .status = status.into();
            assert!(app.request("new_session", json!({"id":id})).await.is_err());
            assert_eq!(
                app.state.lock().unwrap().bots[&id]
                    .record
                    .session_file
                    .as_deref(),
                path.to_str()
            );
        }
        app.state
            .lock()
            .unwrap()
            .bots
            .get_mut(&id)
            .unwrap()
            .view
            .status = "stopped".into();
        let gate = app.gate(&id).unwrap();
        let _guard = gate.lock().await;
        assert!(app.request("new_session", json!({"id":id})).await.is_err());
        assert_eq!(tokio::fs::read(path).await.unwrap(), bytes);
        assert!(runtime_busy(
            &json!({"isSettled":true,"hasPendingAsyncWork":true})
        ));
        assert!(runtime_busy(
            &json!({"isSettled":true,"queuedMessages":[{}]})
        ));
        assert!(runtime_busy(
            &json!({"isSettled":true,"queuedMessages":{"steering":[{}]}})
        ));
        assert!(runtime_busy(
            &json!({"isSettled":true,"queuedMessageCount":1})
        ));
        assert!(runtime_busy(&json!({"isSettled":true,"isCompacting":true})));
        assert!(!runtime_busy(
            &json!({"isSettled":true,"queuedMessages":[]})
        ));
    }

    #[tokio::test]
    async fn failed_new_rolls_back_and_missing_resume_does_not_stop_current_runtime() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (app, id) = native_bot(root.path(), cwd.path()).await;
        let (path, bytes) = legacy(root.path(), &id).await;
        let listed = app
            .request("list_sessions", json!({"id":id}))
            .await
            .unwrap();
        let old_id = listed["sessions"][0]["id"].clone();
        app.state
            .lock()
            .unwrap()
            .providers
            .iter_mut()
            .find(|provider| provider.config.id == "openai")
            .unwrap()
            .config
            .models
            .clear();
        assert!(app.request("new_session", json!({"id":id})).await.is_err());
        assert_eq!(
            app.state.lock().unwrap().bots[&id]
                .record
                .session_file
                .as_deref(),
            path.to_str()
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
        let saved: Value = serde_json::from_slice(
            &tokio::fs::read(root.path().join("desktop.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["bots"][0]["sessionFile"].as_str(), path.to_str());
        app.state
            .lock()
            .unwrap()
            .providers
            .iter_mut()
            .find(|provider| provider.config.id == "openai")
            .unwrap()
            .config
            .models
            .push("saved-model".into());
        app.request("new_session", json!({"id":id})).await.unwrap();
        let current = app.snapshot().bots[0].session_file.clone();
        let current_bytes = tokio::fs::read(current.as_ref().unwrap()).await.unwrap();
        let mut invalid: Value = serde_json::from_slice(&bytes).unwrap();
        invalid["providerId"] = json!("foreign");
        tokio::fs::write(&path, serde_json::to_vec(&invalid).unwrap())
            .await
            .unwrap();
        assert!(app
            .request("resume_session", json!({"id":id,"sessionId":old_id}))
            .await
            .is_err());
        assert_eq!(app.snapshot().bots[0].session_file, current);
        assert!(app.snapshot().bots[0].is_running);
        assert_eq!(
            tokio::fs::read(current.as_ref().unwrap()).await.unwrap(),
            current_bytes
        );
        tokio::fs::remove_file(&path).await.unwrap();
        assert!(app
            .request("resume_session", json!({"id":id,"sessionId":old_id}))
            .await
            .is_err());
        assert_eq!(app.snapshot().bots[0].session_file, current);
        assert!(app.snapshot().bots[0].is_running);
        app.shutdown_all().await.unwrap();
    }

    #[tokio::test]
    async fn catalog_is_bot_owned_and_rejects_paths_foreign_ids_and_incompatible_files() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (app, id) = native_bot(root.path(), cwd.path()).await;
        let (path, _) = legacy(root.path(), &id).await;
        let listed = app
            .request("list_sessions", json!({"id":id}))
            .await
            .unwrap();
        let owned_id = listed["sessions"][0]["id"].clone();
        app.request(
            "create_bot",
            json!({"name":"Other","cwd":cwd.path(),"engine":"native","nativeProvider":"openai"}),
        )
        .await
        .unwrap();
        let other = app
            .snapshot()
            .bots
            .into_iter()
            .find(|bot| bot.id != id)
            .unwrap()
            .id;
        assert!(app
            .request("resume_session", json!({"id":other,"sessionId":owned_id}))
            .await
            .is_err());
        assert!(app
            .request("resume_session", json!({"id":id,"sessionId":path}))
            .await
            .is_err());
        let unrelated = root.path().join("native-sessions").join("unowned.json");
        tokio::fs::copy(&path, &unrelated).await.unwrap();
        assert_eq!(
            app.request("list_sessions", json!({"id":id}))
                .await
                .unwrap()["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let mut value: Value =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        value["providerId"] = json!("foreign");
        tokio::fs::write(&path, serde_json::to_vec(&value).unwrap())
            .await
            .unwrap();
        app.state
            .lock()
            .unwrap()
            .bots
            .get_mut(&id)
            .unwrap()
            .record
            .native_provider = Some("different".into());
        assert!(app
            .request("list_sessions", json!({"id":id}))
            .await
            .unwrap()["sessions"]
            .as_array()
            .unwrap()
            .is_empty());
        app.state
            .lock()
            .unwrap()
            .bots
            .get_mut(&id)
            .unwrap()
            .record
            .native_provider = Some("openai".into());
        assert!(app
            .request("resume_session", json!({"id":id,"sessionId":owned_id}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn omp_catalog_accepts_both_header_layouts_and_workspace_aliases() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let foreign_cwd = tempfile::tempdir().unwrap();
        let app =
            Desktop::open_with_provider_view(root.path().to_path_buf(), offline_provider_view)
                .await
                .unwrap();
        app.request(
            "create_bot",
            json!({"name":"OMP","cwd":cwd.path(),"engine":"omp"}),
        )
        .await
        .unwrap();
        let id = app.snapshot().bots[0].id.clone();
        let legacy_path = root.path().join("legacy.jsonl");
        let modern_path = root.path().join("modern.jsonl");
        let canonical = workspace(cwd.path().to_str().unwrap()).unwrap();
        let legacy_header =
            json!({"type":"session","version":3,"id":"legacy-session","cwd":canonical});
        let mut modern_header =
            json!({"type":"session","version":3,"id":"modern-session","cwd":cwd.path()});
        let entry = json!({"type":"message","id":"entry","parentId":null,"message":{"role":"user","content":"A saved OMP turn"}});
        let mut slot = json!({"type":"title","v":1,"title":"Pinned conversation","updatedAt":"2026-10-25T00:00:00.000Z","pad":""});
        slot["pad"] = json!(" ".repeat(255 - slot.to_string().len()));
        tokio::fs::write(&legacy_path, format!("{legacy_header}\n{entry}\n"))
            .await
            .unwrap();
        tokio::fs::write(&modern_path, format!("{slot}\n{modern_header}\n{entry}\n"))
            .await
            .unwrap();
        {
            let mut state = app.state.lock().unwrap();
            let record = &mut state.bots.get_mut(&id).unwrap().record;
            record.session_file = Some(legacy_path.to_string_lossy().into_owned());
            remember_current(record, None);
            record.session_file = Some(modern_path.to_string_lossy().into_owned());
            remember_current(record, Some("Cached first message".into()));
        }
        let listed = app
            .request("list_sessions", json!({"id":id}))
            .await
            .unwrap();
        let sessions = listed["sessions"].as_array().unwrap();
        assert!(sessions.iter().any(|session| session["sessionFile"]
            == legacy_path.to_str().unwrap()
            && session["current"] == false));
        assert!(sessions.iter().any(|session| session["sessionFile"]
            == modern_path.to_str().unwrap()
            && session["title"] == "Pinned conversation"
            && session["current"] == true));
        modern_header["cwd"] = json!(foreign_cwd.path());
        tokio::fs::write(&modern_path, format!("{slot}\n{modern_header}\n{entry}\n"))
            .await
            .unwrap();
        let filtered = app
            .request("list_sessions", json!({"id":id}))
            .await
            .unwrap();
        assert_eq!(
            filtered["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|session| session["sessionFile"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![legacy_path.to_str().unwrap()]
        );
    }
}
