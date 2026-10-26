import { invoke, isTauri } from "@tauri-apps/api/core";

export type Engine = "omp" | "native";
export type JsonObject = Record<string, unknown>;
export interface Provider {
  id: string;
  name: string;
  kind: "openai" | "anthropic";
  baseUrl: string;
  models: string[];
  apiKeyEnv: string | null;
  hasApiKey: boolean;
  authMethod: "oauth" | "apiKey" | "none";
  oauthAvailable: boolean;
}
export interface Model {
  provider: string;
  id: string;
  name?: string;
  contextWindow?: number;
  reasoning?: boolean;
}
export interface NativeLogin {
  loginId: string;
  url: string;
  instructions: string;
  requiresCode: boolean;
}
export interface NativeLoginStatus {
  status: "pending" | "connected" | "error" | "cancelled";
  error?: string;
}
export interface Message extends JsonObject {
  _key: string;
  role: string;
  content?: unknown;
  toolName?: string;
  toolCallId?: string;
  isError?: boolean;
}
export interface Dialog extends JsonObject {
  id: string;
  method: string;
  title?: string;
  message?: string;
  options?: unknown[];
  questions?: unknown[];
  url?: string;
}
export interface Bot {
  id: string;
  name: string;
  cwd: string;
  instructions: string;
  profile: string | null;
  engine: Engine;
  nativeProvider: string | null;
  capabilities: string[];
  model: { provider: string; id: string } | null;
  thinking: string | null;
  sessionFile: string | null;
  status: "stopped" | "starting" | "idle" | "working" | "needs_input" | "error";
  isRunning: boolean;
  error: string | null;
  engineVersion: string | null;
  pid: number | null;
  messages: Message[];
  models: Model[];
  thinkingLevels: string[];
  dialogs: Dialog[];
  state: JsonObject | null;
  revision: number;
}
export interface SessionSummary {
  id: string;
  title: string;
  sessionFile: string;
  modifiedAt: number | null;
  current: boolean;
}
export interface Snapshot {
  engine: {
    status: "ready" | "missing" | "error";
    path: string | null;
    version: string | null;
    error: string | null;
    configuredPath: string | null;
  };
  native: { providers: Provider[]; error: string | null };
  bots: Bot[];
}
export const inDesktop = isTauri();
const tokenKey = "omp-remote-access-token";
export function setAccessToken(value: string): void {
  if (value.trim()) sessionStorage.setItem(tokenKey, value.trim());
  else sessionStorage.removeItem(tokenKey);
}
export function hasAccessToken(): boolean {
  try {
    return !!sessionStorage.getItem(tokenKey);
  } catch {
    return false;
  }
}
export interface RemoteStatus {
  enabled: boolean;
  running: boolean;
  localUrl: string;
  tailnetUrl: string | null;
  port: number;
  error: string | null;
  serveCommand: string;
  instanceId: string;
}
export interface SmsMessage {
  id: string;
  project_id: string;
  source_id: string;
  from_id: string;
  body: string;
  received_at: number;
  state: "pending" | "processing" | "completed" | "failed" | "unknown" | "dismissed";
  detail: string | null;
  claim_id: string | null;
  review?: boolean;
}
export interface SmsStatus {
  enabled: boolean;
  running: boolean;
  relayUrl: string;
  pipestreamrUrl: string;
  projectId: string;
  sourceId: string;
  phone: string;
  botId: string;
  hasRelayToken: boolean;
  hasApiKey: boolean;
  error: string | null;
  messages: SmsMessage[];
}
export interface SmsConfiguration extends JsonObject {
  enabled: boolean;
  relayUrl: string;
  pipestreamrUrl: string;
  projectId: string;
  sourceId: string;
  phone: string;
  botId: string;
  relayToken?: string;
  apiKey?: string;
}
export class AuthenticationError extends Error {}
export async function request<T = Snapshot>(
  method: string,
  args: JsonObject = {},
  options: { signal?: AbortSignal } = {},
): Promise<T> {
  if (inDesktop) return invoke<T>("desktop_request", { method, args });
  const token = sessionStorage.getItem(tokenKey);
  if (!token)
    throw new AuthenticationError(
      "Enter your desktop's remote access token to connect.",
    );
  let response: Response;
  try {
    response = await fetch("/api/request", {
      method: "POST",
      cache: "no-store",
      credentials: "omit",
      redirect: "error",
      signal: options.signal,
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({ method, args }),
    });
  } catch {
    if (options.signal?.aborted)
      throw new DOMException("Request cancelled", "AbortError");
    throw new Error(
      "Cannot reach your desktop. Keep the host Mac awake, AMP Desktop running, and Tailscale connected on both devices. For the development bridge, keep bun run bridge and Vite running.",
    );
  }
  if (response.status === 401 || response.status === 403) {
    if (sessionStorage.getItem(tokenKey) === token) {
      setAccessToken("");
      window.dispatchEvent(new Event("omp-authentication-required"));
    }
    throw new AuthenticationError(
      response.status === 401
        ? "Access token rejected or rotated. Pair again using the token in your desktop's Remote access settings."
        : "Remote access rejected. Check the host's configured Tailscale URL and pair again.",
    );
  }
  let result: { data?: T; error?: unknown };
  try {
    result = await response.json();
  } catch {
    throw new Error(
      "The host did not return an OMP response. Check the desktop's Remote access settings and Tailscale Serve configuration.",
    );
  }
  if (result.error !== undefined && result.error !== null) {
    throw new Error(
      typeof result.error === "string"
        ? result.error
        : JSON.stringify(result.error),
    );
  }
  if (!response.ok || !("data" in result))
    throw new Error(`Backend request failed (${response.status}).`);
  return result.data as T;
}
export function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
export function safeUrl(value: string): string | null {
  try {
    const url = new URL(value);
    return ["https:", "http:"].includes(url.protocol) ? url.href : null;
  } catch {
    return null;
  }
}
