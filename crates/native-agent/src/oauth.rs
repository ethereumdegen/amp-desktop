//! Native browser OAuth. Browser tasks stage tokens; only `complete` persists them.
//! Wire constants follow oh-my-pi's catalog auth/anthropic.kdl and auth/openai-codex.kdl.
use crate::ProviderConfig;
use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

const OPENAI_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ANTHROPIC_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OPENAI_REDIRECT: &str = "http://localhost:1455/auth/callback";
const ANTHROPIC_REDIRECT: &str = "http://localhost:54545/callback";
const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_LOGINS: usize = 8;
const MAX_TOKEN_BYTES: usize = 64 * 1024;
const KEYCHAIN_SERVICE: &str = "omp-desktop-native-oauth";
// Rotation, login commit and logout share one lock: an in-flight refresh can never
// write an old grant back after logout or overwrite a newly authorized account.
static STORAGE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static CREDENTIAL_CACHE: CredentialCache = CredentialCache::new();

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Provider {
    Openai,
    Anthropic,
}

impl Provider {
    fn from_config(config: &ProviderConfig) -> Option<Self> {
        match (
            config.id.as_str(),
            config.kind.as_str(),
            config.base_url.as_str(),
        ) {
            ("openai", "openai", "https://api.openai.com/v1") => Some(Self::Openai),
            ("anthropic", "anthropic", "https://api.anthropic.com/v1") => Some(Self::Anthropic),
            _ => None,
        }
    }
    fn id(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
        }
    }
    fn cache_index(self) -> usize {
        match self {
            Self::Openai => 0,
            Self::Anthropic => 1,
        }
    }
    fn client(self) -> &'static str {
        match self {
            Self::Openai => OPENAI_CLIENT,
            Self::Anthropic => ANTHROPIC_CLIENT,
        }
    }
    fn redirect(self) -> &'static str {
        match self {
            Self::Openai => OPENAI_REDIRECT,
            Self::Anthropic => ANTHROPIC_REDIRECT,
        }
    }
    fn token_url(self) -> &'static str {
        match self {
            Self::Openai => "https://auth.openai.com/oauth/token",
            Self::Anthropic => "https://api.anthropic.com/v1/oauth/token",
        }
    }
}

pub fn oauth_available(config: &ProviderConfig) -> bool {
    Provider::from_config(config).is_some()
}

/// Reports entry presence without decrypting credentials on macOS. Secret
/// validation and any unlock prompt belong to explicit provider use, not startup.
pub fn auth_method(config: &ProviderConfig) -> Option<&'static str> {
    let provider = Provider::from_config(config)?;
    #[cfg(target_os = "macos")]
    {
        crate::keychain::contains(KEYCHAIN_SERVICE, provider.id()).then_some("oauth")
    }
    #[cfg(not(target_os = "macos"))]
    {
        read_saved(provider).ok().flatten().map(|_| "oauth")
    }
}

/// Request credentials only; deliberately not serializable or Debug-printable.
pub struct OAuthCredential {
    pub access_token: String,
    pub account_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedCredential {
    provider: Provider,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    account_id: Option<String>,
}

// Never Debug-print this state. Missing grants and sanitized storage errors are
// sticky too: only an explicit successful mutation or process restart replaces
// them, so status polling and turn requests cannot repeatedly prompt Keychain.
type CachedCredential = std::result::Result<Option<Arc<SavedCredential>>, &'static str>;

struct CredentialCache {
    entries: Mutex<[Option<CachedCredential>; 2]>,
}

impl CredentialCache {
    const fn new() -> Self {
        Self {
            entries: Mutex::new([None, None]),
        }
    }

    fn read_with(
        &self,
        provider: Provider,
        load: impl FnOnce() -> std::result::Result<Option<SavedCredential>, &'static str>,
    ) -> Result<Option<Arc<SavedCredential>>> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow!("OAuth credential cache is unavailable"))?;
        // The synchronous Keychain load runs under this lock for single-flight
        // initialization, including when another caller is only polling status.
        entries[provider.cache_index()]
            .get_or_insert_with(|| load().map(|saved| saved.map(Arc::new)))
            .clone()
            .map_err(|message| anyhow!(message))
    }

    fn save_with(
        &self,
        provider: Provider,
        credential: SavedCredential,
        persist: impl FnOnce(&SavedCredential) -> Result<()>,
    ) -> Result<Arc<SavedCredential>> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow!("OAuth credential cache is unavailable"))?;
        persist(&credential)?;
        let credential = Arc::new(credential);
        entries[provider.cache_index()] = Some(Ok(Some(Arc::clone(&credential))));
        Ok(credential)
    }

    fn remove_with(&self, provider: Provider, remove: impl FnOnce() -> Result<()>) -> Result<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow!("OAuth credential cache is unavailable"))?;
        remove()?;
        entries[provider.cache_index()] = Some(Ok(None));
        Ok(())
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthLoginStart {
    pub login_id: String,
    pub url: String,
    pub instructions: String,
    pub requires_code: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthLoginStatus {
    pub login_id: String,
    pub status: String,
    pub error: Option<String>,
}

struct Login {
    provider: Provider,
    created: Instant,
    verifier: String,
    state: String,
    status: &'static str,
    error: Option<String>,
    exchanging: bool,
    staged: Option<SavedCredential>,
    cancel: CancellationToken,
}

#[derive(Clone, Default)]
pub struct OAuthManager {
    logins: Arc<Mutex<BTreeMap<String, Login>>>,
}

impl OAuthManager {
    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<String, Login>>> {
        self.logins
            .lock()
            .map_err(|_| anyhow!("OAuth login state is unavailable"))
    }

    pub async fn begin(&self, config: &ProviderConfig) -> Result<OAuthLoginStart> {
        let provider = Provider::from_config(config)
            .ok_or_else(|| anyhow!("OAuth is only available for unmodified official providers"))?;
        let login_id = uuid::Uuid::new_v4().to_string();
        let (verifier, challenge) = pkce();
        let state = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let cancel = CancellationToken::new();
        // Cancel before binding so an earlier login relinquishes its listener.
        self.cancel_provider(provider.id())?;
        let listener = if provider == Provider::Openai {
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455))
                .await
                .ok()
        } else {
            None
        };
        let requires_code = listener.is_none();
        let url = authorization_url(provider, &state, &challenge)?;
        {
            let mut logins = self.lock()?;
            logins.retain(|_, login| login.created.elapsed() < LOGIN_TTL + Duration::from_secs(60));
            if logins.len() >= MAX_LOGINS {
                let oldest = logins
                    .iter()
                    .filter(|(_, login)| login.status != "pending")
                    .min_by_key(|(_, login)| login.created)
                    .map(|(id, _)| id.clone());
                if let Some(id) = oldest {
                    logins.remove(&id);
                }
            }
            if logins.len() >= MAX_LOGINS {
                bail!("Too many OAuth logins are pending");
            }
            logins.insert(
                login_id.clone(),
                Login {
                    provider,
                    created: Instant::now(),
                    verifier,
                    state,
                    status: "pending",
                    error: None,
                    exchanging: false,
                    staged: None,
                    cancel: cancel.clone(),
                },
            );
        }
        let manager = self.clone();
        let id = login_id.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = tokio::time::sleep(LOGIN_TTL) => manager.fail(&id, "OAuth login expired; start again"),
                _ = async {
                    if let Some(listener) = listener { manager.listen(&id, listener, cancel.clone()).await; }
                    else { std::future::pending::<()>().await; }
                } => {},
            }
        });
        let instructions = if provider == Provider::Anthropic {
            "Complete sign-in in your browser, then paste the final redirect URL (even if the page cannot load) or the authorization code#state."
        } else if requires_code {
            "Complete sign-in in your browser. Port 1455 is unavailable; paste the final http://localhost:1455/auth/callback URL here."
        } else {
            "Complete sign-in in your browser. If automatic completion fails, paste the final http://localhost:1455/auth/callback URL here."
        };
        Ok(OAuthLoginStart {
            login_id,
            url,
            instructions: instructions.into(),
            requires_code,
        })
    }

    pub fn provider_id(&self, login_id: &str) -> Result<String> {
        Ok(self
            .lock()?
            .get(login_id)
            .ok_or_else(|| anyhow!("OAuth login was not found"))?
            .provider
            .id()
            .into())
    }

    pub fn status(&self, login_id: &str) -> Result<OAuthLoginStatus> {
        let mut logins = self.lock()?;
        let login = logins
            .get_mut(login_id)
            .ok_or_else(|| anyhow!("OAuth login was not found"))?;
        expire(login);
        Ok(login_status(login_id, login))
    }

    pub fn cancel(&self, login_id: &str) -> Result<()> {
        let mut logins = self.lock()?;
        let login = logins
            .get_mut(login_id)
            .ok_or_else(|| anyhow!("OAuth login was not found"))?;
        cancel_login(login);
        Ok(())
    }

    pub fn cancel_provider(&self, provider_id: &str) -> Result<()> {
        for login in self
            .lock()?
            .values_mut()
            .filter(|login| login.provider.id() == provider_id)
        {
            cancel_login(login);
        }
        Ok(())
    }

    /// Exchanges without persisting. Both browser callback and manual input use this path.
    pub async fn submit_code(&self, login_id: &str, input: &str) -> Result<()> {
        let (provider, verifier, state, code, cancel) = {
            let mut logins = self.lock()?;
            let login = logins
                .get_mut(login_id)
                .ok_or_else(|| anyhow!("OAuth login was not found"))?;
            expire(login);
            if login.status != "pending" {
                bail!("OAuth login is no longer pending");
            }
            if login.exchanging || login.staged.is_some() {
                bail!("OAuth authorization is already being completed");
            }
            if callback_denied(input, login.provider, &login.state) {
                cancel_login(login);
                login.status = "error";
                login.error = Some("OAuth authorization was denied or failed".into());
                bail!("OAuth authorization was denied or failed");
            }
            let code = parse_callback(input, login.provider, &login.state)?;
            login.exchanging = true;
            (
                login.provider,
                login.verifier.clone(),
                login.state.clone(),
                code,
                login.cancel.clone(),
            )
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => return Err(anyhow!("OAuth login was cancelled")),
            result = exchange(provider, &code, &verifier, &state) => result,
        };
        let mut logins = self.lock()?;
        let login = logins
            .get_mut(login_id)
            .ok_or_else(|| anyhow!("OAuth login was not found"))?;
        expire(login);
        if login.status != "pending" || login.cancel.is_cancelled() {
            bail!("OAuth login is no longer pending");
        }
        login.exchanging = false;
        login.verifier.clear();
        login.state.clear();
        match result {
            Ok(credential) => {
                login.staged = Some(credential);
                Ok(())
            }
            Err(error) => {
                login.status = "error";
                login.error = Some(error.to_string());
                login.cancel.cancel();
                Err(error)
            }
        }
    }

    /// The desktop MUST call this only while holding its settings lock and stopped
    /// gate, with the current config. No browser/background task calls this method.
    pub async fn complete(
        &self,
        login_id: &str,
        config: &ProviderConfig,
    ) -> Result<OAuthLoginStatus> {
        let provider = Provider::from_config(config)
            .ok_or_else(|| anyhow!("OAuth provider configuration changed"))?;
        let _storage = STORAGE_LOCK.lock().await;
        let mut logins = self.lock()?;
        let login = logins
            .get_mut(login_id)
            .ok_or_else(|| anyhow!("OAuth login was not found"))?;
        expire(login);
        if login.provider != provider {
            bail!("OAuth provider configuration changed");
        }
        if login.status == "pending" {
            if let Some(credential) = login.staged.take() {
                if let Err(error) = save(provider, credential) {
                    login.status = "error";
                    login.error = Some(error.to_string());
                    login.cancel.cancel();
                    return Err(error);
                }
                login.status = "connected";
                login.cancel.cancel();
            }
        }
        Ok(login_status(login_id, login))
    }

    fn fail(&self, login_id: &str, message: &str) {
        if let Ok(mut logins) = self.lock() {
            if let Some(login) = logins.get_mut(login_id) {
                if login.status == "pending" {
                    login.status = "error";
                    login.error = Some(message.into());
                    login.staged = None;
                    login.verifier.clear();
                    login.state.clear();
                    login.cancel.cancel();
                }
            }
        }
    }

    async fn listen(&self, login_id: &str, listener: TcpListener, cancel: CancellationToken) {
        loop {
            let accepted = tokio::select! {
                _ = cancel.cancelled() => return,
                result = listener.accept() => result,
            };
            let Ok((mut socket, peer)) = accepted else {
                self.fail(login_id, "OAuth callback listener failed; start again");
                return;
            };
            if !peer.ip().is_loopback() {
                continue;
            }
            let request = tokio::select! {
                _ = cancel.cancelled() => return,
                result = tokio::time::timeout(Duration::from_secs(3), async {
                    let mut request = Vec::with_capacity(2048);
                    loop {
                        let mut buffer = [0; 1024];
                        let read = socket.read(&mut buffer).await?;
                        if read == 0 { break; }
                        request.extend_from_slice(&buffer[..read]);
                        if request.len() > 8192 { return Err(std::io::Error::other("request too large")); }
                        if request.windows(4).any(|part| part == b"\r\n\r\n") { break; }
                    }
                    Ok(String::from_utf8_lossy(&request).into_owned())
                }) => result,
            };
            let target = request
                .ok()
                .and_then(|value| value.ok())
                .and_then(|request| {
                    let line = request.lines().next()?;
                    let mut fields = line.split_whitespace();
                    if fields.next()? != "GET" {
                        return None;
                    }
                    let target = fields.next()?;
                    if !target.starts_with("/auth/callback?") {
                        return None;
                    }
                    Some(format!("http://localhost:1455{target}"))
                });
            let valid = target.as_ref().is_some_and(|target| {
                self.lock()
                    .ok()
                    .and_then(|logins| {
                        logins.get(login_id).map(|login| {
                            parse_callback(target, login.provider, &login.state).is_ok()
                                || callback_denied(target, login.provider, &login.state)
                        })
                    })
                    .unwrap_or(false)
            });
            // Never reflect the callback URL, code, state, or provider error into HTML.
            let (status, body) = if valid {
                ("200 OK", "Sign-in received. Return to AMP Desktop.")
            } else {
                ("400 Bad Request", "Invalid OAuth callback. Return to AMP Desktop or paste the final redirect URL.")
            };
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                socket.write_all(response.as_bytes()),
            )
            .await;
            if let Some(target) = target.filter(|_| valid) {
                let _ = self.submit_code(login_id, &target).await;
                // Retain the expiry task until staged credentials are committed.
                cancel.cancelled().await;
                return;
            }
        }
    }
}

fn login_status(id: &str, login: &Login) -> OAuthLoginStatus {
    OAuthLoginStatus {
        login_id: id.into(),
        status: login.status.into(),
        error: login.error.clone(),
    }
}

fn cancel_login(login: &mut Login) {
    if login.status == "pending" {
        login.status = "cancelled";
        login.error = None;
        login.staged = None;
        login.verifier.clear();
        login.state.clear();
        login.cancel.cancel();
    }
}

fn expire(login: &mut Login) {
    if login.status == "pending" && login.created.elapsed() >= LOGIN_TTL {
        cancel_login(login);
        login.status = "error";
        login.error = Some("OAuth login expired; start again".into());
    }
}

fn pkce() -> (String, String) {
    // Three independent UUID-v4 values supply 366 random bits in a 96-char
    // verifier, within RFC 7636's 43..128 unreserved-character requirement.
    let verifier = format!(
        "{}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn authorization_url(provider: Provider, state: &str, challenge: &str) -> Result<String> {
    let (endpoint, scope) = match provider {
        Provider::Openai => ("https://auth.openai.com/oauth/authorize", "openid profile email offline_access api.connectors.read api.connectors.invoke"),
        Provider::Anthropic => ("https://claude.ai/oauth/authorize", "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"),
    };
    let mut url =
        reqwest::Url::parse(endpoint).map_err(|_| anyhow!("OAuth authorization URL is invalid"))?;
    {
        let mut params = url.query_pairs_mut();
        params.extend_pairs([
            ("client_id", provider.client()),
            ("response_type", "code"),
            ("redirect_uri", provider.redirect()),
            ("scope", scope),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
        ]);
        if provider == Provider::Openai {
            params.extend_pairs([
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "omp"),
            ]);
        } else {
            params.append_pair("code", "true");
        }
    }
    Ok(url.into())
}

fn parse_callback(input: &str, provider: Provider, expected_state: &str) -> Result<String> {
    if input.len() > 8192 || expected_state.is_empty() {
        bail!("Invalid OAuth callback");
    }
    let input = input.trim();
    let (code, state) = if input.starts_with("http://") || input.starts_with("https://") {
        let url = reqwest::Url::parse(input).map_err(|_| anyhow!("Invalid OAuth callback URL"))?;
        let expected = reqwest::Url::parse(provider.redirect())
            .map_err(|_| anyhow!("Invalid OAuth redirect URL"))?;
        if url.scheme() != expected.scheme()
            || url.host_str() != expected.host_str()
            || url.port_or_known_default() != expected.port_or_known_default()
            || url.path() != expected.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            bail!("OAuth callback URL does not match the expected redirect");
        }
        let mut code = None;
        let mut state = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "error" => bail!("OAuth authorization was denied or failed"),
                "code" if code.is_none() => code = Some(value.into_owned()),
                "state" if state.is_none() => state = Some(value.into_owned()),
                "code" | "state" => bail!("OAuth callback contains duplicate parameters"),
                _ => {}
            }
        }
        (
            code.ok_or_else(|| anyhow!("OAuth callback is missing its code"))?,
            state.ok_or_else(|| anyhow!("OAuth callback is missing its state"))?,
        )
    } else {
        let (code, state) = input.split_once('#').ok_or_else(|| {
            anyhow!("Paste the complete redirect URL or code#state, not a code alone")
        })?;
        (code.into(), state.into())
    };
    if state != expected_state
        || code.is_empty()
        || code.len() > 4096
        || code.chars().any(char::is_control)
    {
        bail!("OAuth callback code or state is invalid");
    }
    Ok(code)
}

// Validate the redirect and state before treating a provider denial as terminal.
// An unrelated local request must not be able to cancel someone else's login.
fn callback_denied(input: &str, provider: Provider, state: &str) -> bool {
    if input.len() > 8192 {
        return false;
    }
    let Ok(mut url) = reqwest::Url::parse(input.trim()) else {
        return false;
    };
    let pairs: Vec<_> = url.query_pairs().into_owned().collect();
    if !pairs.iter().any(|(key, _)| key == "error") {
        return false;
    }
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in &pairs {
            if key != "error" && key != "code" {
                query.append_pair(key, value);
            }
        }
        query.append_pair("code", "denied");
    }
    parse_callback(url.as_str(), provider, state).is_ok()
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| anyhow!("Could not initialize OAuth client"))
}

async fn token_request(provider: Provider, body: Value, refreshing: bool) -> Result<Value> {
    let mut request = client()?.post(provider.token_url());
    if provider == Provider::Openai {
        request = request.form(&body);
    } else {
        request = request.json(&body);
        if refreshing {
            request = request.header("anthropic-beta", "oauth-2025-04-20").header(
                "User-Agent",
                "anthropic-sdk-typescript/0.112.1 userOAuthProvider",
            );
        }
    }
    let response = request.send().await.map_err(|_| {
        anyhow!("OAuth token request failed; check your connection or sign in again")
    })?;
    if !response.status().is_success() {
        // Endpoint bodies and transport errors may contain credentials or codes.
        bail!(
            "OAuth token request failed (HTTP {}); sign in again if needed",
            response.status().as_u16()
        );
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_TOKEN_BYTES as u64)
    {
        bail!("OAuth token response exceeded the size limit");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow!("Could not read OAuth token response"))?;
        if bytes.len() + chunk.len() > MAX_TOKEN_BYTES {
            bail!("OAuth token response exceeded the size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("OAuth token response was invalid"))
}

async fn exchange(
    provider: Provider,
    code: &str,
    verifier: &str,
    state: &str,
) -> Result<SavedCredential> {
    let mut body = json!({ "grant_type": "authorization_code", "client_id": provider.client(), "code": code,
        "code_verifier": verifier, "redirect_uri": provider.redirect() });
    if provider == Provider::Anthropic {
        body["state"] = state.into();
    }
    parse_token(provider, token_request(provider, body, false).await?, None)
}

fn token_string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && reqwest::header::HeaderValue::from_str(value).is_ok())
        .map(str::to_owned)
}

fn account_from_jwt(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    token_string(
        claims.get("https://api.openai.com/auth")?,
        "chatgpt_account_id",
    )
}

fn parse_token(
    provider: Provider,
    value: Value,
    previous: Option<&SavedCredential>,
) -> Result<SavedCredential> {
    let access_token = token_string(&value, "access_token")
        .ok_or_else(|| anyhow!("OAuth response is missing a valid access token"))?;
    let refresh_token = token_string(&value, "refresh_token")
        .or_else(|| previous.map(|saved| saved.refresh_token.clone()))
        .ok_or_else(|| anyhow!("OAuth response is missing a valid refresh token"))?;
    let lifetime = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 60)
        .ok_or_else(|| anyhow!("OAuth response is missing a valid token lifetime"))?;
    let account_id = match provider {
        Provider::Openai => account_from_jwt(&access_token).or_else(|| {
            value
                .get("id_token")
                .and_then(Value::as_str)
                .and_then(account_from_jwt)
        }),
        Provider::Anthropic => value
            .get("account")
            .and_then(|account| token_string(account, "uuid")),
    }
    .or_else(|| previous.and_then(|saved| saved.account_id.clone()));
    if provider == Provider::Openai && account_id.is_none() {
        bail!("OAuth response is missing the ChatGPT account identifier");
    }
    if let Some(previous) = previous {
        if previous.account_id.is_some() && previous.account_id != account_id {
            bail!("OAuth refresh returned a different account; sign in again");
        }
    }
    Ok(SavedCredential {
        provider,
        access_token,
        refresh_token,
        account_id,
        expires_at: now()
            .checked_add(lifetime)
            .ok_or_else(|| anyhow!("OAuth token expiry is invalid"))?,
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn entry(provider: Provider) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, provider.id())
        .map_err(|_| anyhow!("Could not access OAuth credential storage"))
}

fn read_saved(provider: Provider) -> Result<Option<Arc<SavedCredential>>> {
    CREDENTIAL_CACHE.read_with(provider, || read_saved_uncached(provider))
}

fn read_saved_uncached(
    provider: Provider,
) -> std::result::Result<Option<SavedCredential>, &'static str> {
    let serialized = match entry(provider)
        .map_err(|_| "Could not access OAuth credential storage")?
        .get_password()
    {
        Ok(value) => value,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(_) => return Err("Could not read OAuth credentials from the OS keychain"),
    };
    let credential: SavedCredential = serde_json::from_str(&serialized)
        .map_err(|_| "Saved OAuth credentials are invalid; sign in again")?;
    if credential.provider != provider
        || credential.access_token.is_empty()
        || credential.refresh_token.is_empty()
        || reqwest::header::HeaderValue::from_str(&credential.access_token).is_err()
        || credential
            .account_id
            .as_ref()
            .is_some_and(|id| reqwest::header::HeaderValue::from_str(id).is_err())
    {
        return Err("Saved OAuth credentials are invalid; sign in again");
    }
    Ok(Some(credential))
}

fn save(provider: Provider, credential: SavedCredential) -> Result<Arc<SavedCredential>> {
    CREDENTIAL_CACHE.save_with(provider, credential, |credential| {
        let serialized = serde_json::to_string(credential)
            .map_err(|_| anyhow!("Could not encode OAuth credentials"))?;
        entry(provider)?
            .set_password(&serialized)
            .map_err(|_| anyhow!("Could not save OAuth credentials to the OS keychain"))
    })
}

/// Obtains or rotates an OAuth token. An HTTP/transport failure never deletes the
/// current grant; rotated credentials replace it only after full validation.
pub async fn credentials(config: &ProviderConfig) -> Result<Option<OAuthCredential>> {
    let Some(provider) = Provider::from_config(config) else {
        return Ok(None);
    };
    // Detach the actual rotation from caller cancellation. Once the endpoint has
    // consumed a refresh token, abandoning its response would lose the grant.
    tokio::spawn(async move { credentials_for(provider).await })
        .await
        .map_err(|_| anyhow!("OAuth credential refresh could not finish"))?
}

async fn credentials_for(provider: Provider) -> Result<Option<OAuthCredential>> {
    let _storage = STORAGE_LOCK.lock().await;
    let Some(mut saved) = read_saved(provider)? else {
        return Ok(None);
    };
    if saved.expires_at <= now().saturating_add(60) {
        let value = token_request(provider, json!({ "grant_type": "refresh_token", "client_id": provider.client(), "refresh_token": &saved.refresh_token }), true).await?;
        let refreshed = parse_token(provider, value, Some(&saved))?;
        saved = save(provider, refreshed)?;
    }
    Ok(Some(OAuthCredential {
        access_token: saved.access_token.clone(),
        account_id: saved.account_id.clone(),
    }))
}

pub async fn logout(config: &ProviderConfig) -> Result<()> {
    let provider = Provider::from_config(config)
        .ok_or_else(|| anyhow!("OAuth is only available for unmodified official providers"))?;
    let _storage = STORAGE_LOCK.lock().await;
    CREDENTIAL_CACHE.remove_with(provider, || match entry(provider)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(anyhow!(
            "Could not remove OAuth credentials from the OS keychain"
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Barrier,
    };

    fn cached_grant(access_token: &str) -> SavedCredential {
        SavedCredential {
            provider: Provider::Anthropic,
            access_token: access_token.into(),
            refresh_token: "synthetic-refresh-token".into(),
            expires_at: u64::MAX,
            account_id: None,
        }
    }

    #[test]
    fn credential_cache_single_flights_positive_missing_and_denied_reads() {
        for outcome in 0..3 {
            let cache = CredentialCache::new();
            let loads = AtomicUsize::new(0);
            let start = Barrier::new(8);
            std::thread::scope(|scope| {
                for _ in 0..8 {
                    let (cache, loads, start) = (&cache, &loads, &start);
                    scope.spawn(move || {
                        start.wait();
                        for _ in 0..20 {
                            let result = cache.read_with(Provider::Anthropic, || {
                                loads.fetch_add(1, Ordering::SeqCst);
                                match outcome {
                                    0 => Ok(Some(cached_grant("synthetic-access-token"))),
                                    1 => Ok(None),
                                    _ => Err(
                                        "Could not read OAuth credentials from the OS keychain",
                                    ),
                                }
                            });
                            match outcome {
                                0 => assert_eq!(
                                    result.unwrap().unwrap().access_token,
                                    "synthetic-access-token"
                                ),
                                1 => assert!(result.unwrap().is_none()),
                                _ => assert_eq!(
                                    result.err().unwrap().to_string(),
                                    "Could not read OAuth credentials from the OS keychain"
                                ),
                            }
                        }
                    });
                }
            });
            assert_eq!(loads.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn credential_cache_reconnect_refresh_and_logout_replace_sticky_state() {
        let cache = CredentialCache::new();
        let provider = Provider::Anthropic;
        let denied = "Could not read OAuth credentials from the OS keychain";
        assert!(cache.read_with(provider, || Err(denied)).is_err());
        assert!(cache
            .save_with(provider, cached_grant("failed-login"), |_| {
                bail!("Could not save OAuth credentials to the OS keychain")
            })
            .is_err());
        assert_eq!(
            cache
                .read_with(provider, || panic!("denial must stay cached"))
                .err()
                .unwrap()
                .to_string(),
            denied
        );
        // A denied or missing initial lookup must not affect other providers.
        assert!(cache.read_with(Provider::Openai, || Ok(None)).unwrap().is_none());
        let connected = cache
            .save_with(provider, cached_grant("connected"), |_| Ok(()))
            .unwrap();
        let current = cache
            .read_with(provider, || panic!("login already populated cache"))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&connected, &current));
        let refreshed = cache
            .save_with(provider, cached_grant("refreshed"), |_| Ok(()))
            .unwrap();
        assert!(cache
            .remove_with(provider, || {
                bail!("Could not remove OAuth credentials from the OS keychain")
            })
            .is_err());
        let current = cache
            .read_with(provider, || panic!("refresh already populated cache"))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&refreshed, &current));
        assert_eq!(current.access_token, "refreshed");
        cache.remove_with(provider, || Ok(())).unwrap();
        for _ in 0..20 {
            assert!(cache
                .read_with(provider, || panic!("logout must not reload the old grant"))
                .unwrap()
                .is_none());
        }
        cache
            .save_with(provider, cached_grant("reconnected"), |_| Ok(()))
            .unwrap();
        assert_eq!(
            cache
                .read_with(provider, || panic!("reconnect already populated cache"))
                .unwrap()
                .unwrap()
                .access_token,
            "reconnected"
        );
    }

    #[test]
    fn credential_cache_mutations_cannot_be_overwritten_by_in_flight_loads() {
        for logout in [false, true] {
            let cache = CredentialCache::new();
            let (loading, loaded) = mpsc::channel();
            let (release, released) = mpsc::channel();
            std::thread::scope(|scope| {
                let cache = &cache;
                let reader = scope.spawn(move || {
                    cache
                        .read_with(Provider::Anthropic, || {
                            loading.send(()).unwrap();
                            released.recv().unwrap();
                            Ok(Some(cached_grant("old-grant")))
                        })
                        .unwrap()
                });
                loaded.recv().unwrap();
                let mutation = scope.spawn(move || {
                    if logout {
                        cache.remove_with(Provider::Anthropic, || Ok(()))
                    } else {
                        cache
                            .save_with(Provider::Anthropic, cached_grant("new-grant"), |_| Ok(()))
                            .map(|_| ())
                    }
                });
                release.send(()).unwrap();
                assert_eq!(reader.join().unwrap().unwrap().access_token, "old-grant");
                mutation.join().unwrap().unwrap();
            });
            let current = cache
                .read_with(Provider::Anthropic, || panic!("mutation must populate cache"))
                .unwrap();
            if logout {
                assert!(current.is_none());
            } else {
                assert_eq!(current.unwrap().access_token, "new-grant");
            }
        }
    }

    fn config(provider: &str) -> ProviderConfig {
        crate::default_providers()
            .into_iter()
            .find(|config| config.id == provider)
            .unwrap()
    }

    #[test]
    fn oauth_requires_exact_official_configuration() {
        for id in ["openai", "anthropic"] {
            let original = config(id);
            assert!(oauth_available(&original));
            for url in [
                "https://evil.example/v1",
                "https://api.openai.com/v1/",
                "https://api.openai.com/v1?x=1",
                "https://api.openai.com.evil/v1",
                "https://user@api.openai.com/v1",
            ] {
                let mut changed = original.clone();
                changed.base_url = url.into();
                assert!(!oauth_available(&changed));
            }
            let mut changed = original.clone();
            changed.id = "custom".into();
            assert!(!oauth_available(&changed));
            let mut changed = original;
            changed.kind = "custom".into();
            assert!(!oauth_available(&changed));
        }
    }

    #[test]
    fn pkce_and_authorization_parameters() {
        let (verifier, challenge) = pkce();
        assert!((43..=128).contains(&verifier.len()));
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
        assert_ne!(verifier, pkce().0);
        for provider in [Provider::Openai, Provider::Anthropic] {
            let url = reqwest::Url::parse(
                &authorization_url(provider, "test-state", &challenge).unwrap(),
            )
            .unwrap();
            let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(params["redirect_uri"], provider.redirect());
            assert_eq!(params["state"], "test-state");
            assert_eq!(params["code_challenge_method"], "S256");
            assert_eq!(params["code_challenge"], challenge);
        }
    }

    #[test]
    fn callbacks_require_state_and_exact_redirect() {
        let valid = "http://localhost:1455/auth/callback?code=secret&state=expected";
        assert_eq!(
            parse_callback(valid, Provider::Openai, "expected").unwrap(),
            "secret"
        );
        assert_eq!(
            parse_callback("secret#expected", Provider::Anthropic, "expected").unwrap(),
            "secret"
        );
        for invalid in [
            "secret",
            "secret#wrong",
            "http://localhost:1455/wrong?code=secret&state=expected",
            "http://localhost:1455/auth/callback?code=secret&state=wrong",
            "http://evil.example:1455/auth/callback?code=secret&state=expected",
            "http://localhost:1455/auth/callback?code=a&code=b&state=expected",
            "http://localhost:1455/auth/callback?code=a&state=expected&state=expected",
            "http://localhost:1455/auth/callback?error=secret&state=expected",
        ] {
            let error = parse_callback(invalid, Provider::Openai, "expected")
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret"));
        }
    }

    fn token(account: &str) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(
                    &json!({"https://api.openai.com/auth": {"chatgpt_account_id": account}})
                )
                .unwrap()
            )
        )
    }

    #[test]
    fn refresh_preserves_rotation_and_account_identity() {
        let saved = parse_token(Provider::Openai, json!({"access_token": token("account"), "refresh_token": "original", "expires_in": 3600}), None).unwrap();
        let refreshed = parse_token(Provider::Openai, json!({"access_token": token("account"), "refresh_token": "rotated", "expires_in": 3600}), Some(&saved)).unwrap();
        assert_eq!(refreshed.refresh_token, "rotated");
        let omitted = parse_token(
            Provider::Openai,
            json!({"access_token": token("account"), "expires_in": 3600}),
            Some(&saved),
        )
        .unwrap();
        assert_eq!(omitted.refresh_token, "original");
        assert!(parse_token(
            Provider::Openai,
            json!({"access_token": token("other"), "refresh_token": "rotated", "expires_in": 3600}),
            Some(&saved)
        )
        .is_err());
        assert!(parse_token(
            Provider::Openai,
            json!({"access_token": "invalid", "refresh_token": "refresh", "expires_in": 3600}),
            None
        )
        .is_err());
        assert!(parse_token(Provider::Anthropic, json!({"access_token": "secret\nheader", "refresh_token": "refresh", "expires_in": 3600}), None).is_err());
    }

    #[tokio::test]
    async fn cancellation_expiry_and_configuration_changes_cannot_commit() {
        let manager = OAuthManager::default();
        let config = config("anthropic");
        let start = manager.begin(&config).await.unwrap();
        assert!(start.requires_code);
        assert_eq!(manager.provider_id(&start.login_id).unwrap(), "anthropic");
        assert_eq!(manager.status(&start.login_id).unwrap().status, "pending");
        // Pending completion does not access keychain.
        assert_eq!(
            manager
                .complete(&start.login_id, &config)
                .await
                .unwrap()
                .status,
            "pending"
        );
        let mut changed = config.clone();
        changed.base_url = "https://example.com/v1".into();
        assert!(manager.complete(&start.login_id, &changed).await.is_err());
        manager
            .lock()
            .unwrap()
            .get_mut(&start.login_id)
            .unwrap()
            .staged = Some(SavedCredential {
            provider: Provider::Anthropic,
            access_token: "not-persisted".into(),
            refresh_token: "not-persisted".into(),
            expires_at: now() + 3600,
            account_id: None,
        });
        manager.cancel_provider("anthropic").unwrap();
        assert_eq!(
            manager
                .complete(&start.login_id, &config)
                .await
                .unwrap()
                .status,
            "cancelled"
        );
        assert!(manager
            .submit_code(&start.login_id, "code#state")
            .await
            .is_err());
        let next = manager.begin(&config).await.unwrap();
        manager
            .lock()
            .unwrap()
            .get_mut(&next.login_id)
            .unwrap()
            .created = Instant::now() - LOGIN_TTL;
        assert_eq!(manager.status(&next.login_id).unwrap().status, "error");
        assert!(manager
            .submit_code(&next.login_id, "code#state")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn provider_denial_is_state_checked_and_redacted() {
        let manager = OAuthManager::default();
        let start = manager.begin(&config("anthropic")).await.unwrap();
        let authorize = reqwest::Url::parse(&start.url).unwrap();
        let state = authorize
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let mut redirect = reqwest::Url::parse(ANTHROPIC_REDIRECT).unwrap();
        redirect
            .query_pairs_mut()
            .extend_pairs([("error", "sensitive-provider-body"), ("state", "wrong")]);
        assert!(manager
            .submit_code(&start.login_id, redirect.as_str())
            .await
            .is_err());
        assert_eq!(manager.status(&start.login_id).unwrap().status, "pending");
        redirect.set_query(None);
        redirect.query_pairs_mut().extend_pairs([
            ("error", "sensitive-provider-body"),
            ("state", state.as_str()),
        ]);
        let error = manager
            .submit_code(&start.login_id, redirect.as_str())
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("sensitive-provider-body"));
        assert_eq!(manager.status(&start.login_id).unwrap().status, "error");
    }

    #[tokio::test]
    async fn pending_logins_remain_bounded() {
        let manager = OAuthManager::default();
        for _ in 0..MAX_LOGINS * 3 {
            manager.begin(&config("anthropic")).await.unwrap();
        }
        assert!(manager.lock().unwrap().len() <= MAX_LOGINS);
        manager.cancel_provider("anthropic").unwrap();
    }
}
