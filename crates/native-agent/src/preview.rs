//! Inert design mockups, not a general-purpose browser or app capture tool.
use super::*;
use base64::Engine;
use std::collections::{HashMap, HashSet};

const MAX_HTML_BYTES: usize = 128 * 1024;
// Base64 stays below the supported Claude API's 10 MB per-image ceiling.
const MAX_PNG_BYTES: usize = 6 * 1024 * 1024;
const CSP: &str = "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'none'; frame-src 'none'; object-src 'none'; media-src 'none'; base-uri 'none'; form-action 'none'";

fn static_document(html: &str) -> Result<String> {
    if html.is_empty() || html.len() > MAX_HTML_BYTES {
        bail!("html must contain 1 to 131072 bytes");
    }
    // Parse HTML, then serialize an allowlist. In particular CSP does not stop
    // top-level navigation: never admit meta/base/link/anchors or active markup.
    let tags: HashSet<&str> = [
        "style",
        "div",
        "span",
        "p",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "header",
        "footer",
        "main",
        "section",
        "article",
        "aside",
        "nav",
        "strong",
        "em",
        "b",
        "i",
        "small",
        "br",
        "hr",
        "ul",
        "ol",
        "li",
        "dl",
        "dt",
        "dd",
        "table",
        "thead",
        "tbody",
        "tfoot",
        "tr",
        "th",
        "td",
        "caption",
        "pre",
        "code",
        "blockquote",
        "figure",
        "figcaption",
        "img",
    ]
    .into_iter()
    .collect();
    let attributes = HashMap::from([
        ("img", HashSet::from(["src", "alt", "width", "height"])),
        ("td", HashSet::from(["colspan", "rowspan"])),
        ("th", HashSet::from(["colspan", "rowspan"])),
    ]);
    let clean = ammonia::Builder::new()
        .tags(tags)
        .tag_attributes(attributes)
        .generic_attributes(HashSet::from([
            "class",
            "id",
            "style",
            "title",
            "role",
            "aria-label",
        ]))
        .clean_content_tags(HashSet::from([
            "script", "iframe", "object", "embed", "svg", "math", "template", "noscript",
        ]))
        .url_schemes(HashSet::from(["data"]))
        .url_relative(ammonia::UrlRelative::Deny)
        .link_rel(None)
        .clean(html)
        .to_string();
    if clean.len() > MAX_HTML_BYTES * 2 {
        bail!("Sanitized HTML exceeds the preview limit");
    }
    Ok(format!("<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\"><style>html,body{{margin:0;min-height:100%;}}*{{box-sizing:border-box;}}</style></head><body>{clean}</body></html>"))
}

fn browser_at(explicit: Option<std::ffi::OsString>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let path = PathBuf::from(path);
        if !path.is_absolute() || !path.is_file() {
            bail!("OMP_DESKTOP_BROWSER must be an absolute path to an installed Chrome/Chromium executable");
        }
        return Ok(path);
    }
    for path in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
    ] {
        if Path::new(path).is_file() {
            return Ok(path.into());
        }
    }
    bail!("No installed Chrome/Chromium browser found. Install one or set OMP_DESKTOP_BROWSER to its absolute executable path; no preview was produced.")
}

fn png_blocks(bytes: &[u8], width: u64, height: u64, title: &str) -> Result<Vec<Value>> {
    if bytes.len() < 24
        || bytes.len() > MAX_PNG_BYTES
        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
        || &bytes[12..16] != b"IHDR"
    {
        bail!("Browser did not produce a bounded PNG screenshot");
    }
    let actual_width = u32::from_be_bytes(bytes[16..20].try_into()?);
    let actual_height = u32::from_be_bytes(bytes[20..24].try_into()?);
    if u64::from(actual_width) != width || u64::from(actual_height) != height {
        bail!("Browser screenshot dimensions differ from the requested preview");
    }
    let mut cursor = 8usize;
    let (mut has_data, mut complete) = (false, false);
    while cursor + 12 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into()?) as usize;
        let end = cursor
            .checked_add(size)
            .and_then(|value| value.checked_add(12))
            .filter(|end| *end <= bytes.len())
            .context("Browser PNG chunk is incomplete")?;
        let kind = &bytes[cursor + 4..cursor + 8];
        if cursor == 8 && (kind != b"IHDR" || size != 13) {
            bail!("Browser PNG header is invalid");
        }
        if kind == b"IDAT" && size > 0 {
            has_data = true;
        }
        if kind == b"IEND" {
            complete = size == 0 && end == bytes.len();
            break;
        }
        cursor = end;
    }
    if !has_data || !complete {
        bail!("Browser PNG screenshot has no image data or is incomplete");
    }
    Ok(vec![
        json!({"type":"text","text":format!("Design mockup preview: {title}. Actual {width}×{height} PNG rendered by installed headless Chrome/Chromium from sanitized, static offline HTML/CSS. This is a proposed design mockup, NOT a screenshot of the running/existing app. Scripts, navigation and external resources were removed or blocked. No project files were modified.")}),
        json!({"type":"image","mimeType":"image/png","data":base64::engine::general_purpose::STANDARD.encode(bytes),"name":"design-mockup.png","alt":format!("Design mockup: {title}")}),
    ])
}

pub(super) async fn render(args: &Value, cancel: &CancellationToken) -> Result<Vec<Value>> {
    check_cancel(cancel)?;
    let width = integer(args, "width", 1440, 1920)?;
    let height = integer(args, "height", 1000, 1600)?;
    if width < 320 || height < 240 {
        bail!("Preview dimensions must be width 320–1920 and height 240–1600");
    }
    let title = match args.get("title") {
        Some(_) => string(args, "title", 200)?,
        None => "Proposed app design",
    };
    let document = static_document(string(args, "html", MAX_HTML_BYTES)?)?;
    let browser = browser_at(std::env::var_os("OMP_DESKTOP_BROWSER"))?;
    let temporary = tempfile::Builder::new()
        .prefix("omp-static-preview-")
        .tempdir()
        .context("Cannot create isolated preview directory")?;
    let html = temporary.path().join("mockup.html");
    let png = temporary.path().join("preview.png");
    fs::write(&html, document).context("Cannot write isolated mockup HTML")?;
    let profile = temporary.path().join("profile");
    let mut process = tokio::process::Command::new(browser);
    process
        .args([
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--disable-extensions",
            "--disable-default-apps",
            "--disable-javascript",
            "--use-mock-keychain",
            "--password-store=basic",
            "--disable-breakpad",
            "--no-pings",
            "--metrics-recording-only",
            "--host-resolver-rules=MAP * ~NOTFOUND",
            "--force-device-scale-factor=1",
            "--virtual-time-budget=1000",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--window-size={width},{height}"))
        .arg(format!("--screenshot={}", png.display()))
        .arg(
            reqwest::Url::from_file_path(&html)
                .map_err(|_| anyhow!("Cannot encode preview file URL"))?
                .as_str(),
        )
        .current_dir(temporary.path())
        .env_clear()
        .env("HOME", temporary.path())
        .env("TMPDIR", temporary.path())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LANG", "en_US.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = process
        .spawn()
        .context("Cannot start installed Chrome/Chromium; check OMP_DESKTOP_BROWSER")?;
    let mut group = ProcessGroup(Some(child.id().context("Browser has no process ID")? as i32));
    let mut stderr = PipeCollector(tokio::spawn(drain_pipe(
        child.stderr.take().context("Browser stderr unavailable")?,
    )));
    enum End {
        Exited(std::io::Result<std::process::ExitStatus>),
        Cancelled,
        TimedOut,
    }
    let end = tokio::select! {
        biased;
        _ = cancel.cancelled() => End::Cancelled,
        _ = tokio::time::sleep(Duration::from_secs(30)) => End::TimedOut,
        status = child.wait() => End::Exited(status),
    };
    group.kill();
    if !matches!(&end, End::Exited(_)) {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
    let logs = tokio::time::timeout(Duration::from_secs(2), &mut stderr.0).await;
    stderr.0.abort();
    let logs = match logs {
        Ok(result) => result.context("Browser stderr collector failed")??.0,
        Err(_) => bail!("Browser output did not close after process-group termination"),
    };
    match end {
        End::Cancelled => bail!("Preview cancelled; browser process group terminated"),
        End::TimedOut => {
            bail!("Preview timed out after 30 seconds; browser process group terminated")
        }
        End::Exited(status) => {
            let status = status.context("Cannot wait for browser")?;
            if !status.success() {
                bail!(
                    "Headless browser failed ({status}): {}",
                    String::from_utf8_lossy(&logs)
                );
            }
        }
    }
    check_cancel(cancel)?;
    let mut file = File::open(png).context("Browser exited without producing a PNG preview")?;
    let size = file.metadata()?.len();
    if size > MAX_PNG_BYTES as u64 {
        bail!("PNG preview exceeds the 6 MiB limit; use smaller dimensions");
    }
    let mut bytes = Vec::with_capacity(size as usize);
    Read::by_ref(&mut file)
        .take(MAX_PNG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    png_blocks(&bytes, width, height, title)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitizer_removes_navigation_scripts_and_host_resources() {
        let html = static_document(r#"<meta http-equiv=refresh content='0;url=file:///etc/passwd'><base href='file:///'><a href='https://example.com'>link</a><iframe src='file:///etc/passwd'></iframe><object data='x'></object><form action='https://example.com'><input autofocus></form><div onclick='alert(1)' style='color:red'>Safe</div><script>alert(1)</script><img src='file:///etc/passwd'><img src='https://example.com/a'><style>body{background:url(https://example.com/bg)}</style>"#).unwrap();
        assert!(!html.contains("file:///"));
        for forbidden in [
            "<a ",
            "<base",
            "<iframe",
            "<object",
            "<form",
            "<input",
            "onclick",
            "alert(1)",
            "http-equiv=refresh",
        ] {
            assert!(!html.contains(forbidden), "{forbidden}");
        }
        assert!(html.contains("style=\"color:red\""));
        assert!(html.contains("<style>body{"));
    }
    #[test]
    fn browser_and_png_failures_are_explicit() {
        assert!(browser_at(Some("relative-browser".into())).is_err());
        assert!(browser_at(Some("/nonexistent/omp-preview-browser".into())).is_err());
        assert!(png_blocks(b"not png", 1000, 800, "test").is_err());
        assert!(static_document("").is_err());
        assert!(static_document(&"x".repeat(MAX_HTML_BYTES + 1)).is_err());
    }
    #[test]
    fn png_dimension_mismatches_and_truncation_are_rejected() {
        // Valid bytes isolate rejected dimensions and incomplete image boundaries.
        let png = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a7e8AAAAASUVORK5CYII=").unwrap();
        for end in [8, 24, png.len() - 1] {
            assert!(png_blocks(&png[..end], 1, 1, "test").is_err());
        }
        assert!(png_blocks(&png, 320, 240, "test").is_err());
    }
    #[tokio::test]
    async fn cancellation_and_invalid_dimensions_do_not_start_browser() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(render(&json!({"html":"<p>safe</p>"}), &cancel)
            .await
            .is_err());
        assert!(render(
            &json!({"html":"<p>safe</p>","width":1}),
            &CancellationToken::new()
        )
        .await
        .is_err());
    }

    #[tokio::test]
    #[ignore = "Opt-in real installed Chrome/Chromium renderer; no browser downloads"]
    async fn installed_browser_produces_png_without_page_network() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/must-not-load", listener.local_addr().unwrap());
        let html = format!(
            r#"<meta http-equiv="refresh" content="0;url={url}"><base href="{url}"><iframe src="{url}"></iframe><script>fetch('{url}')</script><style>body{{background:#123456}}main{{background-image:url('{url}');color:white;padding:32px}}</style><main><h1>Proposed civic dashboard</h1><p>Design mockup, not a live screenshot.</p><img src="{url}"></main>"#
        );
        let blocks = render(
            &json!({"html":html,"width":640,"height":480,"title":"Offline renderer regression"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(blocks[1]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 640);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 480);
        assert!(png.windows(4).any(|chunk| chunk == b"IDAT"));
        assert!(png.windows(4).any(|chunk| chunk == b"IEND"));
        assert!(
            tokio::time::timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_err(),
            "Static preview attempted page network access"
        );
    }
}
