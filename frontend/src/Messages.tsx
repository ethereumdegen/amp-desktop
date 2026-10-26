import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Bot as BotIcon, User, Terminal, Copy } from "lucide-react";
import { Children, isValidElement, useState } from "react";
import type { ReactNode } from "react";
import type { Message } from "./api";
import { ExternalLinkButton, JsonDetails } from "./ui";
import { SvgPreview } from "./Preview";

function Code({
  children,
  className,
}: {
  children?: ReactNode;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);
  const text = String(children ?? "");
  if (!className && !text.includes("\n")) return <code>{children}</code>;
  return (
    <span className="code-block">
      <span className="code-heading">
        <span>{className?.replace("language-", "") || "code"}</span>
        <button
          aria-label="Copy code"
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(text);
              setCopied(true);
            } catch {
              setCopied(false);
            }
          }}
        >
          {copied ? "Copied" : <Copy size={13} />}
        </button>
      </span>
      <code className={className}>{children}</code>
    </span>
  );
}
export function Markdown({ text }: { text: string }) {
  return (
    <div className="markdown">
      <ReactMarkdown
        skipHtml
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ href, children }) => (
            <ExternalLinkButton url={href ?? ""}>{children}</ExternalLinkButton>
          ),
          img: ({ src, alt }) => (
            <ExternalLinkButton url={typeof src === "string" ? src : ""}>
              {alt || "External image"}
            </ExternalLinkButton>
          ),
          pre: ({ children }) => {
            const child = Children.toArray(children)[0];
            if (isValidElement<{ className?: string; children?: ReactNode }>(child) &&
                child.props.className?.split(/\s+/).includes("language-svg")) {
              const source = String(child.props.children ?? "");
              return <SvgPreview source={source}><pre><Code className="language-svg">{source}</Code></pre></SvgPreview>;
            }
            return <pre>{children}</pre>;
          },
          code: ({ children, className }) => (
            <Code className={className}>{children}</Code>
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
}
function Content({ value }: { value: unknown }) {
  if (typeof value === "string") return <Markdown text={value} />;
  if (!Array.isArray(value))
    return value == null ? null : (
      <JsonDetails label="Message data" value={value} />
    );
  return (
    <>
      {value.map((entry: unknown, index) => {
        if (!entry || typeof entry !== "object")
          return <JsonDetails key={index} label="Content" value={entry} />;
        const part = entry as Record<string, unknown>;
        if (part.type === "text")
          return <Markdown key={index} text={String(part.text ?? "")} />;
        if (part.type === "thinking" || part.type === "reasoning")
          return (
            <details key={index} className="thinking">
              <summary>Reasoning</summary>
              <Markdown text={String(part.thinking ?? part.text ?? "")} />
            </details>
          );
        if (part.type === "toolCall" || part.type === "tool_use")
          return (
            <details key={index} className="tool-call">
              <summary>
                <Terminal size={14} /> {String(part.name ?? "Tool call")}
              </summary>
              <pre>
                {typeof part.arguments === "string"
                  ? part.arguments
                  : JSON.stringify(part.arguments ?? part.input, null, 2)}
              </pre>
            </details>
          );
        if (part.type === "image") {
          const mime = typeof part.mimeType === "string" ? part.mimeType : "";
          return /^image\/(png|jpeg|gif|webp)$/.test(mime) &&
            typeof part.data === "string" && part.data.length <= 32 * 1024 * 1024 &&
            /^[A-Za-z0-9+/=\s]+$/.test(part.data) ? (
            <figure className="image-preview" key={index}>
              <img
                className="message-image"
                src={`data:${mime};base64,${part.data}`}
                alt={typeof part.alt === "string" ? part.alt.slice(0, 240) : typeof part.name === "string" ? part.name.slice(0, 240) : "Image preview"}
                loading="lazy"
                decoding="async"
              />
              {typeof part.alt === "string" && <figcaption>{part.alt.slice(0, 240)}</figcaption>}
            </figure>
          ) : (
            <JsonDetails
              key={index}
              label="Image metadata"
              value={{ ...part, data: part.data ? "[image data]" : undefined }}
            />
          );
        }
        return (
          <JsonDetails
            key={index}
            label={String(part.type ?? "Content")}
            value={part}
          />
        );
      })}
    </>
  );
}
export function MessageView({ message }: { message: Message }) {
  const tool = message.role === "toolResult" || message.role === "tool";
  const toolImages = tool && Array.isArray(message.content)
    ? message.content.filter((part) => part && typeof part === "object" && part.type === "image")
    : [];
  const toolText = tool && Array.isArray(message.content)
    ? message.content.filter((part) => !part || typeof part !== "object" || part.type !== "image")
    : message.content;
  return (
    <article
      className={`message ${message.role === "user" ? "from-user" : ""} ${message.isError ? "failed" : ""}`}
    >
      <div className="message-avatar">
        {message.role === "user" ? (
          <User size={16} />
        ) : tool ? (
          <Terminal size={16} />
        ) : (
          <BotIcon size={17} />
        )}
      </div>
      <div className="message-body">
        <div className="message-meta">
          {message.role === "user"
            ? "You"
            : tool
              ? `${message.toolName || "Tool"} ${message.isError ? "failed" : "result"}`
              : message.role === "assistant"
                ? "Assistant"
                : message.role}
        </div>
        {tool ? (
          <>
            {!!toolImages.length && <section className="tool-previews" aria-label={message.toolName === "render_preview" ? "Rendered design mockup" : "Tool images"}>
              {message.toolName === "render_preview" && <p className="preview-caption">Design mockup · rendered from HTML/CSS, not a screenshot of the existing app</p>}
              <Content value={toolImages} />
            </section>}
            <details className="tool-result">
              <summary>
                {message.toolName || "Tool"} output
                {message.isError ? " · error" : ""}
              </summary>
              <Content value={toolText} />
            </details>
          </>
        ) : (
          <Content value={message.content ?? message.text ?? message} />
        )}
      </div>
    </article>
  );
}
