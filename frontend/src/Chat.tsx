import { useId, useLayoutEffect, useRef, useState } from "react";
import type { Dispatch, SetStateAction } from "react";
import {
  ArrowDown,
  ArrowUp,
  ImagePlus,
  Info,
  LoaderCircle,
  Play,
  RefreshCw,
  Square,
  X,
} from "lucide-react";
import type { Bot, JsonObject, Model, Snapshot } from "./api";
import { inDesktop } from "./api";
import { MessageView } from "./Messages";
import { Installation } from "./Settings";
import type { Run } from "./Settings";
import { ModelChooser } from "./ui";
import { chatCommands, parseChatCommand, resolveCommandModel, ResumePicker } from "./ChatCommands";

export interface Attachment {
  type: "image";
  data: string;
  mimeType: string;
  name: string;
}
export interface Draft {
  text: string;
  images: Attachment[];
  behavior: "steer" | "followUp";
}
export const emptyDraft: Draft = { text: "", images: [], behavior: "followUp" };
export function Chat({
  bot,
  snapshot,
  run,
  busy,
  draft,
  setDraft,
  scrollPositions,
  showInfo,
  openSettings,
  chooseNative,
  onError,
}: {
  bot: Bot;
  snapshot: Snapshot;
  run: Run;
  busy: boolean;
  draft: Draft;
  setDraft: Dispatch<SetStateAction<Draft>>;
  scrollPositions: Map<string, number>;
  showInfo: () => void;
  openSettings: () => void;
  chooseNative: () => void;
  onError: (error: unknown) => void;
}) {
  const scroll = useRef<HTMLDivElement>(null);
  const bottom = useRef(true);
  const file = useRef<HTMLInputElement>(null);
  const sending = useRef(false);
  const composer = useRef<HTMLTextAreaElement>(null);
  const lifecycle = useRef(0);
  const modelCommand = useRef<Draft | null>(null);
  const [modelOpenRequest, setModelOpenRequest] = useState(0);
  const [resumeDraft, setResumeDraft] = useState<Draft | null>(null);
  const [commandActive, setCommandActive] = useState(0);
  const [commandsDismissed, setCommandsDismissed] = useState(false);
  const [commandNavigating, setCommandNavigating] = useState(false);
  const commandsId = useId();
  const commandPrefix = draft.text.match(/^\/([a-z]*)$/)?.[1];
  const commandChoices = !commandsDismissed && commandPrefix !== undefined &&
    !chatCommands.some((command) => command.name === commandPrefix)
    ? chatCommands.filter((command) => command.name.startsWith(commandPrefix))
    : [];
  const commandIndex = Math.min(commandActive, Math.max(0, commandChoices.length - 1));
  const [refreshing, setRefreshing] = useState(false);
  const [refreshMessage, setRefreshMessage] = useState("");
  const working =
    bot.status === "working" ||
    bot.status === "needs_input" ||
    bot.status === "starting";
  const running = bot.isRunning;
  const missingOmp =
    bot.engine === "omp" && snapshot.engine.status !== "ready" && !running;
  const provider = snapshot.native.providers.find(
    (item) => item.id === bot.nativeProvider,
  );
  const models: Model[] =
    bot.engine === "native"
      ? (provider?.models ?? []).map(
          (id) =>
            bot.models.find(
              (model) => model.id === id && model.provider === provider?.id,
            ) ?? { id, provider: provider!.id },
        )
      : bot.models;
  const providerWorking = snapshot.bots.some(
    (item) => item.engine === "native" &&
      item.nativeProvider === bot.nativeProvider &&
      ["working", "needs_input", "starting"].includes(item.status),
  );
  const modelDisabled = busy || working || missingOmp || refreshing || (bot.engine === "native" && !provider);
  useLayoutEffect(() => {
    function invalidate() {
      lifecycle.current += 1;
      modelCommand.current = null;
      setResumeDraft(null);
    }
    window.addEventListener("omp-authentication-required", invalidate);
    return () => {
      lifecycle.current += 1;
      window.removeEventListener("omp-authentication-required", invalidate);
    };
  }, [bot.id]);
  useLayoutEffect(() => {
    const container = scroll.current;
    if (!container) return;
    const saved = scrollPositions.get(bot.id);
    container.scrollTop = saved ?? container.scrollHeight;
    bottom.current =
      saved === undefined ||
      container.scrollHeight - container.clientHeight - saved < 90;
    return () => {
      scrollPositions.set(bot.id, container.scrollTop);
    };
  }, [bot.id, scrollPositions]);
  useLayoutEffect(() => {
    if (scroll.current && bottom.current)
      scroll.current.scrollTop = scroll.current.scrollHeight;
  }, [bot.revision, bot.messages.length, bot.sessionFile]);
  function completeCommand(index: number) {
    const command = commandChoices[index];
    if (!command) return;
    setDraft((current) => ({ ...current, text: `/${command.name} ` }));
    setCommandsDismissed(false);
    composer.current?.focus();
  }
  async function mutateCommand(method: string, args: JsonObject, sent: Draft | null, resetConversation = false) {
    if (busy || sending.current) return false;
    if (working) {
      onError(new Error("Wait for this bot to finish working and resolve any approvals before changing conversations or models."));
      return false;
    }
    const generation = lifecycle.current;
    sending.current = true;
    try {
      const result = await run(method, { id: bot.id, ...args });
      if (!result || generation !== lifecycle.current) return false;
      if (sent) setDraft((current) => current === sent ? { ...current, text: "" } : current);
      if (resetConversation) {
        scrollPositions.delete(bot.id);
        bottom.current = true;
        if (scroll.current) scroll.current.scrollTop = scroll.current.scrollHeight;
      }
      return true;
    } finally {
      sending.current = false;
    }
  }
  async function commandModels(): Promise<Model[] | null> {
    if (bot.engine !== "omp" || running) return models;
    const generation = lifecycle.current;
    sending.current = true;
    try {
      const result = await run("start_bot", { id: bot.id });
      if (!result || generation !== lifecycle.current) return null;
      return result.bots.find((item) => item.id === bot.id)?.models ?? [];
    } finally {
      sending.current = false;
    }
  }
  async function resumeSession(sessionId: string, sent: Draft) {
    const succeeded = await mutateCommand("resume_session", { sessionId }, sent, true);
    if (succeeded) setResumeDraft(null);
    return succeeded;
  }
  async function send() {
    if (
      busy ||
      sending.current ||
      missingOmp ||
      (!draft.text.trim() && !draft.images.length)
    )
      return;
    const sent = draft;
    try {
      const command = parseChatCommand(sent.text);
      if (command) {
        if (sent.images.length) throw new Error("Chat commands cannot include images. Remove the attachments or send them in a separate message.");
        if (working) throw new Error("Wait for this bot to finish working and resolve any approvals before changing conversations or models.");
        if (command.name === "new") {
          await mutateCommand("new_session", {}, sent, true);
        } else if (command.name === "resume") {
          if (command.argument) await resumeSession(command.argument, sent);
          else setResumeDraft(sent);
        } else {
          const available = await commandModels();
          if (!available) return;
          if (command.argument) {
            const model = resolveCommandModel(available, command.argument);
            await mutateCommand("set_model", { provider: model.provider, modelId: model.id }, sent);
          } else {
            if (modelDisabled) throw new Error("The model picker is unavailable. Connect this bot's provider and wait for model refresh to finish.");
            modelCommand.current = sent;
            setModelOpenRequest((value) => value + 1);
          }
        }
        return;
      }
    } catch (error) {
      onError(error);
      return;
    }
    const generation = lifecycle.current;
    sending.current = true;
    const result = await run("send_message", {
      id: bot.id,
      message: sent.text,
      behavior: sent.behavior,
      images: sent.images.map(({ type, data, mimeType }) => ({
        type,
        data,
        mimeType,
      })),
    });
    sending.current = false;
    if (result && generation === lifecycle.current) {
      setDraft((current) => ({
        ...current,
        text: current.text === sent.text ? "" : current.text,
        images: current.images.filter((image) => !sent.images.includes(image)),
      }));
      bottom.current = true;
      if (scroll.current)
        scroll.current.scrollTop = scroll.current.scrollHeight;
    }
  }
  async function attach(files: FileList | null) {
    if (!files) return;
    try {
      const images = await Promise.all(
        Array.from(files).map(async (item) => {
          if (
            !["image/png", "image/jpeg", "image/gif", "image/webp"].includes(
              item.type,
            )
          )
            throw new Error(
              `${item.name}: use PNG, JPEG, GIF, or WebP images.`,
            );
          if (item.size > 20 * 1024 * 1024)
            throw new Error(`${item.name}: images must be under 20 MB.`);
          const { promise, resolve, reject } = Promise.withResolvers<string>();
          const reader = new FileReader();
          reader.onload = () => resolve(String(reader.result).split(",")[1]);
          reader.onerror = () =>
            reject(new Error(`Could not read ${item.name}`));
          reader.readAsDataURL(item);
          const data = await promise;
          return {
            type: "image" as const,
            data,
            mimeType: item.type,
            name: item.name,
          };
        }),
      );
      setDraft((previous) => ({
        ...previous,
        images: [...previous.images, ...images],
      }));
    } catch (error) {
      onError(error);
    }
    if (file.current) file.current.value = "";
  }
  return (
    <section className="chat" aria-label={`${bot.name} conversation`}>
      <header className="chat-header">
        <div className="chat-title">
          <span className={`status-dot ${bot.status}`} />
          <div>
            <h1>{bot.name}</h1>
            <span className="chat-subtitle">
              {bot.engine === "native" ? "Native Rust" : "Installed OMP"}
              <span> / {!inDesktop && "Host: "}</span>
              {bot.cwd}
            </span>
          </div>
        </div>
        <div className="header-actions">
          <button
            disabled={busy || (missingOmp && !running)}
            onClick={() =>
              void run(running ? "stop_bot" : "start_bot", { id: bot.id })
            }
          >
            {running ? <Square size={14} /> : <Play size={14} />}
            {running ? "Stop bot" : "Start bot"}
          </button>
          <button
            className="icon-button"
            aria-label="Show bot information"
            title="Bot details"
            onClick={showInfo}
          >
            <Info size={18} />
          </button>
        </div>
      </header>
      <div className="model-toolbar">
        <ModelChooser
          key={`${bot.id}/${bot.nativeProvider ?? ""}`}
          models={models}
          selected={bot.model}
          providerNames={Object.fromEntries(snapshot.native.providers.map((item) => [item.id, item.name]))}
          providerScope={bot.engine === "native" ? bot.nativeProvider ?? undefined : undefined}
          disabled={modelDisabled}
          openRequest={modelOpenRequest}
          onClose={() => { modelCommand.current = null; }}
          onChoose={(model) => {
            const sent = modelCommand.current;
            void mutateCommand("set_model", { provider: model.provider, modelId: model.id }, sent);
          }}
        />
        {bot.engine === "native" && (
          <button
            type="button"
            className="icon-button model-refresh"
            disabled={busy || providerWorking || refreshing || !provider}
            aria-label="Refresh native provider models"
            title={providerWorking ? "Wait until bots using this provider finish working" : "Refresh provider model list"}
            onClick={async () => {
              setRefreshing(true);
              setRefreshMessage("");
              try {
                const result = await run("refresh_native_models", { providerId: bot.nativeProvider });
                setRefreshMessage(result ? "Model list refreshed." : "Could not refresh models. Check the error and your provider connection in settings.");
              } finally {
                setRefreshing(false);
              }
            }}
          >
            <RefreshCw size={14} className={refreshing ? "spin" : ""} aria-hidden="true" />
          </button>
        )}
        <label>
          <span className="toolbar-label">Thinking</span>
          <select
            aria-label="Thinking level"
            value={bot.thinking ?? ""}
            disabled={
              busy || bot.status !== "idle" || !bot.thinkingLevels.length
            }
            onChange={(event) =>
              void run("set_thinking", {
                id: bot.id,
                level: event.target.value,
              })
            }
          >
            <option value="" disabled>
              Engine default
            </option>
            {bot.thinking && !bot.thinkingLevels.includes(bot.thinking) && (
              <option value={bot.thinking}>{bot.thinking}</option>
            )}
            {bot.thinkingLevels.map((level) => (
              <option value={level} key={level}>
                {bot.engine === "native" && level === "off" ? "Provider default" : level}
              </option>
            ))}
          </select>
        </label>
        <span className={`status-label ${bot.status}`}>
          {working && <LoaderCircle size={12} className="spin" />}
          {bot.status.replace("_", " ")}
        </span>
      </div>
      {bot.engine === "native" && refreshMessage && (
        <div className="model-refresh-message" role="status">{refreshMessage}</div>
      )}
      {bot.error && (
        <div className="bot-error" role="alert">
          {bot.error}
        </div>
      )}
      {missingOmp ? (
        <div className="chat-scroll">
          <Installation
            snapshot={snapshot}
            run={run}
            busy={busy}
            chooseNative={chooseNative}
          />
        </div>
      ) : (
        <>
          <div
            ref={scroll}
            className="chat-scroll"
            onScroll={(event) => {
              const element = event.currentTarget;
              scrollPositions.set(bot.id, element.scrollTop);
              bottom.current =
                element.scrollHeight -
                  element.clientHeight -
                  element.scrollTop <
                90;
            }}
          >
            {bot.messages.length ? (
              <div className="message-list">
                {bot.messages.map((message) => (
                  <MessageView key={message._key} message={message} />
                ))}
                {working && (
                  <div className="working-indicator">
                    <span className="pulse-dot" />
                    {bot.status === "needs_input"
                      ? "Waiting for your response"
                      : bot.status === "starting"
                        ? "Starting engine"
                        : "Working"}
                  </div>
                )}
              </div>
            ) : (
              <div className="conversation-empty">
                <div className="empty-symbol">✳</div>
                <h2>A workspace, ready for thought.</h2>
                <p>
                  Send your first message to start {bot.name}.<br />
                  The agent will work {inDesktop ? "in" : "on the host in"}{" "}
                  <code>{bot.cwd}</code>.
                </p>
                {bot.engine === "native" && (
                  <p className="field-help">
                    File writes, edits, and shell commands require your
                    approval. Shell commands are not sandboxed.
                  </p>
                )}
              </div>
            )}
          </div>
          <div className="composer-area">
            {!models.length && (
              <div className="model-guidance">
                {bot.engine === "native" ? (
                  <>
                    <span>No models available. Connect this provider in Native settings, then refresh the model list.</span>
                    <button className="text-button" onClick={openSettings}>
                      Open native settings
                    </button>
                  </>
                ) : (
                  <span>
                    {running
                      ? "No models available. Run omp in Terminal on the host Mac, use /login, configure a model, then restart this bot."
                      : "Start this bot to load models from your installed OMP. Credentials and model settings remain OMP-owned."}
                  </span>
                )}
              </div>
            )}
            <form
              className="composer"
              onSubmit={(event) => {
                event.preventDefault();
                void send();
              }}
            >
              {draft.images.length > 0 && (
                <div className="attachments">
                  {draft.images.map((image, index) => (
                    <div className="attachment" key={`${image.name}-${index}`}>
                      <img
                        src={`data:${image.mimeType};base64,${image.data}`}
                        alt={image.name}
                      />
                      <span>{image.name}</span>
                      <button
                        type="button"
                        aria-label={`Remove ${image.name}`}
                        onClick={() =>
                          setDraft((previous) => ({
                            ...previous,
                            images: previous.images.filter(
                              (_, i) => i !== index,
                            ),
                          }))
                        }
                      >
                        <X size={12} />
                      </button>
                    </div>
                  ))}
                </div>
              )}
              <textarea
                ref={composer}
                role="combobox"
                aria-autocomplete="list"
                aria-expanded={commandChoices.length > 0}
                aria-controls={commandChoices.length ? commandsId : undefined}
                aria-activedescendant={commandChoices.length ? `${commandsId}-${commandIndex}` : undefined}
                aria-label={`Message ${bot.name}`}
                rows={3}
                value={draft.text}
                placeholder={
                  working
                    ? "Steer the agent or queue a follow-up…"
                    : `Message ${bot.name}…`
                }
                onChange={(event) => {
                  const text = event.target.value;
                  setCommandsDismissed(false);
                  setCommandActive(0);
                  setCommandNavigating(false);
                  setDraft((previous) => ({ ...previous, text }));
                }}
                onKeyDown={(event) => {
                  if (event.nativeEvent.isComposing) return;
                  if (commandChoices.length) {
                    if (event.key === "Escape") {
                      event.preventDefault();
                      setCommandsDismissed(true);
                      return;
                    }
                    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                      event.preventDefault();
                      setCommandNavigating(true);
                      setCommandActive((commandIndex + (event.key === "ArrowDown" ? 1 : -1) + commandChoices.length) % commandChoices.length);
                      return;
                    }
                    if ((event.key === "Tab" && !event.shiftKey) || (event.key === "Enter" && commandNavigating && !event.shiftKey && !window.matchMedia("(pointer: coarse)").matches)) {
                      event.preventDefault();
                      completeCommand(commandIndex);
                      return;
                    }
                  }
                  if (
                    event.key === "Enter" &&
                    !event.shiftKey &&
                    !window.matchMedia("(pointer: coarse)").matches &&
                    !event.nativeEvent.isComposing
                  ) {
                    event.preventDefault();
                    void send();
                  }
                }}
              />
              {commandChoices.length > 0 && (
                <div id={commandsId} role="listbox" aria-label="Chat commands" className="slash-commands">
                  {commandChoices.map((command, index) => (
                    <button
                      key={command.name}
                      id={`${commandsId}-${index}`}
                      type="button"
                      role="option"
                      aria-selected={index === commandIndex}
                      tabIndex={-1}
                      className={index === commandIndex ? "highlighted" : ""}
                      onMouseDown={(event) => event.preventDefault()}
                      onPointerMove={() => setCommandActive(index)}
                      onClick={() => completeCommand(index)}
                    >
                      <strong>{command.usage}</strong>
                      <span>{command.description}</span>
                    </button>
                  ))}
                </div>
              )}
              <div className="composer-toolbar">
                <div>
                  <input
                    ref={file}
                    className="sr-only"
                    tabIndex={-1}
                    type="file"
                    accept="image/png,image/jpeg,image/gif,image/webp"
                    multiple
                    onChange={(event) => void attach(event.target.files)}
                  />
                  <button
                    className="icon-button"
                    type="button"
                    title="Attach images"
                    aria-label="Attach images"
                    onClick={() => file.current?.click()}
                  >
                    <ImagePlus size={18} />
                  </button>
                  <select
                    aria-label="Message behavior while working"
                    value={draft.behavior}
                    onChange={(event) =>
                      setDraft((previous) => ({
                        ...previous,
                        behavior: event.target.value as Draft["behavior"],
                      }))
                    }
                  >
                    <option value="followUp">Queue follow-up</option>
                    <option value="steer">Steer now</option>
                  </select>
                </div>
                <div>
                  {working && (
                    <button
                      className="abort-button"
                      type="button"
                      disabled={busy}
                      onClick={() => void run("abort_bot", { id: bot.id })}
                    >
                      <Square size={12} />
                      Stop generation
                    </button>
                  )}
                  <button
                    className="send-button"
                    title="Send message"
                    aria-label="Send message"
                    disabled={
                      busy || (!draft.text.trim() && !draft.images.length)
                    }
                    type="submit"
                  >
                    <ArrowUp size={19} />
                  </button>
                </div>
              </div>
            </form>
            <div className="composer-caption">
              <span className="keyboard-hint">
                Enter to send · Shift + Enter for a new line · / for commands
              </span>
              <span className="touch-hint">
                New line with Return · Tap ↑ to send · / for commands
              </span>
              <button
                className="text-button"
                onClick={() => {
                  bottom.current = true;
                  scroll.current?.scrollTo({
                    top: scroll.current.scrollHeight,
                    behavior: "smooth",
                  });
                }}
              >
                <ArrowDown size={11} />
                Latest
              </button>
            </div>
          </div>
        </>
      )}
      {resumeDraft && (
        <ResumePicker
          botId={bot.id}
          busy={busy}
          onChoose={(session) => resumeSession(session.id, resumeDraft)}
          onClose={() => setResumeDraft(null)}
          onError={onError}
        />
      )}
    </section>
  );
}
