use super::*;
use crate::{local_control::LocalControl, remote::RemoteAccess, Desktop};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::Mutex as AsyncMutex;

fn argv(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

struct Fixture {
    directory: tempfile::TempDir,
    remote: Arc<RemoteAccess>,
    control: Arc<LocalControl>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        // Desktop::open probes real provider credentials. These tests only need
        // inert shared state and disabled remote settings, never a credential IO.
        let desktop = Arc::new(Desktop {
            directory: directory.path().to_path_buf(),
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
        });
        let remote =
            RemoteAccess::open(desktop, directory.path().to_path_buf(), Arc::new(|_| None))
                .await
                .unwrap();
        let control = LocalControl::start(
            directory.path().to_path_buf(),
            remote.clone(),
            remote.closing_flag(),
        )
        .await
        .unwrap();
        Self {
            directory,
            remote,
            control,
        }
    }

    async fn command(&self, args: &[&str]) -> Result<String> {
        run(argv(args), self.directory.path().to_path_buf()).await
    }

    async fn close(self) {
        self.control.shutdown().await;
        self.remote.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn help_needs_no_desktop_and_creates_no_directory() {
    let parent = tempfile::tempdir().unwrap();
    let absent = parent.path().join("not-created");
    for command in ["help", "--help"] {
        run(argv(&[command]), absent.clone()).await.unwrap();
        assert!(!absent.exists());
    }
}

#[tokio::test]
async fn malformed_commands_are_rejected_before_contacting_a_desktop() {
    let parent = tempfile::tempdir().unwrap();
    let absent = parent.path().join("not-created");
    let invalid: &[&[&str]] = &[
        &[],
        &["unknown"],
        &["--data-dir", "/tmp", "status"],
        &["help", "status"],
        &["--help", "--data-dir", "/tmp"],
        &["status", "extra"],
        &["status", "--url", "https://host.ts.net"],
        &["disable", "--port", "1234"],
        &["enable", "--clear-url"],
        &["token", "secret-input"],
        &["token", "--token", "secret-input"],
        &["rotate-token", "--url", "secret-input"],
        &["configure", "--unknown", "secret-input"],
        &["configure"],
        &["configure", "--url"],
        &["configure", "--port"],
        &["configure", "--url", "--clear-url"],
        &["configure", "--url", "https://host.ts.net", "--clear-url"],
        &["configure", "--clear-url", "--url", "https://host.ts.net"],
        &["configure", "--clear-url", "--clear-url"],
        &["enable", "--port", "12", "--port", "34"],
        &["status", "--data-dir"],
        &["status", "--data-dir", ""],
        &["status", "--data-dir", "--unknown"],
        &["status", "--data-dir", "/tmp", "--data-dir", "/tmp"],
        &["configure", "--port=1234"],
    ];
    for args in invalid {
        let error = run(argv(args), absent.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret-input"));
        assert!(!absent.exists());
    }
    for port in [
        "0",
        "65536",
        "-1",
        "+1",
        "1.0",
        "1e3",
        " 1",
        "1 ",
        "",
        "１２",
        "999999999999999999999",
    ] {
        assert!(run(argv(&["configure", "--port", port]), absent.clone())
            .await
            .is_err());
        assert!(!absent.exists());
    }
}

#[tokio::test]
async fn real_cli_configures_same_controller_and_preserves_secret_free_status() {
    let fixture = Fixture::new().await;
    for port in ["1", "65535"] {
        let output = fixture
            .command(&[
                "configure",
                "--port",
                port,
                "--url",
                "https://Owner.Example.ts.net:443/",
            ])
            .await
            .unwrap();
        let status: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(status["port"], port.parse::<u16>().unwrap());
        assert_eq!(status["tailnetUrl"], "https://owner.example.ts.net");
        assert_eq!(status["enabled"], false);
        assert!(status.get("token").is_none());
        assert_eq!(
            fixture
                .remote
                .request("remote_status", Value::Null)
                .await
                .unwrap(),
            status
        );
    }
    let cleared: Value = serde_json::from_str(
        &fixture
            .command(&["configure", "--clear-url"])
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(cleared["tailnetUrl"].is_null());
    let disabled: Value =
        serde_json::from_str(&fixture.command(&["disable"]).await.unwrap()).unwrap();
    assert_eq!(disabled["enabled"], false);
    assert_eq!(disabled["running"], false);
    assert!(disabled.get("token").is_none());
    fixture.close().await;
}

#[tokio::test]
async fn rejected_arguments_and_backend_url_policy_leave_live_configuration_unchanged() {
    let fixture = Fixture::new().await;
    let before = fixture.command(&["status"]).await.unwrap();
    for args in [
        vec!["configure", "--port", "0"],
        vec!["configure", "--port", "65536"],
        vec!["configure", "--port", "10000", "--unsupported"],
        vec!["enable", "--port", "10000", "unexpected"],
        vec![
            "configure",
            "--url",
            "https://host.ts.net/?token=secret-input",
        ],
        vec!["configure", "--url", "https://secret-input@host.ts.net"],
        vec!["configure", "--url", "http://host.ts.net"],
        vec!["configure", "--url", "https://example.com"],
    ] {
        let error = fixture.command(&args).await.unwrap_err().to_string();
        assert!(!error.contains("secret-input"));
        assert_eq!(fixture.command(&["status"]).await.unwrap(), before);
    }
    fixture.close().await;
}

#[tokio::test]
async fn data_directory_override_contacts_the_selected_existing_desktop() {
    let fixture = Fixture::new().await;
    let unrelated = tempfile::tempdir().unwrap();
    let output = run(
        vec![
            "configure".into(),
            "--port".into(),
            "23456".into(),
            "--data-dir".into(),
            fixture.directory.path().as_os_str().to_owned(),
        ],
        unrelated.path().join("absent"),
    )
    .await
    .unwrap();
    let status: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(status["port"], 23456);
    assert_eq!(fixture.command(&["status"]).await.unwrap(), output);
    assert!(!unrelated.path().join("absent").exists());
    fixture.close().await;
}

#[tokio::test]
async fn missing_desktop_is_actionable_and_does_not_create_app_data() {
    let parent = tempfile::tempdir().unwrap();
    let absent = parent.path().join("absent");
    assert!(run(argv(&["status"]), absent.clone()).await.is_err());
    assert!(!absent.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn non_utf8_command_values_are_rejected_without_creating_data() {
    use std::os::unix::ffi::OsStringExt;
    let invalid = OsString::from_vec(vec![0xff]);
    let parent = tempfile::tempdir().unwrap();
    let absent = parent.path().join("absent");
    for args in [
        vec![invalid.clone()],
        vec!["configure".into(), "--url".into(), invalid.clone()],
        vec!["configure".into(), "--port".into(), invalid.clone()],
        vec!["status".into(), invalid.clone()],
    ] {
        assert!(run(args, absent.clone()).await.is_err());
    }
    assert!(!absent.exists());
}
