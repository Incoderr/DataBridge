//! Root application view: connection state, tab management, layout.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    canvas, div, px, svg, App, Context, DispatchPhase, Entity, IntoElement, MouseButton,
    MouseMoveEvent, MouseUpEvent, ParentElement, SharedString, Styled, Window,
};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::{h_flex, v_flex};
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, Size};

use crate::core::mock;
use crate::core::model::{ConnectionStatus, DbConnection, DbType, SchemaMeta, TableMeta};
use crate::core::registry;
use crate::core::session::SessionHandle;
use crate::ui::diagram::SchemaDiagram;
use crate::ui::nav_tree::SelectedPath;
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

/// The workspace view of the active tab: the records grid or the schema
/// diagram. Recreated on every tab switch (like the web version's keyed
/// remount).
#[derive(Clone)]
pub enum WorkspaceView {
    Records(Entity<RecordsView>),
    Diagram(Entity<SchemaDiagram>),
}

pub struct DataBridge {
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
    /// Settings dialog visibility (interface scale chooser).
    pub show_settings: bool,
    // visual state
    pub grid_view: bool,
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

        Self {
            connections: mock::seed_connections(),
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
            show_settings: false,
            grid_view: true,
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
        });
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.tabs.remove(ix);
        self.view = None;
        match self.active_tab {
            Some(a) if a == ix => {
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

    /// Back to the connection wizard (disconnect / new connection).
    pub fn terminate(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.active_conn.as_ref().map(|c| c.id.clone()) {
            if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
                c.status = ConnectionStatus::Disconnected;
            }
        }
        self.active_conn = None;
        self.session = None;
        self.schemas.clear();
        self.tabs.clear();
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
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn.id) {
            *c = conn.clone();
        } else {
            self.connections.push(conn.clone());
        }
        self.active_conn = Some(conn);
        self.session = Some(session.clone());
        self.schemas.clear();
        self.tabs.clear();
        self.active_tab = None;
        self.view = None;
        self.query.clear();
        self.expanded = HashSet::from(["conn".to_string(), "schemas".to_string()]);
        self.wizard.phase = Phase::Choose;
        self.wizard.error = None;

        let rx = session.list_schemas();
        cx.spawn(async move |this, cx| {
            match rx.await {
                Ok(Ok(schemas)) => {
                    this.update(cx, |this, cx| {
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
                TabKind::Diagram => None,
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
        if self.active_conn.is_none() {
            return self.render_wizard(window, cx).into_any_element();
        }

        let p = theme::palette(cx);
        let resizing = self.resizing_nav;
        let view = cx.entity();
        let layout = div()
            .id("app")
            .size_full()
            .bg(p.background)
            .text_color(p.foreground)
            .when(resizing, |this| this.cursor_col_resize())
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
                                None => crate::ui::workspace::empty_state(cx).into_any_element(),
                            }),
                    ),
            );

        // Settings overlay (interface scale chooser) on top of the layout.
        if self.show_settings {
            let mut root = div().relative().size_full();
            if resizing {
                root = root.cursor_col_resize();
            }
            root.child(layout)
                .child(self.render_settings_dialog(cx))
                .into_any_element()
        } else {
            layout.into_any_element()
        }
    }
}

/// Settings dialog: interface scale (font / UI size) chooser.
impl DataBridge {
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

        // Click on the backdrop closes the dialog.
        let backdrop = div()
            .id("settings-backdrop")
            .absolute()
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
            .w(px(380.))
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

        backdrop.child(card)
    }
}
