# dsh-rs

A Rust agent harness inspired by DeepSeek Harness (dsh): backend core in Rust,
web console in React.

## Status

- **Phase 1 — model configuration**: OpenAI-compatible provider registry,
  layered settings with revision OCC, credential chain (`env > file > .env`),
  model catalog, default-model selection, chat smoke endpoint.
- **Phase 2 — harness core**: event-sourced sessions (JSONL, torn-tail
  repair, orphan-turn close), agent loop (turn/step, tool continuation,
  cancellation, step limit), tools (`bash`, `read_file`, `write_file`, all
  auto-executing this phase), session REST + SSE follow API, session-centric
  console with sandbox toggle.

## Layout

```
crates/
  core/         shared domain types (StreamChunk, SessionEvent, derive_messages)
  settings/     namespaced YAML settings store (layered resolve, OCC, redaction)
  credentials/  credential store (env > file > .env resolution chain)
  llm/          provider adapter registry + OpenAI-compatible adapters
  tools/        bash / read_file / write_file
  session/      JSONL session storage
  agent-loop/   the session driver (turn/step loop)
  server/       axum HTTP API + SSE push + embedded console (bin: dsh-rs)
web/            React + Vite + TypeScript console (sessions + models)
scripts/        mock-gateway.mjs (stateful OpenAI-compatible test gateway)
```

## Run

One-shot build (kills any running `dsh-rs`, builds the console, release-builds
and installs the global command):

```sh
scripts/build.sh          # build + install
scripts/build.sh --run    # build + install + start
```

Then, from anywhere:

```sh
dsh-rs                 # API + console on http://127.0.0.1:3600
dsh-rs --port 4000     # custom port
dsh-rs --web web/dist  # serve a console directory instead of the embedded one
```

Development with hot reload:

```sh
cargo run -p dshrs-server        # debug builds read web/dist from disk
cd web && pnpm dev               # vite dev server on :5173 proxying /api
```

## Sessions

Sessions are append-only JSONL event logs under `$DSH_RS_HOME/sessions/<id>/`.
By default a session works inside the sandbox (`$DSH_RS_HOME/workspace`);
creating one with `sandbox: false` + a `cwd` opts into a real directory.
Tools (`bash`, `read_file`, `write_file`) auto-execute this phase.

Key API:

```
GET    /api/sessions                 list
POST   /api/sessions {sandbox,cwd?}  create
GET    /api/sessions/:id             cold history {header, events}
DELETE /api/sessions/:id
POST   /api/sessions/:id/prompt      start a turn (202)
POST   /api/sessions/:id/cancel
GET    /api/sessions/:id/follow?after=N   SSE: replay then live
```

## Test gateway

`scripts/mock-gateway.mjs` is a stateful OpenAI-compatible gateway on :8788:
first request answers with a `bash` tool call, requests carrying a tool
result get text. Register it as a provider (`llm-openai` section,
`baseURL: http://127.0.0.1:8788/v1`) to exercise the whole loop keyless.

## Home

State lives in `$DSH_RS_HOME` (default `~/.dsh-rs`):

- `settings.yaml` — namespaced configuration (agent-default-model, llm-openai)
- `.credentials.yaml` — stored API keys (0600), resolved after process env
