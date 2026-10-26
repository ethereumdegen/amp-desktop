import { useEffect, useState } from "react";
import { Info, Pencil, RefreshCw, Trash2, X } from "lucide-react";
import type { Bot, Engine, JsonObject, Snapshot } from "./api";
import { errorText, inDesktop, request } from "./api";
import { DirectoryField } from "./Settings";
import type { Run } from "./Settings";
import { JsonDetails, Modal } from "./ui";

export function BotForm({
  bot,
  defaultEngine,
  snapshot,
  run,
  busy,
  onClose,
  onCreated,
  onError,
  openSettings,
}: {
  bot?: Bot;
  defaultEngine?: Engine;
  snapshot: Snapshot;
  run: Run;
  busy: boolean;
  onClose: () => void;
  onCreated: (id: string) => void;
  onError: (error: unknown) => void;
  openSettings: () => void;
}) {
  const [engine, setEngine] = useState<Engine>(
    bot?.engine ??
      defaultEngine ??
      (snapshot.engine.status === "ready" ? "omp" : "native"),
  );
  const [provider, setProvider] = useState(
    bot?.nativeProvider ?? snapshot.native.providers[0]?.id ?? "",
  );
  const [name, setName] = useState(bot?.name ?? "");
  const [cwd, setCwd] = useState(bot?.cwd ?? "");
  const [instructions, setInstructions] = useState(bot?.instructions ?? "");
  const [profile, setProfile] = useState(bot?.profile ?? "");
  return (
    <Modal title={bot ? "Edit bot" : "Create a bot"} onClose={onClose}>
      <form
        className="dialog-content"
        onSubmit={async (event) => {
          event.preventDefault();
          const previous = new Set(snapshot.bots.map((item) => item.id));
          const result = await run(bot ? "update_bot" : "create_bot", {
            ...(bot
              ? { id: bot.id }
              : {
                  engine,
                  nativeProvider: engine === "native" ? provider : null,
                }),
            name: name.trim(),
            cwd: cwd.trim(),
            instructions,
            profile: engine === "omp" ? profile.trim() || null : null,
          });
          if (result) {
            const created =
              bot ?? result.bots.find((item) => !previous.has(item.id));
            if (created) onCreated(created.id);
            onClose();
          }
        }}
      >
        {!bot && (
          <>
            <fieldset>
              <legend>Agent engine</legend>
              <div className="engine-choices">
                <label className={engine === "omp" ? "selected" : ""}>
                  <input
                    type="radio"
                    name="engine"
                    value="omp"
                    checked={engine === "omp"}
                    onChange={() => setEngine("omp")}
                  />
                  <strong>Installed OMP</strong>
                  <span>Existing CLI, profiles, extensions & tools</span>
                </label>
                <label className={engine === "native" ? "selected" : ""}>
                  <input
                    type="radio"
                    name="engine"
                    value="native"
                    checked={engine === "native"}
                    onChange={() => setEngine("native")}
                  />
                  <strong>Native Rust</strong>
                  <span>Independent harness, direct provider access</span>
                </label>
              </div>
            </fieldset>
            <p className="field-help">
              Engine and native provider belong to this conversation and cannot
              be switched later. Create another bot to change engines.
            </p>
          </>
        )}
        {bot && (
          <p className="callout">
            Engine:{" "}
            <strong>
              {engine === "native" ? "Native Rust" : "Installed OMP"}
            </strong>
            {bot.nativeProvider ? ` · ${bot.nativeProvider}` : ""}. The
            conversation keeps its engine, provider, model, and session.
          </p>
        )}
        {engine === "native" && !bot && (
          <label>
            Native provider
            <select
              required
              value={provider}
              onChange={(event) => setProvider(event.target.value)}
            >
              <option value="" disabled>
                Select a provider
              </option>
              {snapshot.native.providers.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.name}
                </option>
              ))}
            </select>
            <button
              type="button"
              className="text-button"
              onClick={openSettings}
            >
              Configure native providers
            </button>
          </label>
        )}
        {engine === "omp" && snapshot.engine.status !== "ready" && (
          <p className="callout">
            OMP is not installed or could not be found. You can create this bot
            now and connect OMP before starting it, or choose Native.
          </p>
        )}
        <label>
          Bot name
          <input
            autoFocus
            required
            maxLength={120}
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="Name your bot"
          />
        </label>
        <DirectoryField
          value={cwd}
          onChange={setCwd}
          busy={busy}
          onError={onError}
        />
        <label>
          Instructions
          <textarea
            rows={5}
            value={instructions}
            onChange={(event) => setInstructions(event.target.value)}
            placeholder="How should this bot work in this workspace?"
          />
        </label>
        {engine === "omp" && (
          <label>
            OMP profile <span className="muted">(optional)</span>
            <input
              value={profile}
              onChange={(event) => setProfile(event.target.value)}
              placeholder="An existing OMP profile"
            />
          </label>
        )}
        <footer>
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button
            className="primary"
            disabled={busy || (engine === "native" && !provider)}
            type="submit"
          >
            {bot ? "Save changes" : "Create bot"}
          </button>
        </footer>
      </form>
    </Modal>
  );
}
export function BotInfo({
  bot,
  busy,
  run,
  close,
  edit,
}: {
  bot: Bot;
  busy: boolean;
  run: Run;
  close: () => void;
  edit: () => void;
}) {
  const [info, setInfo] = useState<JsonObject | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [refresh, setRefresh] = useState(0);
  useEffect(() => {
    let active = true;
    setLoading(true);
    setError("");
    request<JsonObject>("get_bot_info", { id: bot.id })
      .then((value) => {
        if (active) setInfo(value);
      })
      .catch((reason) => {
        if (active) setError(errorText(reason));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [bot.id, refresh]);
  const state = (bot.state ??
    (info?.state && typeof info.state === "object"
      ? info.state
      : null)) as JsonObject | null;
  return (
    <aside className="info-drawer" aria-label="Bot information">
      <header>
        <h2>
          <Info size={16} /> Bot details
        </h2>
        <button
          className="icon-button"
          onClick={close}
          aria-label="Close bot information"
        >
          <X size={17} />
        </button>
      </header>
      <div className="info-content">
        <dl>
          <dt>Engine</dt>
          <dd>
            {bot.engine === "native" ? "Native Rust agent" : "Installed OMP"}
            {bot.engineVersion ? ` · ${bot.engineVersion}` : ""}
          </dd>
          {bot.nativeProvider && (
            <>
              <dt>Native provider</dt>
              <dd>{bot.nativeProvider}</dd>
            </>
          )}
          <dt>{inDesktop ? "Workspace" : "Workspace on host Mac"}</dt>
          <dd>
            <code>{bot.cwd}</code>
          </dd>
          <dt>Status</dt>
          <dd>{bot.status}</dd>
          <dt>Model</dt>
          <dd>
            {bot.model
              ? `${bot.model.provider}/${bot.model.id}`
              : "Not selected"}
          </dd>
          <dt>Thinking</dt>
          <dd>{bot.thinking ?? "Engine default"}</dd>
          <dt>Process ID</dt>
          <dd>
            {bot.pid ??
              (bot.engine === "native"
                ? "In-process Rust runtime"
                : "Not running")}
          </dd>
          <dt>{inDesktop ? "Session" : "Session on host Mac"}</dt>
          <dd>
            <code>{bot.sessionFile ?? "Created when this bot starts"}</code>
          </dd>
          {bot.engine === "omp" && (
            <>
              <dt>OMP profile</dt>
              <dd>{bot.profile || "Default"}</dd>
            </>
          )}
          <dt>Instructions</dt>
          <dd className="preserve">
            {bot.instructions || "No additional instructions"}
          </dd>
          <dt>Capabilities</dt>
          <dd>
            {bot.capabilities.length
              ? bot.capabilities.join(", ")
              : "Not reported by runtime"}
          </dd>
        </dl>
        <JsonDetails
          label="Context usage"
          value={
            state?.contextUsage ?? state?.context ?? "Not reported by runtime"
          }
        />
        {(bot.engine === "omp" ||
          state?.todos != null ||
          state?.todoPhases != null) && (
          <JsonDetails
            label="Todos"
            value={
              state?.todoPhases ?? state?.todos ?? "Not reported by runtime"
            }
          />
        )}
        {bot.engine === "native" && (
          <p className="callout">
            OMP extensions, LSP, and subagents are not supported by the native
            engine. Available native tools and approval behavior are listed in
            capabilities.
          </p>
        )}
        {bot.engine === "omp" && (
          <JsonDetails
            label="Subagents"
            value={info?.subagents ?? "Not loaded"}
          />
        )}
        <JsonDetails
          label="Session statistics"
          value={info?.stats ?? "Not loaded"}
        />
        <JsonDetails label="Runtime state" value={state} />
        {error && <p className="inline-error">{error}</p>}
        <button
          disabled={loading}
          onClick={() => setRefresh((value) => value + 1)}
        >
          <RefreshCw size={14} />
          {loading ? "Loading details…" : "Refresh details"}
        </button>
        <div className="info-actions">
          <button
            disabled={busy || bot.isRunning || bot.status === "starting"}
            onClick={edit}
          >
            <Pencil size={14} />
            Edit bot
          </button>
          {(bot.isRunning || bot.status === "starting") && (
            <p className="field-help">Stop this bot before editing.</p>
          )}
          <button
            className="danger-text"
            disabled={busy}
            onClick={() => {
              if (
                window.confirm(
                  `Delete “${bot.name}”? This stops the bot and removes only app metadata. Its workspace and engine session files will not be deleted.`,
                )
              )
                void run("delete_bot", { id: bot.id });
            }}
          >
            <Trash2 size={14} />
            Delete bot…
          </button>
        </div>
      </div>
    </aside>
  );
}
