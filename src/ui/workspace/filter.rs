//! Records grid filter bar: AND-ed `column op value` conditions sent to the
//! driver with every page (`RowQuery::filters`) — the database filters the
//! whole table, not just the loaded page. Values are typed per column
//! (`parse_filter_value`) and always bound as query parameters.
//!
//! Toggled by the toolbar's filter icon; Enter in a value field (or Apply)
//! reloads from page 1, Clear drops every condition.

use gpui::prelude::*;
use gpui::{Context, Entity, IntoElement, ParentElement, Styled, Subscription, Window, div, px};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{Icon, IconName, h_flex, v_flex};

use crate::core::model::{ColumnKind, Filter, FilterOp, parse_filter_value};
use crate::ui::theme;

use super::RecordsView;

/// The operator's menu label: symbols as-is, word operators translated.
fn op_label(op: FilterOp) -> gpui::SharedString {
    match op {
        FilterOp::Contains => tr!("filter.contains"),
        FilterOp::IsNull => tr!("filter.is_null"),
        FilterOp::NotNull => tr!("filter.not_null"),
        _ => op.label().into(),
    }
}

/// One condition row of the filter bar.
pub struct FilterRow {
    pub column: String,
    pub op: FilterOp,
    pub input: Entity<InputState>,
    _enter: Subscription,
}

impl RecordsView {
    pub(super) fn toggle_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_filter = !self.show_filter;
        if self.show_filter && self.filter_rows.is_empty() {
            self.add_filter_row(window, cx);
        }
        cx.notify();
    }

    fn add_filter_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Default to the first column not filtered yet; text → "contains".
        let used: Vec<&str> = self.filter_rows.iter().map(|r| r.column.as_str()).collect();
        let col = self
            .table
            .columns
            .iter()
            .find(|c| !used.contains(&c.name.as_str()))
            .or_else(|| self.table.columns.first());
        let (column, op) = match col {
            Some(c) if c.kind == ColumnKind::Text => (c.name.clone(), FilterOp::Contains),
            Some(c) => (c.name.clone(), FilterOp::Eq),
            None => (String::new(), FilterOp::Eq),
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("filter.value")));
        let enter = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.apply_filters(cx);
            }
        });
        self.filter_rows.push(FilterRow { column, op, input, _enter: enter });
        cx.notify();
    }

    /// Read the rows into `RowQuery::filters` and reload from page 1. Rows
    /// with an empty value are skipped (a fresh row does not block Apply).
    pub(super) fn apply_filters(&mut self, cx: &mut Context<Self>) {
        let mut filters = Vec::new();
        for r in &self.filter_rows {
            let Some(c) = self.table.columns.iter().find(|c| c.name == r.column) else {
                continue;
            };
            let raw = r.input.read(cx).value().to_string();
            if r.op.takes_value() && raw.trim().is_empty() {
                continue;
            }
            match parse_filter_value(c, r.op, &raw) {
                Ok(value) => filters.push(Filter {
                    column: c.name.clone(),
                    kind: c.kind,
                    type_: c.type_.clone(),
                    op: r.op,
                    value,
                }),
                Err(e) => {
                    self.filter_error = Some(e);
                    cx.notify();
                    return;
                }
            }
        }
        self.filter_error = None;
        self.state.update(cx, |s, cx| {
            let d = s.delegate_mut();
            d.query.filters = filters;
            d.page = 0;
            d.load(cx);
        });
        cx.notify();
    }

    fn clear_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_rows.clear();
        self.add_filter_row(window, cx);
        self.apply_filters(cx);
    }

    fn set_filter_column(&mut self, ix: usize, column: String, cx: &mut Context<Self>) {
        let kind = self.table.columns.iter().find(|c| c.name == column).map(|c| c.kind);
        if let Some(r) = self.filter_rows.get_mut(ix) {
            r.column = column;
            // "contains" stays meaningful only for text-like columns.
            if r.op == FilterOp::Contains && kind.is_some_and(|k| k != ColumnKind::Text) {
                r.op = FilterOp::Eq;
            }
        }
        cx.notify();
    }

    fn set_filter_op(&mut self, ix: usize, op: FilterOp, cx: &mut Context<Self>) {
        if let Some(r) = self.filter_rows.get_mut(ix) {
            r.op = op;
        }
        cx.notify();
    }

    fn remove_filter_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.filter_rows.len() {
            self.filter_rows.remove(ix);
        }
        if self.filter_rows.is_empty() {
            self.add_filter_row(window, cx);
        }
        self.apply_filters(cx);
    }

    pub(super) fn render_filter_bar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = theme::palette(cx);
        let view = cx.entity().downgrade();
        let columns: Vec<String> = self.table.columns.iter().map(|c| c.name.clone()).collect();

        let mut rows = v_flex().gap_2();
        for (ix, r) in self.filter_rows.iter().enumerate() {
            let col_menu = {
                let (view, columns) = (view.clone(), columns.clone());
                Button::new(("flt-col", ix))
                    .outline()
                    .label(if r.column.is_empty() { tr!("filter.column").to_string() } else { r.column.clone() })
                    .dropdown_menu(move |menu, _, _| {
                        let mut menu = menu.scrollable(true).max_h(px(320.));
                        for name in &columns {
                            let (view, name) = (view.clone(), name.clone());
                            menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.set_filter_column(ix, name.clone(), cx));
                            }));
                        }
                        menu
                    })
            };
            let op_menu = {
                let view = view.clone();
                Button::new(("flt-op", ix))
                    .outline()
                    .label(op_label(r.op))
                    .dropdown_menu(move |menu, _, _| {
                        let mut menu = menu;
                        for op in FilterOp::ALL {
                            let view = view.clone();
                            menu = menu.item(PopupMenuItem::new(op_label(op)).on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.set_filter_op(ix, op, cx));
                            }));
                        }
                        menu
                    })
            };
            rows = rows.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .w(theme::scaled(48.))
                            .flex_none()
                            .text_size(theme::scaled(theme::TEXT_SM))
                            .text_color(p.muted_fg)
                            .child(if ix == 0 { tr!("filter.where") } else { tr!("filter.and") }),
                    )
                    .child(col_menu)
                    .child(op_menu)
                    .child(if r.op.takes_value() {
                        div().flex_1().min_w_0().child(Input::new(&r.input)).into_any_element()
                    } else {
                        div().flex_1().into_any_element()
                    })
                    .child(
                        Button::new(("flt-del", ix))
                            .ghost()
                            .icon(Icon::new(IconName::Close))
                            .tooltip(tr!("filter.remove"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_filter_row(ix, window, cx)
                            })),
                    ),
            );
        }

        let actions = h_flex()
            .gap_1()
            .items_center()
            .child(
                Button::new("flt-add")
                    .ghost()
                    .icon(Icon::new(IconName::Plus))
                    .label(tr!("filter.add"))
                    .on_click(cx.listener(|this, _, window, cx| this.add_filter_row(window, cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme::scaled(theme::TEXT_SM))
                    .text_color(p.conn_err)
                    .truncate()
                    .children(self.filter_error.clone()),
            )
            .child(
                Button::new("flt-clear")
                    .ghost()
                    .label(tr!("filter.clear"))
                    .on_click(cx.listener(|this, _, window, cx| this.clear_filters(window, cx))),
            )
            .child(
                Button::new("flt-apply")
                    .primary()
                    .label(tr!("filter.apply"))
                    .on_click(cx.listener(|this, _, _, cx| this.apply_filters(cx))),
            );

        v_flex()
            .flex_none()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(p.toolbar_border)
            .bg(p.toolbar)
            .child(rows)
            .child(actions)
            .into_any_element()
    }
}
