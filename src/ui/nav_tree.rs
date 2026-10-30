//! Database explorer tree. Port of the web app's `fileTree.tsx`.
//!
//! The tree body scrolls and is virtualized (`v_virtual_list`): every
//! frame `visible_tree_rows` rebuilds a flat *data* model of the visible
//! nodes, and only the on-screen slice of that model is turned into
//! elements — a schema with thousands of tables costs one cheap pass,
//! not thousands of element trees.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Div, ElementId, FontWeight, Hsla, IntoElement, ParentElement,
    SharedString, Stateful, Styled, Window, div, px, rgb, size, svg,
};
use gpui_component::input::Input;
use gpui_component::scroll::Scrollbar;
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_component::{h_flex, v_flex, v_virtual_list, Icon, IconName, Sizable, Size};

use crate::core::model::{group_digits, ConnectionStatus, DbType, NodeKind, TableMeta};
use crate::ui::app::DataBridge;
use crate::ui::theme;

pub type SelectedPath = Option<(String, String)>;

// ---------------- row model ----------------

/// Height of a regular tree row (groups + leaves).
const ROW_H: f32 = 32.;
/// Height of the per-schema "Schema diagram" entry.
const DIAGRAM_H: f32 = 26.;
/// Empty-note metrics: `note_row` renders with exactly these, so the
/// virtual list can position note rows without measuring them.
const NOTE_PAD_Y: f32 = 28.;
const NOTE_LINE_H: f32 = 18.;
const NOTE_GAP: f32 = 4.;

fn note_height(lines: usize) -> f32 {
    let lines = lines.max(1) as f32;
    NOTE_PAD_Y * 2. + lines * NOTE_LINE_H + (lines - 1.) * NOTE_GAP
}

/// The icon of a tree row, kept as data (not an element) so the row model
/// stays cheap to rebuild every frame.
#[derive(Clone)]
pub(crate) enum TreeIcon {
    /// Database brand glyph in the brand's color.
    Brand(DbType),
    /// An SVG from our asset set.
    Glyph(&'static str, Hsla),
    /// A lucide icon shipped with gpui-component.
    Lucide(IconName, Hsla),
}

/// One visible tree row, as data. The virtual list converts only the
/// on-screen range of these into elements.
pub enum TreeRow {
    /// Expandable group: connection, schemas, a schema, a kind group.
    Parent {
        depth: usize,
        label: SharedString,
        node_id: String,
        icon: TreeIcon,
        open: bool,
        /// Schema rows render semibold.
        bold: bool,
    },
    /// Table / view / routine — opens a records tab.
    Leaf {
        depth: usize,
        meta: Arc<TableMeta>,
        schema: String,
        icon: TreeIcon,
        selected: bool,
    },
    /// The per-schema entry to the relationships diagram.
    Diagram { schema: String },
    /// The per-schema "New table" entry (SQL stores only).
    NewTable { schema: String },
    /// Empty-state / no-match note. Lines render single-line truncated at
    /// a fixed line height, so the height is exactly `note_height(len)`.
    Note { lines: Vec<String> },
}

impl TreeRow {
    /// The height the virtual list reserves for this row. Every height is
    /// known up-front — that is what makes the list virtualizable.
    fn height(&self) -> f32 {
        match self {
            Self::Parent { .. } | Self::Leaf { .. } => ROW_H,
            Self::Diagram { .. } | Self::NewTable { .. } => DIAGRAM_H,
            Self::Note { lines } => note_height(lines.len()),
        }
    }
}

fn brand_icon(t: DbType, size: f32) -> AnyElement {
    svg()
        .path(t.brand_icon())
        .size(theme::scaled(size))
        .flex_none()
        .text_color(rgb(t.brand_color()))
        .into_any_element()
}

fn glyph_icon(path: &'static str, color: Hsla, size: f32) -> AnyElement {
    svg()
        .path(path)
        .size(theme::scaled(size))
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

fn render_icon(icon: &TreeIcon) -> AnyElement {
    render_icon_recolored(icon, None)
}

/// Render a tree icon with an optional tint override. Selected leaf rows
/// repaint their icon in the selection foreground — the primary-colored
/// glyphs (tables) would otherwise melt into the equally primary
/// selection background.
fn render_icon_recolored(icon: &TreeIcon, recolor: Option<Hsla>) -> AnyElement {
    match icon {
        TreeIcon::Brand(t) => brand_icon(*t, 15.),
        TreeIcon::Glyph(path, own) => glyph_icon(path, recolor.unwrap_or(*own), 15.),
        TreeIcon::Lucide(name, own) => Icon::new(name.clone())
            .with_size(Size::Size(px(15.)))
            .text_color(recolor.unwrap_or(*own))
            .into_any_element(),
    }
}

impl DataBridge {
    pub fn render_nav_tree(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        // Rebuild the flat visible-row model first (pure data), then let
        // the virtual list render only the on-screen slice of it.
        let selected = self.selected_path();
        let rows = self.visible_tree_rows(&selected, &p);
        self.nav_rows = rows;
        let conn = self
            .active_conn
            .as_ref()
            .expect("render_nav_tree requires an active connection");

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
                    .h(theme::scaled(36.))
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
                            .text_size(theme::scaled(theme::TEXT_MD))
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
                                        .h(theme::scaled(36.)),
                                ),
                            )
                            .when(!self.query.trim().is_empty(), |this| {
                                this.child(
                                    div()
                                        .id("clear-search")
                                        .size(theme::scaled(22.))
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
                            .size(theme::scaled(32.))
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
                                    .size(theme::scaled(15.))
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
                            .size(theme::scaled(32.))
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
                                    .size(theme::scaled(15.))
                                    .text_color(p.muted_fg),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh(cx);
                            })),
                    ),
            )
            // tree body — virtualized scroll: `nav_rows` is the flat model,
            // the list turns only the visible range into elements and the
            // scrollbar overlays the same container.
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .py(px(2.))
                    .child(
                        v_virtual_list(
                            cx.entity(),
                            "nav-tree-rows",
                            Rc::new(
                                self.nav_rows
                                    .iter()
                                    .map(|row| size(px(0.), px(row.height() * p.scale)))
                                    .collect::<Vec<_>>(),
                            ),
                            |this: &mut DataBridge,
                             range: Range<usize>,
                             _window: &mut Window,
                             cx: &mut Context<DataBridge>| {
                                let p = theme::palette(cx);
                                range
                                    .filter_map(|ix| {
                                        this.nav_rows.get(ix).map(|row| {
                                            // Root-laid virtual items shrink-wrap to
                                            // their content; the full-width wrapper
                                            // restores the stretch behavior of the
                                            // plain flex column this tree used before,
                                            // so hover/selection span the whole row.
                                            v_flex()
                                                .w_full()
                                                .child(render_tree_row(row, &p, cx))
                                                .into_any_element()
                                        })
                                    })
                                    .collect::<Vec<AnyElement>>()
                            },
                        )
                        // Keep the scroll position on the entity; the same
                        // handle drives the scrollbar overlay below.
                        .track_scroll(&self.tree_scroll),
                    )
                    .child(Scrollbar::vertical(&self.tree_scroll)),
            )
    }

    /// The flat list of visible tree rows, honoring expansion + search
    /// filter. Pure data — cheap to rebuild every frame; only the
    /// on-screen slice is ever turned into elements.
    fn visible_tree_rows(&self, selected: &SelectedPath, p: &theme::Palette) -> Vec<TreeRow> {
        let conn = self
            .active_conn
            .as_ref()
            .expect("render_nav_tree requires an active connection");
        let q = self.query.trim().to_lowercase();
        let schemas = &self.schemas;
        let expanded = &self.expanded;
        let can_create = self.can_create_tables();

        let mut rows: Vec<TreeRow> = Vec::new();

        let conn_open = expanded.contains("conn");
        rows.push(TreeRow::Parent {
            depth: 0,
            label: SharedString::from(if conn.database.is_empty() {
                conn.id.clone()
            } else {
                conn.database.clone()
            }),
            node_id: "conn".into(),
            icon: TreeIcon::Brand(conn.db_type),
            open: conn_open,
            bold: false,
        });
        if !conn_open {
            return rows;
        }

        let schemas_open = expanded.contains("schemas");
        rows.push(TreeRow::Parent {
            depth: 1,
            label: "Schemas".into(),
            node_id: "schemas".into(),
            icon: TreeIcon::Glyph("icons/cube.svg", p.muted_fg),
            open: schemas_open,
            bold: false,
        });
        if !schemas_open {
            return rows;
        }

        if schemas.is_empty() {
            let hint = match conn.status {
                ConnectionStatus::Error => "Last connection attempt failed.",
                ConnectionStatus::Disconnected => "Connection not open.",
                _ => "Refresh or pick another connection.",
            };
            rows.push(TreeRow::Note {
                lines: vec!["Schemas list is empty.".into(), hint.into()],
            });
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
            rows.push(TreeRow::Parent {
                depth: 2,
                label: SharedString::from(sch.name.clone()),
                node_id: sid,
                icon: TreeIcon::Glyph("icons/cube.svg", p.foreground.opacity(0.8)),
                open: sch_open,
                bold: true,
            });
            if !sch_open {
                continue;
            }

            // The schema-wide relationships diagram entry.
            rows.push(TreeRow::Diagram {
                schema: sch.name.clone(),
            });
            if can_create {
                rows.push(TreeRow::NewTable {
                    schema: sch.name.clone(),
                });
            }

            // Tables group (shown when not filtering or non-empty)
            if q.is_empty() || !tables.is_empty() {
                let gid = format!("ss:{}:tables", sch.name);
                let g_open = expanded.contains(&gid);
                rows.push(TreeRow::Parent {
                    depth: 3,
                    label: SharedString::from(format!("Tables ({})", tables.len())),
                    node_id: gid,
                    icon: TreeIcon::Glyph("icons/cube.svg", p.muted_fg),
                    open: g_open,
                    bold: false,
                });
                if g_open {
                    for t in tables {
                        rows.push(TreeRow::Leaf {
                            depth: 4,
                            meta: t.clone(),
                            schema: sch.name.clone(),
                            icon: TreeIcon::Glyph("icons/table.svg", p.primary),
                            selected: selected
                                .as_ref()
                                .is_some_and(|(s, n)| s == &sch.name && n == &t.name),
                        });
                    }
                }
            }

            // Views group (hidden when filtering only tables)
            if !self.filter_only_tables && !views.is_empty() {
                let gid = format!("ss:{}:views", sch.name);
                let g_open = expanded.contains(&gid);
                rows.push(TreeRow::Parent {
                    depth: 3,
                    label: SharedString::from(format!("Views ({})", views.len())),
                    node_id: gid,
                    icon: TreeIcon::Glyph("icons/cube.svg", p.muted_fg),
                    open: g_open,
                    bold: false,
                });
                if g_open {
                    for t in views {
                        rows.push(TreeRow::Leaf {
                            depth: 4,
                            meta: t.clone(),
                            schema: sch.name.clone(),
                            icon: TreeIcon::Lucide(IconName::Eye, p.chart_2),
                            selected: selected
                                .as_ref()
                                .is_some_and(|(s, n)| s == &sch.name && n == &t.name),
                        });
                    }
                }
            }

            // Routines group (hidden when filtering only tables)
            if !self.filter_only_tables && !funcs.is_empty() {
                let gid = format!("ss:{}:routines", sch.name);
                let g_open = expanded.contains(&gid);
                rows.push(TreeRow::Parent {
                    depth: 3,
                    label: SharedString::from(format!("Routines ({})", funcs.len())),
                    node_id: gid,
                    icon: TreeIcon::Glyph("icons/cube.svg", p.muted_fg),
                    open: g_open,
                    bold: false,
                });
                if g_open {
                    for t in funcs {
                        rows.push(TreeRow::Leaf {
                            depth: 4,
                            meta: t.clone(),
                            schema: sch.name.clone(),
                            icon: TreeIcon::Glyph("icons/sigma.svg", p.chart_3),
                            selected: selected
                                .as_ref()
                                .is_some_and(|(s, n)| s == &sch.name && n == &t.name),
                        });
                    }
                }
            }
        }

        if !visible_any {
            rows.push(TreeRow::Note {
                lines: vec![format!("No matches for \u{201c}{q}\u{201d}")],
            });
        }

        rows
    }
}

/// Turn one model row into its element. Called only for visible rows.
fn render_tree_row(row: &TreeRow, p: &theme::Palette, cx: &mut Context<DataBridge>) -> AnyElement {
    match row {
        TreeRow::Parent {
            depth,
            label,
            node_id,
            icon,
            open,
            bold,
        } => parent_row(
            *depth,
            label.clone(),
            node_id.clone(),
            render_icon(icon),
            *open,
            *bold,
            p,
            cx,
        )
        .into_any_element(),
        TreeRow::Leaf {
            depth,
            meta,
            schema,
            icon,
            selected,
        } => {
            let row = leaf_row(
                *depth,
                meta.clone(),
                schema,
                // On the selected row the icon shares the selection's
                // foreground so the type-colored glyph stays visible.
                render_icon_recolored(icon, selected.then(|| p.primary_fg)),
                *selected,
                p,
                cx,
            );
            // Right-click menu for droppable objects (tables, views).
            if meta.kind.drop_keyword().is_none() {
                return row.into_any_element();
            }
            let app = cx.entity().downgrade();
            let schema = schema.clone();
            let meta = meta.clone();
            row.context_menu(move |menu, _, _| {
                let (open_app, drop_app) = (app.clone(), app.clone());
                let (open_schema, drop_schema) = (schema.clone(), schema.clone());
                let (open_meta, drop_meta) = (meta.clone(), meta.clone());
                let drop_label = if meta.kind == NodeKind::View {
                    "Drop view…"
                } else {
                    "Drop table…"
                };
                menu.item(PopupMenuItem::new("Open").icon(IconName::ArrowRight).on_click(
                    move |_, window, cx| {
                        open_app
                            .update(cx, |this, cx| {
                                this.open_tab(&open_schema, open_meta.clone(), window, cx)
                            })
                            .ok();
                    },
                ))
                .separator()
                .item(PopupMenuItem::new(drop_label).icon(IconName::Delete).on_click(
                    move |_, _, cx| {
                        drop_app
                            .update(cx, |this, cx| {
                                this.ask_drop(&drop_schema, drop_meta.clone(), cx)
                            })
                            .ok();
                    },
                ))
            })
            .into_any_element()
        }
        TreeRow::Diagram { schema } => diagram_row(schema, p, cx).into_any_element(),
        TreeRow::NewTable { schema } => new_table_row(schema, p, cx).into_any_element(),
        TreeRow::Note { lines } => note_row(lines, p).into_any_element(),
    }
}

/// An expandable row: chevron + icon + label. Clicking toggles expansion.
fn parent_row(
    depth: usize,
    label: SharedString,
    node_id: String,
    icon: AnyElement,
    open: bool,
    bold: bool,
    p: &theme::Palette,
    cx: &mut Context<DataBridge>,
) -> Stateful<Div> {
    let node = node_id.clone();
    div()
        .id(ElementId::Name(SharedString::from(node_id)))
        .h(theme::scaled(ROW_H))
        .flex()
        .items_center()
        .gap(px(4.))
        .mx(px(4.))
        .rounded(px(4.))
        .pr_2()
        .pl(px(depth as f32 * 16. + 2.))
        .text_size(theme::scaled(theme::TEXT_MD))
        .cursor_pointer()
        .text_color(p.muted_fg)
        .hover(move |s| s.bg(p.tree_hover))
        // Fixed-width slots keep the icon at the same X position on every
        // level — collapsed/expanded states never shift the labels.
        .child(
            div()
                .w(theme::scaled(16.))
                .h_full()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .child(chevron(open, p.muted_fg)),
        )
        .child(
            div()
                .w(theme::scaled(18.))
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
                .when(bold, |this| {
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
    depth: usize,
    meta: Arc<TableMeta>,
    schema: &str,
    icon: AnyElement,
    is_sel: bool,
    p: &theme::Palette,
    cx: &mut Context<DataBridge>,
) -> Stateful<Div> {
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
        .h(theme::scaled(ROW_H))
        .flex()
        .items_center()
        .gap(px(4.))
        .mx(px(4.))
        .rounded(px(4.))
        .pr_2()
        .pl(px(depth as f32 * 16. + 2.))
        .text_size(theme::scaled(theme::TEXT_MD))
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
        .child(div().w(theme::scaled(16.)).h_full().flex_none())
        .child(
            div()
                .w(theme::scaled(18.))
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
                    .text_size(theme::scaled(theme::TEXT_XS))
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
fn diagram_row(schema: &str, p: &theme::Palette, cx: &mut Context<DataBridge>) -> Stateful<Div> {
    let schema = schema.to_string();
    div()
        .id(ElementId::Name(SharedString::from(format!("d:{schema}"))))
        .h(theme::scaled(DIAGRAM_H))
        .flex()
        .items_center()
        .gap(px(4.))
        .mx(px(4.))
        .rounded(px(4.))
        .pr_2()
        .pl(px(2. * 16. + 6.))
        .text_size(theme::scaled(theme::TEXT_SM))
        .cursor_pointer()
        .text_color(p.muted_fg)
        .hover(move |s| s.bg(p.tree_hover))
        .child(div().w(theme::scaled(16.)).h_full().flex_none())
        .child(
            svg()
                .path("icons/schema.svg")
                .size(theme::scaled(14.))
                .flex_none()
                .text_color(p.primary.opacity(0.7)),
        )
        .child(div().min_w_0().truncate().child("Schema diagram"))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_diagram(&schema, window, cx);
        }))
}

/// The per-schema "New table" entry: a SQL tab with a `CREATE TABLE`
/// template. Same metrics as `diagram_row`.
fn new_table_row(schema: &str, p: &theme::Palette, cx: &mut Context<DataBridge>) -> Stateful<Div> {
    let schema = schema.to_string();
    div()
        .id(ElementId::Name(SharedString::from(format!("nt:{schema}"))))
        .h(theme::scaled(DIAGRAM_H))
        .flex()
        .items_center()
        .gap(px(4.))
        .mx(px(4.))
        .rounded(px(4.))
        .pr_2()
        .pl(px(2. * 16. + 6.))
        .text_size(theme::scaled(theme::TEXT_SM))
        .cursor_pointer()
        .text_color(p.muted_fg)
        .hover(move |s| s.bg(p.tree_hover))
        .child(div().w(theme::scaled(16.)).h_full().flex_none())
        .child(
            Icon::new(IconName::Plus)
                .with_size(Size::Size(px(14.)))
                .flex_none()
                .text_color(p.primary.opacity(0.7)),
        )
        .child(div().min_w_0().truncate().child("New table"))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_create_table(&schema, window, cx);
        }))
}

/// The empty-state / no-match note. Lines render single-line truncated at
/// a fixed line height, so the row's natural height is exactly
/// `note_height(lines)` — the virtual list needs no measuring.
fn note_row(lines: &[String], p: &theme::Palette) -> Div {
    v_flex()
        .px_4()
        .py(theme::scaled(NOTE_PAD_Y))
        .gap(theme::scaled(NOTE_GAP))
        .text_size(theme::scaled(theme::TEXT_MD))
        .line_height(theme::scaled(NOTE_LINE_H))
        .text_color(p.muted_fg)
        .children(
            lines
                .iter()
                .map(|line| div().min_w_0().truncate().child(line.clone())),
        )
}
