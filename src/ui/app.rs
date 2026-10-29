//! Root application view: connection state, tab management, layout.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    canvas, div, px, rgb, svg, App, Context, DispatchPhase, DragMoveEvent, Entity,
    ExternalPaths, FileDropEvent, FontWeight, IntoElement, MouseButton, MouseMoveEvent,
    MouseUpEvent, ParentElement, SharedString, Styled, Window,
};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::{h_flex, v_flex};
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{
    ActiveTheme, Icon, IconName, Sizable, Size, VirtualListScrollHandle,
};

use crate::core::mock;
use crate::core::model::{
    ConnectionStatus, DbConnection, DbType, NodeKind, SchemaMeta, TableMeta, TableRef,
};
use crate::core::registry;
use crate::core::session::SessionHandle;
use crate::core::storage;
use crate::ui::diagram::SchemaDiagram;
use crate::ui::nav_tree::{SelectedPath, TreeRow};
use crate::ui::query::QueryView;
use crate::ui::theme;
use crate::ui::workspace::{OpenTab, RecordsView, TabKind};

/// Wizard phase: pick type/existing → fill form → connecting.
#[derive(PartialEq)]
pub enum Phase {
    Choose,
    Form,
    Testing,
}

/// Connection draft backed by input entities (the form fields).
pub struct Draft {
    pub id: String,
    pub db_type: DbType,
    /// Structured-fields mode vs URL mode (the form toggle).
    pub url_mode: bool,
    pub name: Entity<InputState>,
    pub url: Entity<InputState>,
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub database: Entity<InputState>,
    pub user: Entity<InputState>,
    pub password: Entity<InputState>,
}

pub struct WizardState {
    pub phase: Phase,
    pub draft: Draft,
    pub error: Option<String>,
    pub testing: bool,
    pub chosen_existing: Option<String>,
}

/// A drop confirmation: the object, whether the DROP is in flight, and
/// the driver's error if it failed (shown in the dialog).
pub struct PendingDrop {
    pub schema: String,
    pub meta: Arc<TableMeta>,
    pub running: bool,
    pub error: Option<String>,
}

/// The workspace view of the active tab: the records grid or the schema
/// diagram. Recreated on every tab switch (like the web version's keyed
/// remount).
#[derive(Clone)]
pub enum WorkspaceView {
    Records(Entity<RecordsView>),
    Diagram(Entity<SchemaDiagram>),
    Query(Entity<QueryView>),
}

pub struct DataBridge {
    /// `--mock` demo mode: seeded connections on the mock driver, nothing
    /// read from or written to the connection store.
    pub mock: bool,
    pub connections: Vec<DbConnection>,
    pub active_conn: Option<DbConnection>,
    /// Data-plane handle for the active connection — all DB access.
    pub session: Option<SessionHandle>,
    /// Schema tree of the active connection (loaded via `list_schemas`).
    pub schemas: Vec<SchemaMeta>,
    pub tabs: Vec<OpenTab>,
    pub active_tab: Option<usize>,
    pub view: Option<WorkspaceView>,
    // nav tree state
    pub search: Entity<InputState>,
    pub query: String,
    pub expanded: HashSet<String>,
    pub filter_only_tables: bool,
    pub nav_width: f32,
    pub resizing_nav: bool,
    /// Flat visible-row model of the explorer tree — rebuilt on every
    /// render by `render_nav_tree`; the virtual list turns only the
    /// on-screen range into elements.
    pub nav_rows: Vec<TreeRow>,
    /// Scroll state of the virtualized tree body. Held on the entity so
    /// the position survives repaints; reset when a connection opens.
    pub tree_scroll: VirtualListScrollHandle,
    /// Settings dialog visibility (interface scale chooser).
    pub show_settings: bool,
    /// Counter for query tab ids (multiple SQL tabs per connection).
    pub query_seq: u32,
    /// Stale guard of the schema-tree loads (connect + DDL reloads).
    pub schemas_id: u64,
    /// Live SQL editors by tab id. Records/diagram views remount on every
    /// tab switch (web-app parity); a query tab must not — that would drop
    /// the typed statement and its results.
    pub query_views: HashMap<String, Entity<QueryView>>,
    /// The tree context menu's "Drop table…" awaiting confirmation.
    pub pending_drop: Option<PendingDrop>,
    /// Stale guard of drop requests (a new request / cancel supersedes).
    pub drop_id: u64,
    // visual state
    pub grid_view: bool,
    /// External files are dragged over the window right now — the
    /// "drop it here" hint overlay is up.
    pub drag_over: bool,
    // connection wizard
    pub wizard: WizardState,
}

impl DataBridge {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("schema.table…"));
        cx.subscribe(&search, |this, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = this.search.read(cx).value().to_string();
                if !this.query.trim().is_empty() {
                    this.expand_all_for_query();
                }
                cx.notify();
            }
        })
        .detach();

        let wizard = WizardState {
            phase: Phase::Choose,
            draft: Draft::new(DbType::PostgreSql, window, cx),
            error: None,
            testing: false,
            chosen_existing: None,
        };

        // `--mock` demo mode: seeded connections served by the mock
        // driver, nothing persisted. Otherwise the Stage-3 store supplies
        // the real recent connections (most recently used first).
        let demo = registry::is_mock();

        Self {
            mock: demo,
            connections: if demo {
                mock::seed_connections()
            } else {
                storage::load_connections()
            },
            active_conn: None,
            session: None,
            schemas: Vec::new(),
            tabs: Vec::new(),
            active_tab: None,
            view: None,
            search,
            query: String::new(),
            expanded: HashSet::new(),
            filter_only_tables: false,
            nav_width: 288.0,
            resizing_nav: false,
            nav_rows: Vec::new(),
            tree_scroll: VirtualListScrollHandle::new(),
            show_settings: false,
            query_seq: 0,
            schemas_id: 0,
            query_views: HashMap::new(),
            pending_drop: None,
            drop_id: 0,
            grid_view: true,
            drag_over: false,
            wizard,
        }
    }

    // ---------------- actions ----------------

    pub fn open_tab(
        &mut self,
        schema: &str,
        meta: Arc<TableMeta>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.active_conn.clone() else {
            return;
        };
        let id = format!("{}:{schema}:{}", conn.id, meta.name);
        if let Some(ix) = self.tabs.iter().position(|t| t.id == id) {
            self.set_active(ix, window, cx);
        } else {
            self.tabs.push(OpenTab {
                id,
                schema: schema.into(),
                kind: TabKind::Records(meta),
            });
            let ix = self.tabs.len() - 1;
            self.set_active(ix, window, cx);
        }
    }

    /// Open the Supabase-style relationships diagram of one schema.
    pub fn open_diagram(&mut self, schema: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active_conn.clone() else {
            return;
        };
        let id = format!("{}:{schema}:__diagram__", conn.id);
        if let Some(ix) = self.tabs.iter().position(|t| t.id == id) {
            self.set_active(ix, window, cx);
        } else {
            self.tabs.push(OpenTab {
                id,
                schema: schema.into(),
                kind: TabKind::Diagram,
            });
            let ix = self.tabs.len() - 1;
            self.set_active(ix, window, cx);
        }
    }

    /// Open a new SQL editor tab (Stage 5). Multiple tabs are allowed —
    /// each gets its own editor state, like DataGrip consoles.
    pub fn open_query_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_query_tab_with(None, window, cx);
    }

    /// "New table": a SQL tab seeded with a dialect-specific `CREATE TABLE`
    /// template for `schema`. Running it reloads the tree (see `QueryView`).
    pub fn open_create_table(&mut self, schema: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.active_conn.as_ref().map(|c| c.db_type) else {
            return;
        };
        if let Some(sql) = create_table_template(t, schema) {
            self.open_query_tab_with(Some(sql), window, cx);
        }
    }

    /// Whether the active connection speaks SQL DDL (offers "New table").
    pub fn can_create_tables(&self) -> bool {
        self.active_conn
            .as_ref()
            .is_some_and(|c| create_table_template(c.db_type, "").is_some())
    }

    /// The schema a "New table" without an explicit target goes to: the
    /// connection's database (MySQL), `public` (PostgreSQL), else the first.
    pub fn default_schema(&self) -> Option<String> {
        let db = self.active_conn.as_ref().map(|c| c.database.as_str()).unwrap_or("");
        self.schemas
            .iter()
            .find(|s| s.name == db)
            .or_else(|| self.schemas.iter().find(|s| s.name == "public"))
            .or_else(|| self.schemas.first())
            .map(|s| s.name.clone())
    }

    fn open_query_tab_with(
        &mut self,
        seed: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.query_seq += 1;
        let Some(conn) = self.active_conn.clone() else {
            return;
        };
        let id = format!("{}:__query__{}", conn.id, self.query_seq);
        self.tabs.push(OpenTab {
            id,
            schema: conn.database.clone(),
            kind: TabKind::Sql(seed),
        });
        let ix = self.tabs.len() - 1;
        self.set_active(ix, window, cx);
    }

    pub fn set_active(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active_tab = Some(ix);
        let conn = self.active_conn.clone().expect("active connection");
        let session = self.session.clone().expect("active session");
        let tab = self.tabs[ix].clone();
        let app = cx.entity().downgrade();
        self.view = Some(match tab.kind {
            TabKind::Records(_) => WorkspaceView::Records(cx.new(|cx| {
                let TabKind::Records(table) = &tab.kind else {
                    unreachable!("diagram tabs open the diagram view")
                };
                RecordsView::new(
                    conn.clone(),
                    session.clone(),
                    tab.schema.clone(),
                    table.clone(),
                    app,
                    window,
                    cx,
                )
            })),
            TabKind::Diagram => {
                // The canvas shows tables and views of the schema.
                let tables = self
                    .schemas
                    .iter()
                    .find(|s| s.name == tab.schema)
                    .map(|s| {
                        s.tables
                            .iter()
                            .chain(s.views.iter())
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                WorkspaceView::Diagram(cx.new(|cx| {
                    SchemaDiagram::new(conn, session, tab.schema.clone(), tables, cx)
                }))
            }
            TabKind::Sql(seed) => {
                let view = self
                    .query_views
                    .entry(tab.id.clone())
                    .or_insert_with(|| {
                        cx.new(|cx| QueryView::new(conn, session, seed.clone(), app, window, cx))
                    })
                    .clone();
                WorkspaceView::Query(view)
            }
        });
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        let closed = self.tabs.remove(ix);
        self.query_views.remove(&closed.id);
        match self.active_tab {
            Some(a) if a == ix => {
                self.view = None;
                if self.tabs.is_empty() {
                    self.active_tab = None;
                } else {
                    let n = a.min(self.tabs.len() - 1);
                    self.set_active(n, window, cx);
                    return;
                }
            }
            Some(a) if a > ix => {
                self.set_active(a - 1, window, cx);
                return;
            }
            _ => {}
        }
        cx.notify();
    }

    pub fn toggle_node(&mut self, id: &str, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.expanded.insert(id.to_string()) {
            self.expanded.remove(id);
        }
        cx.notify();
    }

    /// Drop a remembered connection from the wizard's list; outside demo
    /// mode also from the connection store and the OS keyring.
    pub fn forget_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        self.connections.retain(|c| c.id != id);
        if !self.mock {
            storage::forget_connection(id);
        }
        cx.notify();
    }

    /// Back to the connection wizard (disconnect / new connection).
    pub fn terminate(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.active_conn.as_ref().map(|c| c.id.clone()) {
            if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
                c.status = ConnectionStatus::Disconnected;
            }
        }
        self.active_conn = None;
        self.session = None;
        self.schemas_id += 1;
        self.schemas.clear();
        self.tabs.clear();
        self.query_views.clear();
        self.active_tab = None;
        self.view = None;
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.view.clone() {
            Some(WorkspaceView::Records(rec)) => {
                rec.update(cx, |v, cx| v.reload(cx));
            }
            Some(WorkspaceView::Diagram(d)) => {
                d.update(cx, |v, cx| v.reload(cx));
            }
            // The query tab re-runs via its own Run button.
            Some(WorkspaceView::Query(_)) => {}
            None => {}
        }
    }

    /// The session broke after entering the explorer (introspection failed
    /// on connect): back to the wizard with the driver's message.
    pub fn connection_failed(&mut self, message: String, cx: &mut Context<Self>) {
        self.terminate(cx);
        self.wizard.error = Some(message);
        cx.notify();
    }

    /// A connection was established: enter the explorer. The schema tree
    /// loads asynchronously through the session; the tree area shows its
    /// empty state until it arrives.
    pub fn on_connected(&mut self, conn: DbConnection, cx: &mut Context<Self>) {
        // Open the data-plane session first; a failure keeps us in the wizard.
        let session = match registry::open(&conn) {
            Ok(s) => s,
            Err(e) => {
                self.wizard.error = Some(e.message().to_string());
                cx.notify();
                return;
            }
        };
        // Recents first: the just-used connection moves to the top of the
        // wizard's list, replacing older entries of the same target; outside
        // demo mode it is persisted (JSON + keyring).
        let key = conn.target_key();
        self.connections
            .retain(|c| c.id != conn.id && c.target_key() != key);
        self.connections.insert(0, conn.clone());
        if !self.mock {
            storage::remember_connection(&conn);
        }
        self.active_conn = Some(conn);
        self.session = Some(session.clone());
        self.schemas.clear();
        self.tabs.clear();
        self.query_views.clear();
        self.active_tab = None;
        self.view = None;
        self.query.clear();
        self.expanded = HashSet::from(["conn".to_string(), "schemas".to_string()]);
        // Fresh tree for the new connection — also resets the tree scroll.
        self.nav_rows.clear();
        self.tree_scroll = VirtualListScrollHandle::new();
        self.wizard.phase = Phase::Choose;
        self.wizard.error = None;

        self.schemas_id += 1;
        let load = self.schemas_id;
        let rx = session.list_schemas();
        cx.spawn(async move |this, cx| {
            match rx.await {
                Ok(Ok(schemas)) => {
                    this.update(cx, |this, cx| {
                        if this.schemas_id != load {
                            return; // disconnected / superseded meanwhile
                        }
                        this.schemas = schemas;
                        this.expanded = Self::initial_expanded(&this.schemas);
                        cx.notify();
                    })
                    .ok();
                }
                Ok(Err(e)) => {
                    // Introspection failed after a successful ping (e.g.
                    // permissions): back to the wizard with the message.
                    this.update(cx, |this, cx| {
                        this.connection_failed(e.message().to_string(), cx);
                    })
                    .ok();
                }
                Err(_) => {}
            }
        })
        .detach();

        cx.notify();
    }

    /// Re-introspect the active connection after DDL ran in a query tab
    /// (new / dropped / altered tables). Keeps the user's expansion state;
    /// schemas that appear for the first time open like on connect.
    /// Errors keep the current tree — the query tab already reported them.
    pub fn reload_schemas(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.schemas_id += 1;
        let load = self.schemas_id;
        let rx = session.list_schemas();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(schemas)) = rx.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.schemas_id != load {
                    return; // superseded / disconnected
                }
                let fresh: Vec<SchemaMeta> = schemas
                    .iter()
                    .filter(|s| !this.schemas.iter().any(|o| o.name == s.name))
                    .cloned()
                    .collect();
                this.expanded.extend(Self::initial_expanded(&fresh));
                this.schemas = schemas;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Tree context menu → confirmation dialog for dropping `meta`.
    pub fn ask_drop(&mut self, schema: &str, meta: Arc<TableMeta>, cx: &mut Context<Self>) {
        self.drop_id += 1;
        self.pending_drop = Some(PendingDrop {
            schema: schema.to_string(),
            meta,
            running: false,
            error: None,
        });
        cx.notify();
    }

    pub fn cancel_drop(&mut self, cx: &mut Context<Self>) {
        self.drop_id += 1; // a DROP still in flight no longer touches the UI
        self.pending_drop = None;
        cx.notify();
    }

    /// Run the confirmed DROP through the session (stale-guarded by
    /// `drop_id`). Success closes the object's tabs and reloads the tree;
    /// failure keeps the dialog open with the driver's message.
    pub fn confirm_drop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(pending) = self.pending_drop.as_mut() else {
            return;
        };
        if pending.running {
            return;
        }
        pending.running = true;
        pending.error = None;
        let table = TableRef::of(&pending.meta);
        self.drop_id += 1;
        let id = self.drop_id;
        cx.notify();
        let rx = session.drop_table(table.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = rx.await;
            cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    if this.drop_id != id {
                        return; // cancelled / superseded
                    }
                    match result {
                        Ok(Ok(())) => {
                            this.pending_drop = None;
                            this.close_tabs_of(&table, window, cx);
                            this.reload_schemas(cx);
                        }
                        Ok(Err(e)) => {
                            if let Some(p) = this.pending_drop.as_mut() {
                                p.running = false;
                                p.error = Some(e.message().to_string());
                            }
                        }
                        Err(_) => this.pending_drop = None,
                    }
                    cx.notify();
                })
                .ok();
            })
            .ok();
        })
        .detach();
    }

    /// Close every records tab showing `table` (it was just dropped).
    fn close_tabs_of(&mut self, table: &TableRef, window: &mut Window, cx: &mut Context<Self>) {
        let shows = |t: &OpenTab| {
            matches!(&t.kind, TabKind::Records(m) if m.schema == table.schema && m.name == table.name)
        };
        let active_id = self.active_tab.map(|i| self.tabs[i].id.clone());
        let active_closed = self
            .active_tab
            .is_some_and(|i| shows(&self.tabs[i]));
        self.tabs.retain(|t| !shows(t));
        if active_closed {
            self.view = None;
            self.active_tab = None;
            if !self.tabs.is_empty() {
                let last = self.tabs.len() - 1;
                self.set_active(last, window, cx);
            }
        } else {
            // The active tab survived; its index may have shifted.
            self.active_tab = active_id.and_then(|id| self.tabs.iter().position(|t| t.id == id));
        }
        cx.notify();
    }

    fn initial_expanded(schemas: &[SchemaMeta]) -> HashSet<String> {
        let mut s = HashSet::new();
        s.insert("conn".into());
        s.insert("schemas".into());
        for sch in schemas {
            s.insert(format!("s:{}", sch.name));
            s.insert(format!("ss:{}:tables", sch.name));
            s.insert(format!("ss:{}:views", sch.name));
            s.insert(format!("ss:{}:routines", sch.name));
        }
        s
    }

    /// Auto-expand every group when a search query is active.
    fn expand_all_for_query(&mut self) {
        self.expanded.insert("conn".into());
        self.expanded.insert("schemas".into());
        for sch in &self.schemas {
            self.expanded.insert(format!("s:{}", sch.name));
            self.expanded.insert(format!("ss:{}:tables", sch.name));
            self.expanded.insert(format!("ss:{}:views", sch.name));
            self.expanded.insert(format!("ss:{}:routines", sch.name));
        }
    }

    /// (schema, table) of the active tab — drives tree highlight. Diagram
    /// tabs highlight nothing.
    pub fn selected_path(&self) -> SelectedPath {
        self.active_tab
            .and_then(|ix| self.tabs.get(ix))
            .and_then(|t| match &t.kind {
                TabKind::Records(meta) => Some((t.schema.clone(), meta.name.clone())),
                // Diagram and query tabs highlight nothing.
                TabKind::Diagram | TabKind::Sql(_) => None,
            })
    }

    // ---------------- layout ----------------

    fn render_tab_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let is_dark = cx.theme().is_dark();
        let theme_icon = if is_dark {
            IconName::Sun
        } else {
            IconName::Moon
        };

        let mut bar = TabBar::new("tabs");
        for (ix, tab) in self.tabs.iter().enumerate() {
            let active = self.active_tab == Some(ix);
            let (label, icon) = match &tab.kind {
                TabKind::Records(meta) => (meta.name.clone(), "icons/table.svg"),
                TabKind::Diagram => (format!("{} · diagram", tab.schema), "icons/schema.svg"),
                TabKind::Sql(_) => (format!("query {}", ix + 1), "icons/schema.svg"),
            };
            bar = bar.child(
                Tab::new()
                    .label(label)
                    // gpui-component only pads the label branch; with a
                    // prefix/suffix (icon + close button) the base is flush to
                    // the tab edge — set our own horizontal padding.
                    .px(px(8.))
                    .prefix(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .child(
                                svg()
                                    .path(icon)
                                    .size(px(14.))
                                    .text_color(if active { p.primary } else { p.muted_fg }),
                            ),
                    )
                    .suffix(
                        div()
                            .id(("tab-close", ix))
                            .size_4()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(move |s| s.bg(p.tree_hover))
                            .child(Icon::new(IconName::Close).with_size(Size::Size(px(12.))))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, window, cx);
                            })),
                    ),
            );
        }

        bar.selected_index(self.active_tab.unwrap_or(0))
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                if *ix < this.tabs.len() {
                    this.set_active(*ix, window, cx);
                }
            }))
            // New SQL editor tab — always available while connected.
            .suffix(
                div()
                    .id("new-query")
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(p.tree_hover))
                    .child(
                        Icon::new(IconName::SquareTerminal)
                            .with_size(Size::Size(px(14.)))
                            .text_color(p.muted_fg),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_query_tab(window, cx);
                    })),
            )
            .suffix(
                div()
                    .id("theme-toggle")
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(p.tree_hover))
                    .child(
                        Icon::new(theme_icon)
                            .with_size(Size::Size(px(14.)))
                            .text_color(p.muted_fg),
                    )
                    .on_click(cx.listener(|_, _, window, cx| {
                        theme::toggle(window, cx);
                    })),
            )
    }
}

impl Render for DataBridge {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let resizing = self.resizing_nav;
        let show_settings = self.show_settings;
        // Self-heal the hint flag on every frame: any repaint during an
        // active file drag raises the hint even if a move event was
        // missed (our app has no internal drags, so an active drag is
        // always external files).
        let dragging = cx.has_active_drag();
        if dragging != self.drag_over {
            self.drag_over = dragging;
        }
        let drag_over = self.drag_over;
        let view = cx.entity();
        let content = if self.active_conn.is_none() {
            self.render_wizard(window, cx).into_any_element()
        } else {
            self.render_explorer(cx).into_any_element()
        };

        // The shared full-window shell: one drop zone for both the wizard
        // and the explorer, the drag-over hint on top of everything, and
        // the FileDrop watcher that hides the hint when the drag leaves.
        //
        // Child order matters: an absolutely positioned `canvas` (a
        // layout leaf) corrupts the geometry of absolute siblings laid
        // out after it (observed empirically with gpui 0.2 / taffy 0.9),
        // so it must stay LAST — behind the settings overlay and the
        // drop hint.
        div()
            .id("window-root")
            .relative()
            .size_full()
            .when(resizing, |this| this.cursor_col_resize())
            // Files dragged over the window → raise the drop hint.
            .on_drag_move::<ExternalPaths>(cx.listener(
                |this, _: &DragMoveEvent<ExternalPaths>, _, cx| {
                    if !this.drag_over {
                        this.drag_over = true;
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.drag_over = false;
                this.open_dropped_paths(paths, window, cx);
            }))
            .child(content)
            .when(show_settings, |this| {
                this.child(self.render_settings_dialog(cx))
            })
            .when(self.pending_drop.is_some(), |this| {
                this.child(self.render_drop_dialog(cx))
            })
            .when(drag_over, |this| this.child(self.render_drop_hint(cx)))
            .child(
                canvas(
                    |_, _, _| {},
                    {
                        let view = view.clone();
                        move |_, _, window, _cx| {
                            let view = view.clone();
                            window.on_mouse_event(
                                move |e: &FileDropEvent, phase, _, cx: &mut App| {
                                    if phase == DispatchPhase::Bubble
                                        && matches!(e, FileDropEvent::Exited)
                                    {
                                        view.update(cx, |this, cx| {
                                            if this.drag_over {
                                                this.drag_over = false;
                                                cx.notify();
                                            }
                                        });
                                    }
                                },
                            );
                        }
                    },
                )
                // Pinned insets: without them the absolute children land at
                // their *static* position (right below the in-flow content)
                // in drag frames — outside the visible window.
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
    }
}

impl DataBridge {
    /// The explorer: rail + tree + splitter + workspace. Extracted from
    /// `render` so the shared shell (drop zone, drag-over hint, settings
    /// overlay) can wrap both it and the wizard.
    fn render_explorer(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let view = cx.entity();
        div()
            .id("app")
            .size_full()
            .bg(p.background)
            .text_color(p.foreground)
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.resizing_nav && !e.dragging() {
                    // Button was released outside the window and the OS never
                    // delivered the mouse-up here: drop the drag instead of
                    // sticking to the cursor forever.
                    this.resizing_nav = false;
                    cx.notify();
                } else if this.resizing_nav {
                    let x: f32 = e.position.x.into();
                    let new_w = (x - 60.0).clamp(180.0, 560.0);
                    this.nav_width = new_w;
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                if this.resizing_nav {
                    this.resizing_nav = false;
                    cx.notify();
                }
            }))
            // Window-level safety net: any left-button release anywhere ends
            // both resizers (covers releases over other apps / title bar).
            .child(
                canvas(
                    |_, _, _| {},
                    {
                        let view = view.clone();
                        // paint phase — `on_mouse_event` is only legal here
                        move |_, _, window, _cx| {
                            let view = view.clone();
                            window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx: &mut App| {
                                if phase == DispatchPhase::Bubble {
                                    view.update(cx, |this, cx| {
                                        if this.resizing_nav {
                                            this.resizing_nav = false;
                                            cx.notify();
                                        }
                                    });
                                }
                            });
                        }
                    },
                )
                .absolute()
                // Pinned insets — keep it at the window origin in drag
                // frames too (see the shell comment).
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(
                h_flex()
                    .h_full()
                    .p_2()
                    .gap_1()
                    .child(self.render_sidebar(cx))
                    .child(self.render_nav_tree(cx))
                    .child(
                        div()
                            .id("nav-splitter")
                            .w(px(6.))
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_col_resize()
                            .hover(move |s| s.bg(p.primary.opacity(0.35)))
                            .when(self.resizing_nav, move |s| s.bg(p.primary))
                            .child(
                                div()
                                    .w(px(1.))
                                    .h_full()
                                    .bg(p.toolbar_border),
                            )
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                                this.resizing_nav = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        v_flex()
                            .id("workspace")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .rounded_r(px(4.))
                            .overflow_hidden()
                            .child(self.render_tab_strip(cx))
                            .child(match self.view.clone() {
                                Some(WorkspaceView::Records(v)) => v.into_any_element(),
                                Some(WorkspaceView::Diagram(v)) => v.into_any_element(),
                                Some(WorkspaceView::Query(v)) => v.into_any_element(),
                                None => self.render_empty_state(cx).into_any_element(),
                            }),
                    ),
            )
    }

    /// The full-window hint shown while database files are dragged over
    /// the window: a veil plus a dashed card telling the user the drop is
    /// welcome. Purely visual — no ids, no hitboxes — so the drop itself
    /// keeps flowing to the shell's `on_drop`.
    fn render_drop_hint(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let sqlite = DbType::Sqlite;
        div()
            .absolute()
            // Pinned insets — see the shell comment: auto insets let the
            // element slip to its static position (below the window) in
            // drag frames.
            .top_0()
            .left_0()
            .size_full()
            .bg(p.background.opacity(0.55))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .px(px(44.))
                    .py(px(28.))
                    .bg(p.card)
                    .border_2()
                    .border_dashed()
                    .border_color(p.primary.opacity(0.65))
                    .rounded(px(12.))
                    .shadow_lg()
                    .child(
                        svg()
                            .path(sqlite.brand_icon())
                            .size(px(36.))
                            .flex_none()
                            .text_color(rgb(sqlite.brand_color())),
                    )
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.foreground)
                            .child("Drop the database file here"),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(p.muted_fg)
                            .child("SQLite: .db · .sqlite · .sqlite3 · .db3"),
                    ),
            )
    }
}

/// Settings dialog: interface scale (font / UI size) chooser.
impl DataBridge {
    /// "Drop table?" confirmation — same backdrop + occluding-card pattern
    /// as the settings dialog.
    fn render_drop_dialog(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::FontWeight;
        use gpui_component::button::{Button, ButtonVariants};
        use gpui_component::Disableable;
        let p = theme::palette(cx);
        let Some(pending) = self.pending_drop.as_ref() else {
            return div();
        };
        let what = if pending.meta.kind == NodeKind::View {
            "view"
        } else {
            "table"
        };
        let name = format!("{}.{}", pending.schema, pending.meta.name);
        let running = pending.running;
        let error = pending.error.clone();

        let backdrop = div()
            .id("drop-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::black().opacity(0.35))
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.pending_drop.as_ref().is_some_and(|p| p.running) {
                    this.cancel_drop(cx);
                }
            }));

        let card = v_flex()
            .id("drop-dialog")
            .absolute()
            .left_1_2()
            .top_1_2()
            .mt(px(-90.))
            .ml(px(-210.))
            .w(px(420.))
            .occlude()
            .bg(p.card)
            .border_1()
            .border_color(p.border)
            .rounded(px(10.))
            .shadow_lg()
            .overflow_hidden()
            .child(
                v_flex()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(
                                Icon::new(IconName::TriangleAlert)
                                    .with_size(Size::Size(px(16.)))
                                    .text_color(p.destructive),
                            )
                            .child(format!("Drop {what}?")),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(p.muted_fg)
                            .child(format!(
                                "{name} and all of its data will be permanently deleted. This cannot be undone."
                            )),
                    )
                    .when_some(error, |this, e| {
                        this.child(
                            div()
                                .px_2()
                                .py(px(6.))
                                .rounded(px(4.))
                                .bg(p.conn_err.opacity(0.1))
                                .text_size(px(12.5))
                                .text_color(p.conn_err)
                                .child(e),
                        )
                    }),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(p.border)
                    .child(
                        Button::new("drop-cancel")
                            .ghost()
                            .label("Cancel")
                            .disabled(running)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_drop(cx))),
                    )
                    .child(
                        Button::new("drop-confirm")
                            .danger()
                            .label(if running { "Dropping…" } else { "Drop" })
                            .disabled(running)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_drop(window, cx)
                            })),
                    ),
            );

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(backdrop)
            .child(card)
    }

    fn render_settings_dialog(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::{ClickEvent, FontWeight};
        let p = theme::palette(cx);
        let active = crate::ui::settings::current(cx);

        let option_row = |label: &str, hint: &str, s: crate::ui::settings::UiScale| {
            let is_active = s == active;
            h_flex()
                .id(SharedString::from(format!("scale-{}", s.label())))
                .w_full()
                .items_center()
                .gap_2()
                .px_3()
                .py(px(8.))
                .rounded(px(4.))
                .cursor_pointer()
                .border_1()
                .border_color(if is_active { p.primary } else { p.border })
                .bg(if is_active { p.accent } else { gpui::transparent_black() })
                .hover(move |st| st.bg(p.accent))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(13.5))
                        .text_color(p.foreground)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.muted_fg)
                        .child(hint.to_string()),
                )
                .when(is_active, |this| {
                    this.child(
                        Icon::new(IconName::Check)
                            .with_size(Size::Size(px(14.)))
                            .text_color(p.primary),
                    )
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    crate::ui::settings::set(s, Some(window), cx);
                    this.show_settings = false;
                    cx.notify();
                }))
        };

        // Click on the backdrop closes the dialog. The backdrop and the
        // card are SIBLINGS under a transparent layer so clicks inside
        // the card never bubble into the backdrop (same pattern as the
        // record editor modal in workspace.rs).
        let backdrop = div()
            .id("settings-backdrop")
            .absolute()
            // Pinned insets — see the shell comment on drag-frame layout.
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::black().opacity(0.35))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.show_settings = false;
                cx.notify();
            }));

        let card = v_flex()
            .id("settings-dialog")
            .absolute()
            .left_1_2()
            .top_1_2()
            // Optical centering: pull the card back by half its size.
            .mt(px(-170.))
            .ml(px(-190.))
            .w(px(380.))
            // BlockMouse: clicks over the card never count as backdrop
            // clicks (see the record editor modal in workspace.rs).
            .occlude()
            .bg(p.card)
            .border_1()
            .border_color(p.border)
            .rounded(px(10.))
            .shadow_lg()
            .overflow_hidden()
            .child(
                v_flex()
                    .flex_none()
                    .gap_1()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.muted_fg)
                            .child("Interface size — scales fonts and controls."),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .p_3()
                    .gap_2()
                    .child(option_row(
                        "Compact",
                        "85%",
                        crate::ui::settings::UiScale::Compact,
                    ))
                    .child(option_row(
                        "Normal",
                        "100%",
                        crate::ui::settings::UiScale::Normal,
                    ))
                    .child(option_row(
                        "Large",
                        "120%",
                        crate::ui::settings::UiScale::Large,
                    )),
            );

        // Transparent event-passing layer: the backdrop handles outside
        // clicks, the card sits on top as a sibling.
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(backdrop)
            .child(card)
    }
}

/// The "New table" starter statement per SQL dialect; `None` for stores
/// without SQL DDL. The schema name comes from introspection and is quoted
/// per dialect; the user edits the text before running it.
fn create_table_template(t: DbType, schema: &str) -> Option<String> {
    let sql = match t {
        DbType::Sqlite => "CREATE TABLE new_table (\n    \
             id INTEGER PRIMARY KEY AUTOINCREMENT,\n    \
             name TEXT NOT NULL,\n    \
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\n);"
            .to_string(),
        DbType::PostgreSql => format!(
            "CREATE TABLE \"{}\".new_table (\n    \
             id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,\n    \
             name TEXT NOT NULL,\n    \
             created_at TIMESTAMPTZ NOT NULL DEFAULT now()\n);",
            schema.replace('"', "\"\"")
        ),
        DbType::MySql | DbType::MariaDb => format!(
            "CREATE TABLE `{}`.new_table (\n    \
             id BIGINT AUTO_INCREMENT PRIMARY KEY,\n    \
             name VARCHAR(255) NOT NULL,\n    \
             created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP\n);",
            schema.replace('`', "``")
        ),
        DbType::MongoDb
        | DbType::Redis
        | DbType::ClickHouse
        | DbType::Cassandra
        | DbType::ScyllaDb => return None,
    };
    Some(sql)
}
