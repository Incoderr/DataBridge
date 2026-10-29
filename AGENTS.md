# AGENTS.md — DataBridge

Guidance for AI agents working in this repository. Read before making changes.

## What this is

DataBridge is a desktop database explorer built with **GPUI**
(Zed's UI framework) and **gpui-component**. The web frontend (Tauri + React) was
removed; the GPUI Rust app is the only product code in this repo.

- Rust edition 2024, `gpui = "0.2.2"`, `gpui-component = "0.5.1"`
- Package `databridge-gpui`, binary `databridge`
- Repo layout:
  - `/src` — the application (layered tree below; `core::session` / `registry` /
    `runtime` and the drivers land in stages 1–2)
  - `/assets` — fonts (embedded with `include_bytes!`) and SVG icons
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
  drivers/
    postgres.rs   # sqlx, feature "postgres"
    mysql.rs      # sqlx, feature "mysql"
    sqlite.rs     # sqlx, feature "sqlite"
    mongo.rs      # feature "mongo"   (stage 4 — later)
    redis.rs      # feature "redis"   (stage 4 — later)
  ui/
    app.rs        # root entity DataBridge: connections, tabs, wizard state
    connect.rs    # connection wizard renders (impl DataBridge)
    sidebar.rs    # sidebar chrome
    nav_tree.rs   # explorer tree (virtualized: flat row model + v_virtual_list + overlay Scrollbar)
    workspace.rs  # tabs + records grid (RecordsView entity, RecordsDelegate) + the simplified record editor modal
    query.rs      # SQL editor tab (Stage 5): code-editor input, run button, results grid
    theme.rs      # dark/light palettes, Palette struct, MONO_FONT
    settings.rs   # UiScale global
    assets.rs     # AssetSource: brand icons, extra lucide icons, fallback to gpui-component-assets
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
    async fn fetch_rows(&self, table: TableRef, offset: u64, limit: u64) -> Result<Page, DbError>;
    // later: async fn list_tables(&self, schema: &str) -> Result<Vec<TableMeta>, DbError>
    //        (lazy per-schema trees when eager loading proves expensive)
    // stage 5 (done): one SQL statement per call.
    async fn execute(&self, sql: &str, limit: u64) -> Result<QueryPage, DbError>;
    // simplified editor: typed writes (CellChange carries the kind so NULL binds typed).
    async fn update_row(&self, table: TableRef, pk: Vec<CellChange>, changes: Vec<CellChange>) -> Result<u64, DbError>;
    async fn insert_row(&self, table: TableRef, values: Vec<CellChange>) -> Result<u64, DbError>;
    async fn delete_row(&self, table: TableRef, pk: Vec<CellChange>) -> Result<u64, DbError>;
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

`mongo` and `redis` cargo features are declared only when implemented
(stage 4); their `DbType` variants already exist as stubs (see below). The mock is
always available; `cargo check --no-default-features` must always pass
(mock-only demo build).

## Current state (read this first)

Module-level detail lives in each file's `//!` doc — this section keeps
only what works, what is a stub, and the non-obvious behaviors.

**Works for real (sqlx):** PostgreSQL, MySQL, MariaDB (served by the MySQL
driver — same wire protocol, `mariadb://` URLs accepted), SQLite. TLS via
rustls (`sslmode`/`ssl-mode` default to *prefer*). Lazy pools; `ping` =
connect + `SELECT 1` (the wizard's Test Connection); batched introspection
(SQLite: one `pragma_table_info` join); `COUNT(*)` + paged `SELECT *` with
dialect-quoted identifiers; row counts arrive with the first page.

**Stubs:** MongoDB, Redis, ClickHouse, Cassandra, ScyllaDB exist in
`DbType` (brand icons, ports, URL placeholders) but have no driver.
`registry::open` returns a "not supported in this build" error for them
(and for drivers compiled out) — never the mock: fake demo tables must not
pass for the user's database. `registry::is_supported` mirrors that: the
wizard's Choose screen lists every type and dims the unsupported ones. Only `--mock` serves every type from
`core/mock.rs` (deterministic md5-seeded rows, simulated latency, seeded
wizard list).

**Behaviors worth knowing:**
- Recents (`core/storage.rs`): `connections.json` in the platform config
  dir, newest first, capped at 20; passwords / full URLs only in the OS
  keyring (service "DataBridge"). Deduplicated by `model::TargetKey` —
  the same target under a new id replaces the old entry and its secrets;
  old duplicates self-heal on load.
- Wizard (`ui/connect.rs`): SQLite files connect via OS drag & drop
  (drop zone on the shared `window-root` shell in `app.rs`), Browse…
  (`rfd`), or New… (save dialog → empty file → connect; existing files
  are never truncated).
- Tabs: `TabKind::{Records, Diagram, Sql(seed)}`. Records/diagram views
  remount on every switch (web-app parity); SQL tabs are cached in
  `DataBridge::query_views`, so text and results survive switching.
- SQL editor (`ui/query.rs`): one statement per Run, stale-guarded by
  `run_id`. `model::is_select_like` (first keyword after comments, plus
  DML `RETURNING`) picks fetch vs execute. "New table" (tree row per
  schema + the empty workspace) seeds a dialect `CREATE TABLE`; DDL runs
  reload the tree via `DataBridge::reload_schemas` (stale-guarded by
  `schemas_id`).
- Tree context menu (`ui/nav_tree.rs`, gpui-component `context_menu`):
  Open / Drop table… on tables and views → confirmation dialog
  (`render_drop_dialog`, stale-guarded by `drop_id`); success closes the
  object's tabs and reloads the tree. The mock records drops per session.
- Record editor (`ui/workspace.rs`, `render_editor`): typed inputs per
  column (`parse_cell`), `insert_row` / `update_row` (changed columns
  only) / `delete_row`, stale-guarded by `save_id`. FK columns get a
  "→ target" hint and a picker (up to 200 referenced values, labelled by
  the target's first text column) via `list_relations` + `fetch_rows`,
  stale-guarded by `fk_gen`.
- SQLite `auto`: only the rowid alias counts — the sole PK column
  declared exactly `INTEGER` in a rowid table (`mark_rowid_alias`);
  `INT PRIMARY KEY` must be supplied.
- Diagram (`ui/diagram.rs`): Supabase-style ERD, bezier FK edges,
  pan/zoom/fit, relations via `list_relations`.
- Theme (`ui/theme.rs`): use `theme::palette(cx)`; no hardcoded colors
  except `DbType::brand_color()` and grid type tokens (`kind_color`).
  Brand glyphs must be registered in `ui/assets.rs` (`brand/*.svg`).

### Known deferred items

- Settings (UI scale) are not persisted.
- The schema tree loads eagerly via `list_schemas`; per-schema lazy loading
  (`list_tables`) arrives if eager trees prove slow on real databases.
- No SSH tunnels; no query cancellation / Ctrl+Enter yet.

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
  keyring, `--mock` demo flag, forget UI in the wizard). Remaining:
  `settings.json` (UI scale).
- **Stage 4 — Mongo + Redis** (`mongodb` crate; `redis` with `tokio-comp`).
- **Stage 5 — query editor.** `execute(sql)`, result tabs, cancellation (reuse the
  stale-guard/load_id mechanism). **Mostly done.** `execute` + the simplified
  record editor (`update_row`/`insert_row`/`delete_row`) land on all drivers
  and the mock; `QueryView` renders the editor tab with highlighting.
  Remaining niceties: result tabs inside one query view, cancellation,
  Ctrl+Enter keybinding.

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
  `uuid` gained the `v4` feature for stable connection ids.
- Deprecate nothing silently: when a stage changes a rule in this file, update it
  in the same PR.

## Gotchas

- Fonts are embedded via `include_bytes!` with paths relative to each source file
  (`ui/assets.rs` → `../../assets/fonts/…`, `main.rs` → `../assets/fonts/…`) — keep
  the files in `assets/fonts/`; chrome uses `theme::SANS_FONT` (Public Sans), data
  cells use `theme::MONO_FONT` (JetBrains Mono) — family strings must match the
  TTF metadata.
- gpui's `set_rem_size` is driven by `theme.font_size` (settings scale). Rem-based
  utilities scale with it; hand-tuned `px()` values multiply by `palette.scale`.
- Pin `gpui`/`gpui-component` to the versions in `Cargo.toml`; their APIs move fast
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
- `/site` has its own ignore rules and lockfile (pnpm). The root `.gitignore` only
  lists `/target` — that is intentional.
