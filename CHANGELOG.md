# Changelog

## Unreleased

- Prepare the standalone public source repository with an MIT license, secret-scanning configuration, and ignore rules for local secrets, runtime data, conversations, build state, and private evaluation evidence.
- Rename the app to AMP Desktop and its phone/browser client to AMP Remote, including native menus, window titles, bundle names, sign-in messages, and Home Screen metadata; preserve existing storage and credential identifiers.
- Use independently authored Harbor Tasks benchmark prompts, synthetic task-tracker fixtures, and source-grounded real-checkout rubrics without private-project context.
- Add opt-in PipeStreamr SMS through the standalone Rust TUI/headless relay: outbound-only desktop polling, Keychain-held credentials, a local settings/inbox tab, pinned conversations, offline review, and bounded final-text replies.
- Persist SMS claims and pre-submission journals to prevent replay after retries or uncertain execution; retain normal approvals, responsive Stop bot, background credential loading, and local-only connection administration.
- Add chat `/new`, `/resume [id]`, and `/model [provider/model-id]` commands to desktop and remote clients, with slash completion, saved-conversation search, exact model resolution, and draft/attachment safety.
- Preserve per-bot conversation catalogs across restarts, restore history/model state without replay, and support both legacy OMP headers and current title-prefixed sessions with macOS workspace aliases.
- Add independent Native OpenAI/Anthropic browser account sign-in, PKCE/state validation, cancellation, Keychain token refresh, disconnect, and an API-key alternative.
- Use the Codex Responses backend for OpenAI subscription inference and preserve signed Anthropic reasoning across tool continuations.
- Add current starter model catalogs with untouched-legacy migration, account-specific discovery, and idle-bot refresh without runtime restart.
- Replace the model dropdown with a searchable, keyboard-accessible picker, provider identity, current-model preservation, and useful empty states.
- Include GPT-6.1 Sol and GPT-6 starters, migrate the previously shipped subscription lists, and discover account models once after successful sign-in.
- Accept valid Responses SSE bodies when Codex omits or mislabels Content-Type, while still rejecting malformed, incomplete, and error responses.
- Route official GPT-6 API-key tool inference through Responses rather than Chat Completions.
- Stop credential reads during status polling; cache OAuth/API-key reads, absence, and denied access, with coherent connect/refresh/disconnect updates to prevent repeated Keychain prompts.
- Keep core credential-state tests isolated from the production Keychain.
- Embed frontend assets in normal Cargo builds by default and add the canonical debug packaging command, preventing bundled apps from depending on a stopped development server.
- Make startup/account badges query only noninteractive Keychain entry metadata; defer secret unlock to explicit provider use so saved accounts cannot block window creation.
- Give Native agents canonical workspace context and a source-first planning workflow; add safe directory reads and breadth-first, generated-tree-pruned project discovery with directory glob matches.
- Add isolated, offline static HTML/CSS-to-PNG design previews, visible tool images, and sanitized fenced-SVG previews; distinguish proposed mockups from live application screenshots.
- Add opt-in Rust Spice evaluations using real Native provider inference and a real tool-free LLM judge, covering grounded design plans/previews, missing projects, denied mutation, workspace boundaries, and a caller-supplied checkout with private evidence artifacts.
- Keep large-project planning within the Native context budget by explicitly shortening older read/search excerpts only in provider context, preserving full saved history, tool pairs, repository guidance, errors/mutation outcomes, and preview images.

## 0.1.0

- Add per-bot selection between installed OMP RPC and an independent native Rust agent engine.
- Add a resizable 25/75 bot sidebar/chat layout, model/thinking selection, Markdown/tool rendering, images, drafts, unread activity, and bot information.
- Add concurrent runtime supervision, explicit cancellation/shutdown, persisted bot definitions, and exact session recovery without prompt replay.
- Add OMP discovery, Finder PATH handling, protocol-v2 framing, installation guidance, and independent native-mode operation when OMP is unavailable.
- Add native OpenAI-compatible/Anthropic streaming, model discovery, environment/Keychain credentials, workspace file/search/edit tools, shell execution, and explicit write/edit/shell approvals.
- Reuse pinned upstream pi-diff source for native change previews; retain upstream provenance and license.
- Guard provider configuration/credential changes against live clients, coalesce streaming updates, preserve whole tool groups in estimated context budgeting, and retain final interrupted-tool results on stop.
- Add real-engine spike commands, protocol/lifecycle regressions, and an authenticated loopback browser-development bridge.
- Add explicit opt-in, Keychain-paired loopback remote control of the same live desktop, with Tailscale Serve HTTPS guidance, strict request origins, token rotation, and local-only pairing controls.
- Add a focused mobile OMP Remote web client with Home Screen metadata, host-workspace semantics, disconnect isolation, and foreground-aware polling.
- Keep hosting when the Mac window is closed; restore through the Dock, route native Quit through agent cleanup, and preserve remote administration after a failed quit.
- Bound transport teardown against stalled response readers while draining accepted operations; add authentication, revocation, traversal, lifecycle, and shutdown regressions.
- Add low-memory build profiles and local ad-hoc bundle signing.
- Add owner-only CLI pairing/configuration through a permission-checked Unix socket, including locked-host SSH setup without launching another desktop or OMP runtime.
- Gate delayed credential/listener startup during quit, reopen owner administration before shutdown-error dialogs, and derive Serve HTTPS guidance from the configured origin.
