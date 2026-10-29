use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    canvas, AnyElement, App, ClickEvent, ClipboardItem, Context, DispatchPhase, Div, Entity,
    FontWeight, Hsla, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Corner, ParentElement, Render, SharedString, Stateful, Styled, Subscription, Window, div, px,
    svg,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::table::{Column, Table, TableDelegate, TableState};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::{Disableable, Icon, IconName, Sizable, Size, h_flex, v_flex};

use crate::core::model::{
    group_digits, parse_cell, status_line, CellChange, ColumnKind, ColumnMeta, ConnectionStatus,
    DbConnection, Row, TableMeta, TableRef, Value,
};
use crate::core::session::SessionHandle;
use crate::ui::theme;

pub const PAGE: u64 = 50;

#[derive(Clone)]
pub enum TabKind {
    /// The paged records grid of a table/view.
    Records(Arc<TableMeta>),
    /// The Supabase-style relationships canvas for the whole schema.
    Diagram,
    /// The SQL editor (Stage 5), optionally seeded with starter text
    /// (the "New table" template).
    Sql(Option<String>),
}

#[derive(Clone)]
pub struct OpenTab {
    pub id: String,
    pub schema: String,
    pub kind: TabKind,
}

// ---------------- Delegate: data source of the records grid ----------------

pub struct RecordsDelegate {
    conn: DbConnection,
    session: SessionHandle,
    #[allow(dead_code)]
    pub schema: String,
    table: Arc<TableMeta>,
    cols: Vec<Column>,
    pub rows: Vec<Row>,
    pub loading: bool,
    pub error: Option<String>,
    pub page: u64,
    pub total: u64,
    pub elapsed_ms: u32,
    pub load_id: u64,
    /// The selected row, shared with `RecordsView` (which syncs it from
    /// `TableState` on every render). The crate's own selection overlay paints
    /// OVER the cell contents, so we draw the solid background in `render_tr`.
    sel: Rc<Cell<Option<usize>>>,
}

fn column_min_width(c: &ColumnMeta) -> f32 {
    match c.kind {
        ColumnKind::Json => 240.,
        ColumnKind::Text => 165.,
        ColumnKind::Date => 195.,
        ColumnKind::Uuid => 260.,
        ColumnKind::Binary => 120.,
        ColumnKind::Float => 132.,
        ColumnKind::Int => 100.,
        ColumnKind::Bool => 72.,
    }
}

impl RecordsDelegate {
    pub fn new(
        conn: DbConnection,
        session: SessionHandle,
        schema: String,
        table: Arc<TableMeta>,
        sel: Rc<Cell<Option<usize>>>,
    ) -> Self {
        let mut cols = vec![Column::new("#", "#").width(px(44.)).fixed_left()];
        for c in &table.columns {
            cols.push(Column::new(c.name.clone(), c.name.clone()).width(px(column_min_width(c))));
        }
        let total = table.row_count;
        Self {
            conn,
            session,
            schema,
            table,
            cols,
            rows: Vec::new(),
            loading: false,
            error: None,
            page: 0,
            total,
            elapsed_ms: 0,
            load_id: 0,
            sel,
        }
    }

    /// Stretch the data columns so the grid fills `avail_w` px of width.
    /// Columns never shrink below their minimum; extra space is distributed
    /// proportionally (mirrors a "fill viewport" data grid).
    pub fn relayout(&mut self, avail_w: f32) {
        const ROW_NO_W: f32 = 44.0;
        // render_last_empty_col (12px) + slack for borders/scrollbar.
        const CHROME: f32 = 16.0;
        let usable = avail_w - ROW_NO_W - CHROME;
        let mins: Vec<f32> = self.table.columns.iter().map(column_min_width).collect();
        let sum: f32 = mins.iter().sum();
        if sum <= 0.0 || usable <= sum {
            return;
        }
        let scale = usable / sum;
        let mut used = 0.0;
        let last = self.cols.len() - 1;
        for (i, min) in mins.iter().enumerate() {
            let w = if i == last { usable - used } else { (min * scale).floor() };
            used += w;
            self.cols[i + 1].width = px(w);
        }
    }

    pub fn stop(&mut self) {
        self.loading = false;
        self.load_id += 1;
        self.sel.set(None);
    }

    /// Fetch the current page through the session (stale-guarded by load_id).
    pub fn load(&mut self, cx: &mut Context<TableState<Self>>) {
        if self.conn.status != ConnectionStatus::Connected {
            self.error = Some("connection is not active".into());
            cx.notify();
            return;
        }
        self.loading = true;
        self.error = None;
        self.load_id += 1;
        let cur_id = self.load_id;
        cx.notify();
        let table = TableRef::of(&self.table);
        let offset = self.page * PAGE;
        let rx = self.session.fetch_rows(table, offset, PAGE);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |state, cx| {
                let d = state.delegate_mut();
                if d.load_id == cur_id {
                    match result {
                        Ok(Ok(page)) => {
                            d.rows = page.rows;
                            d.total = page.total;
                            d.elapsed_ms = page.elapsed_ms;
                            d.error = None;
                        }
                        Ok(Err(e)) => {
                            d.error = Some(e.message().to_string());
                        }
                        // The session end went away mid-flight (app teardown):
                        // nothing to apply — just stop the spinner.
                        Err(_) => {}
                    }
                    d.loading = false;
                    // New page fetched: drop the stale row selection, like the
                    // web version's `setSelectedRow(null)` on rows change.
                    d.sel.set(None);
                    state.clear_selection(cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn prev_page(&mut self, cx: &mut Context<TableState<Self>>) {
        if self.page > 0 {
            self.page -= 1;
            self.load(cx);
        }
    }

    pub fn next_page(&mut self, cx: &mut Context<TableState<Self>>) {
        if (self.page + 1) * PAGE < self.total {
            self.page += 1;
            self.load(cx);
        }
    }
}

impl TableDelegate for RecordsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.cols.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        &self.cols[col_ix]
    }

    fn loading(&self, _: &App) -> bool {
        self.loading
    }

    fn is_eof(&self, _: &App) -> bool {
        true
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let p = theme::palette(cx);
        div()
            .id(("row", row_ix))
            .when(self.sel.get() == Some(row_ix), |this| this.bg(p.table_selected))
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = theme::palette(cx);
        if col_ix == 0 {
            return div()
                .text_size(px(11.))
                .text_color(p.muted_fg)
                .child("#")
                .into_any_element();
        }
        let c = &self.table.columns[col_ix - 1];
        let mut name = div().flex_1().min_w_0().truncate().child(c.name.clone());
        if c.primary_key {
            name = name.font_weight(FontWeight::SEMIBOLD);
        }
        h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .overflow_x_hidden()
            .child(name)
            .when(c.primary_key, |this| {
                this.child(
                    div()
                        .px(px(3.))
                        .rounded(px(2.))
                        .text_size(px(10.))
                        .bg(p.primary.opacity(0.2))
                        .text_color(p.primary)
                        .child("PK"),
                )
            })
            .child(
                div()
                    .flex_shrink_0()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(10.5))
                    .text_color(theme::kind_color(&c.kind, &p))
                    .child(c.type_.to_string()),
            )
            .into_any_element()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = theme::palette(cx);
        let selected = self.sel.get() == Some(row_ix);
        if col_ix == 0 {
            let n = self.page * PAGE + row_ix as u64 + 1;
            return div()
                .font_family(theme::MONO_FONT)
                .text_size(px(11.))
                .text_color(if selected { p.primary_fg } else { p.muted_fg })
                .child(n.to_string())
                .into_any_element();
        }
        let c = &self.table.columns[col_ix - 1];
        // Safe row access: a live table may have changed since introspection.
        let v = self.rows.get(row_ix).and_then(|r| r.get(col_ix - 1));
        cell_value(v, c, &p, selected).into_any_element()
    }
}

/// Colored cell content; mirrors `CellValue` of the web version. Selected
/// rows render white-on-blue, so every cell switches to the primary fg.
fn cell_value(v: Option<&Value>, c: &ColumnMeta, p: &theme::Palette, selected: bool) -> AnyElement {
    let fg = if selected { p.primary_fg } else { p.muted_fg };
    let null = |fg: Hsla| {
        div()
            .font_family(theme::MONO_FONT)
            .italic()
            .text_size(px(13.5))
            .text_color(fg.opacity(0.55))
            .child(if c.nullable { "null" } else { "NULL" })
            .into_any_element()
    };
    let Some(v) = v else {
        return null(fg);
    };
    let (color, body): (Hsla, String) = match v {
        Value::Null => return null(fg),
        Value::Int(i) => (p.number, i.to_string()),
        Value::Float(f) => (p.number, format!("{f:.2}")),
        Value::Bool(b) => (p.keyword, b.to_string()),
        Value::Bytes(_) => (p.comment, v.display()),
        Value::Str(s) => match c.kind {
            ColumnKind::Json => (p.json, s.clone()),
            ColumnKind::Date => (p.date, s.clone()),
            ColumnKind::Uuid => (p.uuid, s.clone()),
            ColumnKind::Binary => (p.comment, s.clone()),
            // Numbers can arrive as strings (e.g. DECIMAL): no quotes.
            ColumnKind::Float | ColumnKind::Int => (p.number, s.clone()),
            ColumnKind::Bool => (p.keyword, s.clone()),
            _ => (p.string, format!("\"{s}\"")),
        },
    };
    div()
        .font_family(theme::MONO_FONT)
        .text_size(px(13.5))
        .text_color(if selected { p.primary_fg } else { color })
        .min_w_0()
        .truncate()
        .child(body)
        .into_any_element()
}

// ---------------- Simplified record editor ----------------

/// Which write the modal is performing.
enum EditorMode {
    /// New row: every column editable, empty inputs.
    Insert,
    /// Update the selected row: only changed cells are sent.
    Update { row: Row, row_no: u64 },
    /// Delete confirmation for the selected row.
    Delete { row: Row, row_no: u64 },
}

/// Choices for one foreign-key column of the editor: values of the
/// referenced column, labelled with the target's first text column.
struct FkLookup {
    target: String,
    loading: bool,
    error: Option<String>,
    /// (value to put into the input, menu label).
    options: Arc<Vec<(SharedString, SharedString)>>,
}

/// How many referenced rows the FK picker offers.
const FK_OPTIONS: u64 = 200;

struct EditorState {
    mode: EditorMode,
    /// Per column: the FK picker when the column is a foreign key.
    fk: Vec<Option<FkLookup>>,
    /// One input per table column (prefilled for updates).
    inputs: Vec<Entity<InputState>>,
    /// Which inputs render read-only (PK / binary on update).
    locked: Vec<bool>,
    error: Option<String>,
    saving: bool,
    save_id: u64,
}

// ---------------- RecordsView: chrome around the table ----------------

pub struct RecordsView {
    conn: DbConnection,
    schema: String,
    table: Arc<TableMeta>,
    /// Handle to the root app view — used to open the settings dialog.
    app: gpui::WeakEntity<crate::ui::app::DataBridge>,
    state: Entity<TableState<RecordsDelegate>>,
    show_detail: bool,
    detail_height: f32,
    resizing_detail: bool,
    resize_start_y: f32,
    resize_start_h: f32,
    export_copied: bool,
    copied_cell: Option<usize>,
    /// Selected row shared with the delegate; synced from `TableState` on
    /// every render of this view.
    sel: Rc<Cell<Option<usize>>>,
    last_sel: Option<usize>,
    /// Last width reported for the table area (drives column stretching).
    last_table_w: f32,
    /// The simplified record editor modal, when open.
    editor: Option<EditorState>,
    /// Stale guard of the editor's FK lookups (bumped per open / close).
    fk_gen: u64,
    _obs: Subscription,
}

impl RecordsView {
    pub fn new(
        conn: DbConnection,
        session: SessionHandle,
        schema: String,
        table: Arc<TableMeta>,
        app: gpui::WeakEntity<crate::ui::app::DataBridge>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sel: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
        let delegate = RecordsDelegate::new(
            conn.clone(),
            session,
            schema.clone(),
            table.clone(),
            sel.clone(),
        );
        let state = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .row_selectable(true)
                .col_selectable(false)
                .col_resizable(true)
        });
        state.update(cx, |s, cx| s.delegate_mut().load(cx));
        let obs = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            conn,
            schema,
            table,
            app,
            state,
            show_detail: false,
            detail_height: 190.0,
            resizing_detail: false,
            resize_start_y: 0.0,
            resize_start_h: 190.0,
            export_copied: false,
            copied_cell: None,
            sel,
            last_sel: None,
            last_table_w: 0.0,
            editor: None,
            fk_gen: 0,
            _obs: obs,
        }
    }

    /// Paint-time callback: the table area reports its laid-out width and the
    /// columns are stretched to fill it (never below their minimum widths).
    fn sync_table_width(&mut self, w: f32, cx: &mut Context<Self>) {
        if (w - self.last_table_w).abs() < 1.0 {
            return;
        }
        self.last_table_w = w;
        self.state.update(cx, |s, cx| {
            s.delegate_mut().relayout(w);
            s.refresh(cx);
            cx.notify();
        });
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| s.delegate_mut().load(cx));
    }

    /// The selected row's full data (from the current page).
    fn selected_row(&self, cx: &App) -> Option<(usize, Row)> {
        let ix = self.state.read(cx).selected_row()?;
        let row = self.state.read(cx).delegate().rows.get(ix)?.clone();
        Some((ix, row))
    }

    /// True when the table has at least one primary-key column — update
    /// and delete need a stable row identity.
    fn has_pk(&self) -> bool {
        self.table.columns.iter().any(|c| c.primary_key)
    }

    /// The pk cells of a row, in column order — the row's identity for
    /// UPDATE/DELETE.
    fn pk_cells(&self, row: &Row) -> Vec<CellChange> {
        self.table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.primary_key)
            .filter_map(|(i, c)| {
                row.get(i).map(|v| CellChange::new(c.name.clone(), c.kind, v.clone()))
            })
            .collect()
    }

    /// Open the simplified editor for a new row.
    fn open_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = self
            .table
            .columns
            .iter()
            .map(|c| {
                let col = c.clone();
                cx.new(|cx| {
                    let mut s = InputState::new(window, cx);
                    let hint = if col.auto_increment {
                        "auto"
                    } else if col.has_default {
                        "default"
                    } else if !col.nullable {
                        "required"
                    } else {
                        "null"
                    };
                    if hint != "null" {
                        s.set_placeholder(hint, window, cx);
                    }
                    s
                })
            })
            .collect();
        // Identity columns are database-generated on insert.
        let locked = self
            .table
            .columns
            .iter()
            .map(|c| c.auto_increment || c.kind == ColumnKind::Binary)
            .collect();
        self.editor = Some(EditorState {
            mode: EditorMode::Insert,
            inputs,
            locked,
            error: None,
            saving: false,
            save_id: 0,
            fk: Vec::new(),
        });
        self.load_fk_options(cx);
        cx.notify();
    }

    /// Open the simplified editor on the selected row.
    fn open_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((ix, row)) = self.selected_row(cx) else {
            return;
        };
        let row_no = self.state.read(cx).delegate().page * PAGE + ix as u64 + 1;
        let inputs = self
            .table
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let raw = row.get(i).map(|v| v.edit_string()).unwrap_or_default();
                let hint = if c.nullable { "" } else { "required" };
                cx.new(|cx| {
                    let mut s = InputState::new(window, cx);
                    if !hint.is_empty() {
                        s.set_placeholder("required", window, cx);
                    }
                    s.set_value(raw, window, cx);
                    s
                })
            })
            .collect();
        // Identity is fixed on update; binary columns are not editable.
        let locked = self
            .table
            .columns
            .iter()
            .map(|c| c.primary_key || c.kind == ColumnKind::Binary)
            .collect();
        self.editor = Some(EditorState {
            mode: EditorMode::Update { row, row_no },
            inputs,
            locked,
            error: None,
            saving: false,
            save_id: 0,
            fk: Vec::new(),
        });
        self.load_fk_options(cx);
        cx.notify();
    }

    /// Open the delete confirmation for the selected row.
    fn open_delete(&mut self, cx: &mut Context<Self>) {
        let Some((ix, row)) = self.selected_row(cx) else {
            return;
        };
        let row_no = self.state.read(cx).delegate().page * PAGE + ix as u64 + 1;
        self.editor = Some(EditorState {
            mode: EditorMode::Delete { row, row_no },
            inputs: Vec::new(),
            locked: Vec::new(),
            error: None,
            saving: false,
            save_id: 0,
            fk: Vec::new(),
        });
        cx.notify();
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        self.fk_gen += 1;
        cx.notify();
    }

    /// FK picker data for the open editor: this table's FK edges
    /// (`list_relations`), then up to [`FK_OPTIONS`] rows of each referenced
    /// table. Targets resolve through the app's schema tree; every step is
    /// stale-guarded by `fk_gen`.
    fn load_fk_options(&mut self, cx: &mut Context<Self>) {
        self.fk_gen += 1;
        let fk_load = self.fk_gen;
        let session = self.state.read(cx).delegate().session.clone();
        let rx = session.list_relations(&self.schema);
        cx.spawn(async move |this, cx| {
            let Ok(Ok(relations)) = rx.await else {
                return; // no picker — the plain inputs still work
            };
            this.update(cx, |v, cx| v.start_fk_lookups(fk_load, relations, session, cx))
                .ok();
        })
        .detach();
    }

    fn start_fk_lookups(
        &mut self,
        fk_load: u64,
        relations: Vec<crate::core::model::Relation>,
        session: SessionHandle,
        cx: &mut Context<Self>,
    ) {
        if fk_load != self.fk_gen {
            return;
        }
        let schema = self.schema.clone();
        let schemas = match self.app.upgrade() {
            Some(app) => app.read(cx).schemas.clone(),
            None => return,
        };
        let Some(sch) = schemas.iter().find(|s| s.name == schema) else {
            return;
        };
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        editor.fk = (0..self.table.columns.len()).map(|_| None).collect();
        for rel in relations.iter().filter(|r| r.from_table == self.table.name) {
            let Some(col_ix) = self.table.columns.iter().position(|c| c.name == rel.from_column)
            else {
                continue;
            };
            if editor.fk[col_ix].is_some() {
                continue; // first edge wins for multi-FK columns
            }
            let Some(target) = sch.tables.iter().find(|t| t.name == rel.to_table).cloned() else {
                continue;
            };
            let Some(to_ix) = target.columns.iter().position(|c| c.name == rel.to_column) else {
                continue;
            };
            // Label: the target's first text column other than the key.
            let label_ix = target
                .columns
                .iter()
                .enumerate()
                .position(|(i, c)| i != to_ix && c.kind == ColumnKind::Text);
            editor.fk[col_ix] = Some(FkLookup {
                target: target.name.clone(),
                loading: true,
                error: None,
                options: Arc::new(Vec::new()),
            });
            let rx = session.fetch_rows(TableRef::of(&target), 0, FK_OPTIONS);
            cx.spawn(async move |this, cx| {
                let result = rx.await;
                this.update(cx, |v, cx| {
                    if fk_load != v.fk_gen {
                        return;
                    }
                    let Some(fk) = v
                        .editor
                        .as_mut()
                        .and_then(|e| e.fk.get_mut(col_ix))
                        .and_then(|f| f.as_mut())
                    else {
                        return;
                    };
                    fk.loading = false;
                    match result {
                        Ok(Ok(page)) => {
                            let options = page
                                .rows
                                .iter()
                                .filter_map(|row| {
                                    let value = row.get(to_ix)?.edit_string();
                                    let label = match label_ix.and_then(|i| row.get(i)) {
                                        Some(l) => format!("{value} — {}", l.display()),
                                        None => value.clone(),
                                    };
                                    Some((value.into(), label.into()))
                                })
                                .collect();
                            fk.options = Arc::new(options);
                        }
                        Ok(Err(e)) => fk.error = Some(e.message().to_string()),
                        Err(_) => {}
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    /// Collect + validate the modal's inputs into typed cells. For
    /// updates only the changed columns come back; for inserts the
    /// provided ones (empty-and-nullable columns fall back to defaults).
    fn collect_changes(&self, editor: &EditorState, cx: &App) -> Result<Vec<CellChange>, String> {
        let original: Option<&Row> = match &editor.mode {
            EditorMode::Update { row, .. } => Some(row),
            _ => None,
        };
        let mut changes = Vec::new();
        for (i, c) in self.table.columns.iter().enumerate() {
            if c.primary_key && original.is_some() {
                continue; // pk is fixed on update
            }
            if c.kind == ColumnKind::Binary {
                continue; // not editable
            }
            let raw = editor.inputs[i].read(cx).value().to_string();
            // Insert: empty inputs fall back to what the database would do
            // itself — identity (auto_increment), DEFAULT values, or NULL.
            // Only genuinely required columns must be filled.
            if original.is_none()
                && raw.trim().is_empty()
                && (c.nullable || c.auto_increment || c.has_default)
            {
                continue;
            }
            let value = parse_cell(c, &raw)?;
            let unchanged = original.is_some_and(|row| {
                row.get(i).is_some_and(|old| *old == value)
            });
            if !unchanged {
                changes.push(CellChange::new(c.name.clone(), c.kind, value));
            }
        }
        Ok(changes)
    }

    /// Save the modal: INSERT or UPDATE through the session, then reload
    /// the page (stale-guarded by `save_id`). Immutable reads happen
    /// first, the mutation of the editor state last — keeps the borrow
    /// checker happy.
    fn save_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // ---- immutable phase: validate + collect ----
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let changes = match self.collect_changes(editor, cx) {
            Ok(c) => c,
            Err(e) => {
                self.editor.as_mut().map(|ed| ed.error = Some(e));
                cx.notify();
                return;
            }
        };
        let (table, pk, is_update) = match &editor.mode {
            EditorMode::Insert => {
                if changes.is_empty() {
                    self.editor.as_mut().map(|ed| ed.error = Some("fill at least one column".into()));
                    cx.notify();
                    return;
                }
                (TableRef::of(&self.table), Vec::new(), false)
            }
            EditorMode::Update { row, .. } => {
                if changes.is_empty() {
                    self.close_editor(cx);
                    return; // nothing changed
                }
                (TableRef::of(&self.table), self.pk_cells(row), true)
            }
            EditorMode::Delete { .. } => return,
        };
        let session = self.state.read(cx).delegate().session.clone();

        // ---- mutation phase: flag the in-flight save ----
        let id = self
            .editor
            .as_mut()
            .map(|ed| {
                ed.saving = true;
                ed.save_id += 1;
                ed.error = None;
                ed.save_id
            })
            .unwrap_or(0);
        cx.notify();

        let rx = if is_update {
            session.update_row(table, pk, changes)
        } else {
            session.insert_row(table, changes)
        };
        let view = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            let result = rx.await;
            cx.update(|_window, cx| {
                view.update(cx, |v, cx| {
                    let Some(editor) = v.editor.as_mut() else {
                        return;
                    };
                    if editor.save_id != id {
                        return; // a newer save superseded this one
                    }
                    editor.saving = false;
                    match result {
                        Ok(Ok(_)) => {
                            v.editor = None;
                            v.reload(cx);
                        }
                        Ok(Err(e)) => editor.error = Some(e.message().to_string()),
                        Err(_) => editor.error = Some("save was cancelled".into()),
                    }
                    cx.notify();
                })
            })
            .ok();
        })
        .detach();
    }

    /// Delete the confirmed row through the session, then reload.
    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // ---- immutable phase ----
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let EditorMode::Delete { ref row, .. } = editor.mode else {
            return;
        };
        let pk = self.pk_cells(row);
        if pk.is_empty() {
            self.editor.as_mut().map(|ed| ed.error = Some("this table has no primary key".into()));
            cx.notify();
            return;
        }
        let table = TableRef::of(&self.table);
        let session = self.state.read(cx).delegate().session.clone();

        // ---- mutation phase ----
        let id = self
            .editor
            .as_mut()
            .map(|ed| {
                ed.saving = true;
                ed.save_id += 1;
                ed.error = None;
                ed.save_id
            })
            .unwrap_or(0);
        cx.notify();

        let rx = session.delete_row(table, pk);
        let view = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            let result = rx.await;
            cx.update(|_window, cx| {
                view.update(cx, |v, cx| {
                    let Some(editor) = v.editor.as_mut() else {
                        return;
                    };
                    if editor.save_id != id {
                        return;
                    }
                    editor.saving = false;
                    match result {
                        Ok(Ok(_)) => {
                            v.editor = None;
                            v.reload(cx);
                        }
                        Ok(Err(e)) => editor.error = Some(e.message().to_string()),
                        Err(_) => editor.error = Some("delete was cancelled".into()),
                    }
                    cx.notify();
                })
            })
            .ok();
        })
        .detach();
    }

    /// The simplified record editor modal (insert / update / delete).
    /// Rendered as an overlay on top of the grid — pinned insets per the
    /// absolute-overlay gotcha in AGENTS.md.
    fn render_editor(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = theme::palette(cx);
        let Some(editor) = self.editor.as_ref() else {
            return div().into_any_element();
        };
        let saving = editor.saving;
        let (title, sub): (String, String) = match &editor.mode {
            EditorMode::Insert => (
                format!("New row · {}", self.table.name),
                "Fill the values you want to insert; empty nullable columns fall back to their defaults.".into(),
            ),
            EditorMode::Update { row_no, .. } => (
                format!("Edit row {row_no} · {}", self.table.name),
                "Change the values and save; only the changed columns are written.".into(),
            ),
            EditorMode::Delete { row_no, .. } => (
                format!("Delete row {row_no} · {}", self.table.name),
                "This permanently removes the row from the database.".into(),
            ),
        };
        let is_delete = matches!(editor.mode, EditorMode::Delete { .. });
        let is_update = matches!(editor.mode, EditorMode::Update { .. });

        // The fields form (not rendered for the delete confirmation).
        let fields = if is_delete {
            v_flex().into_any_element()
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .px_4()
                .py_2()
                .gap(px(10.))
                .children(
                    self.table
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            let locked = editor.locked.get(i).copied().unwrap_or(false);
                            let fk = editor.fk.get(i).and_then(|f| f.as_ref());
                            let fk_hint = fk.map(|f| {
                                if f.error.is_some() {
                                    format!("→ {} (unavailable)", f.target)
                                } else {
                                    format!("→ {}", f.target)
                                }
                            });
                            let picker = fk.filter(|_| !locked).map(|f| {
                                let options = f.options.clone();
                                let input = editor.inputs[i].clone();
                                Button::new(("fk-pick", i))
                                    .ghost()
                                    .icon(IconName::ChevronDown)
                                    .loading(f.loading)
                                    .disabled(f.loading || options.is_empty())
                                    .dropdown_menu_with_anchor(Corner::TopRight, move |menu, _, _| {
                                        let mut menu = menu.scrollable(true).max_h(px(320.));
                                        for (value, label) in options.iter() {
                                            let input = input.clone();
                                            let value = value.clone();
                                            menu = menu.item(PopupMenuItem::new(label.clone()).on_click(
                                                move |_, window, cx| {
                                                    input.update(cx, |s, cx| {
                                                        s.set_value(value.clone(), window, cx)
                                                    });
                                                },
                                            ));
                                        }
                                        menu
                                    })
                            });
                            h_flex()
                                .id(("f", i))
                                .items_center()
                                .gap(px(10.))
                                .child(
                                    v_flex()
                                        .w(px(170.))
                                        .flex_none()
                                        .gap(px(2.))
                                        .child(
                                            h_flex()
                                                .gap(px(4.))
                                                .items_center()
                                                .child(
                                                    div()
                                                        .text_size(px(12.5))
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(p.foreground)
                                                        .min_w_0()
                                                        .truncate()
                                                        .child(c.name.clone()),
                                                )
                                                .when(c.primary_key, |t| t.child(
                                                    div()
                                                        .px(px(3.))
                                                        .rounded(px(2.))
                                                        .text_size(px(10.))
                                                        .bg(p.primary.opacity(0.2))
                                                        .text_color(p.primary)
                                                        .child("PK"),
                                                ))
                                                .when(c.nullable, |t| t.child(
                                                    div()
                                                        .text_size(px(10.))
                                                        .text_color(p.muted_fg)
                                                        .child("null?"),
                                                )),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .font_family(theme::MONO_FONT)
                                                .text_color(theme::kind_color(&c.kind, &p))
                                                .min_w_0()
                                                .truncate()
                                                .child(c.type_.clone()),
                                        )
                                        .when_some(fk_hint, |t, hint| {
                                            t.child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(p.primary)
                                                    .min_w_0()
                                                    .truncate()
                                                    .child(hint),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            Input::new(&editor.inputs[i])
                                                .large()
                                                .h(px(38.))
                                                .disabled(locked),
                                        ),
                                )
                                .when_some(picker, |t, picker| t.child(picker))
                        })
                        .collect::<Vec<_>>(),
                )
                .into_any_element()
        };

        // Backdrop click cancels (never while a save is in flight). The
        // backdrop and the card are SIBLINGS under a transparent layer:
        // clicks inside the card must never bubble into the backdrop.
        let backdrop = div()
            .id("editor-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::black().opacity(0.35))
            .cursor_pointer()
            .when(!saving, |t| {
                t.on_click(cx.listener(|this, _, _, cx| this.close_editor(cx)))
            });

        let card = v_flex()
            .id("record-editor")
            .absolute()
            .top_1_2()
            .left_1_2()
            // Rough optical centering: pull the card back by half its size.
            .mt(px(-300.))
            .ml(px(-320.))
            .w(px(640.))
            // BlockMouse: clicks over the card never count as backdrop
            // clicks — the card's hitbox cuts the backdrop out of the
            // hover set (see `Hitbox::is_hovered` / `hit_test`).
            .occlude()
            .max_h(px(640.))
            .bg(p.card)
            .border_1()
            .border_color(p.border)
            .rounded(px(10.))
            .shadow_lg()
            .overflow_hidden()
            // header
            .child(
                v_flex()
                    .flex_none()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.muted_fg)
                            .child(sub),
                    ),
            )
            .child(fields)
            // footer
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(p.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.5))
                            .text_color(p.conn_err)
                            .truncate()
                            .when_some(editor.error.clone(), |t, e| t.child(e)),
                    )
                    .child(
                        Button::new("editor-cancel")
                            .ghost()
                            .label("Cancel")
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.close_editor(cx))),
                    )
                    .child(if is_delete {
                        Button::new("editor-confirm")
                            .danger()
                            .label(if saving { "Deleting…" } else { "Delete row" })
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_delete(window, cx);
                            }))
                            .into_any_element()
                    } else {
                        Button::new("editor-save")
                            .primary()
                            .label(if saving {
                                "Saving…".to_string()
                            } else if is_update {
                                "Save changes".to_string()
                            } else {
                                "Insert row".to_string()
                            })
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_editor(window, cx);
                            }))
                            .into_any_element()
                    }),
            );

        // Transparent event-passing layer (no id → no hitbox of its own):
        // the backdrop handles outside clicks, the card sits on top.
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(backdrop)
            .child(card)
            .into_any_element()
    }

    pub fn export_csv(&mut self, cx: &mut Context<Self>) {        let st = self.state.read(cx);
        let d = st.delegate();
        let header = self
            .table
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let mut lines = vec![header];
        for row in &d.rows {
            let line = row
                .iter()
                .map(|v| {
                    let s = v.display();
                    if s.contains(',') || s.contains('"') || s.contains('\n') {
                        format!("\"{}\"", s.replace('"', "\"\""))
                    } else {
                        s
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            lines.push(line);
        }
        let csv = lines.join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(csv));
        self.export_copied = true;
        cx.notify();
    }
}

/// A small square icon button; `on_click` is normally a `cx.listener` closure.
fn icon_btn(
    id: impl Into<gpui::ElementId>,
    size: f32,
    body: impl IntoElement,
    enabled: bool,
    hover_bg: Hsla,
    active_bg: Option<Hsla>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<gpui::Div> {
    div()
        .id(id.into())
        .size(px(size))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .when(active_bg.is_some(), |this| this.bg(active_bg.unwrap()))
        .when(enabled, |this| {
            this.cursor_pointer().hover(move |s| s.bg(hover_bg))
        })
        .when(!enabled, |this| this.opacity(0.4))
        .on_click(on_click)
        .child(body)
}

impl Render for RecordsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let hover = p.tree_hover;
        let st = self.state.read(cx);
        let d = st.delegate();
        // Sync the crate's selection into our shared cell so `render_tr` can
        // paint the solid background; opening a row also reveals the detail
        // panel (web parity: `onClick={() => { setSelectedRow(ix);
        // setShowDetail(true) }}`).
        let sel_now = st.selected_row();
        if sel_now != self.last_sel {
            self.last_sel = sel_now;
            self.sel.set(sel_now);
            self.copied_cell = None;
            if sel_now.is_some() && !self.show_detail {
                self.show_detail = true;
            }
        }
        let page = d.page;
        let total = d.total;
        let rows_on_page = d.rows.len();
        let loading = d.loading;
        let elapsed = d.elapsed_ms;
        let page_count = if total == 0 { 1 } else { (total + PAGE - 1) / PAGE };
        let error = d.error.clone();
        let has_pk = self.has_pk();
        let sel_some = self.state.read(cx).selected_row().is_some();
        let editor_open = self.editor.is_some();

        let toolbar = h_flex()
            .h(px(36.))
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .border_b_1()
            .border_color(p.toolbar_border)
            .bg(p.toolbar)
            .child(icon_btn(
                "run",
                32.,
                svg().path("icons/play.svg").size_3p5().text_color(p.conn_ok),
                !loading,
                hover,
                None,
                cx.listener(|this, _, _, cx| {
                    this.export_copied = false;
                    this.reload(cx);
                }),
            ))
            .child(icon_btn(
                "stop",
                32.,
                svg().path("icons/stop.svg").size_3p5().text_color(if loading { p.conn_err } else { p.muted_fg }),
                loading,
                hover,
                None,
                cx.listener(|this, _, _, cx| {
                    this.state.update(cx, |s, cx| {
                        s.delegate_mut().stop();
                        cx.notify();
                    });
                    cx.notify();
                }),
            ))
            .child(icon_btn(
                "reload",
                32.,
                svg().path("icons/redo-2.svg").size_3p5().text_color(p.muted_fg),
                !loading,
                hover,
                None,
                cx.listener(|this, _, _, cx| {
                    this.export_copied = false;
                    this.reload(cx);
                }),
            ))
            .child(sep(&p))
            // Simplified editor: insert / edit / delete the selected row.
            .child(icon_btn(
                "row-insert",
                32.,
                svg().path("icons/plus.svg").size_3p5().text_color(p.foreground),
                !editor_open,
                hover,
                None,
                cx.listener(|this, _, window, cx| this.open_insert(window, cx)),
            ))
            .child(icon_btn(
                "row-edit",
                32.,
                svg().path("icons/pencil.svg").size_3p5().text_color(if sel_some { p.primary } else { p.muted_fg }),
                sel_some && has_pk && !editor_open,
                hover,
                None,
                cx.listener(|this, _, window, cx| this.open_update(window, cx)),
            ))
            .child(icon_btn(
                "row-delete",
                32.,
                svg().path("icons/trash-2.svg").size_3p5().text_color(if sel_some { p.conn_err } else { p.muted_fg }),
                sel_some && has_pk && !editor_open,
                hover,
                None,
                cx.listener(|this, _, _, cx| this.open_delete(cx)),
            ))
            .child(sep(&p))
            .child(icon_btn(
                "view-data",
                32.,
                svg().path("icons/table.svg").size_3p5().text_color(p.foreground),
                true,
                hover,
                Some(p.accent),
                cx.listener(|_, _, _, _| {}),
            ))
            .child(icon_btn(
                "sql",
                32.,
                Icon::new(IconName::SquareTerminal).with_size(Size::Size(px(14.))),
                true,
                hover,
                None,
                cx.listener(|this, _, window, cx| {
                    // Open a SQL editor tab for this connection (the
                    // placeholder button used to be a no-op).
                    if let Some(app) = this.app.upgrade() {
                        app.update(cx, |app, cx| {
                            app.open_query_tab(window, cx);
                        });
                    }
                }),
            ))
            .child(sep(&p))
            .child(icon_btn(
                "filter",
                32.,
                svg().path("icons/filter.svg").size_3p5().text_color(p.muted_fg),
                true,
                hover,
                None,
                cx.listener(|_, _, _, _| {}),
            ))
            .child(icon_btn(
                "export",
                32.,
                svg().path("icons/download.svg").size_3p5().text_color(if self.export_copied { p.conn_ok } else { p.muted_fg }),
                true,
                hover,
                if self.export_copied { Some(p.conn_ok.opacity(0.15)) } else { None },
                cx.listener(|this, _, _, cx| this.export_csv(cx)),
            ))
            .child(div().flex_1())
            .child(icon_btn(
                "detail",
                32.,
                Icon::new(IconName::Info)
                    .with_size(Size::Size(px(14.)))
                    .text_color(if self.show_detail { p.primary } else { p.muted_fg }),
                true,
                hover,
                if self.show_detail { Some(p.accent) } else { None },
                cx.listener(|this, _, _, cx| {
                    this.show_detail = !this.show_detail;
                    cx.notify();
                }),
            ))
            .child(icon_btn(
                "settings",
                32.,
                Icon::new(IconName::Settings).with_size(Size::Size(px(14.))),
                true,
                hover,
                None,
                cx.listener(|this, _, _, cx| {
                    // Open the root app settings dialog through the handle.
                    if let Some(app) = this.app.upgrade() {
                        app.update(cx, |app, cx| {
                            app.show_settings = true;
                            cx.notify();
                        });
                    }
                }),
            ));

        let path_strip = h_flex()
            .h(px(28.))
            .flex_none()
            .items_center()
            .gap(px(6.))
            .px_3()
            .border_b_1()
            .border_color(p.toolbar_border)
            .bg(p.table_header)
            .text_size(px(12.))
            .text_color(p.muted_fg)
            .child(self.conn.database.clone())
            .child("·")
            .child(div().text_color(p.foreground.opacity(0.8)).child(self.schema.clone()))
            .child("·")
            .child(div().text_color(p.primary).child(self.table.name.clone()))
            .child(
                div()
                    .ml_2()
                    .text_size(px(11.))
                    .child(format!(
                        "({} columns, ~{} total)",
                        self.table.columns.len(),
                        group_digits(self.table.row_count)
                    )),
            );

        // Detail panel splitter (vertical resizer)
        let detail_splitter = div()
            .id("detail-splitter")
            .h(px(6.))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .cursor_row_resize()
            .hover(move |s| s.bg(p.primary.opacity(0.35)))
            .when(self.resizing_detail, move |s| s.bg(p.primary))
            .child(div().h(px(1.)).w_full().bg(p.toolbar_border))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| {
                this.resizing_detail = true;
                let y: f32 = e.position.y.into();
                this.resize_start_y = y;
                this.resize_start_h = this.detail_height;
                cx.notify();
            }));

        // Detail panel (selected row inspector)
        let detail: gpui::Div = if self.show_detail {
            let st = self.state.read(cx);
            let d = st.delegate();
            let sel = st.selected_row();
            let copied_cell = self.copied_cell;
            let inner: AnyElement = match sel.and_then(|ix| d.rows.get(ix)) {
                Some(row) => div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(div().flex().flex_col().children(
                        self.table.columns.iter().enumerate().map(|(i, c)| {
                            let raw = row[i].display();
                            let is_copied = copied_cell == Some(i);
                            h_flex()
                                .id(("drow", i))
                                .h(px(28.))
                                .flex_none()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .border_b_1()
                                .border_color(p.table_border.opacity(0.5))
                                .hover(move |s| s.bg(hover))
                                .child(
                                    h_flex()
                                        .w(px(210.))
                                        .flex_none()
                                        .items_center()
                                        .gap(px(6.))
                                        .border_r_1()
                                        .border_color(p.table_border.opacity(0.5))
                                        .pr_2()
                                        .when(c.primary_key, |this| {
                                            this.child(
                                                div()
                                                    .px(px(3.))
                                                    .rounded(px(2.))
                                                    .text_size(px(10.))
                                                    .bg(p.primary.opacity(0.2))
                                                    .text_color(p.primary)
                                                    .child("PK"),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_size(px(13.5))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(p.foreground)
                                                .truncate()
                                                .child(c.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .ml_auto()
                                                .text_size(px(11.))
                                                .text_color(p.muted_fg)
                                                .child(c.type_.to_string()),
                                        )
                                        .when(c.nullable, |this| {
                                            this.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_size(px(10.))
                                                    .text_color(p.muted_fg)
                                                    .child("null?"),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .child(cell_value(row.get(i), c, &p, false)),
                                )
                                .child(
                                    div()
                                        .id(("copy", i))
                                        .size(px(24.))
                                        .flex()
                                        .flex_none()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .hover(move |s| s.bg(hover))
                                        .child(
                                            if is_copied {
                                                Icon::new(IconName::Check)
                                                    .with_size(Size::Size(px(13.)))
                                                    .text_color(p.conn_ok)
                                            } else {
                                                Icon::new(IconName::Copy)
                                                    .with_size(Size::Size(px(13.)))
                                                    .text_color(p.muted_fg)
                                            },
                                        )
                                        .on_click({
                                            let value = raw.clone();
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    value.clone(),
                                                ));
                                                this.copied_cell = Some(i);
                                                cx.notify();
                                                // Flash feedback like the web
                                                // version: reset after 800ms.
                                                let reset = cx
                                                    .background_executor()
                                                    .timer(std::time::Duration::from_millis(800));
                                                cx.spawn(async move |this, cx| {
                                                    reset.await;
                                                    this.update(cx, |v, cx| {
                                                        if v.copied_cell == Some(i) {
                                                            v.copied_cell = None;
                                                            cx.notify();
                                                        }
                                                    })
                                                    .ok();
                                                })
                                                .detach();
                                            })
                                        })
                                )
                        }),
                    ))
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(p.muted_fg)
                    .child("Click a row to inspect its fields.")
                    .into_any_element(),
            };
            let sel_no = sel.map(|ix| (page * PAGE) as usize + ix + 1);
            v_flex()
                .h(px(self.detail_height))
                .flex_none()
                .bg(p.card)
                .border_t_1()
                .border_color(p.toolbar_border)
                .child(
                    div()
                        .flex_none()
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .px_3()
                        .text_size(px(11.))
                        .text_color(p.muted_fg)
                        .bg(p.table_header)
                        .border_b_1()
                        .border_color(p.table_border)
                        .child(format!(
                            "Row {} · {} fields",
                            sel_no.unwrap_or(0),
                            self.table.columns.len()
                        )),
                )
                .child(inner)
        } else {
            v_flex()
        };

        let lower = if rows_on_page > 0 { page * PAGE + 1 } else { 0 };
        let upper = page * PAGE + rows_on_page as u64;
        let host = if self.conn.host.is_empty() {
            // URL/SQLite connections may have no host: show the file path.
            self.conn.database.clone()
        } else if self.conn.port > 0 {
            format!("{}:{}", self.conn.host, self.conn.port)
        } else {
            self.conn.host.clone()
        };

        let status_bar = h_flex()
            .h(px(28.))
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .bg(p.statusbar)
            .text_size(px(11.))
            .text_color(p.statusbar_fg)
            .border_t_1()
            .border_color(p.toolbar_border)
            .child(div().text_color(p.conn_ok).child("●"))
            .child(host)
            .child("·")
            .child(if loading {
                "fetching…".to_string()
            } else {
                format!("{}-{} of {}", lower, upper, group_digits(total))
            })
            .child("·")
            .child(if loading {
                "—".to_string()
            } else {
                status_line(elapsed, rows_on_page, total)
            })
            .child(div().flex_1())
            .child(icon_btn(
                "prev",
                24.,
                Icon::new(IconName::ChevronLeft).with_size(Size::Size(px(13.))),
                page > 0,
                p.tree_hover,
                None,
                cx.listener(|this, _, _, cx| {
                    this.state.update(cx, |s, cx| s.delegate_mut().prev_page(cx))
                }),
            ))
            .child(format!("Page {} / {}", page + 1, page_count))
            .child(icon_btn(
                "next",
                24.,
                Icon::new(IconName::ChevronRight).with_size(Size::Size(px(13.))),
                (page + 1) < page_count,
                p.tree_hover,
                None,
                cx.listener(|this, _, _, cx| {
                    this.state.update(cx, |s, cx| s.delegate_mut().next_page(cx))
                }),
            ));

        let error_banner = error.map(|e| {
            h_flex()
                .m_2()
                .px_2()
                .py(px(8.))
                .rounded(px(4.))
                .border_1()
                .border_color(p.conn_err.opacity(0.4))
                .bg(p.conn_err.opacity(0.1))
                .text_color(p.conn_err)
                .text_size(px(13.5))
                .gap_2()
                .child(Icon::new(IconName::TriangleAlert).with_size(Size::Size(px(16.))))
                .child(e)
        });

        let view = cx.entity();
        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .when(self.resizing_detail, |this| this.cursor_row_resize())
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.resizing_detail && !e.dragging() {
                    // Button released outside the window: drop the drag
                    // instead of sticking to the cursor.
                    this.resizing_detail = false;
                    cx.notify();
                } else if this.resizing_detail {
                    let y: f32 = e.position.y.into();
                    let dy = this.resize_start_y - y;
                    this.detail_height = (this.resize_start_h + dy).clamp(60.0, 480.0);
                    cx.notify();
                }
            }))
            // Window-level safety net: any left-button release anywhere ends
            // the detail resize (covers releases over other apps / title bar).
            // Pinned insets — see the absolute-overlay gotcha in AGENTS.md.
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
                                        if this.resizing_detail {
                                            this.resizing_detail = false;
                                            cx.notify();
                                        }
                                    });
                                }
                            });
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                if this.resizing_detail {
                    this.resizing_detail = false;
                    cx.notify();
                }
            }))
            .child(toolbar)
            .child(path_strip)
            .when_some(error_banner, |this, b| this.child(b))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(Table::new(&self.state).stripe(true).bordered(false).with_size(Size::Large))
                    .child(
                        canvas(
                            {
                                let view = cx.entity().clone();
                                move |bounds, _, cx| {
                                    let w: f32 = bounds.size.width.into();
                                    view.update(cx, |v, cx| v.sync_table_width(w, cx));
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .when(self.show_detail, |this| this.child(detail_splitter))
            .child(detail)
            .child(status_bar)
            // The record editor modal, on top of everything.
            .when(self.editor.is_some(), |this| this.child(self.render_editor(cx)))
    }
}

fn sep(p: &theme::Palette) -> gpui::Div {
    div()
        .mx(px(2.))
        .h(px(20.))
        .w(px(1.))
        .flex_none()
        .bg(p.toolbar_border)
}

impl crate::ui::app::DataBridge {
    /// The "no open data view" placeholder shown when all tabs are closed.
    /// Also the way into an empty database: "New SQL query" and, for SQL
    /// stores, "New table" (a seeded `CREATE TABLE` tab).
    pub fn render_empty_state(
        &mut self,
        cx: &mut Context<crate::ui::app::DataBridge>,
    ) -> impl IntoElement {
        let p = theme::palette(cx);
        let new_table_schema = self
            .can_create_tables()
            .then(|| self.default_schema())
            .flatten();
        div()
            .id("empty")
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(46.))
                            .text_color(p.muted_fg.opacity(0.7))
                            .child("▦"),
                    )
                    .child(
                        div()
                            .text_size(px(15.))
                            .text_color(p.foreground)
                            .child("No open data view"),
                    )
                    .child(
                        div()
                            .text_size(px(13.5))
                            .text_color(p.muted_fg)
                            .child("Select a table in the explorer on the left to open its records here."),
                    )
                    .child(
                        h_flex()
                            .mt_2()
                            .gap_2()
                            .when_some(new_table_schema, |this, schema| {
                                this.child(
                                    Button::new("empty-new-table")
                                        .primary()
                                        .small()
                                        .icon(IconName::Plus)
                                        .label("New table")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_create_table(&schema, window, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("empty-new-query")
                                    .ghost()
                                    .small()
                                    .icon(IconName::SquareTerminal)
                                    .label("New SQL query")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_query_tab(window, cx);
                                    })),
                            ),
                    ),
            )
    }
}
