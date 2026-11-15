//! Workspace tabs as dock panels (GPUI Kit `dock`): every records grid,
//! SQL editor and diagram is a [`WorkspacePanel`] in the center of one
//! [`DockArea`], so a tab can be dragged by its title and dropped beside,
//! above or below another — the workspace splits like an IDE's, and a
//! table stays visible next to the SQL editor that changes it.
//!
//! `DataBridge` keeps owning the tab list (`tabs`, the views): the dock
//! only arranges the panels. A panel reports back through deferred calls —
//! activation (`DataBridge::tab_activated`, which drives the tree
//! highlight and the AI context) and closing from the dock's own tab
//! close button (`DataBridge::tab_closed`). Deferred, because the dock
//! calls them inside its own update, often inside one `DataBridge` itself
//! started.

use gpui::prelude::*;
use gpui::{
    AnyView, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton,
    ParentElement, Render, SharedString, Styled, WeakEntity, Window, div, px,
};
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_component::dock::{BasePanel, Panel, PanelEvent};
use gpui_component::{ActiveTheme as _, IconName, Sizable as _, h_flex};

use crate::ui::app::DataBridge;

/// Persisted-layout name of every workspace panel (layouts are not saved
/// yet; the name only has to be stable).
const PANEL_NAME: &str = "databridge.workspace";

pub struct WorkspacePanel {
    /// The `OpenTab::id` this panel shows.
    pub tab_id: String,
    title: SharedString,
    /// The only tab left (`DataBridge::sync_lone_tab`): draws its own ×.
    lone: bool,
    content: AnyView,
    app: WeakEntity<DataBridge>,
    focus: FocusHandle,
}

impl WorkspacePanel {
    pub fn new(
        tab_id: String,
        title: impl Into<SharedString>,
        content: AnyView,
        app: WeakEntity<DataBridge>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            tab_id,
            title: title.into(),
            lone: false,
            content,
            app,
            focus: cx.focus_handle(),
        }
    }

    pub fn set_lone(&mut self, lone: bool, cx: &mut Context<Self>) {
        if self.lone != lone {
            self.lone = lone;
            cx.notify();
        }
    }
}

impl EventEmitter<PanelEvent> for WorkspacePanel {}

impl Focusable for WorkspacePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for WorkspacePanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .min_w_0()
            .child(self.content.clone())
    }
}

impl BasePanel for WorkspacePanel {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if !active {
            return;
        }
        let (app, id) = (self.app.clone(), self.tab_id.clone());
        cx.defer(move |cx| {
            app.update(cx, |a, cx| a.tab_activated(&id, cx)).ok();
        });
    }

    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let (app, id) = (self.app.clone(), self.tab_id.clone());
        cx.defer(move |cx| {
            app.update(cx, |a, cx| a.tab_closed(&id, cx)).ok();
        });
    }
}

impl Panel for WorkspacePanel {
    /// The tab label. `None` for the lone tab, so the dock draws
    /// [`Self::title`] instead — the label with its own ×.
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        (!self.lone).then(|| self.title.clone())
    }

    /// Name + × for the very last tab: the dock refuses to close the last
    /// panel, so the skin draws no close button for it — this one goes
    /// through `DockArea::remove_panel` instead. Styled like the skin's
    /// own close button, so one tab looks like one of many.
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.clone();
        let id = self.tab_id.clone();
        h_flex()
            .items_center()
            .child(self.title.clone())
            .when(self.lone, |this| {
                this.child(
                    div()
                        .flex_none()
                        // 8 px + the button's 4 px inset = the skin's 12 px.
                        .ml_2()
                        .mr(-px(4.))
                        // Not a tab drag / select.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Button::new("close-lone-tab")
                                .icon(IconName::Close)
                                .xsmall()
                                .custom(
                                    ButtonCustomVariant::new(cx)
                                        .foreground(cx.theme().secondary_foreground)
                                        .hover(*cx.theme().tokens.secondary_hover)
                                        .active(*cx.theme().tokens.secondary_active),
                                )
                                .tab_stop(false)
                                .tooltip(tr!("app.close_tab"))
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    app.update(cx, |a, cx| a.close_tab_id(&id, window, cx)).ok();
                                }),
                        ),
                )
            })
    }

    /// The views bring their own toolbars and padding.
    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}
