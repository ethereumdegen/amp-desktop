import { useEffect, useRef, useState } from "react";
import { Copy, FolderOpen, KeyRound, LoaderCircle, RefreshCw } from "lucide-react";
import type { JsonObject, NativeLogin, NativeLoginStatus, Provider, Snapshot } from "./api";
import { errorText, inDesktop, request } from "./api";
import { ExternalLinkButton, Modal, openExternalUrl } from "./ui";
import { RemoteAccessSettings } from "./RemoteAccess";
import { SmsSettings } from "./SmsSettings";

export type Run = (
  method: string,
  args?: JsonObject,
) => Promise<Snapshot | null>;
const install = "curl -fsSL https://omp.sh/install | sh";
export function Installation({
  snapshot,
  run,
  busy,
  chooseNative,
}: {
  snapshot: Snapshot;
  run: Run;
  busy: boolean;
  chooseNative: () => void;
}) {
  const [path, setPath] = useState(snapshot.engine.configuredPath ?? "");
  const [copy, setCopy] = useState("Copy command");
  return (
    <section className="installation">
      <div className="eyebrow">Installed OMP engine</div>
      <h2>
        {snapshot.engine.status === "ready"
          ? "OMP is connected"
          : inDesktop
            ? "Connect your local OMP"
            : "Connect the host's OMP"}
      </h2>
      <p className="muted">
        Installed OMP uses its own tools, extensions, profiles, sessions, and
        credentials. Native bots work independently and do not require this
        installation.
      </p>
      {snapshot.engine.status === "ready" ? (
        <div className="success">
          <strong>{snapshot.engine.version}</strong>
          <code>{snapshot.engine.path}</code>
        </div>
      ) : (
        <>
          <p>
            Install OMP in Terminal on {inDesktop ? "this Mac" : "the host Mac"}{" "}
            using the official command. This app will only copy it, never
            execute it.
          </p>
          <div className="copy-command">
            <code>{install}</code>
            <button
              onClick={async () => {
                try {
                  await navigator.clipboard.writeText(install);
                  setCopy("Copied");
                } catch {
                  setCopy("Copy unavailable — select command");
                }
              }}
            >
              <Copy size={14} />
              {copy}
            </button>
          </div>
        </>
      )}
      {snapshot.engine.error && (
        <p className="inline-error">{snapshot.engine.error}</p>
      )}
      <form
        onSubmit={(event) => {
          event.preventDefault();
          void run("configure_engine", { path: path.trim() || null });
        }}
      >
        <label>
          Explicit executable path <span className="muted">(optional)</span>
          <input
            value={path}
            onChange={(event) => setPath(event.target.value)}
            placeholder="/absolute/path/to/omp"
          />
        </label>
        <div className="button-row">
          <button disabled={busy} className="primary" type="submit">
            Save path & connect
          </button>
          <button
            disabled={busy}
            type="button"
            onClick={async () => {
              if (await run("configure_engine", { path: null })) setPath("");
            }}
          >
            <RefreshCw size={14} />
            Retry auto discovery
          </button>
        </div>
      </form>
      <div className="callout">
        <strong>Authentication stays with OMP</strong>
        <p>
          Run <code>omp</code> in Terminal on{" "}
          {inDesktop ? "this Mac" : "the host Mac"}, then use{" "}
          <code>/login</code> and configure a model. If your bot has no
          available models, complete provider authentication in OMP and restart
          that bot. This app does not read OMP credentials.
        </p>
      </div>
      <div className="button-row">
        <button onClick={chooseNative}>Use Native instead</button>
        <ExternalLinkButton url="https://omp.sh">
          Official installation help
        </ExternalLinkButton>
      </div>
    </section>
  );
}
interface ProviderForm {
  id: string;
  name: string;
  kind: "openai" | "anthropic";
  baseUrl: string;
  apiKeyEnv: string;
  models: string;
  apiKey: string;
}
const empty: ProviderForm = {
  id: "",
  name: "",
  kind: "openai",
  baseUrl: "",
  apiKeyEnv: "",
  models: "",
  apiKey: "",
};
function fromProvider(provider: Provider): ProviderForm {
  return {
    ...provider,
    apiKeyEnv: provider.apiKeyEnv ?? "",
    models: provider.models.join("\n"),
    apiKey: "",
  };
}
function NativeAccountCard({
  provider,
  snapshot,
  run,
  busy,
  onApiKey,
}: {
  provider: Provider;
  snapshot: Snapshot;
  run: Run;
  busy: boolean;
  onApiKey: () => void;
}) {
  const [login, setLogin] = useState<(NativeLogin & { startedAt: number }) | null>(null);
  const [code, setCode] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [message, setMessage] = useState("");
  const alive = useRef(true);
  const loginRef = useRef<string | null>(null);
  const providerRunning = snapshot.bots.some(
    (bot) => bot.engine === "native" && bot.nativeProvider === provider.id && bot.isRunning,
  );
  const connected = provider.authMethod === "oauth";
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      if (loginRef.current)
        void request("cancel_native_login", { loginId: loginRef.current }).catch(() => {});
    };
  }, []);
  useEffect(() => {
    if (!login || loading) return;
    const session = login;
    let active = true;
    let timer: number | undefined;
    const controller = new AbortController();
    async function poll() {
      try {
        if (Date.now() - session.startedAt > 10 * 60 * 1000) {
          await request("cancel_native_login", { loginId: session.loginId }, { signal: controller.signal });
          if (!active) return;
          loginRef.current = null;
          setLogin(null);
          setCode("");
          setError("Sign-in timed out. Start again when you are ready.");
          return;
        }
        const result = await request<NativeLoginStatus>(
          "native_login_status", { loginId: session.loginId }, { signal: controller.signal },
        );
        if (!active) return;
        if (result.status === "pending") {
          timer = window.setTimeout(() => void poll(), 1500);
        } else {
          loginRef.current = null;
          setLogin(null);
          setCode("");
          if (result.status === "connected") {
            setMessage("Account connected.");
            await run("bootstrap");
          } else {
            setError(result.error || (result.status === "cancelled" ? "Sign-in cancelled." : "Sign-in failed. Please try again."));
          }
        }
      } catch (reason) {
        if (!active) return;
        setError(errorText(reason));
        setLogin(null);
        setCode("");
        loginRef.current = null;
        void request("cancel_native_login", { loginId: session.loginId }).catch(() => {});
      }
    }
    void poll();
    return () => {
      active = false;
      controller.abort();
      clearTimeout(timer);
    };
  }, [login, loading, run]);
  return (
    <section className="native-account-card" aria-label={`${provider.name} account`}>
      <div className="native-account-heading">
        <div>
          <h3>{provider.kind === "openai" ? "OpenAI" : "Anthropic"}</h3>
          <span className="muted">{provider.name}</span>
        </div>
        <span className={`auth-badge ${connected ? "connected" : ""}`}>
          {connected ? "Account connected" : provider.authMethod === "apiKey" ? "API key available" : "Not connected"}
        </span>
      </div>
      <p>Use your {provider.kind === "openai" ? "ChatGPT" : "Claude"} account for Native inference. Account access and model availability depend on your subscription.</p>
      {providerRunning && <p className="native-account-guard" role="status">Stop all bots using {provider.name} before connecting, disconnecting, or changing credentials.</p>}
      {login ? (
        <div className="native-login-pending">
          <p role="status"><LoaderCircle size={14} className="spin" aria-hidden="true" /> Waiting for account sign-in…</p>
          <p className="native-login-instructions">{login.instructions}</p>
          <ExternalLinkButton url={login.url}>Open sign-in page</ExternalLinkButton>
          <p className="field-help">If the sign-in page did not open, use the link above. Keep these settings open until sign-in finishes.</p>
          {!inDesktop && !login.requiresCode && <p className="field-help">Signing in from another device? The automatic callback runs on the host Mac. If it cannot open, copy the final browser redirect URL and paste it below.</p>}
          <form onSubmit={async (event) => {
            event.preventDefault();
            setLoading(true);
            setError("");
            try {
              const result = await run("complete_native_login", { loginId: login.loginId, code: code.trim() });
              if (!alive.current) return;
              if (result) {
                loginRef.current = null;
                setLogin(null);
                setCode("");
                setMessage("Account connected.");
                await run("bootstrap");
              } else setError("Could not complete sign-in. Check the error and try again.");
            } finally {
              if (alive.current) setLoading(false);
            }
          }}>
            <label>
              {login.requiresCode ? "Authorization code" : "Manual fallback: callback URL"}
              <input
                type="password"
                autoComplete="off"
                autoCapitalize="none"
                spellCheck={false}
                value={code}
                onChange={(event) => setCode(event.target.value)}
                placeholder={login.requiresCode ? "Paste the code from the browser" : "Paste the final redirect URL if automatic sign-in fails"}
                disabled={loading || busy || providerRunning}
              />
            </label>
            <div className="button-row">
              <button type="submit" disabled={!code.trim() || loading || busy || providerRunning}>Complete sign-in</button>
              <button type="button" disabled={loading || busy} onClick={async () => {
                setLoading(true);
                const result = await run("cancel_native_login", { loginId: login.loginId });
                if (!alive.current) return;
                if (result) {
                  loginRef.current = null;
                  setLogin(null);
                  setCode("");
                  setError("");
                  setMessage("Sign-in cancelled.");
                }
                setLoading(false);
              }}>Cancel sign-in</button>
            </div>
          </form>
        </div>
      ) : (
        <div className="button-row">
          <button type="button" className={connected ? "" : "primary"} disabled={busy || loading || providerRunning} onClick={async () => {
            setLoading(true);
            setError("");
            setMessage("");
            try {
              const result = await request<NativeLogin>("start_native_login", { providerId: provider.id });
              if (!alive.current) {
                void request("cancel_native_login", { loginId: result.loginId }).catch(() => {});
                return;
              }
              loginRef.current = result.loginId;
              setLogin({ ...result, startedAt: Date.now() });
              await openExternalUrl(result.url);
            } catch (reason) {
              if (alive.current) setError(errorText(reason));
            } finally {
              if (alive.current) setLoading(false);
            }
          }}>
            {loading && <LoaderCircle size={14} className="spin" aria-hidden="true" />}
            {connected ? "Reconnect account" : "Sign in with browser"}
          </button>
          {connected && <button type="button" disabled={busy || loading || providerRunning} onClick={async () => {
            if (!window.confirm(`Disconnect the ${provider.name} account from Native? Saved API keys, if any, remain available.`)) return;
            setLoading(true);
            setError("");
            setMessage("");
            const result = await run("logout_native_provider", { providerId: provider.id });
            if (!alive.current) return;
            if (result) setMessage("Account disconnected.");
            setLoading(false);
          }}>Disconnect</button>}
          <button type="button" className="text-button" onClick={onApiKey}>Use an API key instead</button>
        </div>
      )}
      {message && <p className="native-account-message" role="status">{message}</p>}
      {error && <p className="inline-error" role="alert">{error}</p>}
    </section>
  );
}
export function EngineSettings({
  snapshot,
  run,
  busy,
  onClose,
  chooseNative,
  initialTab = "native",
}: {
  snapshot: Snapshot;
  run: Run;
  busy: boolean;
  onClose: () => void;
  chooseNative: () => void;
  initialTab?: "native" | "omp" | "remote" | "sms";
}) {
  const [tab, setTab] = useState<"native" | "omp" | "remote" | "sms">(
    (initialTab === "remote" || initialTab === "sms") && !inDesktop ? "native" : initialTab,
  );
  const [selected, setSelected] = useState(
    snapshot.native.providers[0]?.id ?? "",
  );
  const [form, setForm] = useState<ProviderForm>(
    snapshot.native.providers[0]
      ? fromProvider(snapshot.native.providers[0])
      : empty,
  );
  const [saved, setSaved] = useState(false);
  const [advanced, setAdvanced] = useState(false);
  const apiKeyInput = useRef<HTMLInputElement>(null);
  const provider = snapshot.native.providers.find(
    (item) => item.id === selected,
  );
  const providerRunning = snapshot.bots.some(
    (bot) => bot.engine === "native" && bot.nativeProvider === selected && bot.isRunning,
  );
  const providerWorking = snapshot.bots.some(
    (bot) => bot.engine === "native" && bot.nativeProvider === selected &&
      ["starting", "working", "needs_input"].includes(bot.status),
  );
  function change<K extends keyof ProviderForm>(
    key: K,
    value: ProviderForm[K],
  ) {
    setSaved(false);
    setForm((previous) => ({ ...previous, [key]: value }));
  }
  function select(id: string) {
    setSelected(id);
    setSaved(false);
    setForm(
      snapshot.native.providers.find((item) => item.id === id)
        ? fromProvider(
            snapshot.native.providers.find((item) => item.id === id)!,
          )
        : empty,
    );
  }
  return (
    <Modal title="Engine settings" onClose={onClose} wide>
      <div className="tabs" role="tablist" aria-label="Engine settings">
        <button
          role="tab"
          aria-selected={tab === "native"}
          className={tab === "native" ? "active" : ""}
          onClick={() => setTab("native")}
        >
          Native providers
        </button>
        <button
          role="tab"
          aria-selected={tab === "omp"}
          className={tab === "omp" ? "active" : ""}
          onClick={() => setTab("omp")}
        >
          Installed OMP
        </button>
        {inDesktop && (
          <button
            role="tab"
            aria-selected={tab === "remote"}
            className={tab === "remote" ? "active" : ""}
            onClick={() => setTab("remote")}
          >
            Remote access
          </button>
        )}
        {inDesktop && (
          <button
            role="tab"
            aria-selected={tab === "sms"}
            className={tab === "sms" ? "active" : ""}
            onClick={() => setTab("sms")}
          >
            SMS
          </button>
        )}
      </div>
      <div className="dialog-content">
        {tab === "remote" && inDesktop ? (
          <RemoteAccessSettings />
        ) : tab === "sms" && inDesktop ? (
          <SmsSettings bots={snapshot.bots} />
        ) : tab === "omp" ? (
          <Installation
            snapshot={snapshot}
            run={run}
            busy={busy}
            chooseNative={chooseNative}
          />
        ) : (
          <>
            <p className="muted">
              Native runs a Rust agent on{" "}
              {inDesktop ? "this Mac" : "the host Mac"} and connects directly to
              your selected provider. Credentials are stored in the host OS
              keychain or read from the environment, never included in bot
              snapshots.
            </p>
            <div className="native-accounts">
              <div className="native-accounts-title">
                <h3>Connect an account</h3>
                <p className="muted">Sign in directly for Native. Installed OMP credentials are separate and are not used here.</p>
              </div>
              {snapshot.native.providers.filter((item) => item.oauthAvailable).map((item) => (
                <NativeAccountCard
                  key={item.id}
                  provider={item}
                  snapshot={snapshot}
                  run={run}
                  busy={busy}
                  onApiKey={() => {
                    select(item.id);
                    setAdvanced(true);
                    requestAnimationFrame(() => apiKeyInput.current?.focus());
                  }}
                />
              ))}
              {!snapshot.native.providers.some((item) => item.oauthAvailable) && (
                <p className="muted">Account sign-in is available only for the official OpenAI and Anthropic endpoints. Configure an official provider below, or use an API key for a custom endpoint.</p>
              )}
            </div>
            {snapshot.native.error && (
              <p className="inline-error" role="alert">{snapshot.native.error}</p>
            )}
            <details className="native-advanced" open={advanced} onToggle={(event) => setAdvanced(event.currentTarget.open)}>
              <summary>API keys & advanced provider settings</summary>
              <div className="native-advanced-content">
            <label>
              Provider
              <select
                value={selected}
                onChange={(event) => select(event.target.value)}
              >
                {snapshot.native.providers.map((item) => (
                  <option key={item.id} value={item.id}>
                    {item.name}
                  </option>
                ))}
                <option value="">Add provider…</option>
              </select>
            </label>
            {providerRunning && <p className="native-account-guard" role="status">Stop bots using {provider?.name} before changing this provider or its credentials. Model discovery can still refresh while bots are idle.</p>}
            <form
              onSubmit={async (event) => {
                event.preventDefault();
                const result = await run("configure_native_provider", {
                  id: form.id,
                  name: form.name,
                  kind: form.kind,
                  baseUrl: form.baseUrl.trim(),
                  apiKeyEnv: form.apiKeyEnv.trim() || null,
                  models: [
                    ...new Set(
                      form.models
                        .split(/[\n,]/)
                        .map((item) => item.trim())
                        .filter(Boolean),
                    ),
                  ],
                  ...(form.apiKey ? { apiKey: form.apiKey } : {}),
                });
                if (result) {
                  setSelected(form.id);
                  setForm((previous) => ({ ...previous, apiKey: "" }));
                  setSaved(true);
                }
              }}
            >
              <fieldset className="native-provider-fields" disabled={busy || providerRunning}>
              <div className="form-grid">
                <label>
                  Provider ID
                  <input
                    required
                    pattern="[A-Za-z0-9_-]{1,64}"
                    maxLength={64}
                    value={form.id}
                    disabled={!!provider}
                    onChange={(event) => change("id", event.target.value)}
                  />
                </label>
                <label>
                  Display name
                  <input
                    required
                    value={form.name}
                    onChange={(event) => change("name", event.target.value)}
                  />
                </label>
              </div>
              <label>
                API kind
                <select
                  value={form.kind}
                  onChange={(event) =>
                    change("kind", event.target.value as ProviderForm["kind"])
                  }
                >
                  <option value="openai">OpenAI-compatible</option>
                  <option value="anthropic">Anthropic</option>
                </select>
              </label>
              <label>
                Base URL
                <input
                  type="url"
                  required
                  placeholder="https://api.example.com/v1"
                  value={form.baseUrl}
                  onChange={(event) => change("baseUrl", event.target.value)}
                />
              </label>
              <p className="field-help">
                HTTPS required, except HTTP on localhost. No credentials, query,
                or fragment in the URL.
              </p>
              <label>
                API key environment variable
                <input
                  value={form.apiKeyEnv}
                  onChange={(event) => change("apiKeyEnv", event.target.value)}
                  autoComplete="off"
                  placeholder="Optional environment variable"
                />
              </label>
              <label>
                Model IDs
                <textarea
                  rows={4}
                  value={form.models}
                  onChange={(event) => change("models", event.target.value)}
                  placeholder="One model ID per line"
                />
              </label>
              <p className="field-help">
                Explicit model IDs are not validated until a request. Stop bots
                using this provider before changing settings or credentials.
                Save configuration before refreshing live discovery.
              </p>
              <label>
                <span>
                  <KeyRound size={14} /> API key
                </span>
                <input
                  ref={apiKeyInput}
                  type="password"
                  autoComplete="new-password"
                  value={form.apiKey}
                  onChange={(event) => change("apiKey", event.target.value)}
                  placeholder="Enter a new key to save; blank keeps the current key"
                />
              </label>
              <p className="field-help">
                {provider?.hasApiKey
                  ? "A key is available from keychain or environment."
                  : "No key is currently available. Localhost servers may allow no authentication."}{" "}
                Saved keys are never displayed. Saving a new API key switches
                this provider from account sign-in to API-key authentication.
              </p>
              <div className="button-row">
                <button className="primary" disabled={busy} type="submit">
                  Save provider
                </button>
                {saved && (
                  <span className="success-text" role="status">
                    Saved
                  </span>
                )}
              </div>
              <button
                type="button"
                className="danger-text"
                disabled={busy || !provider}
                onClick={async () => {
                  if (
                    window.confirm(
                      `Remove the saved key for ${provider?.name}? An environment key, if set, remains available.`,
                    )
                  )
                    await run("remove_native_key", { id: selected });
                }}
              >
                Remove saved key…
              </button>
              </fieldset>
            </form>
              <div className="button-row">
                <button
                  type="button"
                  disabled={busy || !provider || providerWorking}
                  onClick={async () => {
                    const result = await run("refresh_native_models", { providerId: selected });
                    const updated = result?.native.providers.find((item) => item.id === selected);
                    if (updated) setForm((previous) => ({ ...previous, models: updated.models.join("\n") }));
                  }}
                >
                  <RefreshCw size={14} />
                  Refresh model list
                </button>
              </div>
              </div>
            </details>
          </>
        )}
      </div>
    </Modal>
  );
}
export function DirectoryField({
  value,
  onChange,
  busy,
  onError,
}: {
  value: string;
  onChange: (value: string) => void;
  busy: boolean;
  onError: (error: unknown) => void;
}) {
  return (
    <label>
      {inDesktop ? "Workspace directory" : "Workspace directory on host Mac"}
      <div className="input-with-button">
        <input
          required
          value={value}
          onChange={(event) => onChange(event.target.value)}
          placeholder="/absolute/path/to/workspace"
        />
        {inDesktop && (
          <button
            type="button"
            disabled={busy}
            title="Choose directory in desktop app"
            onClick={async () => {
              try {
                const directory = await request<string | null>(
                  "pick_directory",
                );
                if (directory) onChange(directory);
              } catch (error) {
                onError(error);
              }
            }}
          >
            <FolderOpen size={16} />
            <span className="sr-only">Choose workspace directory</span>
          </button>
        )}
      </div>
      {!inDesktop && (
        <span className="field-help">
          Enter an absolute path on the host Mac, not on this phone. The bot's
          tools run in that host workspace.
        </span>
      )}
    </label>
  );
}
