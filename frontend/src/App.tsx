import { useCallback, useEffect, useRef, useState } from "react";
import type { CSSProperties, SetStateAction } from "react";
import {
  Bot as BotIcon,
  ChevronLeft,
  Menu,
  Plus,
  Search,
  Settings,
  Sparkles,
  X,
} from "lucide-react";
import {
  errorText,
  hasAccessToken,
  inDesktop,
  request,
  setAccessToken,
} from "./api";
import type { Bot, Engine, JsonObject, Snapshot } from "./api";
import { BotForm, BotInfo } from "./BotManagement";
import { Chat, emptyDraft } from "./Chat";
import type { Draft } from "./Chat";
import { RuntimeDialog } from "./Dialogs";
import { EngineSettings } from "./Settings";

const statusLabels: Record<Bot["status"], string> = {
  stopped: "Stopped",
  starting: "Starting",
  idle: "Ready",
  working: "Working",
  needs_input: "Needs your input",
  error: "Error",
};
export default function App() {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const selectedRef = useRef<string | null>(null);
  const [error, setError] = useState("");
  const [connectionError, setConnectionError] = useState("");
  const [accessToken, setToken] = useState("");
  const [authenticated, setAuthenticated] = useState(
    inDesktop || hasAccessToken(),
  );
  const [developmentBridge, setDevelopmentBridge] = useState(false);
  const sessionEpoch = useRef(0);
  const polling = useRef(false);
  const [busy, setBusy] = useState(false);
  const locked = useRef(false);
  const epoch = useRef(0);
  const alive = useRef(false);
  const [pollNonce, setPollNonce] = useState(0);
  const snapshotRef = useRef<Snapshot | null>(null);
  const [settings, setSettings] = useState(false);
  const [settingsTab, setSettingsTab] = useState<"native" | "omp">("native");
  const [form, setForm] = useState<"create" | "edit" | null>(null);
  const [defaultEngine, setDefaultEngine] = useState<Engine | undefined>();
  const [info, setInfo] = useState(false);
  const [query, setQuery] = useState("");
  const [sidebarWidth, setSidebarWidth] = useState(25);
  const [mobileSidebar, setMobileSidebar] = useState(false);
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const scrollPositions = useRef(new Map<string, number>());
  const [unread, setUnread] = useState<Set<string>>(new Set());
  const messageSignatures = useRef(new Map<string, string>());

  const applySnapshot = useCallback((next: Snapshot) => {
    const changed: string[] = [];
    for (const bot of next.bots) {
      const last = bot.messages.at(-1);
      const signature = last
        ? `${bot.messages.length}:${last._key}:${JSON.stringify(last.content ?? last)}`
        : "";
      const previous = messageSignatures.current.get(bot.id);
      if (
        previous !== undefined &&
        previous !== signature &&
        bot.id !== selectedRef.current
      )
        changed.push(bot.id);
      messageSignatures.current.set(bot.id, signature);
    }
    if (changed.length)
      setUnread((previous) => new Set([...previous, ...changed]));
    snapshotRef.current = next;
    setSnapshot(next);
    if (!next.bots.some((bot) => bot.id === selectedRef.current)) {
      const nextId = next.bots[0]?.id ?? null;
      selectedRef.current = nextId;
      setSelectedId(nextId);
    }
  }, []);

  const disconnect = useCallback((message = "") => {
    try {
      setAccessToken("");
    } catch {
      // Forget the in-memory session even if browser storage is unavailable.
    }
    sessionEpoch.current += 1;
    epoch.current += 1;
    setAuthenticated(false);
    snapshotRef.current = null;
    setSnapshot(null);
    selectedRef.current = null;
    setSelectedId(null);
    setToken("");
    setDrafts({});
    setUnread(new Set());
    messageSignatures.current.clear();
    scrollPositions.current.clear();
    setSettings(false);
    setForm(null);
    setInfo(false);
    setMobileSidebar(false);
    setError("");
    setConnectionError(message);
  }, []);

  useEffect(() => {
    alive.current = true;
    const authenticationRequired = () =>
      disconnect(
        "Access rejected. Enter the current token from Remote access settings or the desktop's owner CLI. Check the configured Tailscale URL if access is still rejected.",
      );
    window.addEventListener(
      "omp-authentication-required",
      authenticationRequired,
    );
    if (!inDesktop) {
      document.title = "AMP Remote";
      const controller = new AbortController();
      void fetch("/api/health", {
        cache: "no-store",
        credentials: "omit",
        signal: controller.signal,
      })
        .then((response) => response.json())
        .then((value: { transport?: string }) => {
          if (alive.current)
            setDevelopmentBridge(value.transport === "development-only");
        })
        .catch(() => {});
      return () => {
        alive.current = false;
        controller.abort();
        window.removeEventListener(
          "omp-authentication-required",
          authenticationRequired,
        );
      };
    }
    return () => {
      alive.current = false;
      window.removeEventListener(
        "omp-authentication-required",
        authenticationRequired,
      );
    };
  }, [disconnect]);

  useEffect(() => {
    const viewport = window.visualViewport;
    const resize = () =>
      document.documentElement.style.setProperty(
        "--app-height",
        `${viewport?.height ?? window.innerHeight}px`,
      );
    resize();
    viewport?.addEventListener("resize", resize);
    window.addEventListener("resize", resize);
    return () => {
      viewport?.removeEventListener("resize", resize);
      window.removeEventListener("resize", resize);
    };
  }, []);

  useEffect(() => {
    if (!authenticated) return;
    let active = true;
    let timer: number | undefined;
    let inFlight = false;
    const controller = new AbortController();
    async function poll() {
      if (!active || document.hidden || inFlight) return;
      clearTimeout(timer);
      const before = epoch.current;
      if (!locked.current && !polling.current) {
        inFlight = true;
        polling.current = true;
        try {
          const result = await request<Snapshot>(
            "bootstrap",
            {},
            { signal: controller.signal },
          );
          if (active && before === epoch.current) {
            applySnapshot(result);
            setConnectionError("");
          }
        } catch (reason) {
          if (active && before === epoch.current && !controller.signal.aborted)
            setConnectionError(errorText(reason));
        } finally {
          polling.current = false;
          inFlight = false;
        }
      }
      if (active && !document.hidden)
        timer = window.setTimeout(
          poll,
          snapshotRef.current?.bots.some(
            (bot) => bot.isRunning || bot.status === "starting",
          )
            ? 400
            : 1500,
        );
    }
    const visibilityChanged = () => {
      clearTimeout(timer);
      if (!document.hidden) void poll();
    };
    document.addEventListener("visibilitychange", visibilityChanged);
    void poll();
    return () => {
      active = false;
      controller.abort();
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [applySnapshot, authenticated, pollNonce]);

  const run = useCallback(
    async (method: string, args: JsonObject = {}): Promise<Snapshot | null> => {
      const session = sessionEpoch.current;
      if (locked.current) return null;
      locked.current = true;
      epoch.current += 1;
      setBusy(true);
      setError("");
      try {
        const result = await request<Snapshot>(method, args);
        if (alive.current && session === sessionEpoch.current) {
          applySnapshot(result);
          setConnectionError("");
        }
        return session === sessionEpoch.current ? result : null;
      } catch (reason) {
        if (alive.current && session === sessionEpoch.current)
          setError(errorText(reason));
        return null;
      } finally {
        locked.current = false;
        if (alive.current) setBusy(false);
      }
    },
    [applySnapshot],
  );
  function select(id: string) {
    selectedRef.current = id;
    setSelectedId(id);
    setInfo(false);
    setMobileSidebar(false);
    setUnread((previous) => {
      const next = new Set(previous);
      next.delete(id);
      return next;
    });
  }
  function create(engine?: Engine) {
    setDefaultEngine(engine);
    setForm("create");
    setSettings(false);
    setMobileSidebar(false);
  }
  const bot = snapshot?.bots.find((item) => item.id === selectedId);
  const visibleBots =
    snapshot?.bots.filter((item) =>
      `${item.name} ${item.cwd} ${item.engine}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    ) ?? [];
  const setDraft = (update: SetStateAction<Draft>) => {
    if (!bot) return;
    const id = bot.id;
    setDrafts((previous) => ({
      ...previous,
      [id]:
        typeof update === "function"
          ? update(previous[id] ?? emptyDraft)
          : update,
    }));
  };

  return (
    <div
      className="application"
      style={{ "--sidebar-width": `${sidebarWidth}%` } as CSSProperties}
    >
      <aside
        className={`sidebar ${mobileSidebar ? "mobile-open" : ""}`}
        aria-label="Bots sidebar"
      >
        <div className="brand">
          <div className="brand-mark">✳</div>
          <strong>
            omp<span>{inDesktop ? "desktop" : "remote"}</span>
          </strong>
          <button
            className="icon-button mobile-only"
            aria-label="Close bot sidebar"
            onClick={() => setMobileSidebar(false)}
          >
            <ChevronLeft size={18} />
          </button>
        </div>
        <button
          className="new-bot"
          disabled={!snapshot}
          onClick={() => create()}
        >
          <Plus size={17} />
          New bot<span>＋</span>
        </button>
        <div className="search">
          <Search size={15} />
          <input
            aria-label="Search bots"
            placeholder="Search bots…"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
          />
        </div>
        <div className="sidebar-section">
          <span>{inDesktop ? "Your workspaces" : "Host workspaces"}</span>
          <span>{snapshot?.bots.length ?? 0}</span>
        </div>
        <nav className="bot-list" aria-label="Bot conversations">
          {visibleBots.map((item) => (
            <button
              key={item.id}
              className={`bot-item ${selectedId === item.id ? "selected" : ""}`}
              onClick={() => select(item.id)}
              aria-current={selectedId === item.id ? "page" : undefined}
            >
              <div className={`bot-icon ${item.engine}`}>
                <BotIcon size={18} />
              </div>
              <div className="bot-item-content">
                <div>
                  <strong>{item.name}</strong>
                  {unread.has(item.id) && (
                    <span className="unread-dot" aria-label="Unread activity" />
                  )}
                </div>
                <span>
                  {item.engine === "native" ? "Native" : "OMP"} ·{" "}
                  {statusLabels[item.status]}
                </span>
              </div>
              <span className={`status-dot ${item.status}`} />
            </button>
          ))}
          {snapshot && !visibleBots.length && (
            <p className="sidebar-empty">
              {query
                ? "No bots match your search."
                : "Your bots will appear here."}
            </p>
          )}
        </nav>
        <div className="sidebar-footer">
          <button disabled={!snapshot} onClick={() => setSettings(true)}>
            <Settings size={16} />
            Engine settings
          </button>
          {!inDesktop && authenticated && (
            <button onClick={() => disconnect()}>Disconnect</button>
          )}
          <div>
            <span
              className={`status-dot ${snapshot?.engine.status === "ready" ? "idle" : "stopped"}`}
            />
            {snapshot?.engine.status === "ready"
              ? `OMP ${snapshot.engine.version ?? "connected"}`
              : "Native available · OMP not connected"}
          </div>
        </div>
      </aside>
      <div
        className="resize-handle"
        role="separator"
        tabIndex={0}
        aria-label="Resize bot sidebar"
        aria-orientation="vertical"
        aria-valuenow={Math.round(sidebarWidth)}
        aria-valuemin={17}
        aria-valuemax={45}
        onKeyDown={(event) => {
          if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
            event.preventDefault();
            setSidebarWidth((value) =>
              Math.max(
                17,
                Math.min(45, value + (event.key === "ArrowRight" ? 1 : -1)),
              ),
            );
          }
        }}
        onPointerDown={(event) => {
          event.currentTarget.setPointerCapture(event.pointerId);
        }}
        onPointerMove={(event) => {
          if (
            event.currentTarget.hasPointerCapture(event.pointerId) &&
            event.buttons === 1
          )
            setSidebarWidth(
              Math.max(
                17,
                Math.min(45, (event.clientX / window.innerWidth) * 100),
              ),
            );
        }}
        onPointerUp={(event) => {
          if (event.currentTarget.hasPointerCapture(event.pointerId))
            event.currentTarget.releasePointerCapture(event.pointerId);
        }}
      />
      <main className="main-pane">
        <div className="mobile-topbar">
          <button
            className="icon-button"
            aria-label="Open bot sidebar"
            onClick={() => setMobileSidebar(true)}
          >
            <Menu size={19} />
          </button>
          <span>{inDesktop ? "AMP Desktop" : "AMP Remote"}</span>
          <button
            className="icon-button"
            disabled={!snapshot}
            aria-label="Create bot"
            onClick={() => create()}
          >
            <Plus size={19} />
          </button>
        </div>
        {connectionError && snapshot && (
          <div className="connection-banner" role="status">
            Connection interrupted: {connectionError}
            <button onClick={() => setPollNonce((value) => value + 1)}>
              Reconnect
            </button>
          </div>
        )}
        {!snapshot ? (
          <div className="welcome">
            <div className="empty-symbol">✳</div>
            <h1>
              {!inDesktop && !authenticated
                ? "AMP Remote"
                : connectionError
                  ? "Host connection needed"
                  : "Connecting your workspaces…"}
            </h1>
            <p>
              {connectionError ||
                (inDesktop
                  ? "Loading your local bots and engine settings."
                  : "Control the bots on your running desktop. Your workspaces, tools, and conversations stay on the host Mac.")}
            </p>
            {!inDesktop && (
              <form
                className="provider-form"
                onSubmit={(event) => {
                  event.preventDefault();
                  try {
                    setAccessToken(accessToken);
                    sessionEpoch.current += 1;
                    epoch.current += 1;
                    setAuthenticated(true);
                    setToken("");
                    setConnectionError("");
                    setPollNonce((value) => value + 1);
                  } catch {
                    setConnectionError(
                      "Session storage is unavailable. Allow session storage for this site to connect securely.",
                    );
                  }
                }}
              >
                <label>
                  {developmentBridge
                    ? "Development bridge token"
                    : "Desktop access token"}
                  <input
                    aria-label={
                      developmentBridge
                        ? "Development bridge token"
                        : "Desktop access token"
                    }
                    type="password"
                    autoComplete="off"
                    autoCapitalize="none"
                    autoCorrect="off"
                    spellCheck={false}
                    value={accessToken}
                    onChange={(event) => setToken(event.target.value)}
                    placeholder={
                      developmentBridge
                        ? "Per-run token from bun run bridge"
                        : "Token from desktop settings or owner CLI"
                    }
                  />
                </label>
                <button
                  className="primary"
                  type="submit"
                  disabled={!accessToken.trim()}
                >
                  {developmentBridge
                    ? "Connect development bridge"
                    : "Connect to desktop"}
                </button>
                <p className="muted remote-login-note">
                  {developmentBridge
                    ? "Development transport only. Use the per-run token printed by bun run bridge."
                    : "Get the token from your Mac's Engine settings → Remote access, or its owner CLI: remote token. Connect through your private Tailscale HTTPS address. This token grants full command authority on that Mac."}{" "}
                  The token is kept only in this tab's session. Disconnect
                  forgets it without stopping any agents.
                </p>
              </form>
            )}
            {connectionError && authenticated && (
              <button
                className="primary"
                onClick={() => setPollNonce((value) => value + 1)}
              >
                Retry connection
              </button>
            )}
          </div>
        ) : bot ? (
          <div className="workspace">
            <Chat
              key={bot.id}
              bot={bot}
              snapshot={snapshot}
              run={run}
              busy={busy}
              draft={drafts[bot.id] ?? emptyDraft}
              setDraft={setDraft}
              scrollPositions={scrollPositions.current}
              showInfo={() => setInfo((value) => !value)}
              openSettings={() => {
                setSettingsTab("native");
                setSettings(true);
              }}
              chooseNative={() => create("native")}
              onError={(reason) => setError(errorText(reason))}
            />
            {info && (
              <BotInfo
                key={bot.id}
                bot={bot}
                busy={busy}
                run={run}
                close={() => setInfo(false)}
                edit={() => setForm("edit")}
              />
            )}
          </div>
        ) : (
          <div className="welcome">
            <div className="eyebrow">
              <Sparkles size={14} />
              {inDesktop
                ? "Your local command center"
                : "Your host command center"}
            </div>
            <div className="empty-symbol">✳</div>
            <h1>
              One space.
              <br />
              Independent agents.
            </h1>
            <p>
              Create a bot for a workspace on{" "}
              {inDesktop ? "this Mac" : "the host Mac"}. Choose the host's
              installed OMP or the independent native Rust agent. Conversation
              history stays on
              {inDesktop ? " this device." : " the host, not this browser."}
            </p>
            {snapshot.engine.status !== "ready" ? (
              <div className="onboarding-choices">
                <button className="primary" onClick={() => create("native")}>
                  Use Native<small>No OMP installation required</small>
                </button>
                <button
                  onClick={() => {
                    setSettingsTab("omp");
                    setSettings(true);
                  }}
                >
                  Install or connect OMP
                  <small>Use your CLI, profiles & extensions</small>
                </button>
              </div>
            ) : (
              <button className="primary" onClick={() => create()}>
                <Plus size={16} />
                Create your first bot
              </button>
            )}
            <button
              className="text-button"
              onClick={() => {
                setSettingsTab("native");
                setSettings(true);
              }}
            >
              Configure providers & engines
            </button>
          </div>
        )}
      </main>
      {error && (
        <div className="error-toast" role="alert">
          <div>
            <strong>Action could not complete</strong>
            <p>{error}</p>
          </div>
          <button
            className="icon-button"
            aria-label="Dismiss error"
            onClick={() => setError("")}
          >
            <X size={16} />
          </button>
        </div>
      )}
      {snapshot && settings && (
        <EngineSettings
          snapshot={snapshot}
          run={run}
          busy={busy}
          initialTab={settingsTab}
          onClose={() => setSettings(false)}
          chooseNative={() => create("native")}
        />
      )}
      {snapshot && form && (
        <BotForm
          key={form === "edit" ? bot?.id : `new-${defaultEngine ?? "auto"}`}
          bot={form === "edit" ? bot : undefined}
          defaultEngine={defaultEngine}
          snapshot={snapshot}
          run={run}
          busy={busy}
          onClose={() => setForm(null)}
          onCreated={select}
          onError={(reason) => setError(errorText(reason))}
          openSettings={() => {
            setForm(null);
            setSettingsTab("native");
            setSettings(true);
          }}
        />
      )}
      {bot?.dialogs[0] && !settings && !form && (
        <RuntimeDialog
          key={`${bot.id}-${bot.dialogs[0].id}`}
          dialog={bot.dialogs[0]}
          busy={busy}
          respond={async (response) =>
            !!(await run("respond_dialog", {
              id: bot.id,
              requestId: bot.dialogs[0].id,
              response,
            }))
          }
        />
      )}
    </div>
  );
}
