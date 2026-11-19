//! Schema diagram: a Supabase-style relationships view (React Flow-ish).
//!
//! Table cards are draggable nodes, foreign-key edges are bezier paths
//! painted on a canvas layer below them, from the FK column's row to the
//! referenced column's row, with a dot at each end. Cards start in an
//! automatic layered layout (`auto_layout`: referenced tables left of the
//! tables pointing at them, unrelated ones in a grid below; the toolbar's
//! layout button re-runs it). Hovering or clicking a card highlights its
//! edges and related tables and dims the rest; hovering a column row
//! highlights that column's edge. Pan by dragging the background,
//! zoom with the wheel or the toolbar buttons, `fit` re-frames everything.
//! Relations load asynchronously through the session (stale-guarded by
//! `load_id`); node positions live in diagram space and are scaled by
//! `zoom` + offset by `pan` at render time (there are no CSS transforms
//! in GPUI — everything is multiplied manually).

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, DispatchPhase, ElementId, FontWeight, Hsla,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder,
    Pixels, Render, ScrollDelta, ScrollEvent, SharedString, Styled, Window, canvas, div,
    point, px, svg,
};

use crate::core::model::{DbConnection, NodeKind, Relation, TableMeta};
use crate::core::session::SessionHandle;
use crate::ui::theme;
use gpui_component::{h_flex, v_flex};

// Node metrics, in diagram space (multiplied by `zoom` when painted).
const NODE_W: f32 = 224.0;
const HEADER_H: f32 = 30.0;
const ROW_H: f32 = 22.0;

enum Drag {
    None,
    Pan {
        start: gpui::Point<Pixels>,
        last: gpui::Point<Pixels>,
    },
    Node {
        table: String,
        start: gpui::Point<Pixels>,
        last: gpui::Point<Pixels>,
    },
}

/// A press that moved less than this is a click (select), not a drag.
const CLICK_SLOP: f32 = 4.0;

/// One FK edge, ready to paint: endpoints in diagram space, the direction
/// each end leaves its card (+1 right, −1 left) and whether it is
/// highlighted.
#[derive(Clone, Copy)]
struct EdgeGeom {
    from: (f32, f32),
    to: (f32, f32),
    from_dir: f32,
    to_dir: f32,
    hot: bool,
}

pub struct SchemaDiagram {
    conn: DbConnection,
    session: SessionHandle,
    schema: String,
    tables: Vec<Arc<TableMeta>>,
    relations: Vec<Relation>,
    loading: bool,
    error: Option<String>,
    load_id: u64,
    /// Auto-frame the canvas once the first relations (and paint bounds)
    /// are in.
    pending_fit: bool,
    /// table name -> node origin (top-left, diagram space)
    pos: HashMap<String, (f32, f32)>,
    drag: Drag,
    pan: (f32, f32),
    zoom: f32,
    /// Canvas bounds of the last frame — feeds the wheel-zoom anchor math.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Card under the mouse / clicked card: its relations are highlighted.
    hovered: Option<String>,
    selected: Option<String>,
    /// Column row under the mouse (table, column): its edge is highlighted.
    hovered_col: Option<(String, String)>,
    /// A card was dragged: reloads keep the user's arrangement.
    arranged: bool,
}

fn node_height(t: &TableMeta) -> f32 {
    HEADER_H + ROW_H * t.columns.len() as f32
}

impl SchemaDiagram {
    pub fn new(
        conn: DbConnection,
        session: SessionHandle,
        schema: String,
        tables: Vec<Arc<TableMeta>>,
        cx: &mut Context<Self>,
    ) -> Self {
        // A grid until the relations arrive (`load` lays out again).
        let pos = layout_of(&tables, &[]);
        let mut this = Self {
            conn,
            session,
            schema,
            tables,
            relations: Vec::new(),
            loading: false,
            error: None,
            load_id: 0,
            pending_fit: true,
            pos,
            drag: Drag::None,
            pan: (0., 0.),
            zoom: 1.,
            bounds: Rc::new(Cell::new(Bounds::default())),
            hovered: None,
            selected: None,
            hovered_col: None,
            arranged: false,
        };
        this.load(cx);
        this
    }

    /// Fetch the schema's foreign keys (stale-guarded by load_id).
    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        self.load_id += 1;
        let cur = self.load_id;
        let rx = self.session.list_relations(&self.schema);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |this, cx| {
                if this.load_id != cur {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(Ok(relations)) => {
                        // Only auto-frame the very first load; a manual
                        // refresh keeps the user's layout.
                        if this.relations.is_empty() {
                            this.pending_fit = true;
                        }
                        // Lay out by the relations unless the user has
                        // arranged the cards already.
                        if !this.arranged {
                            this.pos = layout_of(&this.tables, &relations);
                            this.pending_fit = true;
                        }
                        this.relations = relations;
                    }
                    Ok(Err(e)) => this.error = Some(e.message().to_string()),
                    Err(_) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(cx);
    }

    /// Re-run the automatic layout and frame it (toolbar button).
    fn relayout(&mut self) {
        self.pos = layout_of(&self.tables, &self.relations);
        self.arranged = false;
        self.fit();
    }

    /// The table whose relations are highlighted: hovered, else selected.
    fn focus_table(&self) -> Option<&str> {
        self.hovered.as_deref().or(self.selected.as_deref())
    }

    /// Whether a relation is highlighted: a hovered column picks its own
    /// edges, otherwise the focused table's edges.
    fn is_hot(&self, r: &Relation) -> bool {
        if let Some((t, c)) = &self.hovered_col {
            return (r.from_table == *t && r.from_column == *c) || (r.to_table == *t && r.to_column == *c);
        }
        self.focus_table().is_some_and(|t| r.from_table == t || r.to_table == t)
    }

    /// Frame every node inside the canvas (uses the last known bounds).
    pub fn fit(&mut self) {
        let bounds = self.bounds.get();
        let (cw, ch) = (
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        );
        if cw <= 0. || ch <= 0. || self.tables.is_empty() {
            return;
        }
        let mut min = (f32::MAX, f32::MAX);
        let mut max = (f32::MIN, f32::MIN);
        for t in &self.tables {
            let (x, y) = self.pos.get(&t.name).copied().unwrap_or((0., 0.));
            min.0 = min.0.min(x);
            min.1 = min.1.min(y);
            max.0 = max.0.max(x + NODE_W);
            max.1 = max.1.max(y + node_height(t));
        }
        let bw = (max.0 - min.0).max(1.);
        let bh = (max.1 - min.1).max(1.);
        let z = (((cw - 64.) / bw).min((ch - 64.) / bh)).clamp(0.25, 1.25);
        self.zoom = z;
        self.pan.0 = (cw - (min.0 + max.0) * z) / 2.;
        self.pan.1 = (ch - (min.1 + max.1) * z) / 2.;
    }

    /// One FK edge in diagram space: from the FK column's row to the
    /// referenced column's row, on the sides that face each other; cards
    /// that overlap horizontally (stacked, or a self-reference) get a loop
    /// out of their right sides instead of a curve under the cards.
    fn edge_geom(&self, r: &Relation) -> Option<EdgeGeom> {
        let from_meta = self.tables.iter().find(|t| t.name == r.from_table)?;
        let to_meta = self.tables.iter().find(|t| t.name == r.to_table)?;
        let fp = self.pos.get(&r.from_table)?;
        let tp = self.pos.get(&r.to_table)?;
        let fcol = from_meta.columns.iter().position(|c| c.name == r.from_column)? as f32;
        let tcol = to_meta.columns.iter().position(|c| c.name == r.to_column)? as f32;
        let fy = fp.1 + HEADER_H + ROW_H * (fcol + 0.5);
        let ty = tp.1 + HEADER_H + ROW_H * (tcol + 0.5);
        const GAP: f32 = 16.;
        let (fx, fd, tx, td) = if tp.0 + NODE_W + GAP <= fp.0 {
            (fp.0, -1., tp.0 + NODE_W, 1.)
        } else if fp.0 + NODE_W + GAP <= tp.0 {
            (fp.0 + NODE_W, 1., tp.0, -1.)
        } else {
            (fp.0 + NODE_W, 1., tp.0 + NODE_W, 1.)
        };
        Some(EdgeGeom { from: (fx, fy), to: (tx, ty), from_dir: fd, to_dir: td, hot: self.is_hot(r) })
    }

    /// One table card: header + column rows, colored kinds, PK/FK badges.
    fn node_card(
        &mut self,
        t: &Arc<TableMeta>,
        p: &theme::Palette,
        related: Option<&[String]>,
        hot_cols: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Something is highlighted and this card is not part of it.
        let dimmed = related.is_some_and(|r| !r.contains(&t.name));
        let focused = self.focus_table() == Some(t.name.as_str());
        let z = self.zoom;
        let (x, y) = self.pos.get(&t.name).copied().unwrap_or((0., 0.));
        let name = t.name.clone();
        let is_view = t.kind == NodeKind::View;
        let fk_cols: Vec<&str> = self
            .relations
            .iter()
            .filter(|r| r.from_table == t.name)
            .map(|r| r.from_column.as_str())
            .collect();

        let rows: Vec<AnyElement> = t
            .columns
            .iter()
            .map(|c| {
                let type_color = theme::kind_color(&c.kind, p);
                let key = (t.name.clone(), c.name.clone());
                let hot = hot_cols.contains(&key);
                h_flex()
                    .id(ElementId::Name(SharedString::from(format!(
                        "diagram-col:{}:{}",
                        t.name, c.name
                    ))))
                    .when(hot, |this| this.bg(p.primary.opacity(0.14)))
                    .on_hover(cx.listener(move |this, over: &bool, _, cx| {
                        if *over {
                            this.hovered_col = Some(key.clone());
                        } else if this.hovered_col.as_ref() == Some(&key) {
                            this.hovered_col = None;
                        }
                        cx.notify();
                    }))
                    .items_center()
                    .gap(px(4. * z))
                    .px(px(8. * z))
                    .h(px(ROW_H * z))
                    .flex_none()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10. * z))
                            .text_color(p.foreground.opacity(0.85))
                            .child(c.name.clone()),
                    )
                    .when(c.primary_key, |this| this.child(badge("PK", p.primary, z)))
                    .when(fk_cols.contains(&c.name.as_str()), |this| {
                        this.child(badge("FK", p.conn_ok, z))
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(8.5 * z))
                            .font_family(theme::MONO_FONT)
                            .text_color(type_color)
                            .child(c.type_.clone()),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .id(ElementId::Name(SharedString::from(format!(
                "diagram-node:{}",
                t.name
            ))))
            .absolute()
            .left(px(x * z + self.pan.0))
            .top(px(y * z + self.pan.1))
            .w(px(NODE_W * z))
            .rounded(px(8. * z))
            .border_1()
            .border_color(if focused { p.primary } else { p.border })
            .bg(p.card)
            .shadow_md()
            .overflow_hidden()
            .when(dimmed, |this| this.opacity(0.35))
            .on_hover({
                let name = name.clone();
                cx.listener(move |this, over: &bool, _, cx| {
                    if *over {
                        this.hovered = Some(name.clone());
                    } else if this.hovered.as_ref() == Some(&name) {
                        this.hovered = None;
                    }
                    cx.notify();
                })
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.drag = Drag::Node {
                        table: name.clone(),
                        start: e.position,
                        last: e.position,
                    };
                    cx.notify();
                }),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(4. * z))
                    .px(px(8. * z))
                    .h(px(HEADER_H * z))
                    .flex_none()
                    .bg(p.toolbar)
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        svg()
                            .path(if is_view {
                                "icons/cube.svg"
                            } else {
                                "icons/table.svg"
                            })
                            .size(px(11. * z))
                            .flex_none()
                            .text_color(p.primary),
                    )
                    .child(
                        div()
                            .text_size(px(11. * z))
                            .font_weight(FontWeight::SEMIBOLD)
                            .min_w_0()
                            .truncate()
                            .child(t.name.clone()),
                    ),
            )
            .child(v_flex().children(rows))
            .into_any_element()
    }
}

fn badge(label: &str, color: Hsla, z: f32) -> gpui::Div {
    div()
        .px(px(2.5 * z))
        .rounded(px(3. * z))
        .text_size(px(8. * z))
        .flex_none()
        .bg(color.opacity(0.15))
        .text_color(color)
        .child(label.to_string())
}

/// A small square icon button (zoom / fit / reload toolbar controls).
fn canvas_btn(
    id: &'static str,
    icon: gpui_component::IconName,
    hover: Hsla,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use gpui_component::Icon;
    div()
        .id(id)
        .size(theme::scaled(theme::CONTROL_H))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(Icon::new(icon).size_4())
        .on_click(on_click)
}

impl Render for SchemaDiagram {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);

        // First frame after load: frame the diagram once the canvas has
        // actually been painted (bounds are known).
        if self.pending_fit && f32::from(self.bounds.get().size.width) > 0. {
            self.pending_fit = false;
            self.fit();
        }

        // ---------------- toolbar ----------------
        let hover = p.tree_hover;
        let toolbar = h_flex()
            .h(theme::scaled(theme::TOOLBAR_H))
            .flex_none()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(p.toolbar_border)
            .bg(p.toolbar)
            .child(
                svg()
                    .path("icons/schema.svg")
                    .size_4()
                    .flex_none()
                    .text_color(p.muted_fg),
            )
            .child(
                div()
                    .text_size(theme::scaled(theme::TEXT_SM))
                    .text_color(p.muted_fg)
                    .min_w_0()
                    .truncate()
                    .child(format!(
                        "{} · {} — {}",
                        self.conn.database,
                        self.schema,
                        tr!("diagram.summary", tables = self.tables.len(), relations = self.relations.len())
                    )),
            )
            .when_some(self.error.clone(), |this, e| {
                this.child(
                    div()
                        .text_size(theme::scaled(theme::TEXT_SM))
                        .text_color(p.conn_err)
                        .truncate()
                        .child(format!("— {e}")),
                )
            })
            .when(self.loading, |this| {
                this.child(
                    div()
                        .text_size(theme::scaled(theme::TEXT_SM))
                        .text_color(p.muted_fg)
                        .child(tr!("common.loading")),
                )
            })
            .child(div().flex_1())
            .child(canvas_btn(
                "diagram-zoom-out",
                gpui_component::IconName::Minus,
                hover,
                cx.listener(|this, _, _, cx| {
                    this.zoom = (this.zoom / 1.25).max(0.25);
                    cx.notify();
                }),
            ))
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .text_center()
                    .text_size(theme::scaled(theme::TEXT_SM))
                    .text_color(p.muted_fg)
                    .child(format!("{:.0}%", self.zoom * 100.)),
            )
            .child(canvas_btn(
                "diagram-zoom-in",
                gpui_component::IconName::Plus,
                hover,
                cx.listener(|this, _, _, cx| {
                    this.zoom = (this.zoom * 1.25).min(2.);
                    cx.notify();
                }),
            ))
            .child(
                canvas_btn(
                    "diagram-layout",
                    gpui_component::IconName::LayoutDashboard,
                    hover,
                    cx.listener(|this, _, _, cx| {
                        this.relayout();
                        cx.notify();
                    }),
                )
                .tooltip(|_, cx| cx.new(|_| gpui_component::tooltip::Tooltip::new(tr!("diagram.auto_layout"))).into()),
            )
            .child(canvas_btn(
                "diagram-fit",
                gpui_component::IconName::Frame,
                hover,
                cx.listener(|this, _, _, cx| {
                    this.fit();
                    cx.notify();
                }),
            ))
            .child(canvas_btn(
                "diagram-reload",
                gpui_component::IconName::Redo,
                hover,
                cx.listener(|this, _, _, cx| {
                    this.reload(cx);
                }),
            ));

        // ---------------- edges: precomputed geometry (diagram space) ----
        let edges: Vec<EdgeGeom> = self.relations.iter().filter_map(|r| self.edge_geom(r)).collect();
        let any_hot = edges.iter().any(|e| e.hot);
        let z = self.zoom;
        let pan = self.pan;
        let bounds_cell = self.bounds.clone();
        let cold_color = p.primary.opacity(if any_hot { 0.15 } else { 0.5 });
        let hot_color = p.primary;
        let edges_canvas = canvas(
            move |bounds, _, _| {
                // Cache the canvas geometry for the wheel-zoom anchor math.
                bounds_cell.set(bounds);
            },
            move |bounds, _, window, _| {
                let o = bounds.origin;
                let to_screen = |(x, y): (f32, f32)| point(o.x + px(x * z + pan.0), o.y + px(y * z + pan.1));
                // Cold edges first, the highlighted ones on top.
                for hot in [false, true] {
                    let mut b = PathBuilder::stroke(px(if hot { 2.5 } else { 1.5 }));
                    let mut dots = Vec::new();
                    let mut any = false;
                    for e in edges.iter().filter(|e| e.hot == hot) {
                        let (a, c) = (to_screen(e.from), to_screen(e.to));
                        // Control points leave each card horizontally; a
                        // loop (both ends on the right) bulges past both.
                        let (c1x, c2x) = if e.from_dir == e.to_dir {
                            let x = a.x.max(c.x) + px(56. * z);
                            (x, x)
                        } else {
                            let k = ((c.x - a.x).abs() / 2.).max(px(40. * z));
                            (a.x + k * e.from_dir, c.x + k * e.to_dir)
                        };
                        b.move_to(a);
                        b.cubic_bezier_to(c, point(c1x, a.y), point(c2x, c.y));
                        dots.extend([a, c]);
                        any = true;
                    }
                    if !any {
                        continue;
                    }
                    let color = if hot { hot_color } else { cold_color };
                    if let Ok(path) = b.build() {
                        window.paint_path(path, color);
                    }
                    let r = px((if hot { 3.5 } else { 2.5 }) * z.max(0.6));
                    for d in dots {
                        let rect = Bounds::new(point(d.x - r, d.y - r), gpui::size(r * 2., r * 2.));
                        window.paint_quad(gpui::fill(rect, color).corner_radii(r));
                    }
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        // ---------------- nodes ----------------
        // Highlighted relations → the tables and column rows they touch.
        let hot: Vec<&Relation> = self.relations.iter().filter(|r| self.is_hot(r)).collect();
        let mut hot_cols: Vec<(String, String)> = Vec::new();
        let mut related: Vec<String> = self.focus_table().map(|t| vec![t.to_string()]).unwrap_or_default();
        for r in &hot {
            hot_cols.push((r.from_table.clone(), r.from_column.clone()));
            hot_cols.push((r.to_table.clone(), r.to_column.clone()));
            related.extend([r.from_table.clone(), r.to_table.clone()]);
        }
        let something_hot = !hot.is_empty() || self.focus_table().is_some();
        let related = something_hot.then_some(related);
        let tables = self.tables.clone();
        let mut nodes: Vec<AnyElement> = Vec::with_capacity(tables.len());
        for t in &tables {
            nodes.push(self.node_card(t, &p, related.as_deref(), &hot_cols, cx));
        }

        // ---------------- canvas ----------------
        let canvas_div = div()
            .id("diagram-canvas")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(p.background)
            .child(edges_canvas)
            .children(nodes)
            // Pan: dragging the background (node cards stop propagation).
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| {
                this.drag = Drag::Pan { start: e.position, last: e.position };
                cx.notify();
            }))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                match &mut this.drag {
                    Drag::Pan { last, .. } => {
                        this.pan.0 += f32::from(e.position.x - last.x);
                        this.pan.1 += f32::from(e.position.y - last.y);
                        *last = e.position;
                        cx.notify();
                    }
                    Drag::Node { table, last, .. } => {
                        let dx = f32::from(e.position.x - last.x) / this.zoom;
                        let dy = f32::from(e.position.y - last.y) / this.zoom;
                        *last = e.position;
                        let name = table.clone();
                        if let Some(pos) = this.pos.get_mut(&name) {
                            pos.0 += dx;
                            pos.1 += dy;
                        }
                        this.arranged = true;
                        cx.notify();
                    }
                    Drag::None => {}
                }
            }))
            // Zoom anchored at the cursor (bounds come from the edges layer).
            .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| {
                let dy = match e.delta {
                    ScrollDelta::Pixels(d) => f32::from(d.y),
                    ScrollDelta::Lines(d) => d.y * 40.,
                };
                // Inverted: wheel up zooms out, wheel down zooms in.
                let factor = if dy < 0. { 0.9 } else { 1.1 };
                let old = this.zoom;
                let new = (old * factor).clamp(0.25, 2.);
                if new == old {
                    return;
                }
                let bounds = this.bounds.get();
                let mx = f32::from(e.position.x - bounds.origin.x);
                let my = f32::from(e.position.y - bounds.origin.y);
                let dx = (mx - this.pan.0) / old;
                let dy = (my - this.pan.1) / old;
                this.zoom = new;
                this.pan.0 = mx - dx * new;
                this.pan.1 = my - dy * new;
                cx.notify();
            }));

        // Window-level mouse-up: drags must end even when the button is
        // released outside the canvas (same paint-phase pattern as the
        // nav splitter in `app.rs`).
        let view = cx.entity().downgrade();
        let mouseup_guard = canvas(
            |_, _, _| {},
            {
                let view = view.clone();
                move |_, _, window, _| {
                    let view = view.clone();
                    window.on_mouse_event(
                        move |_: &MouseUpEvent, phase, _, cx: &mut gpui::App| {
                            if phase == DispatchPhase::Bubble {
                                view.update(cx, |this, cx| {
                                    let moved = |a: gpui::Point<Pixels>, b: gpui::Point<Pixels>| {
                                        f32::from((a.x - b.x).abs() + (a.y - b.y).abs()) > CLICK_SLOP
                                    };
                                    match std::mem::replace(&mut this.drag, Drag::None) {
                                        Drag::None => return,
                                        // A click on a card pins its relations;
                                        // clicking it again or the background
                                        // clears that.
                                        Drag::Node { table, start, last } if !moved(start, last) => {
                                            this.selected =
                                                (this.selected.as_deref() != Some(table.as_str())).then_some(table);
                                        }
                                        Drag::Pan { start, last } if !moved(start, last) => this.selected = None,
                                        _ => {}
                                    }
                                    cx.notify();
                                })
                                .ok();
                            }
                        },
                    );
                }
            },
        )
        .absolute()
        .size_full();

        v_flex()
            .id("diagram-root")
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(mouseup_guard)
            .child(toolbar)
            .child(canvas_div)
    }
}

/// `auto_layout` over the cards' heights.
fn layout_of(tables: &[Arc<TableMeta>], relations: &[Relation]) -> HashMap<String, (f32, f32)> {
    let nodes: Vec<(&str, f32)> = tables.iter().map(|t| (t.name.as_str(), node_height(t))).collect();
    auto_layout(&nodes, relations)
}

// ---------------- automatic layout ----------------

/// Horizontal gap between layers and vertical gap between stacked cards.
const GAP_X: f32 = 120.0;
const GAP_Y: f32 = 40.0;

/// Layered left-to-right layout (Sugiyama-lite), pure so it is testable:
/// referenced tables sit left of the tables that point at them (a table's
/// layer = the longest FK chain above it), each layer is ordered by the
/// average position of its neighbours (one sweep down, one up) to keep
/// edges short and uncrossed, and layers are centered on each other.
/// Tables without relations go into a grid below. `nodes` = (name, card
/// height) in display order; returns top-left origins in diagram space.
fn auto_layout(nodes: &[(&str, f32)], relations: &[Relation]) -> HashMap<String, (f32, f32)> {
    let n = nodes.len();
    let ix: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, (name, _))| (*name, i)).collect();
    let mut parents: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for r in relations {
        let (Some(&c), Some(&p)) = (ix.get(r.from_table.as_str()), ix.get(r.to_table.as_str())) else {
            continue;
        };
        if c == p || parents[c].contains(&p) {
            continue;
        }
        parents[c].push(p);
        children[p].push(c);
    }

    // Layer = longest path to a root along FK edges (cycles are cut where
    // the walk meets a table already on its stack).
    fn rank_of(i: usize, parents: &[Vec<usize>], rank: &mut [Option<usize>], on_stack: &mut [bool]) -> usize {
        if let Some(r) = rank[i] {
            return r;
        }
        on_stack[i] = true;
        let mut r = 0;
        for &p in &parents[i] {
            if !on_stack[p] {
                r = r.max(rank_of(p, parents, rank, on_stack) + 1);
            }
        }
        on_stack[i] = false;
        rank[i] = Some(r);
        r
    }
    let mut rank = vec![None; n];
    let mut on_stack = vec![false; n];
    let linked: Vec<usize> = (0..n).filter(|&i| !parents[i].is_empty() || !children[i].is_empty()).collect();
    let mut layers: Vec<Vec<usize>> = Vec::new();
    for &i in &linked {
        let r = rank_of(i, &parents, &mut rank, &mut on_stack);
        if layers.len() <= r {
            layers.resize(r + 1, Vec::new());
        }
        layers[r].push(i);
    }

    // Order inside layers: hubs first in the leftmost layer, then by the
    // barycenter of the neighbours in the previous / next layer.
    let mut order = vec![0f32; n];
    let set_order = |layer: &[usize], order: &mut [f32]| {
        for (k, &i) in layer.iter().enumerate() {
            order[i] = k as f32;
        }
    };
    let bary = |of: &[usize], order: &[f32]| -> Option<f32> {
        (!of.is_empty()).then(|| of.iter().map(|&j| order[j]).sum::<f32>() / of.len() as f32)
    };
    if let Some(first) = layers.first_mut() {
        first.sort_by(|&a, &b| children[b].len().cmp(&children[a].len()).then(nodes[a].0.cmp(nodes[b].0)));
        set_order(first, &mut order);
    }
    for k in 1..layers.len() {
        let mut layer = std::mem::take(&mut layers[k]);
        layer.sort_by(|&a, &b| {
            let (ba, bb) = (bary(&parents[a], &order).unwrap_or(f32::MAX), bary(&parents[b], &order).unwrap_or(f32::MAX));
            ba.total_cmp(&bb).then(nodes[a].0.cmp(nodes[b].0))
        });
        set_order(&layer, &mut order);
        layers[k] = layer;
    }
    for k in (0..layers.len().saturating_sub(1)).rev() {
        let mut layer = std::mem::take(&mut layers[k]);
        let key: HashMap<usize, f32> =
            layer.iter().map(|&i| (i, bary(&children[i], &order).unwrap_or(order[i]))).collect();
        layer.sort_by(|a, b| key[a].total_cmp(&key[b]));
        set_order(&layer, &mut order);
        layers[k] = layer;
    }

    let mut pos = HashMap::new();
    let height = |i: usize| nodes[i].1;
    let span = |layer: &[usize]| -> f32 {
        layer.iter().map(|&i| height(i)).sum::<f32>() + GAP_Y * layer.len().saturating_sub(1) as f32
    };
    let tallest = layers.iter().map(|l| span(l)).fold(0f32, f32::max);
    for (k, layer) in layers.iter().enumerate() {
        let x = k as f32 * (NODE_W + GAP_X);
        let mut y = (tallest - span(layer)) / 2.;
        for &i in layer {
            pos.insert(nodes[i].0.to_string(), (x, y));
            y += height(i) + GAP_Y;
        }
    }

    // Unrelated tables: a grid under the graph, as wide as the graph.
    let isolated: Vec<usize> = (0..n).filter(|i| !linked.contains(i)).collect();
    let cols = layers.len().max(3);
    let mut y = if layers.is_empty() { 0. } else { tallest + GAP_Y * 2. };
    for row in isolated.chunks(cols) {
        for (c, &i) in row.iter().enumerate() {
            pos.insert(nodes[i].0.to_string(), (c as f32 * (NODE_W + GAP_X / 2.), y));
        }
        y += row.iter().map(|&i| height(i)).fold(0f32, f32::max) + GAP_Y;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(from: &str, to: &str) -> Relation {
        Relation { from_table: from.into(), from_column: "x_id".into(), to_table: to.into(), to_column: "id".into() }
    }

    fn overlaps(a: (f32, f32, f32), b: (f32, f32, f32)) -> bool {
        a.0 < b.0 + NODE_W && b.0 < a.0 + NODE_W && a.1 < b.1 + b.2 && b.1 < a.1 + a.2
    }

    #[test]
    fn referenced_tables_sit_left_and_nothing_overlaps() {
        let nodes = [("users", 162.), ("orders", 140.), ("products", 162.), ("sessions", 118.), ("user_summary", 118.)];
        let pos = auto_layout(&nodes, &[rel("orders", "users"), rel("sessions", "users")]);
        let (u, o, s) = (pos["users"], pos["orders"], pos["sessions"]);
        assert!(u.0 < o.0 && o.0 == s.0, "{pos:?}");
        // Unrelated tables below the whole graph.
        let graph_bottom = [(o, 140.), (s, 118.), (u, 162.)].iter().map(|(p, h)| p.1 + h).fold(0., f32::max);
        assert!(pos["products"].1 >= graph_bottom && pos["user_summary"].1 >= graph_bottom);
        let rects: Vec<(f32, f32, f32)> = nodes.iter().map(|(n, h)| (pos[*n].0, pos[*n].1, *h)).collect();
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                assert!(!overlaps(rects[i], rects[j]), "{} / {}", nodes[i].0, nodes[j].0);
            }
        }
    }

    #[test]
    fn chains_cycles_and_self_references_stay_finite() {
        let nodes = [("a", 50.), ("b", 50.), ("c", 50.), ("d", 50.)];
        let pos = auto_layout(&nodes, &[rel("b", "a"), rel("c", "b"), rel("a", "c"), rel("d", "d"), rel("c", "a")]);
        assert_eq!(pos.len(), 4);
        assert!(pos.values().all(|p| p.0.is_finite() && p.1.is_finite()));
        // A self-reference alone does not make a table part of the graph.
        assert!(pos["d"].1 > pos["a"].1.max(pos["b"].1).max(pos["c"].1));
        // No relations at all: a plain grid from the origin.
        let grid = auto_layout(&nodes, &[]);
        assert_eq!(grid["a"], (0., 0.));
    }
}
