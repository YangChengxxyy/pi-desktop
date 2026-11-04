# pi-desktop

A desktop client for [pi](https://pi.dev), written in Rust with GPUI + gpui-kit. The agent itself runs in a local `pi --mode rpc` child process; this app only provides the interface. It is a port of omp-desktop with the same layout and styling, rebuilt on pi's RPC protocol.

## Requirements

- pi ≥ 1.5.0 (`curl -fsSL https://pi.dev/install.sh | sh`, or `npm install -g @earendil-works/pi-coding-agent`). The app uses the first `pi` on your login shell's PATH, then `~/.pi/agent/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`, `~/bin`.
- Rust 1.88+ (edition 2024)

## Run

```bash
cargo run -p pi-desktop                      # session in the current directory
cargo run -p pi-desktop -- --cwd ~/work/repo # session in another directory
cargo run -p pi-desktop -- --session-dir /tmp/s # store and list sessions there
```

Each tab runs its own pi process. The sidebar lists saved sessions from pi's session directory (`~/.pi/agent/sessions`, or `PI_CODING_AGENT_SESSION_DIR` / `sessionDir` in settings) grouped by directory, including ones started in the TUI; the title is the session name, else its first message. Click a session to open it; right-click for 分支 (continue in a copy, `pi --fork`) and 在 Finder 中显示.

## macOS app

```bash
scripts/bundle-macos.sh             # target/bundle/pi-desktop.app + pi-desktop-<ver>-<arch>.dmg, ad-hoc signed
scripts/bundle-macos.sh --no-dmg    # .app only
scripts/bundle-macos.sh --sign "Developer ID Application: Name (TEAMID)" --notarize <profile>
```

The ad-hoc build runs on this machine; on another Mac, Gatekeeper blocks it until it is opened with right-click → 打开 (or `xattr -dr com.apple.quarantine pi-desktop.app`). A distributable build needs a Developer ID certificate and a notarytool profile (`xcrun notarytool store-credentials <profile> --apple-id … --team-id … --password <app-specific password>`). pi is not bundled. Launched from Finder, the app takes PATH from `$SHELL -l -i` and starts the first tab in the directory named in `~/Library/Application Support/pi-desktop/default-cwd` (one path, `~/` allowed), or the home directory when that file is missing or the path is not a directory.

## Shortcuts

| Keys | Action |
| --- | --- |
| ⌘N / ⌘O / ⌘W | New tab / new tab in a chosen folder / close tab (stops its pi process) |
| ⌘1–9 | Switch tab |
| ⌘P | Search saved sessions |
| ⌘B | Toggle the session sidebar |
| ⌘J | Toggle the status panel (context, cost, extension status, session state) |
| ⌘K | Command palette: actions, models, thinking levels, slash commands, theme |
| ⌘M | Pick a model for this tab |
| ⌘T | Cycle the thinking level |
| ⌘⇧L | Accounts: log in to or out of a provider |
| Enter / Shift+Enter | Send / newline |
| Enter while running | Queue a follow-up (delivered when the run ends) |
| ⌘Enter while running | Queue a steer (delivered after the current tool calls) |
| Esc | Abort the run (empty composer); queued messages return to the composer |
| `!cmd` / `!!cmd` | Run a shell command; its output joins the next prompt's context (`!!`: it does not) |
| `/` at line start | Slash-command completion: ↑↓ select, Tab/Enter complete, Esc dismiss |
| ⌘V / drag files | Attach images |

Queued messages appear as chips above the composer: × withdraws one, 插队 promotes a follow-up to a steer.

## What it does

- Streams replies from pi's delta-only `message_update` records; `message_end` is authoritative.
- Renders Markdown text and collapsible thinking. Each tool call is one card: `bash` streams its output with elapsed time, `edit` (and extension tools that return `details.diff`) shows a line-numbered diff with +/- counts, `read` collapses to a one-line summary; failures are red and long output folds. Compaction and branch summaries fold too.
- Answers pi's extension dialogs: `select`, `confirm`, `input`, `editor`, with the countdown when pi set a timeout. Extensions' `notify`, `setStatus` (status bar and panel), `setWidget` (above or below the composer), `setTitle` (tab title), and `set_editor_text` work.
- Slash commands: pi's extension commands, prompt templates, and skills (`get_commands`), plus desktop commands for TUI built-ins RPC mode lacks: `/compact [说明]`, `/new`, `/clone`, `/name`, `/export [路径]`, `/copy`, `/session`.
- Status bar: connection state, cwd, extension statuses, model and thinking menus, context usage, compaction, session cost; ⌘J opens the panel with every `get_state` field.
- Accounts panel (title bar 账号, ⌘⇧L): every provider pi knows, including ones your installed extensions register, OAuth login (opens the browser, copyable link or device code, paste-the-code fallback, method choice, cancel) and API-key login (masked), and logout of credentials stored in `auth.json`. A stored credential whose provider is no longer loaded (its extension was removed) is listed as 未加载 and can only be removed, as in the TUI's `/logout`. It runs its own pi process with a small bridge extension (`crates/pi-bridge/extension/accounts.ts`), since pi's RPC mode has no login commands. A tab without a usable model offers it and reconnects after a login.
- Theme: follow the system (live) or force light/dark from the title bar 主题 menu or ⌘K; saved in `~/Library/Application Support/pi-desktop/theme`.
- Startup errors (pi missing, too old, no usable model) and process exits show a reconnect action; a session already on disk reconnects with `--session`.

Differences from omp-desktop follow from pi: pi has no subagent, todo, or approval protocol (its tools run without approval unless an extension asks), so those panels are gone; extension statuses take their place.

## Layout

| Path | Role |
| --- | --- |
| `crates/pi-rpc` | Wire types and a blocking JSONL client for `pi --mode rpc` (LF-only framing, id correlation, process-group teardown) |
| `crates/pi-bridge` | Spawns pi, checks its version, waits for startup, exposes an async record stream; holds the accounts bridge extension |
| `crates/pi-store` | Rebuilds messages from records; projects them to renderable blocks; folds tool runs; lists saved sessions |
| `crates/pi-desktop` | The GPUI app; `assets/` holds the icon and the Info.plist template |
| `scripts/` | `bundle-macos.sh` (.app/.dmg, signing, notarization), `make-icon.swift` |
| `docs/` | Technical plan and UI design (`docs/design/`, shared with omp-desktop) |

## Tests

```bash
cargo test --workspace                                   # offline unit tests
cargo test -p pi-desktop -- --ignored --test-threads 1  # headless UI against real pi
```

The ignored UI tests run the real views in a headless window against real pi, each in its own temp directory. Model-free: confirm/input/editor/select dialogs and a timed-out select from the fixture extension `tests/fixtures/dialogs.ts`; extension status and widgets; slash completion and running an extension command; `!cmd` streaming and Esc stopping it; the status-bar menus, auto-compact switch, compact, and the panel; the no-credentials screen opening the accounts panel; API-key login, OAuth up to the browser link and cancel, and logout, in a scratch `PI_CODING_AGENT_DIR`; tab shortcuts; ⌘K and ⌘M. With a model call: a streamed reply; bash and edit cards; queueing, withdrawing, IME-safe Esc, and abort while `sleep 60` runs; a chip promoting a follow-up to a steer and ⌘Enter queuing a steer; a pasted image the model describes; and a `pi -p` session that is listed, opened, continued, reconnected after `kill -9`, forked, and closed.
