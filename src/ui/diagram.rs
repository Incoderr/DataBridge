//! Schema diagram: a Supabase-style relationships view (React Flow-ish).
//!
//! Table cards are draggable nodes, foreign-key edges are bezier paths
//! painted on a canvas layer below them. Pan by dragging the background,
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
    Pixels, Render, ScrollDelta, ScrollWheelEvent, SharedString, Styled, Window, canvas, div,
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
        last: gpui::Point<Pixels>,
    },
    Node {
        table: String,
        last: gpui::Point<Pixels>,
    },
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
        // Deterministic initial layout: a 3-column grid.
        let mut pos = HashMap::new();
        for (i, t) in tables.iter().enumerate() {
            pos.insert(
                t.name.clone(),
                (
                    48. + (i % 3) as f32 * (NODE_W + 80.),
                    48. + (i / 3) as f32 * 340.,
                ),
            );
        }
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

    /// The facing endpoints of one FK edge, in diagram space.
    fn edge_endpoints(&self, r: &Relation) -> Option<((f32, f32), (f32, f32))> {
        let from_meta = self.tables.iter().find(|t| t.name == r.from_table)?;
        let to_meta = self.tables.iter().find(|t| t.name == r.to_table)?;
        let fp = self.pos.get(&r.from_table)?;
        let tp = self.pos.get(&r.to_table)?;
        let fcol = from_meta.columns.iter().position(|c| c.name == r.from_column)? as f32;
        let tcol = to_meta.columns.iter().position(|c| c.name == r.to_column)? as f32;
        // Exit on the side that faces the other node.
        let from_left = fp.0 + NODE_W / 2. > tp.0 + NODE_W / 2.;
        let fx = if from_left { fp.0 } else { fp.0 + NODE_W };
        let tx = if from_left { tp.0 + NODE_W } else { tp.0 };
        let fy = fp.1 + HEADER_H + ROW_H * (fcol + 0.5);
        let ty = tp.1 + HEADER_H + ROW_H * (tcol + 0.5);
        Some(((fx, fy), (tx, ty)))
    }

    /// One table card: header + column rows, colored kinds, PK/FK badges.
    fn node_card(
        &mut self,
        t: &Arc<TableMeta>,
        p: &theme::Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
                h_flex()
                    .id(ElementId::Name(SharedString::from(format!(
                        "diagram-col:{}:{}",
                        t.name, c.name
                    ))))
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
            .border_color(p.border)
            .bg(p.card)
            .shadow_md()
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.drag = Drag::Node {
                        table: name.clone(),
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
) -> impl IntoElement {
    use gpui_component::{Icon, Sizable, Size};
    div()
        .id(id)
        .size(px(28.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(Icon::new(icon).with_size(Size::Size(px(14.))))
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
            .h(px(36.))
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
                    .size_3p5()
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
                        "{} · {} — {} tables, {} relations",
                        self.conn.database,
                        self.schema,
                        self.tables.len(),
                        self.relations.len()
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
                        .child("loading…"),
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

        // ---------------- edges: precomputed endpoints (canvas-local) ----
        let edges: Vec<((f32, f32), (f32, f32))> = self
            .relations
            .iter()
            .filter_map(|r| self.edge_endpoints(r))
            .collect();
        let z = self.zoom;
        let pan = self.pan;
        let bounds_cell = self.bounds.clone();
        let edge_color = p.primary.opacity(0.45);
        let edges_canvas = canvas(
            move |bounds, _, _| {
                // Cache the canvas geometry for the wheel-zoom anchor math.
                bounds_cell.set(bounds);
                let ox = bounds.origin.x;
                let oy = bounds.origin.y;
                let mut b = PathBuilder::stroke(px(1.5));
                for ((fx, fy), (tx, ty)) in &edges {
                    let sx1 = ox + px(fx * z + pan.0);
                    let sy1 = oy + px(fy * z + pan.1);
                    let sx2 = ox + px(tx * z + pan.0);
                    let sy2 = oy + px(ty * z + pan.1);
                    let mid = (sx1 + sx2) / 2.;
                    b.move_to(point(sx1, sy1));
                    b.cubic_bezier_to(point(sx2, sy2), point(mid, sy1), point(mid, sy2));
                }
                b.build().ok()
            },
            move |_, path, window, _| {
                if let Some(path) = path {
                    window.paint_path(path, edge_color);
                }
            },
        )
        .absolute()
        .size_full();

        // ---------------- nodes ----------------
        let tables = self.tables.clone();
        let mut nodes: Vec<AnyElement> = Vec::with_capacity(tables.len());
        for t in &tables {
            nodes.push(self.node_card(t, &p, cx));
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
                this.drag = Drag::Pan { last: e.position };
                cx.notify();
            }))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                match &mut this.drag {
                    Drag::Pan { last } => {
                        this.pan.0 += f32::from(e.position.x - last.x);
                        this.pan.1 += f32::from(e.position.y - last.y);
                        *last = e.position;
                        cx.notify();
                    }
                    Drag::Node { table, last } => {
                        let dx = f32::from(e.position.x - last.x) / this.zoom;
                        let dy = f32::from(e.position.y - last.y) / this.zoom;
                        *last = e.position;
                        let name = table.clone();
                        if let Some(pos) = this.pos.get_mut(&name) {
                            pos.0 += dx;
                            pos.1 += dy;
                        }
                        cx.notify();
                    }
                    Drag::None => {}
                }
            }))
            // Zoom anchored at the cursor (bounds come from the edges layer).
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
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
                                    if !matches!(this.drag, Drag::None) {
                                        this.drag = Drag::None;
                                        cx.notify();
                                    }
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