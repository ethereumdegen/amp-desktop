import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { Check, LoaderCircle, Search } from "lucide-react";
import { errorText, request } from "./api";
import type { Model, SessionSummary } from "./api";
import { Modal } from "./ui";

export const chatCommands = [
  { name: "new", usage: "/new", description: "Start a new conversation in this bot" },
  { name: "resume", usage: "/resume [id]", description: "Find and resume a saved conversation" },
  { name: "model", usage: "/model [provider/id]", description: "Choose a model or select its exact ID" },
] as const;
export type ChatCommand = { name: typeof chatCommands[number]["name"]; argument: string };

export function parseChatCommand(text: string): ChatCommand | null {
  const match = text.trim().match(/^\/(new|resume|model)(?=\s|$)([\s\S]*)$/);
  if (!match) return null;
  const name = match[1] as ChatCommand["name"];
  const argument = match[2].trim();
  if ((name === "new" && argument) || /\s/.test(argument)) {
    const command = chatCommands.find((item) => item.name === name)!;
    throw new Error(`Usage: ${command.usage}`);
  }
  return { name, argument };
}

export function resolveCommandModel(models: Model[], argument: string): Model {
  const matches = models.filter((model) =>
    model.id === argument || `${model.provider}/${model.id}` === argument);
  const unique = matches.filter((model, index) => matches.findIndex((item) =>
    item.provider === model.provider && item.id === model.id) === index);
  if (!unique.length) throw new Error(`Unknown model: ${argument}. Use /model to browse available models.`);
  if (unique.length !== 1) throw new Error(`Model ID ${argument} is ambiguous. Use provider/model-id.`);
  return unique[0];
}

export function ResumePicker({
  botId,
  busy,
  onChoose,
  onClose,
  onError,
}: {
  botId: string;
  busy: boolean;
  onChoose: (session: SessionSummary) => Promise<boolean>;
  onClose: () => void;
  onError: (error: unknown) => void;
}) {
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const [attempt, setAttempt] = useState(0);
  const search = useRef<HTMLInputElement>(null);
  const reportError = useRef(onError);
  reportError.current = onError;
  const listId = useId();
  const normalized = query.trim().toLowerCase();
  const filtered = sessions.filter((session) =>
    `${session.title} ${session.id}`.toLowerCase().includes(normalized));
  const activeIndex = Math.min(active, Math.max(0, filtered.length - 1));
  useLayoutEffect(() => {
    const controller = new AbortController();
    let live = true;
    setLoading(true);
    setError("");
    void request<{ sessions: SessionSummary[] }>("list_sessions", { id: botId }, { signal: controller.signal })
      .then((result) => { if (live) setSessions(result.sessions); })
      .catch((reason) => {
        if (!live || controller.signal.aborted) return;
        setError(errorText(reason));
        reportError.current(reason);
      })
      .finally(() => { if (live) setLoading(false); });
    function disconnect() { live = false; controller.abort(); }
    window.addEventListener("omp-authentication-required", disconnect);
    return () => {
      disconnect();
      window.removeEventListener("omp-authentication-required", disconnect);
    };
  }, [botId, attempt]);
  useEffect(() => {
    document.getElementById(`${listId}-${activeIndex}`)?.scrollIntoView({ block: "nearest" });
  }, [listId, activeIndex, query, sessions]);
  return (
    <Modal title="Resume conversation" onClose={onClose} initialFocus={search}>
      <div className="resume-picker">
        <div className="model-search">
          <Search size={15} aria-hidden="true" />
          <input
            ref={search}
            role="combobox"
            aria-label="Search saved conversations"
            aria-expanded="true"
            aria-autocomplete="list"
            aria-controls={listId}
            aria-activedescendant={!loading && filtered.length ? `${listId}-${activeIndex}` : undefined}
            value={query}
            placeholder="Search conversation titles or IDs…"
            onChange={(event) => { setQuery(event.target.value); setActive(0); }}
            onKeyDown={(event) => {
              if (event.nativeEvent.isComposing || !["ArrowDown", "ArrowUp", "Enter"].includes(event.key)) return;
              event.preventDefault();
              if (!filtered.length || loading || busy) return;
              if (event.key === "Enter") void onChoose(filtered[activeIndex]);
              else setActive((activeIndex + (event.key === "ArrowDown" ? 1 : -1) + filtered.length) % filtered.length);
            }}
          />
        </div>
        {loading && <p className="resume-status" role="status"><LoaderCircle size={14} className="spin" /> Loading conversations…</p>}
        {error && <div className="resume-status" role="alert"><p>{error}</p><button type="button" onClick={() => setAttempt((value) => value + 1)}>Try again</button></div>}
        <div role="listbox" id={listId} aria-label="Saved conversations" aria-busy={loading || busy} className="model-options resume-options">
          {!loading && !error && filtered.map((session, index) => (
            <button
              type="button"
              role="option"
              id={`${listId}-${index}`}
              key={session.id}
              aria-selected={session.current}
              disabled={busy}
              className={`model-option ${index === activeIndex ? "highlighted" : ""}`}
              tabIndex={-1}
              onMouseDown={(event) => event.preventDefault()}
              onPointerMove={() => setActive(index)}
              onClick={() => void onChoose(session)}
            >
              <span>
                <strong>{session.title || "Untitled conversation"}</strong>
                <small>{session.id}</small>
                <small>{session.current ? "Current conversation · " : ""}{session.modifiedAt === null ? "Saved time unavailable" : new Date(session.modifiedAt).toLocaleString()}</small>
              </span>
              {session.current && <Check size={14} aria-hidden="true" />}
            </button>
          ))}
        </div>
        {!loading && !error && !filtered.length && <p className="resume-status" role="status">{sessions.length ? "No matching conversations." : "No saved conversations for this bot yet."}</p>}
        <p className="model-chooser-footer">↑↓ to browse · Enter or tap to resume · Escape to close</p>
      </div>
    </Modal>
  );
}
