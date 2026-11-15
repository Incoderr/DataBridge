# AGENTS.md — DataBridge

Guidance for AI agents working in this repository. Read before making changes.

## What this is

DataBridge is a desktop database explorer built with **GPUI**
(Zed's UI framework) and **gpui-component**. The web frontend (Tauri + React) was
removed; the GPUI Rust app is the only product code in this repo.

- Rust edition 2024, GPUI Kit 0.9: `gpui-component = "0.9.1"` on the
  `gpui-pre =0.3.12` snapshot (renamed to `gpui` in Cargo.toml, so code
  imports `gpui::`), `gpui_platform` (`gpui-pre-platform`, starts the app:
  `gpui_platform::application()`), `gpui-kit-assets` for the icon fallback
- Package `databridge-gpui`, binary `databridge`
- Repo layout:
  - `/src` — the application (layered tree below; `core::session` / `registry` /
    `runtime` and the drivers land in stages 1–2)
  - `/assets` — fonts (embedded with `include_bytes!`) and SVG icons
  - `/locales/app.yml` — UI translations (rust-i18n, compiled in; en + ru)
  - `/site` — Astro landing page. Separate Node toolchain (pnpm), own `AGENTS.md`,
    deployed to Cloudflare Pages (build root directory `site`, output `site/dist`).
    Do not mix toolchains: the repo root is **cargo-only**,
    Rust changes must never touch `site/` and vice versa.

## Commands

```
cargo run                             # run the app (real connections, Stage-3 store)
cargo run -- --mock                   # demo mode: seeded connections on the mock driver
cargo check                           # fast type check
cargo check --no-default-features     # must compile: mock-only build (verify feature isolation)
cargo test                            # core unit tests (storage mapping, URL sanitize)
cargo build --release
```

CI: `.github/workflows/windows.yml` (windows-latest) runs `cargo test`,
`cargo check --no-default-features` and `cargo build --release` on pushes
to main and PRs (not for `site/**` / docs-only changes) and keeps the
zipped exe as an artifact; a pushed `v*` tag also publishes it as a GitHub
Release. Release builds on Windows use the GUI subsystem (no console —
`windows_subsystem` in `main.rs`), debug builds keep the console.

Tests live in `core` (`storage`: URL sanitizing, JSON round-trip). More
belong there (model mapping, mock determinism); driver integration tests
require a live DB and must be `#[ignore]`d.

## Architecture (decided — do not deviate without asking)

Two planes plus a bridge:

1. **`core`** — driver-agnostic domain. No GPUI view code, no driver crates.
2. **`drivers`** — per-database implementations of one async trait, behind cargo features.
3. **`ui`** — GPUI views. Imports `core` only, **never** `drivers` directly.

```
src/
  main.rs
  core/
    model.rs      # owned domain types: DbConnection, DbType, TableMeta (Arc), Value, Row, Page
    error.rs      # DbError: user-facing message + source chain
    session.rs    # DbSession trait (async), SessionHandle wrapper
    registry.rs   # DbType -> session factory (respects cargo features)
    runtime.rs    # tokio sidecar + bridge into GPUI tasks
    mock.rs       # deterministic mock (the original mock layer, ported from the web app)
    storage.rs    # Stage-3 recent-connections store: connections.json + OS keyring secrets
    completion.rs # SQL autocompletion engine (pure: text + cursor + schema tree -> suggestions)
    ai.rs         # Stage 6: OpenAI-compatible chat client (SSE streaming), providers + presets in ai.json, keys in keyring, schema prompt
  drivers/
    postgres.rs   # sqlx, feature "postgres"
    mysql.rs      # sqlx, feature "mysql"
    sqlite.rs     # sqlx, feature "sqlite"
    mongo/        # own light client, always compiled: mod.rs (DbSession), wire.rs (OP_MSG + TLS),
                  #   scram.rs (SCRAM-SHA-256/1), uri.rs (mongodb[+srv]:// parse), dns.rs (SRV/TXT),
                  #   shell.rs (mongosh subset)
    ssh.rs        # SSH tunnels (russh, always compiled): TunneledSession wraps any driver
    redis.rs      # feature "redis"   (stage 4 — later)
  ui/
    app.rs        # root entity DataBridge: connections, tabs, wizard state
    connect.rs    # connection wizard renders (impl DataBridge)
    sidebar.rs    # sidebar chrome
    nav_tree.rs   # explorer tree (virtualized: flat row model + v_virtual_list + overlay Scrollbar)
    workspace.rs  # tab model + records grid (RecordsView entity, RecordsDelegate) + the simplified record editor modal
    panes.rs      # workspace tabs as dock panels (WorkspacePanel in a gpui-component DockArea)
    workspace/documents.rs # document stores: JSON cards, field tree, whole-document editor
    query.rs      # SQL editor tab (Stage 5): `Editor`/`EditorState` (sql), run/stop button, results grid
    completion.rs # CompletionProvider adapter: core::completion -> the code editor's menu
    ai_chat.rs    # right-hand AI chat panel (AiChatView entity), toggled from the left rail
    theme.rs      # dark/light palettes, Palette struct, MONO_FONT
    settings.rs   # UiScale global
    assets.rs     # AssetSource: brand icons, extra lucide icons, fallback to gpui-kit-assets
```

### Hard rules

- **UI imports `core` only.** Drivers are reachable exclusively through
  `registry`/`SessionHandle`. Drivers never import `ui`.
- **All DB access flows through `DbSession`** — including the mock. The mock is a
  permanent driver (dev/demo mode without a live DB), always compiled in.
- **Owned data only.** `Arc<TableMeta>` everywhere in app state. `&'static` is
  acceptable only for literal assets (font bytes, SVG path strings).
- **One async world for drivers: tokio sidecar** (details below).
- **sqlx per-dialect pools** (`PgPool`/`MySqlPool`/`SqlitePool`) in a small internal
  enum — do **not** use `sqlx::Any` (weak type mapping, especially SQLite).
  Map every dialect row into `core::model::Value`; extend the `Value` kinds
  (`Date`, `Json`, `Bytes`, …) as real drivers need them.
- Never format user input into SQL. Values go through query parameters; identifiers
  (schema/table names) must be validated against introspected names and quoted
  per dialect.

### Async model — tokio sidecar

`sqlx` and (later) the `mongodb`/`redis` crates need tokio; GPUI has its own
executor. The bridge:

- `core::runtime::init` (called once in `main`) starts a multi-thread
  `tokio::runtime::Runtime` on a dedicated thread, parks it forever, and stores
  the process-wide `Handle` in a static.
- UI code calls `SessionHandle` methods (`list_schemas`, `fetch_rows`, `ping`),
  which `core::runtime::spawn` onto the sidecar → returns a
  `tokio::sync::oneshot::Receiver<T>`.
- The GPUI side awaits that receiver inside `cx.spawn(async …)`, then
  `this.update(...)` applies the result.
- `tokio::sync::{oneshot, mpsc}` receivers are runtime-agnostic — safe to await in
  GPUI tasks. Tokio timers (`timeout`, `sleep`, `interval`) are **not**: schedule
  them on the tokio side only.
- **Stale-guard pattern is mandatory** for every async load: bump a generation
  counter (`load_id`) before spawning; re-check it before applying results.
  The existing implementation in `workspace.rs` (`RecordsDelegate::load`) is the
  reference — preserve it during refactors.

### DbSession contract

```rust
#[async_trait::async_trait]
pub trait DbSession: Send + Sync {
    async fn ping(&self) -> Result<Ping, DbError>;                    // status + measured latency
    async fn list_schemas(&self) -> Result<Vec<SchemaMeta>, DbError>; // schemas w/ tables/views/functions
    async fn list_relations(&self, schema: &str) -> Result<Vec<Relation>, DbError>; // FK edges → diagram
    async fn fetch_rows(&self, table: TableRef, query: RowQuery, offset: u64, limit: u64) -> Result<Page, DbError>;
    // RowQuery = { filters: Vec<Filter{column, kind, type_, op: FilterOp, value}>, sort: Option<(column, desc)> }
    //   SQL drivers build WHERE / ORDER BY with `SqlDialect::clauses` (values bound, identifiers quoted;
    //   PG casts text values to the column's introspected type); Mongo → `$and` find filter + sort.
    // Page = { rows, total, exact (false = estimate), truncated, elapsed_ms,
    //          columns: Option<Vec<ColumnMeta>> (schemaless drivers report discovered columns),
    //          documents: Option<Vec<DocNode>> (document stores: full documents, parallel to rows) }
    // later: async fn list_tables(&self, schema: &str) -> Result<Vec<TableMeta>, DbError>
    //        (lazy per-schema trees when eager loading proves expensive)
    // stage 5 (done): one SQL statement per call. `cancel` (Stop button) goes through
    // `session::cancellable`: server-side kill where the driver can, else the call is dropped.
    async fn execute(&self, sql: &str, limit: u64, cancel: &CancelToken) -> Result<QueryPage, DbError>;
    // simplified editor: typed writes (CellChange carries the kind so NULL binds typed).
    async fn update_row(&self, table: TableRef, pk: Vec<CellChange>, changes: Vec<CellChange>) -> Result<u64, DbError>;
    async fn insert_row(&self, table: TableRef, values: Vec<CellChange>) -> Result<u64, DbError>;
    async fn delete_row(&self, table: TableRef, pk: Vec<CellChange>) -> Result<u64, DbError>;
    // document stores: replace (pk = _id) or insert (empty pk) a whole document from mongosh text.
    async fn save_document(&self, table: TableRef, pk: Vec<CellChange>, text: String) -> Result<u64, DbError>;
    // tree context menu: DROP TABLE / DROP VIEW (`NodeKind::drop_keyword`).
    async fn drop_table(&self, table: TableRef) -> Result<(), DbError>;
}
```

The write methods have default `Err` bodies — a driver that cannot mutate
reports "does not support …" instead of panicking. `QueryPage = { columns,
rows, rows_affected, truncated, elapsed_ms }`: SELECT-like statements
(`is_select_like` first-keyword sniff) return columns+rows, everything else
reports `rows_affected`.

`TableRef = { schema, name, kind }` identifies a result set; `Page =
{ rows, total, truncated, elapsed_ms }` is one fetched page; `Ping = { status,
latency_ms }`; `Relation = { from_table, from_column, to_table, to_column }`
is a same-schema FK edge. The UI never implements the trait — it holds a
cloneable `SessionHandle` (all calls go through `core::registry::open`).

### Cargo features

```
default = ["postgres", "mysql", "sqlite"]
```

MongoDB has **no** feature: it is our own client on top of `bson` (all
its other crates are already in the tree via sqlx/reqwest), so it is
always compiled in. A `redis` feature is declared only when implemented
(stage 4); its `DbType` variant already exists as a stub (see below). The mock is
always available; `cargo check --no-default-features` must always pass
(mock-only demo build).

## Current state (read this first)

Module-level detail lives in each file's `//!` doc — this section keeps
only what works, what is a stub, and the non-obvious behaviors.

**Works for real (sqlx):** PostgreSQL, MySQL, MariaDB (served by the MySQL
driver — same wire protocol, `mariadb://` URLs accepted), SQLite. TLS via
rustls (`sslmode`/`ssl-mode` default to *prefer*). Lazy pools; `ping` =
connect + `SELECT 1` (the wizard's Test Connection); batched introspection
(SQLite: one `pragma_table_info` join; PG/MySQL: the four catalog queries
per schema via `try_join!`, schemas concurrently via `JoinSet`, bounded
by the 4-connection pool); paged `SELECT *` with dialect-quoted
identifiers. Totals (`model::total_without_count`): a short page settles
the total; otherwise the planner estimate (PG `reltuples`, MySQL
`TABLE_ROWS`, fetched in parallel with the page) stands in for tables ≥
`ESTIMATE_THRESHOLD` (100k) — `Page::exact = false`, the grid shows
`~N` and keeps "next" enabled while pages come back full; only smaller
tables pay for an exact `COUNT(*)`.

**Works for real (own client):** MongoDB 3.6+ (`drivers/mongo/`, no
official `mongodb` crate — it pulls ~200 crates). `OP_MSG` over tokio TCP,
rustls (webpki + OS roots; `tls=true`, `tlsAllowInvalidCertificates`),
SCRAM-SHA-256/SHA-1 negotiated via `saslSupportedMechs`, seed lists
(first reachable host, one hop to the reported primary), a 3-connection
lazy pool (`slot()`), reads retried once after a transport error.
Databases → schemas (admin/config/local hidden unless in the URL),
collections/views → tables/views. The tree lists names only (all
databases concurrently, no sampling) — collections arrive with **no
columns**; `fetch_rows` infers them from the page's documents (`_id`
first, majority BSON type as `type_`), widens them with keys later pages
bring, caches them per collection, and always returns them in
`Page::columns`. The grid adopts them (`RecordsDelegate::set_columns`)
and `DataBridge::patch_table_meta` swaps the new `Arc<TableMeta>` into
the tree and tabs — so the AI prompt / completion know a collection's
fields only after it was opened. Rows are projected onto those columns;
count (metadata) and page run in parallel. Each page also carries the
full documents (`Page::documents`, `model::DocNode` — leaves are mongosh
literals). The records view (`ui/workspace/documents.rs`) then offers a
**JSON view** (toolbar `{ }`, default for document stores: one card per
document, collapsible field tree, click selects, double-click edits;
virtualized — gpui `list()` + `ListState` (`doc_list`), reset by
`sync_doc_list` when the page's `Arc<Vec<DocNode>>` changes, a card
re-measured via `splice` when one of its tree paths toggles; the page's
documents are shared as `Arc`, never cloned per frame), the
same tree in the detail panel, and a **whole-document editor**: Edit /
New / Duplicate open `format_document` text in a JavaScript code editor,
saved via `DbSession::save_document` (replace by `_id`, or insert). The
round trip is exact per BSON type (`documents_round_trip_through_the_editor`;
only a regex's `\/` comes back as `/`). `--mock` serves MongoDB
connections the Postgres demo tables with demo documents. Nested values show as
relaxed Extended JSON and parse back on edit (`cell_to_bson` by type
label: ObjectId/long/decimal/date survive). The query tab speaks a mongosh
subset (`shell.rs`: `use`, `{cmd}`, `db.c.find(…).sort().limit()`,
aggregate, count/distinct, insert/update/delete, indexes, drop…).
`mongodb+srv://` (Atlas) resolves per the seed-list spec on every connect
(`dns.rs`, no resolver crate): Windows via the system `DnsQuery_W`
(`windows-sys` Dns feature), elsewhere a minimal UDP/TCP DNS client over
`/etc/resolv.conf` nameservers (that path is not compiled on Windows CI —
check it on Linux/macOS when touching it). SRV hosts must sit under the
SRV name's parent domain; TXT may set only `authSource`/`replicaSet`/
`loadBalanced`; TLS defaults on. Not supported: x.509/LDAP/Kerberos,
compression, relations (diagram is empty).

**SSH tunnels** (`drivers/ssh.rs`, russh with ring crypto): a connection
with `DbConnection::ssh` (`SshConfig`: host, port, user, `SshAuth::{Password,
Key, Agent}`, key path; the password / passphrase is keyring-only) gets a
`TunneledSession` from `registry::open` (not for SQLite or `--mock`). On
the first call it connects + authenticates, probes one `direct-tcpip`
channel to the DB host (so "bastion cannot reach the DB" is an SSH
error), listens on `127.0.0.1:0` and builds the real driver for the
rewritten connection (`model::rewrite_url_host` for URL mode) — drivers
know nothing about SSH. Every pooled DB connection is its own channel over
one SSH session; a dropped session reconnects on the next new channel;
keepalive 30 s. Agent = Windows OpenSSH agent pipe, then Pageant;
`SSH_AUTH_SOCK` elsewhere. Host keys: `~/.ssh/known_hosts` (shared with
OpenSSH), unknown hosts recorded on first use (accept-new), a changed key
is refused. `TargetKey::via` keeps `localhost:5432` behind two bastions
apart. Not supported: jump hosts / `~/.ssh/config`, keyboard-interactive,
seed lists and `+srv` URLs; TLS that verifies the server name sees
`127.0.0.1`. Tested end-to-end against an in-process russh server
(`tunnel_forwards_and_checks_host_keys`).

**Stubs:** Redis, ClickHouse, Cassandra, ScyllaDB exist in
`DbType` (brand icons, ports, URL placeholders) but have no driver.
`registry::open` returns a "not supported in this build" error for them
(and for drivers compiled out) — never the mock: fake demo tables must not
pass for the user's database. `registry::is_supported` mirrors that: the
wizard's Choose screen lists every type and dims the unsupported ones. Only `--mock` serves every type from
`core/mock.rs` (deterministic md5-seeded rows, simulated latency, seeded
wizard list).

**Behaviors worth knowing:**
- Recents (`core/storage.rs`): `connections.json` in the platform config
  dir, newest first, capped at 20; passwords / full URLs / SSH secrets
  only in the OS keyring (service "DataBridge"; suffixes `""`, `::url`,
  `::ssh`). Deduplicated by `model::TargetKey` —
  the same target under a new id replaces the old entry and its secrets;
  old duplicates self-heal on load.
- Wizard (`ui/connect.rs`): SQLite files connect via OS drag & drop
  (drop zone on the shared `window-root` shell in `app.rs`), Browse…
  (`rfd`), or New… (save dialog → empty file → connect; existing files
  are never truncated).
- Tabs: `TabKind::{Records, Diagram, Sql(seed)}`, shown as panels of
  one GPUI Kit `DockArea` (`ui/panes.rs`, `WorkspacePanel` wraps the
  view): drag a tab by its title to split the workspace left / right /
  top / bottom, IDE-style. `DataBridge` still owns the tab list and the
  views (`views`, `panels` by tab id; created once per tab — diagrams no
  longer remount); the dock only arranges them. Panels report back
  deferred (they are called inside the dock's update): `set_active` →
  `tab_activated` (`active_tab` / `last_active`: tree highlight, AI
  context, Refresh; new tabs join the last active panel's group via
  `move_panel`), `on_removed` → `tab_closed`. The dock refuses to close
  the last panel from its tab, so a lone tab draws its own × inside its
  tab title (`DataBridge::sync_lone_tab` flags it; its `tab_name` turns
  `None` so the TabBar renders `WorkspacePanel::title` — the skin's
  close-button look, `close_tab_id` → `DockArea::remove_panel`); the other tabs
  show the skin's close buttons (`set_close_button_visible`, off by
  default). `PanelStyle::TabBar`: a one-tab group still draws a tab bar —
  the default plain title offers no tab to drag back and hides Close in
  its "…" menu. New SQL editors open from the records toolbar and the empty
  workspace, not the tab bar. The dock is created lazily
  (`ensure_dock`) and dropped with the connection (`clear_tabs`); layouts
  are not persisted yet.
- SQL editor (`ui/query.rs`): one statement per Run, stale-guarded by
  `run_id`; Ctrl+Enter runs (`RunQuery`, bound in `query::init` to
  `SqlEditor > Input` after gpui-component's init, so other inputs keep
  Ctrl+Enter = newline). While running, Run becomes Stop: a
  `CancelToken` → `pg_cancel_backend(pid)` / `KILL QUERY id` (the
  statement runs on a dedicated pool connection whose pid / id is read
  first), then `CANCEL_GRACE` (3 s) before the call is dropped; SQLite
  (no `sqlite3_interrupt` in sqlx) and Mongo just drop the call. Mongo's
  `run` takes a connection out of its slot for the round trip, so a
  dropped call never returns a half-read socket to the pool. The AI
  chat's Stop cancels its statement the same way. `--mock` honours
  `sleep(n)` / `pg_sleep(n)` so Stop is demoable. A result set or an error opens a results pane under the
  editor behind a draggable divider (`v_resizable`, state in
  `QueryView::split`); otherwise the editor fills the tab and DDL/DML
  report in the toolbar status. `model::is_select_like` (first keyword after comments, plus
  DML `RETURNING`) picks fetch vs execute. "New table" (tree row per
  schema + the empty workspace) seeds a dialect `CREATE TABLE`; DDL runs
  reload the tree via `DataBridge::reload_schemas` (stale-guarded by
  `schemas_id`).
- SQL autocompletion (`core/completion.rs`, DataGrip-style, tested in
  core): tables after FROM/JOIN/INTO/UPDATE/TABLE, columns after
  `alias.` (aliases resolved from the whole statement, so `SELECT c.|
  FROM customers c` works), in-scope columns + functions + keywords
  elsewhere, statement keywords at a statement start; prefix → word-part
  → first-letter-anchored subsequence ranking; identifiers quoted per
  dialect; inside CREATE/ALTER only types/constraints (+ tables after
  REFERENCES, columns in `INTO t (` / `REFERENCES t (`). `predict` gives
  the grey ghost "next step" (`FROM` after `SELECT *`, `ON a.fk = b.pk`
  guessed from `<table>_id` names, the INSERT column list, …; Tab
  or → accepts — → via `AcceptGhostOrRight`: an open completion menu
  first (→ = Enter on its selected item), else the ghost. gpui-component
  exposes neither, so the provider records where the menu opened
  (`completion::MenuSlot`) and the ghost it offered (`GhostSlot`); each
  counts only while text and cursor are unchanged, Escape forgets both). Menu items carry no `detail` — the gpui-component popover is
  fixed at 120 px min and doesn't grow — descriptions go to
  `documentation` (side popover). Editor: 4-space indent, Enter after `(`
  indents (the editor's own smart indent — structural brackets — since
  GPUI Kit 0.9), Ctrl+wheel font zoom
  (`settings::EditorFontSize`, captured by a canvas before the editor
  scrolls). `ui/completion.rs` reads `DataBridge::schemas` per request —
  attach the provider only after `set_value` (it fires the trigger while
  `DataBridge` is leased).
- Tree context menu (`ui/nav_tree.rs`, gpui-component `context_menu`):
  Open / Drop table… on tables and views → confirmation dialog
  (`render_drop_dialog`, stale-guarded by `drop_id`); success closes the
  object's tabs and reloads the tree. The mock records drops per session.
- Records grid filter / sort / export (`ui/workspace/filter.rs`,
  `RecordsDelegate::query`): the toolbar filter icon opens a bar of
  AND-ed `column op value` rows (`FilterOp`: = ≠ < ≤ > ≥ contains, is
  [not] null; values typed by `parse_filter_value`); header sort icons
  (`Column::sortable`, not on json/binary) set `query.sort` via
  `perform_sort`. Both run in the database over the whole table and
  reset to page 1; filtered pages skip the planner estimate. The mock
  filters/sorts its first 20k generated rows in memory. Export writes a
  CSV file (save dialog, UTF-8 BOM, `model::csv_line`) of every row
  matching the filter, paging 1000 at a time; a second click cancels
  (`export_id`).
- Record editor (`ui/workspace.rs`, `render_editor`): typed inputs per
  column (`parse_cell`), `insert_row` / `update_row` (changed columns
  only) / `delete_row`, stale-guarded by `save_id`. FK columns get a
  "→ target" hint and a picker (up to 200 referenced values, labelled by
  the target's first text column) via `list_relations` + `fetch_rows`,
  stale-guarded by `fk_gen`. Rows have a context menu
  (`RecordsDelegate::context_menu` → `RecordsView::row_action`): edit,
  duplicate (prefilled insert without key columns), new, copy as TSV /
  JSON, delete; edit/delete need a PK.
- SQLite `auto`: only the rowid alias counts — the sole PK column
  declared exactly `INTEGER` in a rowid table (`mark_rowid_alias`);
  `INT PRIMARY KEY` must be supplied.
- Diagram (`ui/diagram.rs`): Supabase-style ERD, bezier FK edges,
  pan/zoom/fit, relations via `list_relations`.
- AI chat (`core/ai.rs` + `ui/ai_chat.rs`, Stage 6): **OpenAI-compatible
  servers only** (Ollama, LM Studio, llama.cpp, vLLM, …; no vendor SDKs).
  Right-hand panel toggled by the bot icon in the left rail (`sidebar.rs`);
  a list of **providers** (`ai::Provider`: name, base URL, model), each
  added from `ai::PRESETS` (OpenRouter, Groq, OpenAI, Gemini, DeepSeek,
  Mistral, Together, Cerebras, xAI, Ollama, LM Studio, Custom) — chips in
  the settings strip switch the active one. `ai.json` = `AiSettings
  { providers, active, mode }` (the old single `base_url`/`model` shape
  migrates on load); each key lives in the keyring as
  `ai:<provider id>::api_key`, never on disk. `GET /models` fills a
  picker filtered by the Model field (≤30 chips — routers list hundreds).
  A 400 mentioning `temperature` retries once without it (reasoning
  models). The system prompt carries the
  dialect and a compact schema listing (names/types, capped at 12k chars).
  Answers stream over SSE (`mpsc` from the sidecar, stale-guarded by
  `send_id`; Stop drops the task and cancels the request). Enter sends,
  Ctrl+Enter is a newline. Modes: **Manual** — code blocks get Run / Open
  in SQL tab / Copy; **Auto** — the answer's statements run in order (stop
  on first error), outcomes go back as `[ok]`/`[error]` user messages
  (`describe_outcome`: ≤20 rows TSV — so in Auto mode result rows *do*
  reach the model) and it continues, ≤ `MAX_AUTO_STEPS` rounds;
  `needs_confirmation` (DROP, TRUNCATE, ALTER…DROP, UPDATE/DELETE without
  WHERE; checked per statement via `split_statements`) always waits for
  Run; a statement that already failed pauses the loop (`repeats_failure`).
  Retry / Edit message under the last turn. Cyrillic user turns get a
  "reply in Russian" note (`language_hint`) — small models ignore the
  system prompt. Conversations persist in `ai_history.json`
  (≤50, history list in the panel header).
- sqlx cell decoding (`drivers::util::dyn_cell`) tries UUID / JSON only
  for columns whose type name says so: SQLite decodes any 16-byte TEXT as
  a UUID ('Валентин' read back as `d092d0b0-…`) and JSON-looking TEXT as
  JSON (`text_is_not_mistaken_for_uuid_or_json`).
- Driver errors carry the server message (`drivers::util::query_error`:
  "query failed: no such table: dual"), not a bare "query failed".
- Settings dialog (gear in the left rail, `render_settings_dialog`):
  interface size, light/dark theme (`theme::set`), language
  (`settings::set_language`). **Every user-facing string is translated**:
  `tr!("key")` / `tr!("key", n = x)` (macro in `main.rs`, returns
  `SharedString`; `%{n}` placeholders) from `locales/app.yml`
  (`_version: 2`: every language next to its key, missing ones fall back
  to `en`). The locale is rust-i18n's global, shared with gpui-component's
  own strings. New UI text needs a key in both languages —
  `every_ui_key_is_translated` (ui/settings.rs) fails otherwise. Not
  translated on purpose: driver/server error messages, SQL, brand names,
  `PK`/`FK`/`NULL`. Text already sitting in an input (placeholders of
  open views) updates when the view is recreated.
- Theme (`ui/theme.rs`): use `theme::palette(cx)`; no hardcoded colors
  except `DbType::brand_color()` and grid type tokens (`kind_color`).
  Brand glyphs must be registered in `ui/assets.rs` (`brand/*.svg`).

### Known deferred items

- The schema tree loads eagerly via `list_schemas`; per-schema lazy loading
  (`list_tables`) arrives if eager trees prove slow on real databases.
- SSH: no jump hosts / `~/.ssh/config`; host-key prompts are TOFU, not
  interactive. Cancellation of grid loads (`fetch_rows`) is stale-guard
  only (the query keeps running server-side).

## Roadmap

- **Stage 0 — restructure.** Create the `core/` / `drivers/` / `ui/` tree, move the
  existing files into it with no behavior change. App must compile and look
  identical. (`data.rs` splits into `core/model.rs` + `core/mock.rs`.) **Done.**
- **Stage 1 — session + runtime.** Add `DbSession`, `SessionHandle`, tokio sidecar;
  move the mock behind the trait; migrate `&'static TableMeta` → `Arc<TableMeta>`;
  route `RecordsDelegate::load` through the session. Mock-only build still works.
  **Done.**
- **Stage 2 — relational trio (sqlx).** Land in order PostgreSQL → MySQL → SQLite.
  Wire real connect flow into the wizard (test connection = `ping()`). **Done.**
- **Stage 3 — persistence.** `connections.json` + `settings.json` in the platform
  config dir; passwords via OS keyring (never plaintext on disk).
  **Mostly done.** `core/storage.rs` covers connections (`connections.json` +
  keyring, `--mock` demo flag, forget UI in the wizard) and
  `settings.json` (`storage::AppSettings`: UI scale, theme, language, SQL editor
  font — `ui::settings::restore` in `main`, `persist` on every change).
  **Done.**
- **Stage 4 — Mongo + Redis.** **Mongo done** — own client instead of
  the `mongodb` crate (see Current state). Next: Redis (`redis` with
  `tokio-comp`).
- **Stage 5 — query editor.** `execute(sql)`, result tabs, cancellation (reuse the
  stale-guard/load_id mechanism). **Mostly done.** `execute` + the simplified
  record editor (`update_row`/`insert_row`/`delete_row`) land on all drivers
  and the mock; `QueryView` renders the editor tab with highlighting.
  Cancellation (Stop) and Ctrl+Enter are done. Remaining niceties: result
  tabs inside one query view, run the selection / statement under the
  cursor.

- **Stage 6 — AI assistant.** OpenAI-compatible only (cloud vendor SDKs
  deferred). **Started:** side chat with schema context, streaming, model
  picker, Manual/Auto execution, history. Next: "Fix with AI" on query errors, explain selection, inline
  completions.

## Conventions

- Edition 2024. English comments; every module starts with a `//!` doc comment.
  Keep the historical "port of the web app's X" references — they explain design
  choices (e.g. remount-on-tab-switch, row selection resets).
- UI split pattern: one root entity (`DataBridge` in `ui/app.rs`); feature-specific
  renders live next to it as `impl DataBridge` blocks (`connect.rs`, …);
  heavyweight interactive views become their own entities (`RecordsView`).
- Prefer `gpui-component` widgets (buttons, inputs, `Table` + `TableDelegate`,
  `h_flex`/`v_flex`) over raw `gpui` building blocks.
- `anyhow` is allowed only at the binary edge; `core` and drivers use `DbError`.
- No new dependencies without justification. `async_trait` is pre-approved for
  `DbSession` (dyn-compatible trait objects). Stage 3 sanctioned `serde` +
  `keyring` (persistence, OS keyring secrets) and `rfd` (native file picker);
  `uuid` gained the `v4` feature for stable connection ids. `lsp-types`
  (same version as gpui-component's) is required by the editor's
  `CompletionProvider` trait. Stage 6 sanctioned `reqwest` (HTTP + SSE for
  the AI chat; `rustls-no-provider`) and a direct `rustls` dep with `ring` —
  the provider sqlx already links, installed in `core::ai` (avoids
  aws-lc-rs and its native build). Stage 4 (Mongo) sanctioned `bson` plus
  direct deps on crates already in the tree at the same versions
  (`tokio-rustls` without default features, `webpki-roots`,
  `rustls-native-certs`, `hmac`, `sha1`, `sha2`, `md-5`, `base64`, `rand`,
  `stringprep`) and tokio's `net` + `io-util` features; on Windows the
  `Win32_NetworkManagement_Dns` + `Win32_Foundation` features of the
  `windows-sys` 0.61 already in the tree (SRV lookups).
- SSH tunnels sanctioned `russh` (0.65, `default-features = false`,
  features `ring` + `rsa`: no aws-lc-rs, no flate2). It brings ~60 small
  RustCrypto crates (ed25519/curve25519, p256/384/521, ssh-key, …) — the
  price of in-process SSH with key/agent auth and known_hosts checks.
- UI translations: `rust-i18n` 4 (already in the tree via gpui-component,
  which reads the same global locale).
- Deprecate nothing silently: when a stage changes a rule in this file, update it
  in the same PR.

## Gotchas

- Fonts are embedded via `include_bytes!` with paths relative to each source file
  (`ui/assets.rs` → `../../assets/fonts/…`, `main.rs` → `../assets/fonts/…`) — keep
  the files in `assets/fonts/`; chrome uses `theme::SANS_FONT` (Public Sans), data
  cells use `theme::MONO_FONT` (JetBrains Mono) — family strings must match the
  TTF metadata.
- gpui's `set_rem_size` is driven by `theme.font_size` (settings scale). Rem-based
  utilities scale with it. Chrome text uses the type scale `theme::TEXT_XS..XL`
  (11/12/13/14/16) via `theme::scaled(n)` (design px → rems), never a raw
  `text_size(px(..))`; control heights, icon and card sizes use `scaled` too.
  Only hairlines/splitters stay `px`; virtual-list heights multiply by `palette.scale`.
- Pin `gpui`/`gpui-component` to the versions in `Cargo.toml`; their APIs move fast
  (GPUI Kit pins `gpui-pre` exactly — bump `gpui`, `gpui_platform`,
  `gpui-component` and `gpui-kit-assets` together; `rust-i18n` must match
  the kit's major version or the shared locale splits in two).
  Kit 0.9 notes: inputs are three types — `InputState` (single line),
  `TextareaState` + `Textarea` (AI chat), `EditorState` + `Editor` (SQL /
  document editors; `.language(..)`, `lsp_mut().completion_provider`);
  `PressEnter { secondary, shift }`; tables render with `DataTable::new`
  and `TableDelegate::column` returns an owned `Column`; `KeyBinding::new`
  takes a `'static` keystroke (`ctrl-enter` / `cmd-enter` by platform, no
  `secondary-`); `Modifiers::secondary(cx)`; `ScrollEvent` / `on_scroll`.
  (component lib is pre-1.0). Upgrades are deliberate, isolated changes.
- **Pin absolute overlays with explicit insets (`top_0().left_0()`), keep
  absolute `canvas` elements as the last child.** In gpui 0.2 / taffy 0.9 an
  absolute child with auto insets that follows an in-flow `size_full` sibling
  can resolve to its *static* position — right below that sibling, i.e.
  outside the visible window. Observed during external file drag frames
  (overlay/canvas at `y = window height`); pinning the insets removes the
  static-position path entirely. All full-cover overlays in `app.rs` (drop
  hint, settings backdrop, both watcher canvases) are pinned.
- **Modal cards need `.occlude()` (BlockMouse hitbox).** Click listeners fire
  on `hitbox.is_hovered`, and a backdrop's hitbox covers the whole window —
  sibling layout alone does not stop clicks over the card from reaching the
  backdrop. The card's BlockMouse hitbox cuts the backdrop (and everything
  behind it) out of the hover set. See `render_editor` in `workspace.rs` and
  `render_settings_dialog` in `app.rs`: backdrop + card are siblings under a
  transparent layer, the card occludes.
- **Theme edits go through `Theme::update`, never `Theme::global_mut`.**
  Kit 0.9 components read `theme.tokens` (checkbox fill, buttons), which
  only `update` reconciles with edited `colors`; buttons also have their
  own `button_*` color fields (`theme::theme_colors` sets them) — a raw
  write leaves the stock white primary.
- **Striped tables need `query::settle_stripes` after creating a
  `TableState`.** gpui-component sizes the stripe filler rows from the
  previous frame's bounds, so a fresh table paints without them until an
  unrelated repaint (mouse move). Used by `RecordsView::new` and
  `QueryView::apply_result`.
- `/site` has its own ignore rules and lockfile (pnpm). The root `.gitignore` only
  lists `/target` — that is intentional.
