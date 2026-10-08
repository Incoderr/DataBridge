//! Database explorer tree. Port of the web app's `fileTree.tsx`.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Div, ElementId, FontWeight, Hsla, IntoElement, ParentElement,
    SharedString, Styled, div, px, rgb, svg,
};
use gpui_component::input::Input;
use gpui_component::{h_flex, v_flex, Icon, IconName, Sizable, Size};

use crate::core::model::{group_digits, ConnectionStatus, DbType, SchemaMeta, TableMeta};
use crate::ui::app::DataBridge;
use crate::ui::theme;

pub type SelectedPath = Option<(String, String)>;

fn brand_icon(t: DbType, size: f32) -> AnyElement {
    svg()
        .path(t.brand_icon())
        .size(px(size))
        .flex_none()
        .text_color(rgb(t.brand_color()))
        .into_any_element()
}

fn glyph_icon(path: &'static str, color: Hsla, size: f32) -> AnyElement {
    svg()
        .path(path)
        .size(px(size))
        .flex_none()
        .text_color(color)
        .into_any_element()
}

fn chevron(open: bool, color: Hsla) -> AnyElement {
    let icon = if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    };
    Icon::new(icon)
        .with_size(Size::Size(px(13.)))
        .text_color(color)
        .into_any_element()
}

impl DataBridge {
    pub fn render_nav_tree(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let conn = self
            .active_conn
            .clone()
            .expect("render_nav_tree requires an active connection");
        // Clone the schemas out for the row builder (avoids a &self / &mut
        // self overlap inside `tree_rows`).
        let schemas = self.schemas.clone();
        let selected = self.selected_path();

        v_flex()
            .id("nav-tree")
            .w(px(self.nav_width))
            .min_w(px(180.))
            .flex_none()
            .h_full()
            .bg(p.tree_bg)
            .text_color(p.tree_fg)
            .rounded_r(px(4.))
            .overflow_hidden()
            // tiny DataGrip title strip
            .child(
                h_flex()
                    .h(px(36.))
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .border_b_1()
                    .border_color(p.toolbar_border)
                    .bg(p.toolbar)
                    .child(brand_icon(conn.db_type, 13.))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(p.muted_fg)
                            .min_w_0()
                            .truncate()
                            .child(conn.name.clone()),
                    ),
            )
            // search / tools bar
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.))
                    .px_2()
                    .py(px(8.))
                    .border_b_1()
                    .border_color(p.toolbar_border)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.search)
                                        .large()
                                        .h(px(36.)),
                                ),
                            )
                            .when(!self.query.trim().is_empty(), |this| {
                                this.child(
                                    div()
                                        .id("clear-search")
                                        .size(px(22.))
                                        .ml(px(-24.))
                                        .flex()
                                        .flex_none()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .rounded(px(2.))
                                        .text_color(p.muted_fg)
                                        .hover(move |s| s.text_color(p.foreground))
                                        .child(Icon::new(IconName::Close).with_size(Size::Size(px(12.))))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.query.clear();
                                            this.search.update(cx, |s, cx| s.set_value(String::new(), window, cx));
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id("tree-filter")
                            .size(px(32.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .rounded(px(2.))
                            .hover(move |s| s.bg(p.tree_hover))
                            .when(self.filter_only_tables, move |s| s.bg(p.accent))
                            .child(
                                svg()
                                    .path("icons/filter.svg")
                                    .size(px(15.))
                                    .text_color(if self.filter_only_tables { p.primary } else { p.muted_fg }),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.filter_only_tables = !this.filter_only_tables;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("tree-reload")
                            .size(px(32.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .rounded(px(2.))
                            .hover(move |s| s.bg(p.tree_hover))
                            .child(
                                svg()
                                    .path("icons/redo-2.svg")
                                    .size(px(15.))
                                    .text_color(p.muted_fg),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh(cx);
                            })),
                    ),
            )
            // tree body
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .py(px(2.))
                    .child(v_flex().children(self.tree_rows(&schemas, &selected, &p, cx))),
            )
    }

    /// The flat list of visible tree rows, honoring expansion + search filter.
    fn tree_rows(
        &mut self,
        schemas: &[SchemaMeta],
        selected: &SelectedPath,
        p: &theme::Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let conn = self.active_conn.clone().unwrap();
        let q = self.query.trim().to_lowercase();
        let expanded: HashSet<String> = self.expanded.clone();

        let mut rows: Vec<AnyElement> = Vec::new();
        let conn_open = expanded.contains("conn");

        rows.push(
            self.parent_row(
                0,
                if conn.database.is_empty() {
                    conn.id.clone()
                } else {
                    conn.database.clone()
                },
                "conn",
                brand_icon(conn.db_type, 15.),
                conn_open,
                p,
                cx,
            )
            .into_any_element(),
        );

        if !conn_open {
            return rows;
        }

        let schemas_open = expanded.contains("schemas");
        rows.push(
            self.parent_row(
                1,
                "Schemas",
                "schemas",
                glyph_icon("icons/cube.svg", p.muted_fg, 15.),
                schemas_open,
                p,
                cx,
            )
            .into_any_element(),
        );
        if !schemas_open {
            return rows;
        }

        if schemas.is_empty() {
            let (a, b) = match conn.status {
                ConnectionStatus::Error => (
                    "Schemas list is empty.",
                    "Last connection attempt failed.",
                ),
                ConnectionStatus::Disconnected => (
                    "Schemas list is empty.",
                    "Connection not open.",
                ),
                _ => (
                    "Schemas list is empty.",
                    "Refresh or pick another connection.",
                ),
            };
            rows.push(empty_note(a, b, "", p).into_any_element());
            return rows;
        }

        let matches = |label: &str| q.is_empty() || label.to_lowercase().contains(&q);

        let mut visible_any = false;
        for sch in schemas {
            let tables: Vec<Arc<TableMeta>> = sch
                .tables
                .iter()
                .filter(|t| matches(&t.name))
                .cloned()
                .collect();
            let views: Vec<Arc<TableMeta>> = sch
                .views
                .iter()
                .filter(|v| matches(&v.name))
                .cloned()
                .collect();
            let funcs: Vec<Arc<TableMeta>> = sch
                .functions
                .iter()
                .filter(|f| matches(&f.name))
                .cloned()
                .collect();
            let keep = q.is_empty()
                || matches(&sch.name)
                || !tables.is_empty()
                || !views.is_empty()
                || !funcs.is_empty();
            if !keep {
                continue;
            }
            visible_any = true;

            let sid = format!("s:{}", sch.name);
            let sch_open = expanded.contains(&sid);
            rows.push(
                self.parent_row(
                    2,
                    sch.name.clone(),
                    sid,
                    glyph_icon("icons/cube.svg", p.foreground.opacity(0.8), 15.),
                    sch_open,
                    p,
                    cx,
                )
                .into_any_element(),
            );
            if !sch_open {
                continue;
            }

            // The schema-wide relationships diagram entry.
            rows.push(self.diagram_row(&sch.name, p, cx).into_any_element());

            let group = |kind: &str| format!("ss:{}:{}", sch.name, kind);

            // Tables group (shown when not filtering or non-empty)
            if q.is_empty() || !tables.is_empty() {
                let gid = group("tables");
                let g_open = expanded.contains(&gid);
                rows.push(
                    self.parent_row(
                        3,
                        format!("Tables ({})", tables.len()),
                        gid,
                        glyph_icon("icons/cube.svg", p.muted_fg, 15.),
                        g_open,
                        p,
                        cx,
                    )
                    .into_any_element(),
                );
                if g_open {
                    for t in tables {
                        rows.push(
                            self.leaf_row(
                                4,
                                t,
                                &sch.name,
                                glyph_icon("icons/table.svg", p.primary, 15.),
                                selected,
                                p,
                                cx,
                            )
                            .into_any_element(),
                        );
                    }
                }
            }

            // Views group (hidden when filtering only tables)
            if !self.filter_only_tables && !views.is_empty() {
                let gid = group("views");
                let g_open = expanded.contains(&gid);
                rows.push(
                    self.parent_row(
                        3,
                        format!("Views ({})", views.len()),
                        gid,
                        glyph_icon("icons/cube.svg", p.muted_fg, 15.),
                        g_open,
                        p,
                        cx,
                    )
                    .into_any_element(),
                );
                if g_open {
                    for t in views {
                        rows.push(
                            self.leaf_row(
                                4,
                                t,
                                &sch.name,
                                Icon::new(IconName::Eye)
                                    .with_size(Size::Size(px(15.)))
                                    .text_color(p.chart_2)
                                    .into_any_element(),
                                selected,
                                p,
                                cx,
                            )
                            .into_any_element(),
                        );
                    }
                }
            }

            // Routines group (hidden when filtering only tables)
            if !self.filter_only_tables && !funcs.is_empty() {
                let gid = group("routines");
                let g_open = expanded.contains(&gid);
                rows.push(
                    self.parent_row(
                        3,
                        format!("Routines ({})", funcs.len()),
                        gid,
                        glyph_icon("icons/cube.svg", p.muted_fg, 15.),
                        g_open,
                        p,
                        cx,
                    )
                    .into_any_element(),
                );
                if g_open {
                    for t in funcs {
                        rows.push(
                            self.leaf_row(
                                4,
                                t,
                                &sch.name,
                                glyph_icon("icons/sigma.svg", p.chart_3, 15.),
                                selected,
                                p,
                                cx,
                            )
                            .into_any_element(),
                        );
                    }
                }
            }
        }

        if !visible_any {
            rows.push(
                empty_note(
                    &format!("No matches for \u{201c}{q}\u{201d}"),
                    "",
                    "",
                    p,
                )
                .into_any_element(),
            );
        }

        rows
    }

    /// An expandable row: chevron + icon + label. Clicking toggles expansion.
    fn parent_row(
        &mut self,
        depth: usize,
        label: impl Into<SharedString>,
        node_id: impl Into<String>,
        icon: AnyElement,
        open: bool,
        p: &theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id: String = node_id.into();
        let label = label.into();
        let node = id.clone();
        let bold_schema = depth == 2;
        div()
            .id(ElementId::Name(SharedString::from(id)))
            .h(px(32.))
            .flex()
            .items_center()
            .gap(px(4.))
            .mx(px(4.))
            .rounded(px(4.))
            .pr_2()
            .pl(px(depth as f32 * 16. + 2.))
            .text_size(px(13.5))
            .cursor_pointer()
            .text_color(p.muted_fg)
            .hover(move |s| s.bg(p.tree_hover))
            // Fixed-width slots keep the icon at the same X position on every
            // level — collapsed/expanded states never shift the labels.
            .child(
                div()
                    .w(px(16.))
                    .h_full()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .child(chevron(open, p.muted_fg)),
            )
            .child(
                div()
                    .w(px(18.))
                    .h_full()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .child(icon),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .when(bold_schema, |this| {
                        this.font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.foreground)
                    })
                    .child(label),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_node(&node, window, cx);
            }))
    }

    /// A leaf row (table / view / routine). Clicking opens a tab.
    fn leaf_row(
        &mut self,
        depth: usize,
        meta: Arc<TableMeta>,
        schema: &str,
        icon: AnyElement,
        selected: &SelectedPath,
        p: &theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_sel = selected
            .as_ref()
            .is_some_and(|(s, t)| s == schema && t == &meta.name);
        let count = if meta.row_count > 0 {
            Some(group_digits(meta.row_count))
        } else {
            None
        };
        let node = (schema.to_string(), meta.clone());
        div()
            .id(ElementId::Name(SharedString::from(format!(
                "t:{schema}:{}",
                meta.name
            ))))
            .h(px(32.))
            .flex()
            .items_center()
            .gap(px(4.))
            .mx(px(4.))
            .rounded(px(4.))
            .pr_2()
            .pl(px(depth as f32 * 16. + 2.))
            .text_size(px(13.5))
            .cursor_pointer()
            .when(is_sel, |this| {
                this.bg(p.tree_selected).text_color(p.primary_fg)
            })
            .when(!is_sel, |this| {
                this.text_color(p.tree_fg)
                    .hover(move |s| s.bg(p.tree_hover))
            })
            // Same fixed slots as `parent_row`: chevron slot (empty for
            // leaves) + icon slot, so labels align on every level.
            .child(div().w(px(16.)).h_full().flex_none())
            .child(
                div()
                    .w(px(18.))
                    .h_full()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .child(icon),
            )
            .child(div().min_w_0().truncate().child(meta.name.clone()))
            .when_some(count, |this, c| {
                this.child(
                    div()
                        .ml_auto()
                        .pl_2()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(if is_sel {
                            p.primary_fg.opacity(0.8)
                        } else {
                            p.muted_fg
                        })
                        .child(c),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                let (schema, meta) = &node;
                this.open_tab(schema, meta.clone(), window, cx);
            }))
    }

    /// The per-schema entry to the relationships diagram (opens the ERD tab).
    fn diagram_row(
        &mut self,
        schema: &str,
        p: &theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let schema = schema.to_string();
        div()
            .id(ElementId::Name(SharedString::from(format!("d:{schema}"))))
            .h(px(26.))
            .flex()
            .items_center()
            .gap(px(4.))
            .mx(px(4.))
            .rounded(px(4.))
            .pr_2()
            .pl(px(2. * 16. + 6.))
            .text_size(px(12.))
            .cursor_pointer()
            .text_color(p.muted_fg)
            .hover(move |s| s.bg(p.tree_hover))
            .child(div().w(px(16.)).h_full().flex_none())
            .child(
                svg()
                    .path("icons/schema.svg")
                    .size(px(14.))
                    .flex_none()
                    .text_color(p.primary.opacity(0.7)),
            )
            .child(div().min_w_0().truncate().child("Schema diagram"))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_diagram(&schema, window, cx);
            }))
    }
}

fn empty_note(a: &str, b: &str, c: &str, p: &theme::Palette) -> Div {
    v_flex()
        .px_4()
        .py(px(28.))
        .gap_1()
        .text_size(px(13.))
        .text_color(p.muted_fg)
        .when(!a.is_empty(), |this| this.child(div().child(a.to_string())))
        .when(!b.is_empty(), |this| this.child(div().child(b.to_string())))
        .when(!c.is_empty(), |this| this.child(div().child(c.to_string())))
}
