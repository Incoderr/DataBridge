//! SQL editor tab (Stage 5): a syntax-highlighted code editor on top, the
//! results grid below. One Run per statement — SELECT-like statements go
//! through the fetch path, everything else reports affected rows. The
//! editor autocompletes tables, columns and keywords and shows the
//! predicted next step as ghost text (`ui/completion.rs`, Tab or →
//! accepts); the editor's smart indent handles Enter after `(`, Ctrl+wheel
//! zooms the editor font. Ctrl+Enter runs; while a statement runs, Run
//! turns into Stop (`CancelToken`: the server is asked to cancel it where
//! the driver can). A result set or an error opens a results pane below
//! the editor behind a draggable divider (`v_resizable`); until then, and
//! for DDL/DML (the toolbar status reports them), the editor fills the tab.
//! A result set shows as a table or a chart (`core::chart` suggests the
//! axes; kind / X / series / aggregation are switchable in the pane's
//! header, and the choice survives re-runs with the same columns).

use std::cell::RefCell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Context, DispatchPhase, Div, Entity, IntoElement, KeyBinding, ParentElement, Render,
    ScrollEvent, SharedString, Styled, Subscription, WeakEntity, Window, actions, canvas, div,
    px, relative, svg,
};

use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Editor, EditorState, Enter, Escape, MoveRight, TabSize};
use gpui_component::resizable::{ResizableState, resizable_panel, v_resizable};
use gpui_component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{Disableable, Icon, IconName, Selectable as _, Sizable, Size, h_flex, v_flex};

use crate::core::chart::{self, Agg, ChartData, ChartKind, ChartSpec};
use crate::core::model::{group_digits, ColumnMeta, DbConnection, QueryPage, Row};
use crate::core::session::{CancelToken, SessionHandle};
use crate::ui::app::DataBridge;
use crate::ui::completion::{GhostSlot, MenuSlot, SqlCompletions};
use crate::ui::{settings, theme};

/// One indentation level in the SQL editor.
const INDENT: &str = "    ";

/// Rows kept in the result grid from one statement (like the web
/// version's `QUERY_LIMIT`).
pub const RESULT_LIMIT: u64 = 500;

actions!(query, [RunQuery, AcceptGhostOrRight]);

/// Key context of the SQL editor pane (scopes Ctrl+Enter to it — the AI
/// chat's input keeps Ctrl+Enter as a newline).
const EDITOR_CONTEXT: &str = "SqlEditor";

/// Ctrl+Enter (Cmd+Enter on macOS) runs the statement. Registered after
/// `gpui_component::init`, so it wins over the input's own
/// "secondary-enter" (newline) binding inside the SQL editor only.
///
/// → confirms the open completion menu's item, or accepts the grey ghost
/// suggestion at the cursor (else it moves right as usual).
pub fn init(cx: &mut App) {
    let keys = if cx.platform_info().is_mac() { "cmd-enter" } else { "ctrl-enter" };
    let context = format!("{EDITOR_CONTEXT} > Input");
    cx.bind_keys([
        KeyBinding::new(keys, RunQuery, Some(&context)),
        KeyBinding::new("right", AcceptGhostOrRight, Some(&context)),
    ]);
}

/// Statements that change the schema tree (SQL DDL, mongosh collection
/// calls).
fn is_ddl(sql: &str) -> bool {
    crate::core::model::changes_schema(sql)
}

// ---------------- Delegate: dynamic result columns ----------------

struct QueryDelegate {
    cols: Vec<Column>,
    /// Column labels for `render_th` (`Column` keeps its id private).
    names: Vec<String>,
    rows: Vec<Row>,
}

impl QueryDelegate {
    fn new(page: &QueryPage) -> Self {
        let mut cols = vec![Column::new("#", "#").width(px(44.)).fixed_left()];
        let mut names = vec!["#".into()];
        for c in &page.columns {
            cols.push(Column::new(c.name.clone(), c.name.clone()).width(px(150.)));
            names.push(c.name.clone());
        }
        Self {
            cols,
            names,
            rows: page.rows.clone(),
        }
    }
}

impl TableDelegate for QueryDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.cols.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.cols[col_ix].clone()
    }

    fn loading(&self, _: &App) -> bool {
        false
    }


    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = theme::palette(cx);
        if col_ix == 0 {
            return div().text_size(theme::scaled(theme::TEXT_XS)).text_color(p.muted_fg).child("#");
        }
        let c_name = self.names[col_ix].clone();
        div()
            .w_full()
            .min_w_0()
            .truncate()
            .text_size(theme::scaled(theme::TEXT_XS))
            .font_family(theme::MONO_FONT)
            .text_color(p.muted_fg)
            .child(c_name)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = theme::palette(cx);
        if col_ix == 0 {
            return div()
                .font_family(theme::MONO_FONT)
                .text_size(theme::scaled(theme::TEXT_XS))
                .text_color(p.muted_fg)
                .child((row_ix + 1).to_string())
                .into_any_element();
        }
        let v = self
            .rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix - 1))
            .map(|v| v.display())
            .unwrap_or_else(|| "null".into());
        div()
            .font_family(theme::MONO_FONT)
            .text_size(theme::scaled(theme::TEXT_MD))
            .text_color(p.muted_fg)
            .min_w_0()
            .truncate()
            .child(v)
            .into_any_element()
    }
}

// ---------------- QueryView ----------------

/// What the last run produced — drives the status line.
struct RunSummary {
    /// The statement returned a result set (SELECT-like), even if empty.
    has_columns: bool,
    rows: usize,
    affected: u64,
    truncated: bool,
    elapsed_ms: u32,
}

pub struct QueryView {
    #[allow(dead_code)]
    conn: DbConnection,
    session: SessionHandle,
    /// The root entity — successful DDL reloads its schema tree.
    app: WeakEntity<DataBridge>,
    editor: Entity<EditorState>,
    /// The ghost suggestion the completion provider last offered.
    ghost: GhostSlot,
    /// Where the completion menu last opened (→ confirms its item).
    menu: MenuSlot,
    /// Editor / results split (sizes survive re-renders).
    split: Entity<ResizableState>,
    /// Rebuilt after every run; `None` until the first statement.
    table: Option<Entity<TableState<QueryDelegate>>>,
    running: bool,
    run_id: u64,
    /// Stops the running statement (Stop button); `None` when idle.
    cancel: Option<CancelToken>,
    /// Stop was pressed: the run's outcome reads "cancelled", not an error.
    cancelling: bool,
    /// The last run ended by Stop.
    cancelled: bool,
    error: Option<String>,
    last: Option<RunSummary>,
    _obs: Option<Subscription>,
    /// The last result set (chart input).
    result_cols: Vec<ColumnMeta>,
    result_rows: Rc<Vec<Row>>,
    /// Results pane shows the chart instead of the table.
    show_chart: bool,
    /// Chart settings (`None`: nothing plottable) and their points.
    chart: Option<ChartSpec>,
    chart_data: Rc<ChartData>,
    /// Bumped on every new chart data — replays the draw-in animation.
    chart_seq: u64,
}

/// Chart kind icons (Lucide).
fn kind_icon(k: ChartKind) -> &'static str {
    match k {
        ChartKind::Bar => "icons/chart-column.svg",
        ChartKind::Line => "icons/chart-line.svg",
        ChartKind::Area => "icons/chart-area.svg",
        ChartKind::Pie => "icons/chart-pie.svg",
    }
}

fn kind_label(k: ChartKind) -> SharedString {
    match k {
        ChartKind::Bar => tr!("chart.bar"),
        ChartKind::Line => tr!("chart.line"),
        ChartKind::Area => tr!("chart.area"),
        ChartKind::Pie => tr!("chart.pie"),
    }
}

fn agg_label(a: Agg) -> SharedString {
    match a {
        Agg::Sum => tr!("chart.agg_sum"),
        Agg::Avg => tr!("chart.agg_avg"),
        Agg::Min => tr!("chart.agg_min"),
        Agg::Max => tr!("chart.agg_max"),
        Agg::Count => tr!("chart.agg_count"),
    }
}

impl QueryView {
    pub fn new(
        conn: DbConnection,
        session: SessionHandle,
        seed: Option<String>,
        app: WeakEntity<DataBridge>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ghost: GhostSlot = Rc::new(RefCell::new(None));
        let menu: MenuSlot = Rc::new(RefCell::new(None));
        let editor = cx.new(|cx| {
            // Smart indent (structural brackets) and bracket auto-closing
            // come with the editor: Enter after `(` indents one level.
            let mut s = EditorState::new(window, cx)
                .language("sql")
                .tab_size(TabSize {
                    tab_size: INDENT.len(),
                    hard_tabs: false,
                })
                .placeholder("SELECT * FROM table_name LIMIT 50;");
            if let Some(seed) = seed {
                s.set_value(seed, window, cx);
            }
            // Attached after the seed: `set_value` fires the completion
            // trigger, and the provider reads `DataBridge`, which is being
            // updated while this view is created.
            s.lsp_mut().completion_provider = Some(Rc::new(SqlCompletions {
                app: app.clone(),
                dialect: conn.db_type,
                ghost: ghost.clone(),
                menu: menu.clone(),
            }));
            s
        });
        Self {
            conn,
            session,
            app,
            editor,
            ghost,
            menu,
            split: cx.new(|_| ResizableState::default()),
            table: None,
            running: false,
            run_id: 0,
            cancel: None,
            cancelling: false,
            cancelled: false,
            error: None,
            last: None,
            _obs: None,
            result_cols: Vec::new(),
            result_rows: Rc::new(Vec::new()),
            show_chart: false,
            chart: None,
            chart_data: Rc::new(ChartData::default()),
            chart_seq: 0,
        }
    }

    /// Run the editor's statement (stale-guarded: only the latest Run
    /// applies, mirroring `RecordsDelegate::load`).
    pub fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).value().to_string();
        if sql.trim().is_empty() {
            self.error = Some(tr!("query.empty").into());
            cx.notify();
            return;
        }
        if self.running {
            return;
        }
        self.running = true;
        self.cancelling = false;
        self.cancelled = false;
        self.error = None;
        self.run_id += 1;
        let id = self.run_id;
        let ddl = is_ddl(&sql);
        cx.notify();
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        let rx = self.session.execute(sql, RESULT_LIMIT, cancel);
        cx.spawn_in(window, async move |this, cx| {
            let result = rx.await;
            cx.update(|window, cx| {
                this.update(cx, |q, cx| {
                    if q.run_id != id {
                        return; // a newer run superseded this one
                    }
                    q.running = false;
                    q.cancel = None;
                    let cancelled = std::mem::take(&mut q.cancelling);
                    match result {
                        // Stopped by the user: whatever came back (usually
                        // the server's "canceling statement" error) is moot.
                        _ if cancelled => {
                            q.cancelled = true;
                            // A DDL statement may have committed anyway.
                            if ddl {
                                q.app.update(cx, |app, cx| app.reload_schemas(cx)).ok();
                            }
                        }
                        Ok(Ok(page)) => {
                            q.apply_result(page, window, cx);
                            // New / dropped / altered objects: refresh the tree.
                            if ddl {
                                q.app.update(cx, |app, cx| app.reload_schemas(cx)).ok();
                            }
                        }
                        Ok(Err(e)) => q.error = Some(e.message().to_string()),
                        Err(_) => {}
                    }
                    cx.notify();
                })
                .ok();
            })
            .ok();
        })
        .detach();
    }

    /// →: confirm the completion menu's selected item if the menu is open
    /// (as Enter does), else insert the ghost suggestion if it was computed
    /// for exactly this text and cursor, else move right.
    fn accept_ghost_or_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, cursor) = {
            let s = self.editor.read(cx);
            (s.value(), s.cursor())
        };
        let menu_open = self
            .menu
            .borrow_mut()
            .take_if(|m| m.offset == cursor && m.text.as_str() == text.as_ref())
            .is_some();
        if menu_open {
            // The editor routes Enter to its open completion menu first.
            window.dispatch_action(Box::new(Enter { secondary: false, shift: false }), cx);
            return;
        }
        let ghost = self
            .ghost
            .borrow_mut()
            .take_if(|g| g.offset == cursor && g.text.as_str() == text.as_ref());
        match ghost {
            Some(g) => self.editor.update(cx, |s, cx| s.insert(g.insert, window, cx)),
            None => window.dispatch_action(Box::new(MoveRight), cx),
        }
    }

    /// Stop the running statement. The run ends once the driver reports
    /// back (at most `CANCEL_GRACE` after the server was asked to stop).
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
            self.cancelling = true;
            cx.notify();
        }
    }

    fn apply_result(&mut self, page: QueryPage, window: &mut Window, cx: &mut Context<Self>) {
        self.last = Some(RunSummary {
            has_columns: !page.columns.is_empty(),
            rows: page.rows.len(),
            affected: page.rows_affected,
            truncated: page.truncated,
            elapsed_ms: page.elapsed_ms,
        });
        self.error = None;
        if page.columns.is_empty() {
            // DDL / DML without RETURNING: the results pane shows the
            // outcome instead of a grid.
            self.table = None;
            self._obs = None;
            return;
        }
        // Keep the chart settings when the columns did not change (the
        // same query re-run); otherwise suggest new ones.
        let same_cols = self.result_cols.len() == page.columns.len()
            && self.result_cols.iter().zip(&page.columns).all(|(a, b)| a.name == b.name);
        self.result_cols = page.columns.clone();
        self.result_rows = Rc::new(page.rows.clone());
        if !same_cols || self.chart.is_none() {
            self.chart = chart::suggest(&self.result_cols, &self.result_rows);
        }
        self.rebuild_chart();
        let delegate = QueryDelegate::new(&page);
        let state = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .col_resizable(true)
        });
        self._obs = Some(cx.observe(&state, |_, _, cx| cx.notify()));
        settle_stripes(&state, window, cx);
        self.table = Some(state);
    }
}

impl QueryView {
    fn rebuild_chart(&mut self) {
        self.chart_data = Rc::new(match &self.chart {
            Some(spec) => chart::build(spec, &self.result_cols, &self.result_rows, &tr!("chart.other")),
            None => ChartData::default(),
        });
        self.chart_seq += 1;
    }

    fn update_chart(&mut self, f: impl FnOnce(&mut ChartSpec), cx: &mut Context<Self>) {
        if let Some(spec) = &mut self.chart {
            f(spec);
            self.rebuild_chart();
            cx.notify();
        }
    }

    /// Table / chart switch plus, in chart mode, the chart settings.
    fn render_results_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = theme::palette(cx);
        let view = cx.entity().downgrade();
        let icon = |path: &'static str, active: bool| {
            svg().path(path).size_3p5().text_color(if active { p.primary } else { p.muted_fg })
        };
        let sep = || div().mx_1().h(theme::scaled(18.)).w(px(1.)).bg(p.toolbar_border);
        let mut bar = h_flex()
            .h(theme::scaled(34.))
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .border_b_1()
            .border_color(p.toolbar_border)
            .bg(p.toolbar)
            .child(
                Button::new("res-table")
                    .xsmall()
                    .ghost()
                    .selected(!self.show_chart)
                    .child(icon("icons/table.svg", !self.show_chart))
                    .label(tr!("chart.table"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_chart = false;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("res-chart")
                    .xsmall()
                    .ghost()
                    .selected(self.show_chart)
                    .disabled(self.chart.is_none())
                    .tooltip(if self.chart.is_none() { tr!("chart.nothing") } else { tr!("chart.title") })
                    .child(icon("icons/chart-column.svg", self.show_chart))
                    .label(tr!("chart.title"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_chart = true;
                        this.chart_seq += 1;
                        cx.notify();
                    })),
            );
        let Some(spec) = self.chart.clone().filter(|_| self.show_chart) else {
            return bar.into_any_element();
        };
        bar = bar.child(sep());
        for k in ChartKind::ALL {
            bar = bar.child(
                Button::new(SharedString::from(format!("chart-kind-{k:?}")))
                    .xsmall()
                    .ghost()
                    .selected(spec.kind == k)
                    .tooltip(kind_label(k))
                    .child(icon(kind_icon(k), spec.kind == k))
                    .on_click(cx.listener(move |this, _, _, cx| this.update_chart(|s| s.kind = k, cx))),
            );
        }
        let names: Vec<String> = self.result_cols.iter().map(|c| c.name.clone()).collect();
        let numeric: Vec<usize> = (0..names.len())
            .filter(|&i| chart::is_numeric(i, &self.result_cols, &self.result_rows))
            .collect();

        let x_menu = {
            let (view, names) = (view.clone(), names.clone());
            let current = spec.x;
            Button::new("chart-x")
                .xsmall()
                .outline()
                .label(format!("X: {}", names.get(current).cloned().unwrap_or_default()))
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu.scrollable(true).max_h(px(320.));
                    for (i, name) in names.iter().enumerate() {
                        let view = view.clone();
                        menu = menu.item(PopupMenuItem::new(name.clone()).checked(i == current).on_click(
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.update_chart(
                                        |s| {
                                            s.x = i;
                                            s.ys.retain(|&y| y != i);
                                        },
                                        cx,
                                    )
                                });
                            },
                        ));
                    }
                    menu
                })
        };
        let y_label = if spec.agg == Agg::Count {
            tr!("chart.agg_count").to_string()
        } else {
            let ys: Vec<&str> = spec
                .ys
                .iter()
                .take(if spec.kind.single_series() { 1 } else { chart::MAX_SERIES })
                .filter_map(|&i| names.get(i).map(String::as_str))
                .collect();
            if ys.is_empty() { "—".into() } else { ys.join(", ") }
        };
        let y_menu = {
            let (view, names, ys) = (view.clone(), names.clone(), spec.ys.clone());
            let numeric = numeric.clone();
            let single = spec.kind.single_series();
            Button::new("chart-y")
                .xsmall()
                .outline()
                .label(format!("Y: {y_label}"))
                .disabled(numeric.is_empty() || spec.agg == Agg::Count)
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu.scrollable(true).max_h(px(320.));
                    for &i in &numeric {
                        let view = view.clone();
                        let on = ys.contains(&i);
                        menu = menu.item(PopupMenuItem::new(names[i].clone()).checked(on).on_click(
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.update_chart(|s| toggle_series(s, i, single), cx)
                                });
                            },
                        ));
                    }
                    menu
                })
        };
        let agg_menu = {
            let view = view.clone();
            let current = spec.agg;
            let has_numeric = !numeric.is_empty();
            let first_num = numeric.iter().copied().find(|&i| i != spec.x);
            Button::new("chart-agg")
                .xsmall()
                .outline()
                .label(agg_label(current))
                .tooltip(tr!("chart.agg_hint"))
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu;
                    for a in Agg::ALL {
                        if a != Agg::Count && !has_numeric {
                            continue;
                        }
                        let view = view.clone();
                        menu = menu.item(PopupMenuItem::new(agg_label(a)).checked(a == current).on_click(
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.update_chart(
                                        |s| {
                                            s.agg = a;
                                            // Back from Count: pick a series.
                                            if a != Agg::Count && s.ys.is_empty() {
                                                if let Some(y) = first_num.filter(|&y| y != s.x) {
                                                    s.ys.push(y);
                                                }
                                            }
                                        },
                                        cx,
                                    )
                                });
                            },
                        ));
                    }
                    menu
                })
        };
        bar = bar
            .child(sep())
            .child(x_menu)
            .child(y_menu)
            .child(agg_menu)
            .child(div().flex_1());
        if self.chart_data.clipped {
            bar = bar.child(
                div()
                    .text_size(theme::scaled(theme::TEXT_XS))
                    .text_color(p.muted_fg)
                    .child(if spec.kind.single_series() {
                        tr!("chart.clipped_categories", n = chart::MAX_CATEGORIES)
                    } else {
                        tr!("chart.clipped_points", n = chart::MAX_POINTS)
                    }),
            );
        }
        if !spec.kind.single_series() && self.chart_data.series.len() > 1 {
            bar = bar.child(crate::ui::chart::legend(&self.chart_data, cx));
        }
        bar.into_any_element()
    }

    fn render_chart_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = theme::palette(cx);
        match &self.chart {
            Some(spec) if !self.chart_data.is_empty() => {
                crate::ui::chart::chart_element(&self.chart_data, spec.kind, self.chart_seq, cx)
            }
            _ => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme::scaled(theme::TEXT_MD))
                .text_color(p.muted_fg)
                .child(tr!("chart.nothing"))
                .into_any_element(),
        }
    }
}

/// Y menu click: bar / pie draw one series (the clicked one becomes it);
/// line / area toggle it (at least one stays, at most `MAX_SERIES`).
fn toggle_series(s: &mut ChartSpec, i: usize, single: bool) {
    if single {
        s.ys.retain(|&y| y != i);
        s.ys.insert(0, i);
    } else if s.ys.contains(&i) {
        if s.ys.len() > 1 {
            s.ys.retain(|&y| y != i);
        }
    } else if s.ys.len() < chart::MAX_SERIES {
        s.ys.push(i);
    }
    if s.x == i {
        // The X column became a series: move X to the first free column.
        s.x = (0..).find(|c| !s.ys.contains(c)).unwrap_or(0);
    }
}

impl Render for QueryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);

        let status: String = match (&self.last, self.running) {
            (_, true) if self.cancelling => tr!("query.cancelling").into(),
            (_, true) => tr!("query.running_status").into(),
            _ if self.cancelled => tr!("query.cancelled").into(),
            (Some(RunSummary { affected, elapsed_ms, .. }), _) if *affected > 0 => tr!("query.affected", n = affected, ms = elapsed_ms).to_string(),
            (Some(RunSummary { has_columns: false, elapsed_ms, .. }), _) => {
                tr!("query.executed", ms = elapsed_ms).to_string()
            }
            (Some(RunSummary { rows, truncated, elapsed_ms, .. }), _) => tr!(
                "query.rows",
                n = group_digits(*rows as u64),
                more = if *truncated { tr!("query.truncated").to_string() } else { String::new() },
                ms = elapsed_ms
            )
            .to_string(),
            (None, _) => tr!("query.idle").into(),
        };

        let toolbar = h_flex()
            .h(theme::scaled(38.))
            .flex_none()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(p.toolbar_border)
            .bg(p.toolbar)
            .child(if self.running {
                Button::new("stop")
                    .danger()
                    .small()
                    .icon(IconName::CircleX)
                    .label(tr!("query.stop"))
                    .tooltip(tr!("query.stop_hint"))
                    .disabled(self.cancelling)
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
            } else {
                Button::new("run")
                    .primary()
                    .small()
                    .label(tr!("common.run"))
                    .tooltip(tr!("query.run_hint"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run(window, cx);
                    }))
            })
            .child(
                div()
                    .text_size(theme::scaled(theme::TEXT_SM))
                    .text_color(if self.error.is_some() {
                        p.conn_err
                    } else {
                        p.muted_fg
                    })
                    .child(if let Some(e) = &self.error {
                        e.clone()
                    } else {
                        status.clone()
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(theme::scaled(theme::TEXT_XS))
                    .text_color(p.muted_fg)
                    .child(tr!("query.limit", n = RESULT_LIMIT)),
            );

        // The editor pane; the code editor scrolls internally.
        let editor_pane = v_flex()
            .key_context(EDITOR_CONTEXT)
            .on_action(cx.listener(|this, _: &RunQuery, window, cx| this.run(window, cx)))
            .on_action(cx.listener(|this, _: &AcceptGhostOrRight, window, cx| {
                this.accept_ghost_or_right(window, cx)
            }))
            // Escape hides the menu and the ghost: forget them too (capture: the editor
            // still handles the key).
            .capture_action(cx.listener(|this, _: &Escape, _, _| {
                this.ghost.borrow_mut().take();
                this.menu.borrow_mut().take();
            }))
            .size_full()
            .bg(p.card)
            .p_2()
            .relative()
            .child(
                Editor::new(&self.editor)
                    .h(relative(1.))
                    .text_size(theme::scaled(settings::editor_font_size(cx))),
            )
            // Ctrl+wheel zoom: captured before the editor scrolls itself.
            // Pinned, last child (see AGENTS.md gotchas); no hitbox, so the
            // editor keeps every other mouse event.
            .child(
                canvas(
                    |_, _, _| {},
                    |bounds, _, window, _| {
                        window.on_mouse_event(move |e: &ScrollEvent, phase, window, cx| {
                            if phase != DispatchPhase::Capture
                                || !e.modifiers.secondary(cx)
                                || !bounds.contains(&e.position)
                            {
                                return;
                            }
                            let dy = e.delta.pixel_delta(px(16.)).y;
                            if dy != px(0.) {
                                let step = if dy > px(0.) { 1. } else { -1. };
                                settings::bump_editor_font_size(step, cx);
                                window.refresh();
                            }
                            cx.stop_propagation();
                        });
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );

        // The results pane — only for a result set or an error; otherwise
        // (nothing run yet, DDL/DML: the toolbar status says it) the
        // editor keeps the whole tab.
        let results: Option<Div> = if let Some(e) = self.error.clone() {
            Some(div().size_full().p_2().child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(8.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(p.conn_err.opacity(0.4))
                    .bg(p.conn_err.opacity(0.1))
                    .text_color(p.conn_err)
                    .text_size(theme::scaled(theme::TEXT_MD))
                    .child(Icon::new(IconName::TriangleAlert).with_size(Size::Size(px(15.))))
                    .child(e),
            ))
        } else {
            self.table.clone().map(|state| {
                let body = if self.show_chart {
                    self.render_chart_body(cx)
                } else {
                    DataTable::new(&state)
                        .stripe(true)
                        .bordered(false)
                        .with_size(Size::Large)
                        .into_any_element()
                };
                v_flex()
                    .size_full()
                    .child(self.render_results_header(cx))
                    .child(div().flex_1().min_h_0().relative().child(body))
            })
        };

        let body = match results {
            Some(results) => v_resizable("query-split")
                .with_state(&self.split)
                .child(
                    resizable_panel()
                        .size_range(theme::scaled(80.).to_pixels(window.rem_size())..px(f32::MAX))
                        .child(editor_pane),
                )
                .child(
                    resizable_panel()
                        .size(theme::scaled(260.).to_pixels(window.rem_size()))
                        .size_range(theme::scaled(60.).to_pixels(window.rem_size())..px(f32::MAX))
                        .child(results),
                )
                .into_any_element(),
            None => editor_pane.into_any_element(),
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(toolbar)
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// gpui-component sizes the stripe filler rows from the table's bounds of
/// the *previous* frame, so a freshly created table paints without them
/// until something else repaints it. Repaint once more two frames later —
/// the first of them may still be the table's very first paint.
pub fn settle_stripes<D: TableDelegate>(
    state: &Entity<TableState<D>>,
    window: &mut Window,
    _: &mut App,
) {
    let state = state.downgrade();
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |_, cx| {
            state.update(cx, |_, cx| cx.notify()).ok();
        });
    });
}
