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
    workspace.rs  # tabs + records grid (RecordsView entity, RecordsDelegate)
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
    // stage 5: async fn execute(&self, sql: &str, limit: u64) -> Result<Page, DbError>;
}
```

`TableRef = { schema, name, kind }` identifies a result set; `Page =
{ rows, total, truncated, elapsed_ms }` is one fetched page; `Ping = { status,
latency_ms }`; `Relation = { from_table, from_column, to_table, to_column }`
is a same-schema FK edge. The UI never implements the trait — it holds a
cloneable `SessionHandle` (all calls go through `core::registry::open`).

### Cargo features

```
default = ["postgres", "mysql", "sqlite"]
```

`mongo` and `redis` are declared only when implemented (stage 4). The mock is
always available; `cargo check --no-default-features` must always pass
(mock-only demo build).

## Current state (read this first)

The UI is complete and polished **on top of the session contract, served by
the mock driver**:

- `core/model.rs` — domain types (`DbConnection` with an optional `url` for
  URL-mode connections, `Arc<TableMeta>`, `Page`, `Ping`, `TableRef`,
  `SchemaMeta`) plus the `status_line` / `group_digits` helpers and
  `parse_db_url` (display-oriented URL parsing; the drivers re-parse URLs
  natively with sqlx, preserving query options like `sslmode`).
- `core/session.rs` — `DbSession` trait + cloneable `SessionHandle`.
- `core/runtime.rs` — the tokio sidecar (`init` in `main`, `spawn` + oneshot).
- `core/registry.rs` — `open(conn) -> SessionHandle`; dispatches the sqlx trio
  behind cargo features, mock for everything else. `--mock` (via `init` /
  `is_mock`) forces the mock driver for every connection — the built-in demo
  mode that needs no live database.
- `drivers/` — `postgres.rs` / `mysql.rs` / `sqlite.rs`: lazily-connected pools
  (`ping` = connect + `SELECT 1`), batched per-schema introspection, `COUNT(*)` +
  paged `SELECT *` with dialect-quoted identifiers; row counts are not
  introspected (real totals arrive on the first page load). `drivers/util.rs`
  holds the shared `dyn_cell!` decode ladder (runtime-typed cells → `Value`).
  The wizard's Test Connection is a real `SessionHandle::ping()`.
- `core/mock.rs` — `MockSession: DbSession`: deterministic rows (same
  (connection, schema, table, offset, limit) → same rows, md5-seeded),
  simulated latency slept on the tokio side; `--mock` runs the whole app on
  it and seeds the wizard's connection list (`seed_connections`).
- `core/storage.rs` — Stage-3 recent-connections store. `connections.json`
  in the platform config dir (newest first, capped at 20); secrets —
  fields-mode passwords and full URL-mode URLs — go to the OS keyring
  (service "DataBridge", entry per connection id; the JSON keeps only a
  password-stripped URL copy for display). Loaded into the wizard on
  startup, remembered on every successful connect; best-effort everywhere —
  a missing keyring just means typing the password again.
- `ui/workspace.rs` — records grid (IDE-scale: `Size::Large` rows, cell font
  13.5) with pagination (PAGE = 50); the page loads through
  `SessionHandle::fetch_rows` with the stale-guarded `load_id`. Tabs are
  `TabKind::Records(Arc<TableMeta>) | TabKind::Diagram`; `app.rs` holds the
  active one in a `WorkspaceView` enum (records grid or diagram).
- `ui/diagram.rs` — the schema relationships view (Supabase-style ERD):
  draggable table cards with colored types and PK/FK badges, FK edges as
  bezier paths (`PathBuilder` + `window.paint_path`), background-pan,
  wheel-zoom anchored at the cursor, fit-to-view, relations loaded via
  `SessionHandle::list_relations` (stale-guarded). Opened from the tree's
  per-schema "Schema diagram" row.
- `ui/app.rs` — root entity `DataBridge`: connection list, active connection,
  `session` + `schemas` (loaded asynchronously in `on_connected`),
  tab management, wizard state machine (`Phase::Choose → Form → Testing`).
  Renders the shared full-window shell (`window-root`) that wraps both the
  wizard and the explorer: one drop zone, the drag-over hint overlay and
  the settings dialog on top.
- `ui/connect.rs` — the wizard is also the landing zone for SQLite files:
  OS drag & drop anywhere on the window connects immediately (the drop
  zone lives on the shared `window-root` shell in `app.rs`; a drag-over
  overlay plus standing hints — Choose phase and the file field — show
  that it's allowed); the file-path field has a Browse… button (native
  OS picker via `rfd`, run async off the UI thread). Dropped/picked
  files become regular SQLite drafts, so `--mock` routes them to the
  demo tree and the Stage-3 store remembers them like any other
  connection.
- `ui/sidebar.rs` / `ui/nav_tree.rs` — render blocks of `DataBridge`.
- `ui/theme.rs` — near-black dark + clean light palettes (UI Bakery / DrawSQL
  inspired), sans chrome + mono data, radius/shadow tuned in `apply()`.
  Use `theme::palette(cx)`; do not hardcode colors except `DbType::brand_color()`
  (simple-icons brand colors) and grid type tokens (`kind_color`).
- `ui/settings.rs` — UI scale only, process-global, not persisted.
- `ui/assets.rs` — `AssetSource`; brand glyphs at `brand/*.svg`, extra lucide icons at
  `icons/*.svg`, then falls back to `gpui_component_assets`.

### Known deferred items (fix in stages 3–4)

- sqlx is built without TLS features — plaintext connections (local/trusted
  networks). Add `tls-rustls` to the sqlx features when remote TLS hosts
  appear.
- Settings (UI scale) are not persisted.
- The schema tree loads eagerly via `list_schemas`; per-schema lazy loading
  (`list_tables`) arrives if eager trees prove slow on real databases.

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
  stale-guard/load_id mechanism).

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
- `/site` has its own ignore rules and lockfile (pnpm). The root `.gitignore` only
  lists `/target` — that is intentional.
