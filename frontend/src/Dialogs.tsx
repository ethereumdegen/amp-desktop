import { useState } from "react";
import type { Dialog, JsonObject } from "./api";
import { ExternalLinkButton, JsonDetails, Modal } from "./ui";
import { Markdown } from "./Messages";

function optionValue(option: unknown): string {
  if (typeof option === "string") return option;
  if (option && typeof option === "object") {
    const item = option as JsonObject;
    return String(item.value ?? item.label ?? item.id ?? JSON.stringify(item));
  }
  return String(option);
}
export function RuntimeDialog({
  dialog,
  busy,
  respond,
}: {
  dialog: Dialog;
  busy: boolean;
  respond: (response: JsonObject) => Promise<boolean>;
}) {
  const [value, setValue] = useState(
    String(
      dialog.prefill ??
        dialog.initialValue ??
        dialog.defaultValue ??
        dialog.value ??
        "",
    ),
  );
  const [answers, setAnswers] = useState<
    Record<string, { selectedOptions: string[]; customInput: string }>
  >({});
  const [copyStatus, setCopyStatus] = useState("");
  const cancel = () => {
    if (!busy) void respond({ cancelled: true, confirmed: false });
  };
  const known = [
    "select",
    "confirm",
    "input",
    "editor",
    "ask",
    "open_url",
  ].includes(dialog.method);
  return (
    <Modal
      title={
        dialog.title ||
        (dialog.method === "confirm"
          ? "Permission required"
          : "Agent needs your input")
      }
      onClose={cancel}
      wide
    >
      <div className="dialog-content">
        <div className="eyebrow">
          {dialog.method === "confirm"
            ? "Review before approving"
            : dialog.method}
        </div>
        {dialog.message &&
          (dialog.method === "confirm" ? (
            <pre className="approval-diff">{dialog.message}</pre>
          ) : (
            <Markdown text={dialog.message} />
          ))}
        {typeof dialog.diff === "string" && (
          <pre className="approval-diff">{dialog.diff}</pre>
        )}
        {typeof dialog.command === "string" && <pre>{dialog.command}</pre>}
        {typeof dialog.description === "string" && <p>{dialog.description}</p>}
        {dialog.method === "select" && (
          <div className="option-list">
            {(dialog.options ?? []).map((option, index) => {
              const detail = Array.isArray(dialog.optionDetails)
                ? (dialog.optionDetails[index] as JsonObject | undefined)
                : undefined;
              return (
                <div key={index}>
                  <button
                    disabled={busy}
                    onClick={() => void respond({ value: optionValue(option) })}
                  >
                    {optionValue(option)}
                  </button>
                  {typeof detail?.description === "string" && (
                    <p className="field-help">{detail.description}</p>
                  )}
                </div>
              );
            })}
          </div>
        )}
        {(dialog.method === "input" || dialog.method === "editor") && (
          <form
            onSubmit={(event) => {
              event.preventDefault();
              void respond({ value });
            }}
          >
            <label>
              {dialog.title || "Your response"}
              {dialog.method === "editor" ? (
                <textarea
                  autoFocus
                  rows={12}
                  value={value}
                  onChange={(event) => setValue(event.target.value)}
                />
              ) : (
                <input
                  autoFocus
                  placeholder={
                    typeof dialog.placeholder === "string"
                      ? dialog.placeholder
                      : undefined
                  }
                  value={value}
                  onChange={(event) => setValue(event.target.value)}
                />
              )}
            </label>
            <button className="primary" disabled={busy} type="submit">
              Submit response
            </button>
          </form>
        )}
        {dialog.method === "ask" && (
          <form
            onSubmit={(event) => {
              event.preventDefault();
              void respond({
                answers: (dialog.questions ?? []).map((raw, index) => {
                  const question =
                    raw && typeof raw === "object" ? (raw as JsonObject) : {};
                  const id = String(question.id ?? index);
                  const answer = answers[id];
                  return {
                    id,
                    selectedOptions: answer?.selectedOptions ?? [],
                    ...(answer?.customInput.trim()
                      ? { customInput: answer.customInput }
                      : {}),
                  };
                }),
              });
            }}
          >
            {(dialog.questions ?? []).map((raw, index) => {
              const question: JsonObject =
                raw && typeof raw === "object"
                  ? (raw as JsonObject)
                  : { question: raw };
              const id = String(question.id ?? index);
              const choices = Array.isArray(question.options)
                ? question.options
                : [];
              const multi = Boolean(question.multi);
              return (
                <fieldset key={id}>
                  <legend>
                    {String(question.question ?? `Question ${index + 1}`)}
                  </legend>
                  {typeof question.header === "string" && (
                    <p className="field-help">{question.header}</p>
                  )}
                  {choices.map((option, choice) => {
                    const label = optionValue(option);
                    const detail =
                      option && typeof option === "object"
                        ? (option as JsonObject)
                        : {};
                    return (
                      <div key={choice}>
                        <label className="choice">
                          <input
                            type={multi ? "checkbox" : "radio"}
                            name={id}
                            value={label}
                            checked={
                              answers[id]?.selectedOptions.includes(label) ??
                              false
                            }
                            onChange={(event) =>
                              setAnswers((previous) => {
                                const current = previous[id] ?? {
                                  selectedOptions: [],
                                  customInput: "",
                                };
                                return {
                                  ...previous,
                                  [id]: {
                                    ...current,
                                    selectedOptions: multi
                                      ? event.target.checked
                                        ? [...current.selectedOptions, label]
                                        : current.selectedOptions.filter(
                                            (item) => item !== label,
                                          )
                                      : [label],
                                  },
                                };
                              })
                            }
                          />
                          {label}
                          {question.recommended === choice && (
                            <span className="muted"> · recommended</span>
                          )}
                        </label>
                        {typeof detail.description === "string" && (
                          <p className="field-help">{detail.description}</p>
                        )}
                        {typeof detail.preview === "string" && (
                          <JsonDetails label="Preview" value={detail.preview} />
                        )}
                      </div>
                    );
                  })}
                  <label>
                    Custom response{" "}
                    <span className="muted">
                      (optional, may accompany a selection)
                    </span>
                    <textarea
                      rows={3}
                      value={answers[id]?.customInput ?? ""}
                      onChange={(event) =>
                        setAnswers((previous) => ({
                          ...previous,
                          [id]: {
                            selectedOptions:
                              previous[id]?.selectedOptions ?? [],
                            customInput: event.target.value,
                          },
                        }))
                      }
                    />
                  </label>
                  {!!answers[id]?.selectedOptions.length && (
                    <button
                      type="button"
                      className="text-button"
                      onClick={() =>
                        setAnswers((previous) => ({
                          ...previous,
                          [id]: { ...previous[id], selectedOptions: [] },
                        }))
                      }
                    >
                      Clear selection
                    </button>
                  )}
                </fieldset>
              );
            })}
            {!dialog.questions?.length && (
              <p className="muted">
                This request contains no questions. You can cancel it below.
              </p>
            )}
            <button
              className="primary"
              disabled={busy || !dialog.questions?.length}
              type="submit"
            >
              Submit answers
            </button>
          </form>
        )}
        {dialog.method === "open_url" && (
          <>
            <p>
              Opening this link is your choice. The agent cannot open it
              automatically.
            </p>
            {typeof dialog.instructions === "string" && (
              <Markdown text={dialog.instructions} />
            )}
            <ExternalLinkButton
              url={
                typeof dialog.launchUrl === "string"
                  ? dialog.launchUrl
                  : (dialog.url ?? "")
              }
            >
              {dialog.url || "No URL provided"}
            </ExternalLinkButton>
            <div className="button-row">
              <button
                onClick={async () => {
                  try {
                    await navigator.clipboard.writeText(
                      typeof dialog.launchUrl === "string"
                        ? dialog.launchUrl
                        : (dialog.url ?? ""),
                    );
                    setCopyStatus("Link copied");
                  } catch {
                    setCopyStatus(
                      "Could not access clipboard. Select and copy the displayed URL.",
                    );
                  }
                }}
              >
                Copy link
              </button>
              <span className="field-help" role="status">
                {copyStatus}
              </span>
            </div>
          </>
        )}
        {!known && (
          <>
            <p>
              This agent request is not supported by this interface. Cancel to
              let the agent continue without approval.
            </p>
            <JsonDetails label="Request details" value={dialog} open />
          </>
        )}
      </div>
      <footer>
        <button disabled={busy} onClick={cancel}>
          Cancel
        </button>
        {dialog.method === "confirm" && (
          <>
            <button
              disabled={busy}
              onClick={() => void respond({ confirmed: false })}
            >
              Deny
            </button>
            <button
              className="primary"
              disabled={busy}
              onClick={() => void respond({ confirmed: true })}
            >
              Approve
            </button>
          </>
        )}
        {dialog.method === "open_url" && (
          <button
            disabled={busy}
            onClick={() => void respond({ confirmed: true })}
          >
            Done
          </button>
        )}
      </footer>
    </Modal>
  );
}
