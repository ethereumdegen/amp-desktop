import { useCallback, useEffect, useRef, useState } from "react";
import { KeyRound, LoaderCircle, RefreshCw } from "lucide-react";
import { errorText, inDesktop, request } from "./api";
import type { Bot, JsonObject, SmsConfiguration, SmsMessage, SmsStatus } from "./api";

type SmsForm = Pick<SmsStatus,
  "enabled" | "relayUrl" | "pipestreamrUrl" | "projectId" | "sourceId" | "phone" | "botId"
>;
const emptyForm: SmsForm = {
  enabled: false,
  relayUrl: "",
  pipestreamrUrl: "",
  projectId: "",
  sourceId: "",
  phone: "",
  botId: "",
};
const uuidPattern = "[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}";
function settingsFrom(status: SmsStatus): SmsForm {
  return {
    enabled: status.enabled,
    relayUrl: status.relayUrl,
    pipestreamrUrl: status.pipestreamrUrl,
    projectId: status.projectId,
    sourceId: status.sourceId,
    phone: status.phone,
    botId: status.botId,
  };
}
function receivedDate(value: number): string {
  const date = new Date(value);
  return Number.isFinite(date.getTime()) ? date.toLocaleString() : "Date unavailable";
}
function messageState(message: SmsMessage): string {
  if (message.state === "pending") return message.review ? "Needs review" : "Pending";
  if (message.state === "unknown") return "Unknown outcome";
  return message.state.charAt(0).toUpperCase() + message.state.slice(1);
}

export function SmsSettings({ bots }: { bots: Bot[] }) {
  const [status, setStatus] = useState<SmsStatus | null>(null);
  const [form, setForm] = useState<SmsForm>(emptyForm);
  const [relayToken, setRelayToken] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [pollError, setPollError] = useState("");
  const [mutationError, setMutationError] = useState("");
  const [notice, setNotice] = useState("");
  const [refresh, setRefresh] = useState(0);
  const [refreshing, setRefreshing] = useState(false);
  const [lastUpdated, setLastUpdated] = useState<number | null>(null);
  const initialized = useRef(false);
  const alive = useRef(false);
  const mutation = useRef<AbortController | null>(null);
  const queue = useRef<Promise<void>>(Promise.resolve());

  // Native IPC cannot be aborted mid-flight. Serialize even cancelled requests,
  // and ignore stale results, so a poll cannot overwrite a newer mutation.
  const serial = useCallback(<T,>(work: () => Promise<T>): Promise<T> => {
    const result = queue.current.then(work);
    queue.current = result.then(() => undefined, () => undefined);
    return result;
  }, []);

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      mutation.current?.abort();
    };
  }, []);

  useEffect(() => {
    if (!inDesktop || busy) return;
    let active = true;
    let timer: number | undefined;
    const controller = new AbortController();
    async function poll() {
      setRefreshing(true);
      try {
        const next = await serial(async () => {
          if (!active || controller.signal.aborted) throw new DOMException("Request cancelled", "AbortError");
          return request<SmsStatus>("sms_status", {}, { signal: controller.signal });
        });
        if (!active) return;
        setStatus(next);
        setPollError("");
        setLastUpdated(Date.now());
        if (!initialized.current) {
          initialized.current = true;
          setForm(settingsFrom(next));
        }
      } catch (reason) {
        if (active) setPollError(errorText(reason));
      } finally {
        if (active) {
          setRefreshing(false);
          timer = window.setTimeout(() => void poll(), 3000);
        }
      }
    }
    void poll();
    return () => {
      active = false;
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [busy, refresh, serial]);

  if (!inDesktop) return null;

  function change<K extends keyof SmsForm>(key: K, value: SmsForm[K]) {
    setForm((previous) => ({ ...previous, [key]: value }));
    setNotice("");
  }

  async function mutate(
    method: "sms_configure" | "sms_accept" | "sms_dismiss",
    args: JsonObject,
    key: string,
    success: string,
  ) {
    if (mutation.current) return;
    const controller = new AbortController();
    mutation.current = controller;
    setBusy(key);
    setNotice("");
    setMutationError("");
    try {
      const next = await serial(async () => {
        if (controller.signal.aborted) throw new DOMException("Request cancelled", "AbortError");
        return request<SmsStatus>(method, args, { signal: controller.signal });
      });
      if (!alive.current || controller.signal.aborted) return;
      setStatus(next);
      setPollError("");
      setLastUpdated(Date.now());
      if (method === "sms_configure") {
        setForm(settingsFrom(next));
        setRelayToken("");
        setApiKey("");
      }
      setNotice(success);
    } catch (reason) {
      if (alive.current && !controller.signal.aborted) setMutationError(errorText(reason));
    } finally {
      if (mutation.current === controller) mutation.current = null;
      if (alive.current && !controller.signal.aborted) {
        setBusy(null);
        // Refresh after failures too: configuration may have been saved, or a
        // relay transition may have completed before the response was lost.
        setRefresh((value) => value + 1);
      }
    }
  }

  function save() {
    const configuration: SmsConfiguration = {
      ...form,
      relayUrl: form.relayUrl.trim(),
      pipestreamrUrl: form.pipestreamrUrl.trim(),
      projectId: form.projectId.trim(),
      sourceId: form.sourceId.trim(),
      phone: form.phone.trim(),
      ...(relayToken ? { relayToken } : {}),
      ...(apiKey ? { apiKey } : {}),
    };
    void mutate("sms_configure", configuration, "configuration", "SMS settings saved. Connection state is shown above.");
  }

  const selectedBot = bots.find((bot) => bot.id === form.botId);
  const pendingCount = status?.messages.filter((message) => message.state === "pending").length ?? 0;
  const unavailable = !!busy || !!pollError;

  return (
    <section className="sms-settings">
      <div className="eyebrow">This desktop only</div>
      <h2>PipeStreamr SMS</h2>
      <p className="muted">
        Text your PipeStreamr number from your verified phone to reach an OMP bot
        and receive its final answer by SMS. Your desktop connects outbound to a separately hosted relay; no
        public listener is opened on this Mac.
      </p>
      <div className="callout">
        <strong>Keep this Mac awake and AMP Desktop running</strong>
        <p>
          Run <code>pipestreamr-relay</code> on an always-on host behind HTTPS and
          point PipeStreamr's signed webhook to its <code>/webhooks/pipestreamr</code> endpoint.
          Configure that relay with the same project, source, verified phone, and
          bearer token used below. Only the relay needs the webhook signing secret;
          the PipeStreamr API key stays on this desktop for replies.
        </p>
        <p>
          Existing tool approvals still apply. Review them in AMP Desktop or your
          private remote UI, never by SMS. Messages received while offline or
          disconnected require explicit review before they can run.
        </p>
      </div>
      {pollError && (
        <p className="inline-error" role="alert">
          Cannot refresh SMS status: {pollError}
          {status && " Displayed status and inbox may be out of date. Message actions are paused until status refreshes."}
        </p>
      )}
      {mutationError && <p className="inline-error" role="alert">{mutationError}</p>}
      {status ? (
        <>
          <div className={status.enabled && status.running && !status.error && !pollError ? "success" : "callout"} role="status">
            <strong>
              {pollError ? "Connection status unavailable"
                : !status.enabled ? "SMS connector is disabled"
                : status.error ? "SMS connector needs attention"
                : status.running ? "SMS connector is running"
                : "SMS enabled — connector is not running"}
            </strong>
            <span className="sms-status-detail">
              {status.botId ? `Target: ${bots.find((bot) => bot.id === status.botId)?.name ?? "Unavailable bot"}` : "No target bot configured"}
              {lastUpdated && ` · Last checked ${new Date(lastUpdated).toLocaleTimeString()}`}
            </span>
          </div>
          {status.error && <p className="inline-error" role="alert">{status.error}</p>}
        </>
      ) : !pollError ? (
        <p className="muted" role="status"><LoaderCircle size={14} className="spin" /> Loading SMS settings…</p>
      ) : null}
      <div className="button-row">
        <button type="button" disabled={!!busy || refreshing} onClick={() => setRefresh((value) => value + 1)}>
          <RefreshCw size={14} className={refreshing ? "spin" : undefined} />
          Refresh status
        </button>
        {notice && <span className="success-text" role="status">{notice}</span>}
      </div>
      {status && (
        <>
          <form onSubmit={(event) => { event.preventDefault(); save(); }}>
            <fieldset className="native-provider-fields" disabled={!!busy}>
              <h3>Connector setup</h3>
              <label>
                Relay URL
                <input type="url" required={form.enabled} value={form.relayUrl}
                  onChange={(event) => change("relayUrl", event.target.value)}
                  placeholder="https://relay.example.com" autoCapitalize="none" autoCorrect="off" spellCheck={false} />
              </label>
              <label>
                PipeStreamr URL
                <input type="url" required={form.enabled} value={form.pipestreamrUrl}
                  onChange={(event) => change("pipestreamrUrl", event.target.value)}
                  placeholder="https://your-pipestreamr.example.com" autoCapitalize="none" autoCorrect="off" spellCheck={false} />
              </label>
              <p className="field-help">Use HTTPS base URLs without credentials, query, or fragment. HTTP is allowed only on loopback for local development.</p>
              <div className="form-grid">
                <label>
                  Project UUID
                  <input required={form.enabled} pattern={uuidPattern} title="Enter the PipeStreamr project UUID" value={form.projectId}
                    onChange={(event) => change("projectId", event.target.value)}
                    placeholder="xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" autoCapitalize="none" spellCheck={false} />
                </label>
                <label>
                  Source / integration UUID
                  <input required={form.enabled} pattern={uuidPattern} title="Enter the PipeStreamr source or integration UUID" value={form.sourceId}
                    onChange={(event) => change("sourceId", event.target.value)}
                    placeholder="xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" autoCapitalize="none" spellCheck={false} />
                </label>
              </div>
              <label>
                Verified sender phone (E.164)
                <input type="tel" required={form.enabled} pattern="\+[1-9][0-9]{1,14}"
                  title="Use international E.164 format, such as +14155550123" value={form.phone}
                  onChange={(event) => change("phone", event.target.value)} placeholder="+14155550123" />
              </label>
              <p className="field-help">Only inbound SMS from this verified sender is accepted. Include the country code; omit spaces and punctuation.</p>
              <label>
                Target bot
                <select required={form.enabled} value={form.botId} onChange={(event) => change("botId", event.target.value)}>
                  <option value="">{bots.length ? "Choose a bot" : "No bots available"}</option>
                  {form.botId && !selectedBot && <option value={form.botId}>Previously selected bot (unavailable)</option>}
                  {bots.map((bot) => <option key={bot.id} value={bot.id}>{bot.name} — {bot.status.replaceAll("_", " ")}</option>)}
                </select>
              </label>
              <p className="field-help">
                {bots.length ? "A dedicated SMS bot is recommended. Configure its model first. The connector starts it as needed, creates a fresh SMS conversation, and pins that conversation; switching it manually pauses SMS."
                  : "Create a dedicated SMS bot from the bots sidebar, then return here to finish setup."}
              </p>
              {selectedBot && !selectedBot.isRunning && <p className="callout">The selected bot is stopped. The connector will start it when an eligible SMS arrives.</p>}
              <label>
                <span><KeyRound size={14} /> Relay bearer token</span>
                <input type="password" autoComplete="new-password" value={relayToken}
                  onChange={(event) => { setRelayToken(event.target.value); setNotice(""); }}
                  placeholder={status.hasRelayToken ? "Leave blank to keep the saved token" : "Enter the relay's bearer token"} />
              </label>
              <p className="field-help">{status.hasRelayToken ? "A relay token is saved." : "No relay token is saved."} Stored in the Mac's Keychain; never displayed. Enter a value only to replace it.</p>
              <label>
                <span><KeyRound size={14} /> PipeStreamr API key</span>
                <input type="password" autoComplete="new-password" value={apiKey}
                  onChange={(event) => { setApiKey(event.target.value); setNotice(""); }}
                  placeholder={status.hasApiKey ? "Leave blank to keep the saved key" : "Enter a PipeStreamr key for SMS replies"} />
              </label>
              <p className="field-help">{status.hasApiKey ? "A PipeStreamr API key is saved." : "No PipeStreamr API key is saved."} Saved credentials are write-only and are never returned to this form.</p>
              <label className="choice">
                <input type="checkbox" checked={form.enabled} onChange={(event) => change("enabled", event.target.checked)} />
                Enable SMS connector on this desktop
              </label>
              <p className="field-help sms-enabled-help">Saving applies this setting. Disabling stops SMS monitoring, not a bot turn already submitted. An interrupted prompt or reply is marked unknown and never replayed automatically; inspect its transcript before sending another request.</p>
              <div className="button-row">
                <button type="submit" className="primary" disabled={!!busy || (form.enabled && !selectedBot)}>
                  {busy === "configuration" && <LoaderCircle size={14} className="spin" />}
                  Save SMS settings
                </button>
              </div>
            </fieldset>
          </form>
          <div className="sms-inbox-heading">
            <h3>SMS inbox</h3>
            <span className="muted">{pendingCount} pending · {status.messages.length} shown</span>
          </div>
          <p className="muted sms-inbox-help">
            Fresh messages can run automatically while connected. Accept sends a pending
            message to the configured bot; dismiss discards it without running.
            Offline backlog needs review. Unknown outcomes are never retried here:
            inspect the bot transcript and PipeStreamr before sending a new message.
          </p>
          {status.messages.length ? (
            <ol className="sms-inbox" aria-label="SMS messages">
              {status.messages.map((message) => (
                <li key={message.id} className="sms-message">
                  <div className="sms-message-heading">
                    <strong>{message.from_id}</strong>
                    <span className={`sms-state sms-state-${message.state}`}>{messageState(message)}</span>
                  </div>
                  <time className="muted">{receivedDate(message.received_at)}</time>
                  <p className="sms-message-body">{message.body}</p>
                  {message.detail && <p className="sms-message-detail">{message.detail}</p>}
                  {message.state === "unknown" && <p className="sms-message-detail">The prompt or reply may already have been delivered. This message cannot be accepted again.</p>}
                  {message.state === "processing" && <p className="sms-message-detail">Claimed for processing. Check the target bot for progress or approval requests.</p>}
                  {message.state === "pending" && (
                    <div className="button-row">
                      <button type="button" className="primary" disabled={unavailable || !status.enabled || !status.running}
                        title={!status.enabled || !status.running ? "Enable and connect the SMS connector before accepting messages" : undefined}
                        onClick={() => void mutate("sms_accept", { id: message.id }, message.id, "Message accepted. Follow its progress in the inbox and bot conversation.")}>
                        {busy === message.id && <LoaderCircle size={13} className="spin" />}
                        Accept & send to bot
                      </button>
                      <button type="button" disabled={unavailable}
                        onClick={() => void mutate("sms_dismiss", { id: message.id }, message.id, "Message dismissed without running the bot.")}>
                        Dismiss
                      </button>
                    </div>
                  )}
                </li>
              ))}
            </ol>
          ) : <div className="callout">No messages to show. Once configured, send an SMS from your verified phone to your PipeStreamr number.</div>}
        </>
      )}
    </section>
  );
}
