//! Narrow vertical toolbar on the far left. Port of the web app's `sidebar.tsx`.

use gpui::prelude::*;
use gpui::{App, ClickEvent, Context, Div, Hsla, IntoElement, ParentElement, Stateful, Styled, Window, div, px, rgb, svg};
use gpui_component::tooltip::Tooltip;
use gpui_component::{v_flex, Icon, IconName, Sizable, Size};

use crate::core::model::ConnectionStatus;
use crate::ui::app::DataBridge;
use crate::ui::theme;

fn status_color(s: ConnectionStatus, p: &theme::Palette) -> Hsla {
    match s {
        ConnectionStatus::Connected => p.conn_ok,
        ConnectionStatus::Error => p.conn_err,
        ConnectionStatus::Disconnected => p.muted_fg,
        ConnectionStatus::Connecting => p.primary,
    }
}

fn tool_btn(
    id: &'static str,
    label: &'static str,
    body: impl IntoElement,
    hover_bg: Hsla,
    active: Option<Hsla>,
    danger_fg: Option<Hsla>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(36.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .when_some(active, |this, bg| this.bg(bg))
        .hover(move |s| {
            let s = s.bg(hover_bg);
            if let Some(fg) = danger_fg {
                s.text_color(fg)
            } else {
                s
            }
        })
        .tooltip(move |_, cx| cx.new(|_| Tooltip::new(label)).into())
        .on_click(on_click)
        .child(body)
}

impl DataBridge {
    pub fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let hover = p.tree_hover;
        let conn = self
            .active_conn
            .clone()
            .expect("render_sidebar requires an active connection");

        v_flex()
            .id("sidebar")
            .w(px(48.))
            .flex_none()
            .h_full()
            .items_center()
            .gap_1()
            .py_2()
            .bg(p.toolbar)
            .border_r_1()
            .border_color(p.toolbar_border)
            .rounded_l(px(4.))
            // connection status dot
            .child(div().mt(px(2.)).mb_1().size_2().rounded_full().bg(status_color(conn.status, &p)))
            // active connection (brand glyph)
            .child(
                div()
                    .id("active-conn-brand")
                    .size(px(32.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .tooltip({
                        let name = conn.name.clone();
                        move |_, cx| cx.new(|_| Tooltip::new(format!("Active: {}", name))).into()
                    })
                    .child(
                        svg()
                            .path(conn.db_type.brand_icon())
                            .size_4()
                            .flex_none()
                            .text_color(rgb(conn.db_type.brand_color())),
                    ),
            )
            .child(
                div()
                    .my_1()
                    .h(px(1.))
                    .w(px(28.))
                    .flex_none()
                    .bg(p.toolbar_border),
            )
            .child(tool_btn(
                "new-conn",
                "New connection",
                Icon::new(IconName::Plus).with_size(Size::Size(px(16.))),
                hover,
                None,
                None,
                cx.listener(|this, _, _, cx| this.terminate(cx)),
            ))
            .child(tool_btn(
                "refresh",
                "Refresh",
                Icon::new(IconName::Redo2).with_size(Size::Size(px(16.))),
                hover,
                None,
                None,
                cx.listener(|this, _, _, cx| this.refresh(cx)),
            ))
            .child(div().flex_1())
            .child(tool_btn(
                "grid-view",
                "Grid view",
                Icon::new(IconName::LayoutDashboard).with_size(Size::Size(px(15.))),
                hover,
                if self.grid_view { Some(p.accent) } else { None },
                None,
                cx.listener(|this, _, _, cx| {
                    this.grid_view = true;
                    cx.notify();
                }),
            ))
            .child(tool_btn(
                "list-view",
                "List view",
                Icon::new(IconName::Menu).with_size(Size::Size(px(15.))),
                hover,
                if !self.grid_view { Some(p.accent) } else { None },
                None,
                cx.listener(|this, _, _, cx| {
                    this.grid_view = false;
                    cx.notify();
                }),
            ))
            .child(
                div()
                    .my_1()
                    .h(px(1.))
                    .w(px(28.))
                    .flex_none()
                    .bg(p.toolbar_border),
            )
            .child(tool_btn(
                "settings",
                "Settings",
                Icon::new(IconName::Settings).with_size(Size::Size(px(16.))),
                hover,
                None,
                None,
                cx.listener(|this, _, _, cx| {
                    this.show_settings = true;
                    cx.notify();
                }),
            ))
            .child(tool_btn(
                "disconnect",
                "Disconnect",
                Icon::new(IconName::Close).with_size(Size::Size(px(16.))),
                hover,
                None,
                Some(p.destructive),
                cx.listener(|this, _, _, cx| this.terminate(cx)),
            ))
    }
}
