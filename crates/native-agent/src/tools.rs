use anyhow::{anyhow, bail, Context, Result};
use globset::GlobBuilder;
use regex::RegexBuilder;
use serde_json::{json, Value};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use std::collections::VecDeque;
#[path = "discovery.rs"]
mod discovery;
use discovery::{discover, open_directory, read_directory};
#[path = "preview.rs"]
mod preview;

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_PREVIEW_BYTES: usize = 96 * 1024;
const MAX_CHANGE_BYTES: usize = 32 * 1024;
const MAX_DIFF_LINES: usize = 1000;
const MAX_ENTRIES: usize = 20_000;
const MAX_SCAN_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULTS: usize = 200;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_PIPE_BYTES: usize = 8 * 1024;

pub struct ApprovalRequest {
    pub id: String,
    pub title: String,
    pub message: String,
    pub answer: oneshot::Sender<bool>,
}

pub struct ToolExecutor {
    pub cwd: PathBuf,
    pub approvals: mpsc::UnboundedSender<ApprovalRequest>,
    pub cancel: CancellationToken,
}

pub fn definitions() -> Vec<Value> {
    vec![
        definition("read", "Read UTF-8 text or list a directory inside the workspace. Directory entries are sorted and bounded; offset is a 1-based line/entry number. File input limit 1 MiB, output 64 KiB. Tilde paths expand only within the workspace.", json!({
            "path": {"type":"string"}, "offset": {"type":"integer","minimum":1},
            "limit": {"type":"integer","minimum":1,"maximum":2000}
        }), &["path"]),
        definition("glob", "Find files AND directories using a glob relative to path (default workspace root); directories have trailing /. Does not follow symlinks. Dependency/build/VCS trees are pruned unless path explicitly scopes inside one. Results report truncation and pruned directories; scope path to narrow incomplete searches.", json!({
            "pattern": {"type":"string"}, "path": {"type":"string"},
            "limit": {"type":"integer","minimum":1,"maximum":200}
        }), &["pattern"]),
        definition("grep", "Search UTF-8 workspace files using a Rust regular expression. Optional glob filters relative file paths. Does not follow symlinks. Dependency/build/VCS trees are pruned unless path explicitly scopes inside one. Bounded traversal, file sizes, scanned bytes, and results are reported.", json!({
            "pattern": {"type":"string"}, "path": {"type":"string"},
            "glob": {"type":"string"}, "ignoreCase": {"type":"boolean"},
            "limit": {"type":"integer","minimum":1,"maximum":200}
        }), &["pattern"]),
        definition("write", "Create or replace a UTF-8 workspace file, creating missing parent directories. Requires explicit user approval of the exact target and complete actual diff. Content and complete preview must fit safety limits; nothing is silently truncated.", json!({
            "path": {"type":"string"}, "content": {"type":"string"}
        }), &["path", "content"]),
        definition("edit", "Replace exactly one occurrence of oldText with newText in a UTF-8 workspace file. Missing or ambiguous matches fail. Requires explicit approval of the complete actual diff.", json!({
            "path": {"type":"string"}, "oldText": {"type":"string"}, "newText": {"type":"string"}
        }), &["path", "oldText", "newText"]),
        definition("bash", "Run /bin/bash -c in the workspace after explicit approval. Shell is NOT sandboxed and can access files/network outside the workspace. stdout/stderr are bounded. timeout is seconds (default 30, maximum 120); cancellation/timeout kills the process group. Provider credentials are not injected; only basic environment variables are inherited.", json!({
            "command": {"type":"string"}, "timeout": {"type":"integer","minimum":1,"maximum":120}
        }), &["command"]),
        definition("render_preview", "Render a static HTML/CSS DESIGN MOCKUP to an actual PNG using installed headless Chrome/Chromium. Not an existing-app screenshot. No scripts, links, forms, navigation, external resources or workspace file access; HTML is sanitized to static layout/style and data images. Returns visible PNG image plus provenance. Use after reading project sources; no source files are modified.", json!({
            "html":{"type":"string","maxLength":131072},
            "width":{"type":"integer","minimum":320,"maximum":1920},
            "height":{"type":"integer","minimum":240,"maximum":1600},
            "title":{"type":"string","maxLength":200}
        }), &["html"]),
    ]
}

fn definition(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,
        "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
}

impl ToolExecutor {
    pub async fn execute(&self, name: &str, args: &Value) -> Result<Vec<Value>> {
        check_cancel(&self.cancel)?;
        if !args.is_object() {
            bail!("Tool arguments must be an object");
        }
        let root = fs::canonicalize(&self.cwd).context("Cannot resolve workspace")?;
        if !root.is_dir() {
            bail!("Workspace is not a directory");
        }
        if name == "render_preview" {
            return preview::render(args, &self.cancel).await;
        }
        let text = match name {
            "read" | "glob" | "grep" => {
                let name = name.to_owned();
                let args = args.clone();
                let cancel = self.cancel.clone();
                let task_cancel = cancel.clone();
                let work = tokio::task::spawn_blocking(move || match name.as_str() {
                    "read" => read_tool(&root, &args, &task_cancel),
                    "glob" => glob_tool(&root, &args, &task_cancel),
                    _ => grep_tool(&root, &args, &task_cancel),
                });
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => bail!("Tool cancelled"),
                    result = work => result.context("File tool worker failed")?,
                }
            }
            "write" | "edit" => self.change(&root, name, args).await,
            "bash" => self.bash(&root, args).await,
            _ => bail!("Unknown native tool"),
        }?;
        Ok(vec![json!({"type":"text","text":text})])
    }

    async fn approve(&self, title: String, message: String) -> Result<()> {
        check_cancel(&self.cancel)?;
        if message.len() > MAX_PREVIEW_BYTES {
            bail!("Complete approval preview exceeds the safety limit; use a smaller change");
        }
        let (answer, receive) = oneshot::channel();
        self.approvals
            .send(ApprovalRequest {
                id: uuid::Uuid::new_v4().to_string(),
                title,
                message,
                answer,
            })
            .map_err(|_| anyhow!("Approval UI is unavailable"))?;
        let approved = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => bail!("Tool cancelled while awaiting approval"),
            answer = receive => answer.context("Approval was cancelled")?,
        };
        if !approved {
            bail!("User denied approval");
        }
        check_cancel(&self.cancel)
    }

    async fn change(&self, root: &Path, name: &str, args: &Value) -> Result<String> {
        let requested = string(args, "path", 4096)?;
        let target = resolve_path(root, requested, name == "write")?;
        let before = optional_text(root, &target)?;
        let after = if name == "write" {
            string(args, "content", MAX_CHANGE_BYTES)?.to_owned()
        } else {
            let before = before.as_deref().context("Edit target does not exist")?;
            replace_exact(
                before,
                string(args, "oldText", MAX_CHANGE_BYTES)?,
                string(args, "newText", MAX_CHANGE_BYTES)?,
            )?
        };
        if after.len() > MAX_FILE_BYTES {
            bail!("Resulting file exceeds the 1 MiB input limit");
        }
        if before.as_deref() == Some(after.as_str()) {
            bail!("Requested change does not change the file");
        }
        let preview = change_preview(&target, before.as_deref(), &after)?;
        self.approve(format!("Approve {name}"), preview).await?;
        // Approval applies only to this exact resolved target and old content.
        let current_target = resolve_path(root, requested, name == "write")?;
        if current_target != target {
            bail!("Target changed while awaiting approval; request a new approval");
        }
        check_cancel(&self.cancel)?;
        commit_change(root, &target, before.as_deref(), &after, &self.cancel)?;
        Ok(format!(
            "{} {} ({} bytes)",
            if before.is_some() {
                "Updated"
            } else {
                "Created"
            },
            target.display(),
            after.len()
        ))
    }

    async fn bash(&self, root: &Path, args: &Value) -> Result<String> {
        let command = string(args, "command", MAX_COMMAND_BYTES)?;
        if command.is_empty() {
            bail!("command must not be empty");
        }
        let seconds = integer(args, "timeout", 30, 120)?;
        self.approve("Approve shell command".into(), format!(
            "Working directory: {:?}\nTimeout: {seconds} seconds\n\nWARNING: This shell is NOT sandboxed. It can modify or read files outside the workspace, access the network, and launch programs. Approve only if you trust the entire command.\n\nExact /bin/bash -c command:\n{command}", root
        )).await?;
        if fs::canonicalize(&self.cwd).context("Cannot recheck workspace")? != root {
            bail!("Workspace changed while awaiting approval");
        }
        check_cancel(&self.cancel)?;
        let mut process = tokio::process::Command::new("/bin/bash");
        process
            .arg("-c")
            .arg(command)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .kill_on_drop(true)
            .process_group(0);
        // Do not inherit API-key environment variables into the shell.
        for key in [
            "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TERM", "USER", "LOGNAME",
        ] {
            if let Some(value) = std::env::var_os(key) {
                process.env(key, value);
            }
        }
        let mut child = process.spawn().context("Cannot start shell")?;
        let mut group = ProcessGroup(Some(child.id().context("Shell has no process ID")? as i32));
        let stdout = child.stdout.take().context("Shell stdout unavailable")?;
        let stderr = child.stderr.take().context("Shell stderr unavailable")?;
        let mut stdout = PipeCollector(tokio::spawn(drain_pipe(stdout)));
        let mut stderr = PipeCollector(tokio::spawn(drain_pipe(stderr)));
        enum End {
            Exited(std::io::Result<std::process::ExitStatus>),
            Cancelled,
            TimedOut,
        }
        let end = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => End::Cancelled,
            _ = tokio::time::sleep(Duration::from_secs(seconds)) => End::TimedOut,
            status = child.wait() => End::Exited(status),
        };
        // Kill descendants even if the direct shell exited with background jobs.
        group.kill();
        if !matches!(&end, End::Exited(_)) {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
        let drained = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(&mut stdout.0, &mut stderr.0)
        })
        .await;
        stdout.0.abort();
        stderr.0.abort();
        let (out, err) = match drained {
            Ok((out, err)) => (
                out.context("stdout collector failed")??,
                err.context("stderr collector failed")??,
            ),
            Err(_) => bail!("Shell output pipes did not close after process-group termination"),
        };
        let output = format!(
            "stdout{}:\n{}\nstderr{}:\n{}",
            if out.1 { " (truncated)" } else { "" },
            String::from_utf8_lossy(&out.0),
            if err.1 { " (truncated)" } else { "" },
            String::from_utf8_lossy(&err.0)
        );
        match end {
            End::Cancelled => bail!("Shell cancelled; process group terminated\n{output}"),
            End::TimedOut => {
                bail!("Shell timed out after {seconds} seconds; process group terminated\n{output}")
            }
            End::Exited(status) => {
                let status = status.context("Cannot wait for shell")?;
                if !status.success() {
                    bail!("Shell exited with {status}\n{output}");
                }
                Ok(format!("Exit status: {status}\n{output}"))
            }
        }
    }
}

struct ProcessGroup(Option<i32>);
impl ProcessGroup {
    fn kill(&mut self) {
        // The child created its own process group. A negative PID targets all
        // descendants that remain in it, including when the future is dropped.
        if let Some(pid) = self.0.take() {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

// Dropping execute must also release collectors if an unsandboxed shell launches
// a detached process that escapes its process group while retaining a pipe.
struct PipeCollector(tokio::task::JoinHandle<Result<(Vec<u8>, bool)>>);
impl Drop for PipeCollector {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn drain_pipe(mut pipe: impl AsyncRead + Unpin) -> Result<(Vec<u8>, bool)> {
    let mut captured = Vec::with_capacity(MAX_PIPE_BYTES);
    let mut truncated = false;
    let mut buffer = [0u8; 4096];
    loop {
        let count = pipe
            .read(&mut buffer)
            .await
            .context("Cannot read shell output")?;
        if count == 0 {
            break;
        }
        let keep = count.min(MAX_PIPE_BYTES - captured.len());
        captured.extend_from_slice(&buffer[..keep]);
        truncated |= keep != count;
    }
    Ok((captured, truncated))
}

fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        bail!("Tool cancelled");
    }
    Ok(())
}

fn string<'a>(args: &'a Value, key: &str, max: usize) -> Result<&'a str> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{key} must be a string"))?;
    if value.len() > max {
        bail!("{key} exceeds the {max}-byte limit");
    }
    Ok(value)
}

fn integer(args: &Value, key: &str, default: u64, max: u64) -> Result<u64> {
    let value = match args.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow!("{key} must be a positive integer"))?,
    };
    if value == 0 || value > max {
        bail!("{key} must be between 1 and {max}");
    }
    Ok(value)
}

// Reject lexical '..' even when it would eventually normalize back into the root.
// Resolve every existing ancestor, including symlinks, before permitting a new path.
fn resolve_path(root: &Path, input: &str, allow_missing: bool) -> Result<PathBuf> {
    if input.is_empty() || input.as_bytes().contains(&0) {
        bail!("Invalid empty or NUL-containing path");
    }
    let expanded;
    let input = if input == "~" || input.starts_with("~/") {
        let home = std::env::var_os("HOME").context("Cannot expand ~: HOME is unavailable")?;
        expanded = PathBuf::from(home).join(input.strip_prefix("~/").unwrap_or(""));
        expanded.as_path()
    } else {
        Path::new(input)
    };
    if input
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        bail!("Path traversal is not allowed");
    }
    let joined = if input.is_absolute() {
        input.to_owned()
    } else {
        root.join(input)
    };
    if !joined.starts_with(root) {
        bail!("Path is outside the workspace");
    }
    let mut current = root.to_owned();
    for part in joined.strip_prefix(root)?.components() {
        match part {
            Component::Normal(name) => current.push(name),
            Component::CurDir => continue,
            _ => bail!("Invalid workspace path"),
        }
        match fs::symlink_metadata(&current) {
            Ok(_) => {
                current = fs::canonicalize(&current).context("Cannot resolve path ancestor")?;
                if !current.starts_with(root) {
                    bail!("Symlink escapes the workspace");
                }
            }
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot resolve workspace path"),
        }
    }
    Ok(current)
}

fn c_name(name: &std::ffi::OsStr) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(name.as_bytes()).context("Path contains NUL")
}

// Descriptor-relative opens prevent symlink swaps between canonicalization and I/O.
// Open every canonical root ancestor without following symlinks as well.
fn parent_handle(root: &Path, target: &Path, create: bool) -> Result<(File, CString)> {
    use std::os::unix::fs::OpenOptionsExt;
    let relative = target
        .strip_prefix(root)
        .context("Path is outside the workspace")?;
    let name = c_name(
        relative
            .file_name()
            .context("File path must not be the workspace root")?,
    )?;
    let mut directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .context("Cannot open filesystem root")?;
    for component in root.components() {
        let part = match component {
            Component::RootDir => continue,
            Component::Normal(part) => c_name(part)?,
            _ => bail!("Workspace root must be a canonical absolute path"),
        };
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                part.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("Cannot safely open workspace directory");
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    if let Some(parent) = relative.parent() {
        for part in parent.components() {
            let Component::Normal(part) = part else {
                bail!("Invalid parent component");
            };
            let part = c_name(part)?;
            let mut fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    part.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0
                && create
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
            {
                let created = unsafe { libc::mkdirat(directory.as_raw_fd(), part.as_ptr(), 0o755) };
                if created < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(std::io::Error::last_os_error())
                        .context("Cannot create parent directory");
                }
                fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        part.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
            }
            if fd < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("Cannot safely open parent directory");
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
    }
    Ok((directory, name))
}

fn open_file(root: &Path, target: &Path) -> Result<File> {
    let (parent, name) = parent_handle(root, target, false)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot safely open file");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        bail!("Target is not a regular file");
    }
    Ok(file)
}

fn file_text(file: &mut File) -> Result<String> {
    let size = file.metadata().context("Cannot inspect file")?.len();
    if size > MAX_FILE_BYTES as u64 {
        bail!("File exceeds the 1 MiB input limit");
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("Cannot read file")?;
    if bytes.len() > MAX_FILE_BYTES {
        bail!("File exceeds the 1 MiB input limit");
    }
    String::from_utf8(bytes).context("File is not UTF-8 text")
}

fn optional_text(root: &Path, target: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Cannot inspect target"),
        Ok(_) => Ok(Some(file_text(&mut open_file(root, target)?)?)),
    }
}

fn replace_exact(before: &str, old: &str, new: &str) -> Result<String> {
    if old.is_empty() {
        bail!("oldText must not be empty");
    }
    let start = before.find(old).context("oldText was not found")?;
    // Include overlapping matches: 'aa' in 'aaa' is ambiguous too.
    let next = start
        + before[start..]
            .chars()
            .next()
            .expect("nonempty match")
            .len_utf8();
    if before[next..].contains(old) {
        bail!("oldText is ambiguous: more than one occurrence");
    }
    let resulting_len = before.len() - old.len() + new.len();
    if resulting_len > MAX_FILE_BYTES {
        bail!("Resulting file exceeds the 1 MiB input limit");
    }
    let mut result = String::with_capacity(resulting_len);
    result.push_str(&before[..start]);
    result.push_str(new);
    result.push_str(&before[start + old.len()..]);
    Ok(result)
}

fn change_preview(target: &Path, before: Option<&str>, after: &str) -> Result<String> {
    let old = before.unwrap_or("");
    // Bound both rendering and upstream Myers computation before doing either.
    if old.len() + after.len() > MAX_PREVIEW_BYTES / 2
        || old.split_inclusive('\n').count() + after.split_inclusive('\n').count() > MAX_DIFF_LINES
    {
        bail!("Complete approval diff is too large; use a smaller file/change");
    }
    let old_lines = pi_diff::line_tokens_str(old);
    let new_lines = pi_diff::line_tokens_str(after);
    let runs = pi_diff::line_runs_str(old, after);
    let mut preview = format!("Exact target: {:?}\nOperation: {} (missing parent directories will be created if needed)\nComplete line diff; no lines omitted:\n--- before\n+++ after\n", target,
        if before.is_some() { "replace existing file" } else { "create new file" });
    let (mut old_at, mut new_at) = (0, 0);
    for run in runs {
        let count = run.count as usize;
        let lines = if run.removed {
            &old_lines[old_at..old_at + count]
        } else {
            &new_lines[new_at..new_at + count]
        };
        let prefix = if run.added {
            '+'
        } else if run.removed {
            '-'
        } else {
            ' '
        };
        for line in lines {
            preview.push(prefix);
            preview.push_str(line);
            if !line.ends_with('\n') {
                preview.push_str("\n\\ No newline at end of file\n");
            }
        }
        if !run.added {
            old_at += count;
        }
        if !run.removed {
            new_at += count;
        }
    }
    if preview.len() > MAX_PREVIEW_BYTES {
        bail!("Complete approval diff exceeds the safety limit");
    }
    Ok(preview)
}

fn commit_change(
    root: &Path,
    target: &Path,
    before: Option<&str>,
    after: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancel(cancel)?;
    let (parent, name) = parent_handle(root, target, true)?;
    let flags = if before.is_some() {
        libc::O_RDWR
    } else {
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
    };
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o644,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("Cannot safely open approved target; request a new approval");
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        bail!("Approved target must be a regular file with no additional hard links");
    }
    if let Some(expected) = before {
        if file_text(&mut file)? != expected {
            bail!("File changed while awaiting approval; request a new approval");
        }
        check_cancel(cancel)?;
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0).context("Cannot truncate approved target")?;
    }
    file.write_all(after.as_bytes())
        .context("Cannot write approved content")?;
    file.sync_all().context("Cannot flush approved content")?;
    Ok(())
}

fn read_tool(root: &Path, args: &Value, cancel: &CancellationToken) -> Result<String> {
    let target = resolve_path(root, string(args, "path", 4096)?, false)?;
    if open_directory(root, &target).is_ok() {
        return read_directory(root, &target, args, cancel);
    }
    let text = file_text(&mut open_file(root, &target)?)?;
    check_cancel(cancel)?;
    let offset = integer(args, "offset", 1, u64::MAX)?;
    let limit = integer(args, "limit", 200, 2000)? as usize;
    let mut output = String::new();
    let mut returned = 0;
    let mut truncated = false;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if (index as u64) < offset - 1 {
            continue;
        }
        check_cancel(cancel)?;
        let prefix = format!("{}: ", index + 1);
        if returned == limit || output.len() + prefix.len() + line.len() + 128 > MAX_OUTPUT_BYTES {
            truncated = true;
            // A single huge line still produces a bounded useful prefix.
            if returned < limit {
                let budget = MAX_OUTPUT_BYTES.saturating_sub(output.len() + prefix.len() + 128);
                let end = utf8_prefix(line, budget);
                if end > 0 {
                    output.push_str(&prefix);
                    output.push_str(&line[..end]);
                    output.push('\n');
                }
            }
            break;
        }
        output.push_str(&prefix);
        output.push_str(line);
        if !line.ends_with('\n') {
            output.push('\n');
        }
        returned += 1;
    }
    if truncated {
        output.push_str("\n[Output truncated: request a later line offset or smaller limit.]\n");
    }
    if output.is_empty() {
        output.push_str("[No lines at the requested offset.]\n");
    }
    Ok(output)
}

fn utf8_prefix(text: &str, max: usize) -> usize {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn search_root(root: &Path, args: &Value) -> Result<PathBuf> {
    resolve_path(
        root,
        match args.get("path") {
            Some(_) => string(args, "path", 4096)?,
            None => ".",
        },
        false,
    )
}

fn glob_matcher(pattern: &str) -> Result<globset::GlobMatcher> {
    if pattern.len() > 4096 {
        bail!("Glob pattern is too long");
    }
    if Path::new(pattern).is_absolute() || pattern.split('/').any(|part| part == "..") {
        bail!("Glob must be workspace-relative without traversal");
    }
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .context("Invalid glob pattern")
        .map(|glob| glob.compile_matcher())
}

fn glob_tool(root: &Path, args: &Value, cancel: &CancellationToken) -> Result<String> {
    let start = search_root(root, args)?;
    let requested_pattern = string(args, "pattern", 4096)?;
    if requested_pattern.starts_with('/') { bail!("Glob must be workspace-relative without traversal"); }
    let directories_only = requested_pattern.ends_with('/');
    let pattern = glob_matcher(requested_pattern.trim_end_matches('/'))?;
    let limit = integer(args, "limit", MAX_RESULTS as u64, MAX_RESULTS as u64)? as usize;
    let discovery = discover(root, &start, cancel)?;
    let visited = discovery.visited;
    let mut matches = Vec::new();
    let mut truncated = discovery.truncated;
    let mut bytes = 0;
    for entry in discovery.entries {
        check_cancel(cancel)?;
        if !matches!(entry.kind, "file" | "directory") { continue; }
        if directories_only && entry.kind != "directory" { continue; }
        let relative = entry.path.strip_prefix(&start)?;
        let relative = if relative.as_os_str().is_empty() {
            Path::new(entry.path.file_name().context("Invalid filename")?)
        } else { relative };
        if !pattern.is_match(relative) { continue; }
        let mut display = entry.path.strip_prefix(root)?.to_string_lossy().into_owned();
        if entry.kind == "directory" { display.push('/'); }
        let encoded_len = serde_json::to_string(&display)?.len();
        if matches.len() == limit || bytes + encoded_len > MAX_OUTPUT_BYTES - 1024 {
            truncated = true;
            break;
        }
        bytes += encoded_len + 1;
        matches.push(display);
    }
    bounded_json(json!({"matches":matches,"truncated":truncated,"visited":visited,
        "prunedDirectories":discovery.pruned,"maxDepth":64}))
}

fn grep_tool(root: &Path, args: &Value, cancel: &CancellationToken) -> Result<String> {
    let start = search_root(root, args)?;
    let mut builder = RegexBuilder::new(string(args, "pattern", 4096)?);
    let ignore_case = match args.get("ignoreCase") {
        None => false,
        Some(value) => value.as_bool().context("ignoreCase must be a boolean")?,
    };
    let regex = builder
        .case_insensitive(ignore_case)
        .size_limit(1024 * 1024)
        .dfa_size_limit(1024 * 1024)
        .build()
        .context("Invalid or excessively large regular expression")?;
    let filter = args
        .get("glob")
        .map(|_| string(args, "glob", 4096).and_then(glob_matcher))
        .transpose()?;
    let limit = integer(args, "limit", 100, MAX_RESULTS as u64)? as usize;
    let mut matches = Vec::new();
    let discovery = discover(root, &start, cancel)?;
    let visited = discovery.visited;
    let (mut scanned, mut skipped, mut output_bytes) = (0, 0, 0);
    let mut truncated = discovery.truncated;
    'files: for entry in discovery.entries {
        check_cancel(cancel)?;
        if entry.kind != "file" { continue; }
        let relative = entry.path.strip_prefix(&start)?;
        let relative = if relative.as_os_str().is_empty() {
            entry.path
                .file_name()
                .map(Path::new)
                .context("Invalid filename")?
        } else {
            relative
        };
        if filter.as_ref().is_some_and(|glob| !glob.is_match(relative)) {
            continue;
        }
        let target = entry.path;
        let mut file = open_file(root, &target)?;
        let size = file.metadata()?.len();
        if size > MAX_FILE_BYTES as u64 {
            skipped += 1;
            continue;
        }
        if scanned + size as usize > MAX_SCAN_BYTES {
            truncated = true;
            break;
        }
        let text = match file_text(&mut file) {
            Ok(text) => text,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if scanned + text.len() > MAX_SCAN_BYTES {
            truncated = true;
            break;
        }
        scanned += text.len();
        if text.contains('\0') {
            skipped += 1;
            continue;
        }
        let display = target.strip_prefix(root)?.to_string_lossy();
        for (index, line) in text.lines().enumerate() {
            check_cancel(cancel)?;
            if !regex.is_match(line) {
                continue;
            }
            let end = utf8_prefix(line, 2000);
            let found = json!({"path":display,"line":index + 1,"text":&line[..end],"lineTruncated":end != line.len()});
            let bytes = serde_json::to_string(&found)?.len() + 1;
            if matches.len() == limit || output_bytes + bytes > MAX_OUTPUT_BYTES - 1024 {
                truncated = true;
                break 'files;
            }
            output_bytes += bytes;
            matches.push(found);
        }
    }
    bounded_json(
        json!({"matches":matches,"truncated":truncated,"visited":visited.min(MAX_ENTRIES),
        "scannedBytes":scanned,"skippedBinaryOrOversized":skipped,"prunedDirectories":discovery.pruned,"maxDepth":64}),
    )
}

fn bounded_json(value: Value) -> Result<String> {
    let output = serde_json::to_string(&value)?;
    if output.len() > MAX_OUTPUT_BYTES {
        bail!("Tool result exceeds output limit");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn executor(root: &Path) -> (ToolExecutor, mpsc::UnboundedReceiver<ApprovalRequest>) {
        let (approvals, receive) = mpsc::unbounded_channel();
        (
            ToolExecutor {
                cwd: root.to_owned(),
                approvals,
                cancel: CancellationToken::new(),
            },
            receive,
        )
    }

    #[test]
    fn paths_reject_traversal_and_symlink_escape_but_allow_missing_children() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let root = fs::canonicalize(workspace.path()).unwrap();
        assert!(resolve_path(&root, "../outside", true).is_err());
        assert!(resolve_path(&root, "safe/../file", true).is_err());
        assert!(resolve_path(&root, outside.path().to_str().unwrap(), true).is_err());
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
        assert!(resolve_path(&root, "escape/new/file", true).is_err());
        assert_eq!(
            resolve_path(&root, "new/child/file", true).unwrap(),
            root.join("new/child/file")
        );
        fs::create_dir(root.join("inside")).unwrap();
        std::os::unix::fs::symlink(root.join("inside"), root.join("alias")).unwrap();
        assert_eq!(
            resolve_path(&root, "alias/new", true).unwrap(),
            root.join("inside/new")
        );
    }

    #[test]
    fn edits_require_one_including_overlapping_occurrence() {
        assert!(replace_exact("abc abc", "abc", "x")
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        assert!(replace_exact("aaa", "aa", "x").is_err());
        assert!(replace_exact("abc", "missing", "x").is_err());
        assert!(replace_exact("abc", "", "x").is_err());
        assert_eq!(
            replace_exact("hello\nworld\n", "world", "earth").unwrap(),
            "hello\nearth\n"
        );
        assert_eq!(replace_exact("éé", "éé", "x").unwrap(), "x");
    }

    #[test]
    fn preview_uses_actual_before_after_and_preserves_newline_changes() {
        let preview = change_preview(
            Path::new("/workspace/file"),
            Some("old\nkeep\n"),
            "new\nkeep\n",
        )
        .unwrap();
        assert!(preview.contains("-old\n"));
        assert!(preview.contains("+new\n"));
        assert!(preview.contains(" keep\n"));
        let preview = change_preview(Path::new("/workspace/file"), Some("old"), "old\n").unwrap();
        assert!(preview.contains("No newline at end of file"));
        assert!(
            change_preview(Path::new("/file"), None, &"x\n".repeat(MAX_DIFF_LINES + 1)).is_err()
        );
    }

    #[tokio::test]
    async fn denied_and_cancelled_approvals_do_not_write() {
        let workspace = TempDir::new().unwrap();
        let (tools, mut receive) = executor(workspace.path());
        let arguments = json!({"path":"new","content":"not approved"});
        let run = tools.execute("write", &arguments);
        let decide = async {
            let request = receive.recv().await.unwrap();
            assert!(request.message.contains("not approved"));
            request.answer.send(false).unwrap();
        };
        let (result, ()) = tokio::join!(run, decide);
        assert!(result.is_err());
        assert!(!workspace.path().join("new").exists());
        let arguments = json!({"path":"new","content":"cancelled"});
        let run = tools.execute("write", &arguments);
        let decide = async {
            let _request = receive.recv().await.unwrap();
            tools.cancel.cancel();
        };
        let (result, ()) = tokio::join!(run, decide);
        assert!(result.is_err());
        assert!(!workspace.path().join("new").exists());
    }

    #[tokio::test]
    async fn approved_write_and_edit_change_actual_files() {
        let workspace = TempDir::new().unwrap();
        let (tools, mut receive) = executor(workspace.path());
        let arguments = json!({"path":"nested/file","content":"before\n"});
        let (result, ()) = tokio::join!(tools.execute("write", &arguments), async {
            receive.recv().await.unwrap().answer.send(true).unwrap();
        });
        result.unwrap();
        let arguments = json!({"path":"nested/file","oldText":"before","newText":"after"});
        let (result, ()) = tokio::join!(tools.execute("edit", &arguments), async {
            let request = receive.recv().await.unwrap();
            assert!(request.message.contains("-before\n+after\n"));
            request.answer.send(true).unwrap();
        });
        result.unwrap();
        assert_eq!(
            fs::read_to_string(workspace.path().join("nested/file")).unwrap(),
            "after\n"
        );
    }

    #[tokio::test]
    async fn approval_rechecks_old_content_and_symlink_ancestors() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "before").unwrap();
        let (tools, mut receive) = executor(workspace.path());
        let arguments = json!({"path":"file","content":"after"});
        let (result, ()) = tokio::join!(tools.execute("write", &arguments), async {
            let request = receive.recv().await.unwrap();
            fs::write(workspace.path().join("file"), "concurrent").unwrap();
            request.answer.send(true).unwrap();
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "concurrent"
        );
        let arguments = json!({"path":"new/file","content":"after"});
        let (result, ()) = tokio::join!(tools.execute("write", &arguments), async {
            let request = receive.recv().await.unwrap();
            std::os::unix::fs::symlink(outside.path(), workspace.path().join("new")).unwrap();
            request.answer.send(true).unwrap();
        });
        assert!(result.is_err());
        assert!(!outside.path().join("file").exists());
    }

    #[tokio::test]
    async fn read_glob_grep_are_real_and_bounded() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let (tools, _receive) = executor(workspace.path());
        let read = tools
            .execute("read", &json!({"path":"a.txt","offset":2,"limit":1}))
            .await
            .unwrap();
        let text = read[0]["text"].as_str().unwrap();
        assert!(text.contains("two") && !text.contains("one") && !text.contains("three"));
        let glob: Value = serde_json::from_str(
            &tools
                .execute("glob", &json!({"pattern":"*.txt"}))
                .await
                .unwrap()[0]["text"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(glob["matches"], json!(["a.txt"]));
        let grep: Value = serde_json::from_str(
            &tools
                .execute("grep", &json!({"pattern":"t.o"}))
                .await
                .unwrap()[0]["text"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(grep["matches"][0]["text"], "two");
        fs::write(
            workspace.path().join("large"),
            "x".repeat(MAX_FILE_BYTES + 1),
        )
        .unwrap();
        assert!(tools
            .execute("read", &json!({"path":"large"}))
            .await
            .is_err());
        fs::write(workspace.path().join("line"), "é".repeat(MAX_OUTPUT_BYTES)).unwrap();
        let read = tools
            .execute("read", &json!({"path":"line"}))
            .await
            .unwrap();
        assert!(read[0]["text"].as_str().unwrap().len() <= MAX_OUTPUT_BYTES);
    }

    #[test]
    fn discovery_lists_directories_prunes_generated_trees_and_allows_scoped_search() {
        let workspace = TempDir::new().unwrap();
        let root = fs::canonicalize(workspace.path()).unwrap();
        fs::create_dir_all(root.join("aaa/node_modules/large-package")).unwrap();
        fs::create_dir_all(root.join("aaa/target")).unwrap();
        fs::create_dir_all(root.join("aaa/.git")).unwrap();
        fs::create_dir_all(root.join("zeta-civic/src")).unwrap();
        fs::write(root.join("zeta-civic/README.md"), "Civic source of truth").unwrap();
        fs::write(root.join("aaa/node_modules/large-package/source.txt"), "scoped dependency").unwrap();
        // More than the global traversal budget must not obscure the later project.
        for index in 0..=MAX_ENTRIES {
            fs::write(root.join("aaa/target").join(index.to_string()), "").unwrap();
        }
        let cancel = CancellationToken::new();
        let result: Value = serde_json::from_str(&glob_tool(&root, &json!({"pattern":"**/*civic*"}), &cancel).unwrap()).unwrap();
        assert_eq!(result["matches"], json!(["zeta-civic/"]));
        assert_eq!(result["truncated"], false);
        assert_eq!(result["prunedDirectories"], 3);
        let listing: Value = serde_json::from_str(&read_tool(&root, &json!({"path":"."}), &cancel).unwrap()).unwrap();
        assert_eq!(listing["entries"][1], json!({"name":"zeta-civic","type":"directory"}));
        let scoped: Value = serde_json::from_str(&grep_tool(&root, &json!({"path":"aaa/node_modules","pattern":"scoped dependency"}), &cancel).unwrap()).unwrap();
        assert_eq!(scoped["matches"][0]["path"], "aaa/node_modules/large-package/source.txt");
        let bounded: Value = serde_json::from_str(&glob_tool(&root, &json!({"pattern":"**","limit":1}), &cancel).unwrap()).unwrap();
        assert_eq!(bounded["truncated"], true);
    }

    #[test]
    fn directory_symlinks_never_discover_outside_and_tilde_stays_bounded() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let root = fs::canonicalize(workspace.path()).unwrap();
        fs::write(outside.path().join("private-marker"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
        let cancel = CancellationToken::new();
        let found = glob_tool(&root, &json!({"pattern":"**"}), &cancel).unwrap();
        assert!(!found.contains("private-marker"));
        assert!(read_tool(&root, &json!({"path":"escape"}), &cancel).is_err());
        // A HOME-root workspace supports ~ without mutating process-global env.
        if let Some(home) = std::env::var_os("HOME") {
            let home = fs::canonicalize(home).unwrap();
            assert_eq!(resolve_path(&home, "~", false).unwrap(), home);
            if !home.starts_with(&root) {
                assert!(resolve_path(&root, "~", false).is_err());
                assert!(resolve_path(&root, "~/private-marker", true).is_err());
            }
        }
    }

    #[tokio::test]
    async fn cancelled_shell_kills_its_process_group() {
        let workspace = TempDir::new().unwrap();
        let (tools, mut receive) = executor(workspace.path());
        let arguments = json!({"command":"sleep 30 & child=$!; printf '%s\\n' \"$child\" > child.pid; wait","timeout":120});
        let run = tools.execute("bash", &arguments);
        let cancel = async {
            let request = receive.recv().await.unwrap();
            request.answer.send(true).unwrap();
            let pid_file = workspace.path().join("child.pid");
            let pid = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = fs::read_to_string(&pid_file) {
                        if let Ok(pid) = text.trim().parse::<i32>() {
                            break pid;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            tools.cancel.cancel();
            pid
        };
        let (result, pid) = tokio::join!(run, cancel);
        assert!(result.is_err());
        // Reaping an orphan is OS-owned, so a zombie is acceptable but a running
        // descendant is not. ps works on both macOS and Linux.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = tokio::process::Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .await
                    .unwrap();
                let state = String::from_utf8_lossy(&status.stdout);
                if !status.status.success()
                    || state.trim().is_empty()
                    || state.trim().starts_with('Z')
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shell descendant survived cancellation");
    }

    #[tokio::test]
    async fn shell_timeout_and_output_caps_are_enforced() {
        let workspace = TempDir::new().unwrap();
        let (tools, mut receive) = executor(workspace.path());
        let arguments =
            json!({"command":"while :; do printf 'abcdefghijklmnop'; done","timeout":1});
        let (result, ()) = tokio::join!(tools.execute("bash", &arguments), async {
            receive.recv().await.unwrap().answer.send(true).unwrap();
        });
        let error = result.unwrap_err().to_string();
        assert!(error.len() <= MAX_OUTPUT_BYTES);
    }
}
