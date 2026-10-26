use super::*;
use reqwest::{Client, RequestBuilder, StatusCode};

const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const SMALL_RESPONSE_LIMIT: usize = 32 * 1024;

pub(super) struct Network {
    client: Client,
}
impl Network {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .user_agent("AMP-Desktop-SMS/1")
                .build()
                .context("Cannot initialize outbound SMS networking")?,
        })
    }
    fn relay(
        &self,
        settings: &Settings,
        secrets: &Secrets,
        method: reqwest::Method,
        suffix: &str,
    ) -> Result<RequestBuilder> {
        let url = endpoint(&settings.relay_url, suffix)?;
        Ok(self.client.request(method, url).bearer_auth(
            secrets
                .relay_token
                .as_deref()
                .ok_or_else(|| anyhow!("SMS relay bearer token is missing"))?,
        ))
    }
    pub(super) async fn inbox(
        &self,
        settings: &Settings,
        secrets: &Secrets,
    ) -> Result<Vec<SmsMessage>> {
        #[derive(Deserialize)]
        struct Inbox {
            messages: Vec<SmsMessage>,
        }
        let bytes = response(
            self.relay(settings, secrets, reqwest::Method::GET, "v1/inbox")?,
            RESPONSE_LIMIT,
        )
        .await?;
        let inbox: Inbox =
            serde_json::from_slice(&bytes).context("Relay returned an invalid inbox response")?;
        if inbox.messages.len() > 120 {
            bail!("Relay returned too many SMS messages");
        }
        Ok(inbox.messages)
    }
    pub(super) async fn claim(
        &self,
        settings: &Settings,
        secrets: &Secrets,
        message: &SmsMessage,
    ) -> Result<()> {
        let bytes = response(
            self.relay(
                settings,
                secrets,
                reqwest::Method::POST,
                &format!("v1/inbox/{}/claim", message.id),
            )?
            .json(&json!({"claim_id":message.claim_id})),
            SMALL_RESPONSE_LIMIT,
        )
        .await?;
        let claimed: SmsMessage =
            serde_json::from_slice(&bytes).context("Relay claim returned invalid message")?;
        if claimed.id != message.id
            || claimed.project_id != message.project_id
            || claimed.source_id != message.source_id
            || claimed.from_id != message.from_id
            || claimed.body != message.body
            || claimed.received_at != message.received_at
            || claimed.state != "processing"
            || claimed.claim_id != message.claim_id
        {
            bail!("Relay claim identity mismatch; no prompt was submitted");
        }
        Ok(())
    }
    pub(super) async fn finish(
        &self,
        settings: &Settings,
        secrets: &Secrets,
        message: &SmsMessage,
    ) -> Result<bool> {
        let request = self
            .relay(
                settings,
                secrets,
                reqwest::Method::POST,
                &format!("v1/inbox/{}/finish", message.id),
            )?
            .json(
                &json!({"claim_id":message.claim_id,"state":message.state,"detail":message.detail}),
            );
        let mut result = request
            .send()
            .await
            .map_err(|_| anyhow!("Relay finish request failed or timed out"))?;
        // Another claim/terminal state cannot be overwritten. Preserve the local
        // unknown marker, but do not let this row starve unrelated pending work.
        if result.status() == StatusCode::CONFLICT {
            return Ok(false);
        }
        if !result.status().is_success() {
            bail!("Relay finish returned HTTP {}", result.status().as_u16());
        }
        bounded_body(&mut result, SMALL_RESPONSE_LIMIT).await?;
        Ok(true)
    }
    pub(super) async fn send(
        &self,
        settings: &Settings,
        secrets: &Secrets,
        message: &SmsMessage,
        reply: &str,
    ) -> Result<String> {
        let request = self.client.post(endpoint(&settings.pipestreamr_url, "api/v1/messages/send")?)
            .bearer_auth(secrets.api_key.as_deref().ok_or_else(|| anyhow!("PipeStreamr API key is missing"))?)
            .header("Idempotency-Key", format!("reply-{}-1", message.id))
            .json(&json!({"integration_id":message.source_id,"project_id":message.project_id,"to":message.from_id,"body":reply}));
        let bytes = response(request, SMALL_RESPONSE_LIMIT).await?;
        #[derive(Deserialize)]
        struct Receipt {
            id: String,
            sid: String,
            status: String,
        }
        let receipt: Receipt =
            serde_json::from_slice(&bytes).context("Invalid PipeStreamr SMS acceptance receipt")?;
        if Uuid::parse_str(&receipt.id).is_err()
            || receipt.sid.len() > 128
            || receipt.sid.is_empty()
            || receipt.status.len() > 64
            || !receipt
                .status
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            bail!("Invalid PipeStreamr acceptance receipt");
        }
        Ok(format!("event {}, status {}", receipt.id, receipt.status))
    }
}

pub(super) fn base_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("SMS URL must be an absolute HTTPS URL")?;
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if url.host().is_none()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("SMS URLs require HTTPS (HTTP loopback only for development), with no credentials, query, or fragment");
    }
    // Reject ambiguous encoded path separators/dot traversal rather than altering
    // the user's endpoint when joining the fixed authenticated API suffix.
    let path = url.path().to_ascii_lowercase();
    if path.contains("%2f") || path.contains("%5c") || path.contains("%2e") {
        bail!("SMS URL path contains ambiguous escaping");
    }
    Ok(url)
}
fn endpoint(base: &str, suffix: &str) -> Result<Url> {
    let mut url = base_url(base)?;
    let path = format!("{}/{}", url.path().trim_end_matches('/'), suffix);
    url.set_path(&path);
    Ok(url)
}
async fn response(request: RequestBuilder, limit: usize) -> Result<Vec<u8>> {
    let mut response = request.send().await.map_err(|_| anyhow!("SMS HTTP request failed or timed out; inspect relay/PipeStreamr before retrying uncertain work"))?;
    if !response.status().is_success() {
        bail!("SMS endpoint returned HTTP {}", response.status().as_u16());
    }
    bounded_body(&mut response, limit).await
}
async fn bounded_body(response: &mut reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        bail!("SMS HTTP response exceeds size limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("SMS HTTP response interrupted"))?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            bail!("SMS HTTP response exceeds size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
