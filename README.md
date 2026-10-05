# Denia

Rust agent runtime with a React console and an optional Tauri desktop host.

## Architecture

- `core`: domain records, JSON wire types, event projection.
- `agent-loop`: turn/step orchestration, provider retries, tool dispatch and compaction.
- `llm`, `tools`, `browser`, `mcp`, `terminal`: capability adapters.
- `session`: JSONL storage, recovery, history, projections and indexing in separate modules.
- `settings`, `credentials`: layered configuration and credential resolution.
- `server/application`: prompt admission and business orchestration.
- `server/infrastructure`: shared filesystem adapters.
- `server/api`: HTTP parameter and response translation.
- `server/state` and `host`: composition, resource lifecycle and listener setup.
- `desktop`: Tauri host using the same server startup path.
- `web/src/features/conversation`: conversation state helpers and composer hooks.

Both listeners use the same authentication gate. Local clients retain direct access;
remote clients need the configured remote session. Model tools follow the session's
permission mode: read-only, auto-edit, plan or full.

## Development

`cargo run -p denia-server` serves the console at http://127.0.0.1:3600.
Run `pnpm dev` inside `web` for the Vite development server.

State lives under `DENIA_HOME`, defaulting to `~/.denia`. A running host holds an
OS lock on its data directory. Use a separate `--home` / `DESKTOP_HOME` when
running an additional development or desktop instance; multiple browser clients
can use one server normally.

Console compaction, microcompaction and tool concurrency settings are resolved
when each turn begins. An active turn keeps a consistent configuration snapshot;
the next turn reads saved changes immediately. Compaction circuit breakers belong
to their session and are released when that session leaves the runtime cache.
Settings writes publish their value and revision only after persistence succeeds.

Sessions are append-only JSONL logs under `sessions/<id>/session.jsonl`. Loading
repairs partial tails and closes orphaned turns. After an I/O failure the log
writer stops; reloading repairs any partial tail before further writes.

## Build and verification

- `pwsh scripts/check.ps1`: Rust format, Clippy correctness/suspicious checks,
  generated protocol freshness, all Rust tests, frontend tests/types and build.
- `pwsh scripts/check.ps1 -SkipBuild`: the same checks without the Vite build.
- `pnpm check` in `web`: every frontend regression suite, including memory tests.
- `pnpm typecheck`: TypeScript verification.
- `pnpm build`: the same frontend test inventory, type checking and Vite build.
- `scripts/build.ps1 -NoRun` or `scripts/build.sh`: install the CLI.
- `scripts/desktop.ps1 -BuildOnly`: build the desktop app.

Windows CI uses the same verification script. Browser/network integration tests
that require external dependencies remain explicitly ignored by default.

Shared event and request types come from Rust/serde. Regenerate after changing
those records with:

`cargo run -p denia-core --features bindings --example bindings`

The bindings feature is optional and adds no dependency to ordinary core builds.
The generator's `--check` mode verifies committed TypeScript is current.

## Test gateway

`node scripts/mock-gateway.mjs` starts a local OpenAI-compatible gateway on
port 8788. Register its `http://127.0.0.1:8788/v1` endpoint to exercise the agent
loop without a provider key.
