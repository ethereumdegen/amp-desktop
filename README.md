# AMP Desktop

A Tauri 2 desktop interface with a resizable bot sidebar and a focused chat workspace. Choose an engine when creating each bot:

The app is named **AMP Desktop**, with **AMP Remote** for its phone/browser client. Internal package/executable names, the bundle identifier, data directories, `OMP_DESKTOP_DATA_DIR`, owner sockets, and Keychain services retain their existing identifiers so the rename preserves saved bots, sessions, and credentials. The installed OMP engine is still called OMP.

| Engine | Runtime dependency | Models and credentials | Updates |
| --- | --- | --- | --- |
| Installed OMP | A working local `omp` executable | OMP's existing configuration, profiles, authentication, tools, and extensions | New processes use the currently installed OMP; stop/start a bot after updating it |
| Native Rust | Included in the desktop executable; no OMP or Bun required at runtime | Independent OpenAI/Anthropic account sign-in or API-key/custom-provider settings; credentials in macOS Keychain | Engine and pinned native library updates ship with this app |

Engine and native-provider selection belong to a conversation. Create another bot to change them; histories are not silently converted between engines.

## Run from source

Requires macOS, Xcode command-line tools, Rust, and Bun for frontend development. Native runtime/tool process management currently targets Unix/macOS. Keep at least 5 GB free for native builds, with additional headroom for release/LTO builds and OS swap on low-memory machines. Low-space builds can leave invalid executable output even when a build step reports success; validate the bundle before using it.

```sh
bun install
bun install --cwd frontend
bun run desktop
```

Create a bot, enter an existing workspace directory, choose its engine, and send a message. OMP models load when the bot starts. Native models come from the selected provider's configured/discovered model IDs.

The development profile disables debug symbols/incremental output and uses one code-generation unit. On memory-constrained machines, also use one compiler job:

```sh
CARGO_BUILD_JOBS=1 bun run package:debug
```

The debug bundle is generated under `target/debug/bundle/macos/AMP Desktop.app`. `bun run package` builds a release bundle instead. Bundles are locally ad-hoc signed, not Developer-ID-signed or notarized distributions. Validate before opening:

Normal Cargo builds enable `custom-protocol` by default, embedding the built frontend so a packaged app never depends on Vite. `bun run desktop` uses Tauri's development mode, which disables that default feature and starts the configured dev server. Use `package:debug` or `package` for distributable app bundles; `tauri bundle` alone only packages the existing binary and does not rebuild it.

```sh
codesign --verify --deep --strict --verbose=2 "target/debug/bundle/macos/AMP Desktop.app"
open "target/debug/bundle/macos/AMP Desktop.app"
```

If workspace file watchers or OS swap pressure interfere, set `CARGO_TARGET_DIR` to a directory outside the checkout, such as `$HOME/Library/Caches/omp-desktop-build`; the bundle then appears under that directory instead. Do not use a partially generated bundle after a disk-full failure.

## Installed OMP setup

If OMP is missing, its bot view shows an installation page with the official command, a copy button, executable-path selection, and retry auto-discovery. It never runs an installer automatically. Native bots remain usable.

Official installation: <https://omp.sh/>

```sh
curl -fsSL https://omp.sh/install | sh
```

Run `omp` in Terminal and use `/login` to configure a provider. The desktop does not read or copy OMP credentials. A named OMP profile can be selected per bot.

Discovery checks an explicit saved path, `OMP_DESKTOP_OMP` when set, PATH, and common user/Homebrew install locations. The child PATH includes likely Bun locations for Finder launches. An explicit invalid path is an error, not permission to silently use another installation.

OMP runs directly in RPC mode, not through terminal scraping or an interpolated shell command. Startup negotiates protocol v2, handles chunked frames, and correlates commands separately from turn-completion and session-settlement events. An incompatible engine reports a startup error. Arbitrary future protocol compatibility is not guaranteed.

The app does not install/update OMP itself or interrupt active work after an external update. Stop and start a bot to launch the updated executable. Each live bot displays its actual engine version.

## Native provider setup

Open **Engine settings → Native providers → Connect an account**:

1. Choose **Sign in with browser** on OpenAI or Anthropic. Account/model access depends on the provider and subscription.
2. Complete authorization in your browser. OpenAI normally returns automatically through a loopback callback on the host Mac, port 1455. If that port is occupied or you are signing in from another device, paste the complete final redirect URL into the manual fallback field.
3. Anthropic requires pasting the final redirect URL or `code#state` shown by the browser; a code without its state is rejected. Keep settings open until connected. Closing settings cancels a pending login.
4. Account sign-in loads the live model catalog once. Create a Native Rust bot using that provider, then search the model picker by name, ID, or provider. Use its refresh button for later catalog updates. A discovery failure keeps the account connected and displays an error without discarding the current list.

Native account sign-in is independent of installed OMP: no OMP credentials are read or copied. OpenAI subscription requests use the Codex Responses backend. Official OpenAI GPT-6 API-key requests also use Responses because their native tool calls are not supported through Chat Completions. **Disconnect** removes only the Native OAuth grant; saved API keys remain available. Login sessions expire after ten minutes, and access tokens refresh through Keychain-held refresh credentials.

**Use an API key instead** opens **API keys & advanced provider settings**, which also supports custom providers:

- Provider format: OpenAI-compatible Chat Completions or Anthropic Messages.
- HTTPS base URL, or HTTP for a loopback server.
- Editable model IDs, or **Refresh model list** to discover them. Official OpenAI API discovery excludes non-chat model families; compatible custom providers keep their own IDs.
- An API-key environment-variable name or an API key saved in macOS Keychain. Saving a new key explicitly switches an official provider away from OAuth. Otherwise, a connected account takes precedence over API keys; environment keys take precedence over saved API keys.

Stop bots using a provider before changing its endpoint, credentials, or configuration. Catalog-only refresh is allowed while all its running bots are idle, and newly discovered models can be selected without restarting. Credential storage does not return tokens/key values in snapshots or write them into session files. Conversation text and tool results can still contain secrets you explicitly send or read.

Connection-status polling never reads Keychain. On macOS, startup and account badges use only noninteractive entry-presence queries, not password decryption, so a saved account cannot hold window creation behind an unlock prompt. Actual provider use loads OAuth grants or API keys once per process and caches them in memory, including missing entries and denied reads, so inference/tool continuations do not repeatedly request permission. Successful credential changes and token refresh update that cache. A macOS unlock/access prompt can still occur on first provider use or an explicit credential mutation; denied reads require reconnecting/updating the credential or restarting after fixing Keychain access. External Keychain changes take effect after restarting the app.

Default definitions cover OpenAI, OpenRouter, and Anthropic. Current starter IDs include GPT-6.1 Sol, GPT-6 Astra/Sol/Luna, and Claude 5.5, with live discovery preferred for account-specific availability. Exact previously shipped API/subscription lists are upgraded; edited catalogs are preserved. Model references: [OpenAI Sol 6.1](https://developers.openai.com/api/docs/models/gpt-6.1-sol), [Anthropic](https://platform.claude.com/docs/en/models/overview). A localhost-compatible server may run without authentication.

Native mode implements an actual streaming model/tool loop with `read`, `glob`, `grep`, `write`, `edit`, `bash`, and `render_preview`. It supports image messages, queued follow-ups, steering, abort, exact provider/model selection, and persistent history. Thinking uses the provider default; models requiring adaptive reasoning can still reason, and signed Anthropic thinking is retained for tool continuation. Fine-grained reasoning controls, OMP extensions, LSP, and subagents are not native-mode features.

Context usage is explicitly estimated. Native mode selects recent complete user/tool groups within a conservative 24,000-token budget while retaining the full saved history; it does not fabricate a summary. In an overlong active turn, older successful read/search text can be shortened or omitted with an explicit notice in provider context only. This preserves tool pairs, `AGENTS.md` guidance, errors/mutation results, and images. Irreducibly oversized input still fails visibly.

### Workspace discovery and design previews

Native agents receive their canonical workspace and a built-in source-first workflow, in addition to bot instructions. `read` lists directories; `glob` returns directory matches with a trailing `/`. Discovery is breadth-first and prunes generated/dependency/VCS trees, so a large earlier checkout does not hide a sibling project. Explicitly scoped dependency searches remain possible. File tools never widen the bot's workspace to find a project: for `~/projects/harbor-tasks`, use `~/projects` or the project itself as the bot workspace. Restart an existing bot after updating the app to load the new runtime.

For design requests, the agent can read project guidance, package metadata, UI source, and styles, then call `render_preview` to produce a genuine PNG from its proposed static HTML/CSS. This requires installed Chrome/Chromium; set `OMP_DESKTOP_BROWSER` to an absolute executable path for a nonstandard installation. It never downloads a browser. Rendering uses an isolated temporary profile, disables JavaScript, and blocks page navigation/external resources. It does not start the project, browse URLs, or modify project files.

These images are **design mockups, not screenshots of the existing/running app**. Tool images appear directly in the conversation, outside collapsed tool details. Fenced `svg` drawings also render as sanitized static images with escaped source available separately. Raw Markdown HTML remains disabled, and external Markdown images remain explicit links rather than automatic network requests.

## Private remote control with Tailscale

AMP Remote is a focused phone/browser frontend for **this running desktop instance**. It controls the same bots, sessions, models, and approvals as the Mac window; it does not start another OMP process for each connection or use an SSH terminal. A custom iOS app is not required.

1. Install [Tailscale](https://tailscale.com/download) on the home Mac and phone. Connect both to the same private tailnet. Keep the Mac awake, logged in, and AMP Desktop running; the remote client cannot wake the Mac or launch a stopped desktop app.
2. In the Mac app, open **Engine settings → Remote access**, or use the owner CLI below over an existing SSH connection. Access starts disabled. Enable it to bind only `127.0.0.1:14501` (or your configured loopback port).
3. Check `tailscale serve status` before changing any existing routes. Run the displayed command yourself on the Mac:

   ```sh
   tailscale serve --bg --https=8443 http://127.0.0.1:14501
   ```

4. Copy the exact private HTTPS address printed by Serve, such as `https://home-mac.your-tailnet.ts.net:8443`, into **Exact Tailscale HTTPS URL** in the app and save. If you choose another unused Serve port, use that port in both the command and address.
5. Reveal/copy the token in the local settings, or run the owner CLI's `remote token` on the Mac. Open the HTTPS address on the phone and enter the token. Never put tokens in URLs or share them with untrusted devices.
6. On iPhone, optionally use Safari **Share → Add to Home Screen**. Select a bot to chat, change models, steer/queue messages, stop generation, or answer tool approvals. Workspace paths refer to the home Mac, not the phone.

This uses [Tailscale Serve](https://tailscale.com/docs/reference/tailscale-cli/serve), **not Funnel** or a public listener. Tailnet access and the bearer token are both required. Restrict tailnet access to trusted users/devices: possession of the token grants the same agent/tool command authority as the desktop UI, including approved shell execution under the Mac user's account. There are no per-bot permission roles.

Browser credentials use `sessionStorage`, never URL parameters or `localStorage`. Disconnect forgets browser state without stopping agents. Polling pauses while the page is hidden and refreshes on return; dropped connections never automatically replay prompts. Pairing/token administration is available only in the local desktop UI or owner CLI, never through the network API.

Disabling remote access closes the listener without stopping bots. Token rotation rejects subsequent requests using the old token; accepted operations are not rolled back. Enablement and the token survive normal app restarts. Closing the Mac window while the listener is running hides the window and keeps hosting; click the Dock icon to reopen. **Quit AMP Desktop** stops both remote access and the agents. A failed quit keeps local administration usable so you can inspect the error and retry.

The app never installs Tailscale, changes tailnet settings, or executes Serve automatically. If another service uses the chosen HTTPS port, select an unused port rather than overwriting it. Disabling the app's listener does not remove a persistent Serve route; remove only the route you created, for example `tailscale serve --bg --https=8443 off`. Do not use `tailscale serve reset` on a host with other configured services.

### Owner CLI, including a locked Mac over SSH

The bundled executable administers the **already-running app** through a protected local Unix socket. It does not launch a window, create another desktop runtime, start OMP, or automatically retry mutations. Run it as the same OS user as the desktop, locally or over your existing SSH connection through Tailscale. The Mac must remain awake and logged in; a locked screen is supported.

From the repository root, with the debug app already running:

```sh
DESKTOP="$PWD/target/debug/bundle/macos/AMP Desktop.app/Contents/MacOS/omp-desktop"
"$DESKTOP" remote status
"$DESKTOP" remote enable --url https://home-mac.your-tailnet.ts.net:8443
"$DESKTOP" remote token
```

Point `DESKTOP` at the installed bundle instead if you moved it. Follow the Serve and phone steps above; the CLI does not configure SSH, Tailscale, or HTTPS itself. Only `token` and `rotate-token` print the raw secret. Protect their terminal output.

Other commands: `remote configure --port 14502`, `remote configure --url https://home-mac.your-tailnet.ts.net:8443`, `remote configure --clear-url`, `remote disable`, `remote rotate-token`, and `remote help`. An empty `configure` is rejected. The displayed Serve command uses the configured origin's HTTPS port.

`--data-dir PATH` selects a running instance with a custom data directory. Otherwise the CLI uses `OMP_DESKTOP_DATA_DIR` or the same app-identifier-based directory as the GUI. A missing host fails with launch guidance rather than silently starting one.

The socket directory is `/private/tmp/omp-desktop-owner-<uid>/` with mode `0700`; socket and lifetime-lock files use `0600`, and both endpoints verify the peer's kernel UID. **Every process running as that OS user is a trusted administrator**, including one that can export or rotate the pairing token. Network bearer tokens do not authorize this socket.

Credentials remain in macOS Keychain, not socket files or JSON preferences. Creating a fresh pairing credential and restarting the same signed app were exercised with the screen locked. If Keychain requires permission after an app/signature change, unlock the Mac and resolve that OS prompt; the CLI never bypasses it or falls back to plaintext storage. Failed quit reopens owner administration before showing its error dialog.

## SMS through PipeStreamr

**Engine settings → SMS** connects a verified phone to a desktop bot through the standalone Rust `pipestreamr-relay` program. The relay has an interactive inbox TUI and a headless hosting mode. It receives public signed HTTPS webhooks; the desktop only makes outbound requests. It never exposes the desktop remote-control API or requires Tailscale Funnel.

1. In PipeStreamr, activate an approved SMS sender, consent/link your mobile using the physical `LINK` text, and note your project UUID and the mobile binding's **integration/source UUID** (not the phone binding UUID).
2. Build and host `pipestreamr-relay` from the PipeStreamr repository's `relay/` package. Its `relay/README.md` covers environment variables, persistent SQLite storage, HTTPS reverse-proxy setup, container deployment, and TUI controls. Configure the expected project, integration, verified phone, selected webhook signing secret, and an independently generated relay bearer token.
3. Create a project webhook pointing to `https://YOUR_RELAY/webhooks/pipestreamr` and explicitly select it for that linked mobile. PipeStreamr rejects private/loopback webhook destinations. The relay needs the signing secret, **not** your model credentials or PipeStreamr reply API key.
4. Create a dedicated desktop bot with its workspace, engine, and model configured. In **Engine settings → SMS**, enter the relay HTTPS base URL, your exact PipeStreamr HTTPS base URL, matching project/source/phone, target bot, relay token, and a PipeStreamr key scoped to `messages:send`. Enable and save. HTTP is permitted only for loopback development; credentials in URLs, redirects, query strings, and fragments are rejected.
5. Keep the Mac awake, logged in, and AMP Desktop running. A fresh live SMS starts the bot as needed. On first dispatch, the connector creates a fresh SMS conversation while preserving the bot's previous conversation, then pins the SMS conversation. The full transcript remains visible in the desktop.

Arrivals predating the current connection, after an observed disconnect/restart, or more than two minutes old require **Accept & send to bot** or **Dismiss** in the SMS inbox. Accept authorizes that pending message only; reconnecting requires review again. Changing the selected bot's conversation pauses SMS with an error until you resume its pinned conversation. Use a separate bot for unrelated desktop work: additional prompts, session changes, or interrupted execution can invalidate reply correlation rather than send the wrong answer to a phone.

Tool approval policy is unchanged. Review approvals in the desktop or private AMP Remote; SMS never approves a write, edit, or shell command. Stop generation and Stop bot remain available. Only final assistant text is returned, at most 1,200 characters including a truncation notice; thinking, tool output, and intermediate messages are not sent. Final text is not a secret scrubber: do not ask the agent to send sensitive material over SMS.

Event claims and pre-submission markers are durable. Repeated webhooks do not replay prompts; replies use `reply-EVENT_UUID-1` as their idempotency key. An interrupted/ambiguous claim, prompt, or SMS submission becomes **Unknown outcome**, never an automatic retry. Inspect the bot transcript and PipeStreamr event/carrier status before deciding what to do. **Completed means PipeStreamr accepted submission, not handset delivery.** Sender approval, opt-out rules, verification expiry, and PipeStreamr budgets still apply.

Disabling SMS stops monitoring, not already-submitted bot work; an interrupted turn/reply can become unknown. Closing the Mac window while SMS is enabled hides it and keeps hosting; explicit Quit stops the connector and agents. No login item, wake service, or always-on agent daemon is installed. An enabled connector reads its Keychain credentials in the background on restart, so an OS permission prompt cannot block window creation, status inspection, or quit.

SMS settings, Accept/Dismiss, and credential administration are local-desktop-only. The authenticated loopback developer bridge supports these methods for development; production remote HTTP rejects every `sms_*` command. Run one intended desktop against a relay inbox: atomic claims prevent duplicate claims, not incorrect bot selection across several differently configured desktops.



## Chat commands

Commands work in both the Mac window and AMP Remote. Type `/` to browse them, use arrow keys and Tab to complete, or tap a suggestion. Submit with Enter on desktop or the Send button on a phone; phone Return still inserts a newline.

| Command | Behavior |
| --- | --- |
| `/new` | Start an empty conversation in the current bot, keeping its workspace, instructions, and model preference. The previous conversation remains saved. |
| `/resume` | Search this bot's saved conversations and select one to restore. |
| `/resume <id>` | Resume an exact ID from that bot's saved-conversation picker. |
| `/model` | Open the existing searchable model picker. A stopped OMP bot starts first to load its available models. |
| `/model <provider>/<model-id>` | Select an exact available model. A bare model ID also works if it is unambiguous; IDs containing `/` are supported. |

Conversation changes and model selection wait for the bot to be idle: stop generation or resolve approvals first. Resuming restores saved history and model state without replaying prompts or tool calls. The picker lists this bot's compatible, app-recorded sessions, not every unrelated OMP session on the Mac. Empty OMP conversations appear after their first durable turn.

Known commands with invalid arguments, unknown/ambiguous model IDs, or attached images report an error and preserve the draft/attachments. They are never sent as model prompts. Other slash text retains the engine's ordinary message handling.

## Bot lifecycle and safety

- Multiple bots can run concurrently. Selecting another bot does not stop previous work.
- Each bot keeps its own composer draft, scroll position, model preference, runtime state, and session reference.
- **Stop generation** aborts a turn. **Stop bot** shuts down its runtime and preserves conversation state.
- Stopping during a native tool approval does not execute the tool; the failed/interrupted tool result remains in the stopped transcript.
- Explicit Quit asks before stopping active work, enabled remote access, or enabled SMS. With both connections disabled, closing the main window follows the same quit path. No login item or always-on daemon is installed.
- A crash or restart never automatically replays a prompt or side-effecting tool call.
- Bots in the same checkout can modify the same files. Use separate workspaces/worktrees when working-copy isolation is needed.

Native write/edit/shell operations require explicit confirmation, including exact targets, diffs, or commands. File tools enforce workspace-path and symlink checks. **Approved shell commands are not sandboxed**: they can access the user's files/network outside the workspace. OMP mode preserves OMP's configured approval policy; it is not forced into auto-approve.

The UI renders Markdown without raw HTML, uses a restricted Tauri CSP/capability surface, and opens only explicitly selected HTTP(S) links. Treat model output and tool content as untrusted.

## Storage

Default macOS application data:

```text
~/Library/Application Support/com.ompdesktop.desktop/
  desktop.json                 # bot/provider/session-catalog metadata; no API keys
  remote.json                  # opt-in listener settings and instance ID; no token
  sms.json                    # opt-in connector settings, conversation pin, durable inbox/reply journal; no keys
  native-sessions/<bot-id>.json # initial Native conversation history
  sessions/<bot-id>/<conversation-id>/ # /new: Native JSON or OMP JSONL history
```

OMP creates and owns its session files. Initial installed-OMP sessions in AMP Desktop use explicit isolated directories under:

```text
~/Library/Application Support/omp-desktop/omp-sessions/<session-directory>/
```

Exact session paths are saved and resumed; `--continue` is not used. Existing missing session files report an error instead of silently replacing the conversation. Removing a bot deletes app metadata only, not its workspace or engine session files.

Native API keys use macOS Keychain service `omp-desktop-native`, keyed by provider ID. Native account grants use the separate `omp-desktop-native-oauth` service, also keyed by provider ID. Remote tokens use `omp-desktop-remote`, keyed by the persisted remote instance ID. `OMP_DESKTOP_DATA_DIR` overrides application metadata/native-session storage for isolated development or smoke runs, not Keychain account isolation.

Keep runtime data and evaluation output outside the source checkout. Git ignores environment files, signing keys, local runtime state, sessions, logs, and known evaluation reports, but ignore rules are not a secret scrubber. Before publishing changes, inspect the staged file list and scan it with `gitleaks git --pre-commit --staged --redact`. Never commit API keys, pairing/relay tokens, saved grants, or private transcripts.

SMS credentials use Keychain service `com.omp.desktop.sms`, accounts `<credentialId>:relayToken` and `<credentialId>:apiKey`, with a random credential ID in `sms.json`. Separate data directories therefore isolate SMS credentials as well as their journals. The settings form never reads saved values; a blank form field keeps the current secret. The local `sms_configure` API can explicitly delete a secret by sending an empty string while disabled. Protect and back up `sms.json`: it contains SMS bodies/final replies and the markers preventing duplicate execution. Never roll back or discard either the desktop journal or relay inbox while processing messages.

## Verification commands

```sh
cargo test -j 1 -p omp-control -p native-agent -p desktop-core -p native-spice
cargo run -p omp-control --bin omp-spike
```

The OMP spike uses real installed-engine inference and may incur provider charges. Its workspace, extension, and sessions are temporary. It checks streaming, model switching/restoration, cancellation, input/select/editor dialogs, explicit extension confirmations with actual allow/deny filesystem effects, and completed-turn persistence after resume. The extension-confirmation probe is not a claim that every built-in OMP approval policy has been tested.

For native real-provider verification, supply real credentials and an appropriate model:

```sh
NATIVE_SPIKE_PROVIDER=openai NATIVE_SPIKE_MODEL=gpt-4.1 \
  cargo run -p native-agent --bin native-spike
```

This exercises actual provider discovery, streamed inference, a real read-tool continuation, and session restart. It fails clearly when credentials are absent. Scripted local HTTP tests verify native protocol/tool behavior; they are not evidence of real-model inference or live Anthropic compatibility.

### Real LLM-in-the-loop design evaluations with Spice

`crates/native-spice` uses the published Rust `spice-framework` 0.2.0 with the actual Native Rust client, streaming tool loop, saved native-provider authentication, and a separate real tool-free model judge. Ordinary `cargo test` runs only offline validators; it does not access credentials or call models.

Opt in to real provider calls, which may incur charges:

```sh
NATIVE_SPICE_MODEL=gpt-5.6-sol NATIVE_SPICE_JUDGE_MODEL=gpt-6.1-sol \
cargo run -p native-spice -- \
  --live --suite fixtures \
  --output /tmp/amp-spice-fixtures-001
```

Choose a **new output directory** each run; its parent must exist. Default actor/judge model: `gpt-6.1-sol`. `NATIVE_SPICE_PROVIDER`, `NATIVE_SPICE_MODEL`, `NATIVE_SPICE_JUDGE_MODEL`, and `NATIVE_SPICE_PROVIDER_CONFIG` select other native settings; `--help` lists the full contract.

The fictional **Harbor Tasks** benchmark uses an independently authored prompt and a synthetic volunteer-team task tracker. The fixture suite covers project discovery plus a source-grounded plan and actual PNG, truthful missing-project handling, denied writes, and workspace/symlink escape rejection. An optional `--suite real` (or `all`) evaluates a checkout you supply as `--workspace DIR`'s `harbor-tasks/` child; the real-case rubric judges only its observed sources, not assumed fixture features. Output inside that checkout is rejected for every suite. All write/edit/shell approvals are denied. Source snapshots and an actual session reopen check verify immutability and persistence. Missing credentials/browser or failed assertions are failures, not skips. Each selected case runs once, concurrency one, without retries or consensus.

The mode-0700 output includes `report.json`, Spice traces, per-case native events, denied approvals, source/persistence evidence, decoded PNGs, and actual judge requests/responses. It contains source excerpts and model output: keep it private. Unrelated discovery entries and known sibling-name repetitions are redacted from retained real-workspace traces; sensitive/out-of-project reads abort the real case. This is not a general-purpose secret scrubber.

A passing sample proves that exercised run, not every future model answer or aesthetic quality. Using the same actor and judge model risks correlated judgments. PNG validation proves image structure/dimensions, not visual quality; inspect the saved previews. To exercise installed-browser network isolation without an LLM call:

```sh
cargo test -p native-agent installed_browser_produces_png_without_page_network -- --ignored
```

### Browser development with the real Rust backend

Two terminals:

```sh
bun run bridge
bun run dev
```

Open `http://127.0.0.1:1420` and enter the per-run developer token printed by the bridge. This is an authenticated loopback development transport, not a production server. The token is stored only in that browser tab's session storage. Host/Origin restrictions remain in addition to authentication. The packaged Tauri app uses IPC and never starts this listener.

Browser mode uses the actual supervisor and engines, not sample bots or a mock backend. Enter workspace paths manually there; the directory picker is native-app-only.

## Upstream Rust code

Native mode reuses OMP's pure Rust `pi-diff` library for actual write/edit approval previews. The unchanged leaf source is vendored under `vendor/pi-diff`, pinned to upstream revision `0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0`; its Git blob is recorded in that crate's manifest and its MIT license is retained. This avoids fetching the entire upstream repository/history during every clean dependency setup.

`pi-natives` is an N-API aggregation library, not the agent harness. This app's native provider orchestration, agent loop, tools, approvals, sessions, and lifecycle are Rust code in `crates/native-agent`. Installed-OMP integration remains separate in `crates/omp-control`; shared bot management lives in `crates/desktop-core`.
