import { useEffect, useState } from "react";
import { Copy, RefreshCw } from "lucide-react";
import { errorText, inDesktop, request } from "./api";
import type { JsonObject, RemoteStatus } from "./api";
import { ExternalLinkButton } from "./ui";

export function RemoteAccessSettings() {
  const [status, setStatus] = useState<RemoteStatus | null>(null);
  const [port, setPort] = useState("14501");
  const [tailnetUrl, setTailnetUrl] = useState("");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");

  useEffect(() => {
    if (!inDesktop) return;
    let active = true;
    void request<RemoteStatus>("remote_status")
      .then((next) => {
        if (!active) return;
        setStatus(next);
        setPort(String(next.port));
        setTailnetUrl(next.tailnetUrl ?? "");
      })
      .catch((reason) => {
        if (active) setError(errorText(reason));
      });
    return () => {
      active = false;
    };
  }, []);

  if (!inDesktop) return null;

  async function configure(patch: JsonObject) {
    if (busy) return;
    setBusy(true);
    setError("");
    setNotice("");
    try {
      const next = await request<RemoteStatus>("remote_configure", patch);
      setStatus(next);
      setPort(String(next.port));
      setTailnetUrl(next.tailnetUrl ?? "");
      setNotice(
        next.running
          ? "Remote listener is running."
          : "Remote listener is stopped. Host agents are unchanged.",
      );
    } catch (reason) {
      setError(errorText(reason));
      // A failed start can still save configuration; retrieve its visible status.
      try {
        const next = await request<RemoteStatus>("remote_status");
        setStatus(next);
      } catch {
        /* Keep the original configuration error visible. */
      }
    } finally {
      setBusy(false);
    }
  }

  async function accessToken(action: "reveal" | "copy" | "rotate") {
    if (busy) return;
    if (
      action === "rotate" &&
      !window.confirm(
        "Rotate the remote access token? All paired browsers will need the new token. Running agents will not stop.",
      )
    )
      return;
    setBusy(true);
    setError("");
    setNotice("");
    if (action === "rotate") setToken("");
    try {
      const result = await request<{ token: string }>(
        action === "rotate" ? "remote_rotate_token" : "remote_token",
      );
      if (action === "reveal") setToken(result.token);
      if (action === "copy") {
        await navigator.clipboard.writeText(result.token);
        setNotice(
          "Access token copied. Share it only with trusted devices; clear it from clipboard history when finished.",
        );
      }
      if (action === "rotate")
        setNotice(
          "Token rotated. Pair browsers again using Reveal or Copy token. Host agents continue running.",
        );
    } catch (reason) {
      setError(
        action === "copy"
          ? "Could not copy the token. Use Reveal token and copy it manually."
          : errorText(reason),
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="remote-settings">
      <div className="eyebrow">This desktop only</div>
      <h2>Remote access</h2>
      <p className="muted">
        Use AMP Remote in your phone's browser to control this same desktop's
        bots, models, conversations, and approvals. Disconnecting a browser
        never stops host agents.
      </p>
      <div className="callout">
        <strong>Full command authority on this Mac</strong>
        <p>
          Anyone with the token and network access can run agent tools and
          approve commands as you. Keep it secret. Use your private Tailscale
          network, never Funnel or a public proxy. Tokens are stored in the
          Mac's Keychain and are available only here, not to browser clients.
        </p>
      </div>
      {error && (
        <p className="inline-error" role="alert">
          {error}
        </p>
      )}
      {status ? (
        <>
          <div className={status.running ? "success" : "callout"} role="status">
            <strong>
              {status.running
                ? "Listening on this Mac"
                : status.enabled
                  ? "Enabled — listener not running"
                  : "Remote access is disabled"}
            </strong>
            <span>{status.localUrl}</span>
            {status.tailnetUrl && (
              <span>Phone address: {status.tailnetUrl}</span>
            )}
          </div>
          {status.error && (
            <p className="inline-error" role="alert">
              {status.error}
            </p>
          )}
          <div className="button-row">
            <button
              className={status.enabled ? "danger-text" : "primary"}
              disabled={busy}
              onClick={() => void configure({ enabled: !status.enabled })}
            >
              {status.enabled
                ? "Disable remote access"
                : "Enable remote access"}
            </button>
            {status.enabled && !status.running && (
              <button
                disabled={busy}
                onClick={() => void configure({ enabled: true })}
              >
                <RefreshCw size={14} />
                Retry listener
              </button>
            )}
          </div>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              void configure({
                port: Number(port),
                tailnetUrl: tailnetUrl.trim() || null,
              });
            }}
          >
            <label>
              Loopback port
              <input
                type="number"
                min={1}
                max={65535}
                required
                value={port}
                onChange={(event) => setPort(event.target.value)}
              />
            </label>
            <label>
              Exact Tailscale HTTPS URL{" "}
              <span className="muted">(optional during initial setup)</span>
              <input
                type="url"
                autoCapitalize="none"
                autoCorrect="off"
                spellCheck={false}
                value={tailnetUrl}
                onChange={(event) => setTailnetUrl(event.target.value)}
                placeholder="https://your-mac.your-tailnet.ts.net:8443"
              />
            </label>
            <p className="field-help">
              Only 127.0.0.1 is bound. Use the exact HTTPS *.ts.net origin
              printed by Tailscale Serve, including its port, without a path,
              query, or fragment. Saving while enabled restarts the listener; it
              does not restart bots.
            </p>
            <button type="submit" disabled={busy}>
              Save remote configuration
            </button>
          </form>
          <h3>Pair a trusted browser</h3>
          <div className="button-row">
            <button
              disabled={busy}
              onClick={() =>
                token ? setToken("") : void accessToken("reveal")
              }
            >
              {token ? "Hide token" : "Reveal token"}
            </button>
            <button disabled={busy} onClick={() => void accessToken("copy")}>
              <Copy size={14} />
              Copy token
            </button>
            <button
              className="danger-text"
              disabled={busy}
              onClick={() => void accessToken("rotate")}
            >
              Rotate token…
            </button>
          </div>
          {token && (
            <label>
              Access token — keep secret
              <input
                className="revealed-token"
                readOnly
                value={token}
                autoComplete="off"
                spellCheck={false}
                onFocus={(event) => event.currentTarget.select()}
              />
            </label>
          )}
          <p className="field-help">
            Open the phone address on a trusted device connected to your
            tailnet, then paste this token into AMP Remote. Never put the token
            in a URL. Disabling access retains the token; rotation revokes
            previous tokens.
          </p>
          <h3>Tailscale setup</h3>
          <ol className="remote-setup">
            <li>
              Install Tailscale on this Mac and your phone, sign in to the same
              private tailnet, and connect both.
            </li>
            <li>
              Enable remote access here, then run this command yourself in
              Terminal on the Mac:
            </li>
          </ol>
          <div className="copy-command">
            <code>{status.serveCommand}</code>
            <button
              disabled={busy}
              onClick={async () => {
                try {
                  await navigator.clipboard.writeText(status.serveCommand);
                  setNotice(
                    "Tailscale Serve command copied. Run it yourself on the host Mac.",
                  );
                } catch {
                  setError(
                    "Clipboard unavailable. Select and copy the displayed command manually.",
                  );
                }
              }}
            >
              <Copy size={14} />
              Copy command
            </button>
          </div>
          <p className="field-help">
            Save the HTTPS address printed by Serve above, then open it on your
            phone. Keep this Mac awake and AMP Desktop running. This app does
            not install Tailscale, run the command, or change tailnet settings.
            To remove Serve separately, manage it with Tailscale on the Mac.
          </p>
          <ExternalLinkButton url="https://tailscale.com/kb/1312/serve">
            Official Tailscale Serve setup
          </ExternalLinkButton>
        </>
      ) : (
        <p className="muted">Loading local remote-access status…</p>
      )}
      {notice && (
        <p className="success-text" role="status">
          {notice}
        </p>
      )}
    </section>
  );
}
