import { useEffect, useId, useRef, useState } from "react";
import type { ReactNode, RefObject } from "react";
import { Check, ChevronDown, Search, X, ExternalLink } from "lucide-react";
import { errorText, inDesktop, request, safeUrl } from "./api";
import type { Model } from "./api";

export function Modal({
  title,
  children,
  onClose,
  wide = false,
  initialFocus,
}: {
  title: string;
  children: ReactNode;
  onClose: () => void;
  wide?: boolean;
  initialFocus?: RefObject<HTMLElement | null>;
}) {
  const root = useRef<HTMLDivElement>(null);
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const element = root.current;
    element?.focus();
    initialFocus?.current?.focus();
    function key(event: KeyboardEvent) {
      if (event.key === "Escape") {
        event.preventDefault();
        close.current();
      }
      if (event.key !== "Tab" || !element) return;
      const controls = Array.from(
        element.querySelectorAll<HTMLElement>(
          'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex="0"]',
        ),
      ).filter((item) => item.getClientRects().length > 0);
      const first = controls[0],
        last = controls.at(-1);
      if (!first) {
        event.preventDefault();
        return;
      }
      if (
        event.shiftKey &&
        (document.activeElement === first || document.activeElement === element)
      ) {
        event.preventDefault();
        last?.focus();
      } else if (
        !event.shiftKey &&
        (document.activeElement === last || document.activeElement === element)
      ) {
        event.preventDefault();
        first.focus();
      }
    }
    document.addEventListener("keydown", key);
    return () => {
      document.removeEventListener("keydown", key);
      previous?.focus();
    };
  }, []);
  return (
    <div className="modal-shade">
      <div
        ref={root}
        className={`modal ${wide ? "wide" : ""}`}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        tabIndex={-1}
      >
        <header>
          <h2>{title}</h2>
          <button
            className="icon-button"
            aria-label="Close dialog"
            onClick={onClose}
          >
            <X size={18} />
          </button>
        </header>
        {children}
      </div>
    </div>
  );
}
export async function openExternalUrl(url: string): Promise<void> {
  const valid = safeUrl(url);
  if (!valid) throw new Error("Only HTTP and HTTPS links may be opened.");
  if (!window.confirm(`Open this URL in your browser?\n\n${valid}`)) return;
  if (inDesktop) await request("open_external", { url: valid });
  else window.open(valid, "_blank", "noopener,noreferrer");
}
export function ExternalLinkButton({
  url,
  children,
}: {
  url: string;
  children: ReactNode;
}) {
  const [error, setError] = useState("");
  const valid = safeUrl(url);
  return valid ? (
    <>
      <a
        href={valid}
        target="_blank"
        rel="noopener noreferrer"
        onClick={(event) => {
          event.preventDefault();
          setError("");
          void openExternalUrl(valid).catch((reason) =>
            setError(errorText(reason)),
          );
        }}
      >
        {children}
        <ExternalLink size={12} aria-hidden="true" />
      </a>
      {error && (
        <span role="alert" className="inline-error">
          {error}
        </span>
      )}
    </>
  ) : (
    <span title="Only HTTP and HTTPS links may be opened">{children}</span>
  );
}

export function ModelChooser({
  models,
  selected,
  providerNames,
  providerScope,
  disabled,
  onChoose,
  openRequest = 0,
  onClose,
}: {
  models: Model[];
  selected: { provider: string; id: string } | null;
  providerNames: Record<string, string>;
  providerScope?: string;
  disabled: boolean;
  onChoose: (model: Model) => void;
  openRequest?: number;
  onClose?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const listId = useId();
  const same = (model: Model) =>
    model.provider === selected?.provider && model.id === selected?.id;
  const current = models.find(same);
  const choices: Model[] =
    selected && !current && (!providerScope || selected.provider === providerScope)
      ? [{ ...selected }, ...models]
      : models;
  const normalized = query.trim().toLowerCase();
  const filtered = choices.filter((model) =>
    [model.id, model.name, model.provider, providerNames[model.provider]]
      .filter(Boolean).join(" ").toLowerCase().includes(normalized),
  );
  const activeIndex = Math.min(active, Math.max(0, filtered.length - 1));
  function close(restoreFocus = false) {
    setOpen(false);
    onClose?.();
    if (restoreFocus) trigger.current?.focus();
  }
  function show() {
    setQuery("");
    setActive(Math.max(0, choices.findIndex(same)));
    setOpen(true);
  }
  useEffect(() => {
    if (openRequest && !disabled) show();
  }, [openRequest]);
  useEffect(() => {
    if (!open) return;
    search.current?.focus();
    function outside(event: PointerEvent) {
      if (!root.current?.contains(event.target as Node)) close();
    }
    document.addEventListener("pointerdown", outside);
    return () => document.removeEventListener("pointerdown", outside);
  }, [open]);
  useEffect(() => {
    if (disabled) close();
  }, [disabled]);
  useEffect(() => {
    if (open)
      document.getElementById(`${listId}-${activeIndex}`)?.scrollIntoView({ block: "nearest" });
  }, [open, activeIndex, listId, query]);
  return (
    <div
      className="model-chooser"
      ref={root}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget as Node | null))
          close();
      }}
      onKeyDown={(event) => {
        if (event.key === "Escape" && open) {
          event.preventDefault();
          event.stopPropagation();
          close(true);
        }
      }}
    >
      <button
        ref={trigger}
        type="button"
        className="model-chooser-trigger"
        aria-label={`Choose model. Current: ${selected ? `${providerNames[selected.provider] ?? selected.provider} / ${current?.name || selected.id}` : "none"}`}
        aria-expanded={open}
        aria-haspopup="listbox"
        aria-controls={open ? listId : undefined}
        disabled={disabled}
        onClick={() => open ? close() : show()}
        onKeyDown={(event) => {
          if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            show();
          }
        }}
      >
        <span className="model-chooser-selection">
          <span className="model-provider">{selected ? providerNames[selected.provider] ?? selected.provider : "Model"}</span>
          <span className="model-name">{current?.name || selected?.id || "Choose a model"}</span>
        </span>
        <ChevronDown size={13} aria-hidden="true" />
      </button>
      {open && (
        <div className="model-chooser-popover">
          <div className="model-search">
            <Search size={15} aria-hidden="true" />
            <input
              ref={search}
              role="combobox"
              aria-label="Search models"
              aria-expanded="true"
              aria-autocomplete="list"
              aria-controls={listId}
              aria-activedescendant={filtered.length ? `${listId}-${activeIndex}` : undefined}
              value={query}
              placeholder="Search models or providers…"
              onChange={(event) => { setQuery(event.target.value); setActive(0); }}
              onKeyDown={(event) => {
                if (["ArrowDown", "ArrowUp", "Enter"].includes(event.key)) {
                  event.preventDefault();
                  if (!filtered.length) return;
                  if (event.key === "Enter") {
                    onChoose(filtered[activeIndex]);
                    close(true);
                  } else {
                    const step = event.key === "ArrowDown" ? 1 : -1;
                    setActive((activeIndex + step + filtered.length) % filtered.length);
                  }
                }
              }}
            />
          </div>
          <div role="listbox" id={listId} aria-label="Models" className="model-options">
            {filtered.map((model, index) => (
              <button
                type="button"
                role="option"
                id={`${listId}-${index}`}
                key={JSON.stringify([model.provider, model.id])}
                aria-selected={same(model)}
                className={`model-option ${index === activeIndex ? "highlighted" : ""}`}
                tabIndex={-1}
                onMouseDown={(event) => event.preventDefault()}
                onPointerMove={() => setActive(index)}
                onClick={() => { onChoose(model); close(true); }}
              >
                <span>
                  <strong>{model.name || model.id}</strong>
                  <small>{providerNames[model.provider] ?? model.provider}{model.name && model.name !== model.id ? ` · ${model.id}` : ""}</small>
                  {same(model) && !current && <small>Current model · not in refreshed catalog</small>}
                </span>
                {same(model) && <Check size={14} aria-hidden="true" />}
              </button>
            ))}
          </div>
          {!filtered.length && (
            <p className="model-chooser-empty" role="status">
              {choices.length ? "No matching models. Try another name or ID." : "No models available. Connect a provider in settings, then refresh the model list."}
            </p>
          )}
          {!!filtered.length && <div className="model-chooser-footer">{filtered.length} model{filtered.length === 1 ? "" : "s"} · ↑↓ to browse · Enter to select</div>}
        </div>
      )}
    </div>
  );
}
export function JsonDetails({
  label,
  value,
  open = false,
}: {
  label: string;
  value: unknown;
  open?: boolean;
}) {
  return (
    <details open={open}>
      <summary>{label}</summary>
      <pre className="json">
        {typeof value === "string"
          ? value
          : (JSON.stringify(value, null, 2) ?? "Not available")}
      </pre>
    </details>
  );
}
