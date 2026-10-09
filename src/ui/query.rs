//! SQL editor tab (Stage 5): a syntax-highlighted code editor on top, the
//! results grid below. One Run per statement — SELECT-like statements go
//! through the fetch path, everything else reports affected rows.

use gpui::prelude::*;
use gpui::{
    App, Context, Div, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window,
    div, px,
};

use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::Input;
use gpui_component::input::InputState;
use gpui_component::table::{Column, Table, TableDelegate, TableState};
use gpui_component::{Disableable, Icon, IconName, Sizable, Size, h_flex, v_flex};

use crate::core::model::{group_digits, DbConnection, QueryPage, Row};
use crate::core::session::SessionHandle;
use crate::ui::theme;

/// Rows kept in the result grid from one statement (like the web
/// version's `QUERY_LIMIT`).
pub const RESULT_LIMIT: u64 = 500;

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

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        &self.cols[col_ix]
    }

    fn loading(&self, _: &App) -> bool {
        false
    }

    fn is_eof(&self, _: &App) -> bool {
        true
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = theme::palette(cx);
        if col_ix == 0 {
            return div().text_size(px(11.)).text_color(p.muted_fg).child("#");
        }
        let c_name = self.names[col_ix].clone();
        div()
            .w_full()
            .min_w_0()
            .truncate()
            .text_size(px(11.))
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
                .text_size(px(11.))
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
            .text_size(px(13.5))
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
    rows: usize,
    affected: u64,
    truncated: bool,
    elapsed_ms: u32,
}

pub struct QueryView {
    #[allow(dead_code)]
    conn: DbConnection,
    session: SessionHandle,
    editor: Entity<InputState>,
    /// Rebuilt after every run; `None` until the first statement.
    table: Option<Entity<TableState<QueryDelegate>>>,
    running: bool,
    run_id: u64,
    error: Option<String>,
    last: Option<RunSummary>,
    _obs: Option<Subscription>,
}

impl QueryView {
    pub fn new(
        conn: DbConnection,
        session: SessionHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("sql")
                .placeholder("SELECT * FROM table_name LIMIT 50;")
        });
        Self {
            conn,
            session,
            editor,
            table: None,
            running: false,
            run_id: 0,
            error: None,
            last: None,
            _obs: None,
        }
    }

    /// Run the editor's statement (stale-guarded: only the latest Run
    /// applies, mirroring `RecordsDelegate::load`).
    pub fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).value().to_string();
        if sql.trim().is_empty() {
            self.error = Some("type a statement first".into());
            cx.notify();
            return;
        }
        self.running = true;
        self.error = None;
        self.run_id += 1;
        let id = self.run_id;
        cx.notify();
        let rx = self.session.execute(sql, RESULT_LIMIT);
        cx.spawn_in(window, async move |this, cx| {
            let result = rx.await;
            cx.update(|window, cx| {
                this.update(cx, |q, cx| {
                    if q.run_id != id {
                        return; // a newer run superseded this one
                    }
                    q.running = false;
                    match result {
                        Ok(Ok(page)) => q.apply_result(page, window, cx),
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

    fn apply_result(&mut self, page: QueryPage, window: &mut Window, cx: &mut Context<Self>) {
        self.last = Some(RunSummary {
            rows: page.rows.len(),
            affected: page.rows_affected,
            truncated: page.truncated,
            elapsed_ms: page.elapsed_ms,
        });
        self.error = None;
        let delegate = QueryDelegate::new(&page);
        let state = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .col_resizable(true)
        });
        self._obs = Some(cx.observe(&state, |_, _, cx| cx.notify()));
        self.table = Some(state);
    }
}

impl Render for QueryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);

        let status: String = match (&self.last, self.running) {
            (_, true) => "running…".into(),
            (
                Some(RunSummary {
                    rows,
                    affected,
                    truncated,
                    elapsed_ms,
                }),
                _,
            ) if *affected > 0 => format!("{affected} row(s) affected · {elapsed_ms} ms"),
            (Some(RunSummary { rows, truncated, elapsed_ms, .. }), _) => format!(
                "{} rows{} · {elapsed_ms} ms",
                group_digits(*rows as u64),
                if *truncated { " (truncated)" } else { "" }
            ),
            (None, _) => "no statement run yet".into(),
        };

        let toolbar = h_flex()
            .h(px(38.))
            .flex_none()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(p.toolbar_border)
            .bg(p.toolbar)
            .child(
                Button::new("run")
                    .primary()
                    .small()
                    .label(if self.running { "Running…" } else { "Run" })
                    .disabled(self.running)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run(window, cx);
                    })),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(if self.error.is_some() {
                        p.conn_err
                    } else {
                        p.muted_fg
                    })
                    .child(if let Some(e) = &self.error {
                        e.clone()
                    } else {
                        status
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(p.muted_fg)
                    .child(format!("limit {} rows", RESULT_LIMIT)),
            );

        // The editor pane: fixed share of the view; the code-editor input
        // scrolls internally. The Input widget's own `h_full` (not Styled's
        // `.h`) is what makes a multi-line editor actually grow.
        let editor_pane = v_flex()
            .h(px(220.))
            .flex_none()
            .min_h_0()
            .border_b_1()
            .border_color(p.toolbar_border)
            .bg(p.card)
            .p_2()
            .child(Input::new(&self.editor).h_full());

        let results: Div = match &self.table {
            Some(state) => div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(Table::new(state).stripe(true).bordered(false).with_size(Size::Large)),
            None => div().flex_1().min_h_0().flex().items_center().justify_center().child(
                div()
                    .text_size(px(13.))
                    .text_color(p.muted_fg)
                    .child("Run a statement to see its results here."),
            ),
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(toolbar)
            .child(editor_pane)
            .child(results)
            .when_some(self.error.clone(), |this, e| {
                this.child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .m_2()
                        .px_2()
                        .py(px(8.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(p.conn_err.opacity(0.4))
                        .bg(p.conn_err.opacity(0.1))
                        .text_color(p.conn_err)
                        .text_size(px(13.))
                        .child(
                            Icon::new(IconName::TriangleAlert).with_size(Size::Size(px(15.))),
                        )
                        .child(e),
                )
            })
    }
}
