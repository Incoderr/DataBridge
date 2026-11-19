//! Update UI (`core::update`): the startup check, the floating
//! "update available" card (bottom-right, over the wizard and the
//! explorer alike) and the Updates section of the Settings dialog.
//!
//! Flow: Available → Download (the installer, verified) → Ready → an
//! installed copy offers "Restart and update" (silent setup, the app comes
//! back on the new version); a portable copy offers "Run installer" (the
//! setup wizard: updating a portable copy means installing DataBridge).
//!
//! Every check / install bumps `DataBridge::update_id`; results of a
//! superseded request are dropped (the stale-guard pattern).

use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{div, px, AnyElement, ClickEvent, Context, FontWeight, IntoElement, ParentElement, Styled};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{h_flex, v_flex, Disableable, Icon, IconName};

use crate::core::update::{self, InstallEvent, InstallKind, Release};
use crate::ui::app::DataBridge;
use crate::ui::theme;

/// Where the updater stands.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading { release: Release, done: u64, total: Option<u64> },
    /// The verified installer is on disk; running it updates the app.
    Ready { release: Release, setup: PathBuf },
    /// `release` is set when the failure happened while installing it
    /// (the card offers a retry); a failed check has none.
    Failed { release: Option<Release>, error: String },
}

fn mb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024. * 1024.))
}

impl DataBridge {
    /// Startup check (release builds, unless switched off in Settings).
    pub fn startup_update_check(&mut self, cx: &mut Context<Self>) {
        if !cfg!(debug_assertions) && crate::ui::settings::check_updates(cx) {
            self.check_for_updates(false, cx);
        }
    }

    /// Ask GitHub for a newer release. A `manual` check (Settings) reports
    /// "up to date" and errors; the startup one stays silent on failure
    /// (offline is normal).
    pub fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if matches!(self.update, UpdateState::Downloading { .. } | UpdateState::Ready { .. }) {
            return;
        }
        self.update_id += 1;
        let id = self.update_id;
        self.update = UpdateState::Checking;
        self.update_hidden = false;
        cx.notify();
        let rx = update::check();
        cx.spawn(async move |this, cx| {
            let res = rx.await;
            this.update(cx, |this, cx| {
                if this.update_id != id {
                    return;
                }
                this.update = match res {
                    Ok(Ok(Some(r))) => UpdateState::Available(r),
                    Ok(Ok(None)) if manual => UpdateState::UpToDate,
                    Ok(Err(e)) if manual => {
                        UpdateState::Failed { release: None, error: e.message().to_string() }
                    }
                    _ => UpdateState::Idle,
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn pending_release(&self) -> Option<Release> {
        match &self.update {
            UpdateState::Available(r) | UpdateState::Failed { release: Some(r), .. } => Some(r.clone()),
            _ => None,
        }
    }

    /// Download + verify the installer; ends in `Ready` or `Failed`.
    pub fn install_update(&mut self, cx: &mut Context<Self>) {
        let Some(release) = self.pending_release() else { return };
        if !release.installable() {
            cx.open_url(&release.page_url);
            return;
        }
        self.update_id += 1;
        let id = self.update_id;
        self.update = UpdateState::Downloading { release: release.clone(), done: 0, total: None };
        cx.notify();
        let mut rx = update::download(release.clone());
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let alive = this
                    .update(cx, |this, cx| {
                        if this.update_id != id {
                            return false;
                        }
                        this.update = match ev {
                            InstallEvent::Progress(done, total) => {
                                UpdateState::Downloading { release: release.clone(), done, total }
                            }
                            InstallEvent::Done(setup) => {
                                UpdateState::Ready { release: release.clone(), setup }
                            }
                            InstallEvent::Error(error) => {
                                UpdateState::Failed { release: Some(release.clone()), error }
                            }
                        };
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Start the installer and quit: it replaces this exe (waiting for the
    /// instance mutex to go) and, for an installed copy, starts the new one.
    pub fn restart_after_update(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Ready { release, setup } = &self.update else { return };
        match update::run_installer(setup) {
            Ok(()) => cx.quit(),
            Err(e) => {
                self.update = UpdateState::Failed {
                    release: Some(release.clone()),
                    error: e.message().to_string(),
                };
                cx.notify();
            }
        }
    }

    /// The floating bottom-right card; `None` when there is nothing to say.
    pub fn render_update_notice(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.update_hidden {
            return None;
        }
        let p = theme::palette(cx);
        let portable = update::install_kind() == InstallKind::Portable;
        let (title, body, release) = match &self.update {
            UpdateState::Available(r) => (
                tr!("update.available"),
                if portable {
                    tr!("update.portable_body", new = r.version, current = update::current_version())
                } else {
                    tr!("update.available_body", new = r.version, current = update::current_version())
                },
                r,
            ),
            UpdateState::Downloading { release, done, total } => (
                tr!("update.downloading"),
                match total {
                    Some(t) => tr!("update.progress", done = mb(*done), total = mb(*t)),
                    None => tr!("update.progress_unknown", done = mb(*done)),
                },
                release,
            ),
            UpdateState::Ready { release: r, .. } => (
                tr!("update.ready"),
                if portable {
                    tr!("update.ready_portable_body", new = r.version)
                } else {
                    tr!("update.ready_body", new = r.version)
                },
                r,
            ),
            UpdateState::Failed { release: Some(r), error } => (tr!("update.failed"), error.clone().into(), r),
            _ => return None,
        };
        let release = release.clone();
        let downloading = matches!(self.update, UpdateState::Downloading { .. });
        let fraction = match &self.update {
            UpdateState::Downloading { done, total: Some(t), .. } if *t > 0 => {
                (*done as f32 / *t as f32).clamp(0., 1.)
            }
            _ => 0.,
        };

        let mut actions = h_flex().gap_2().justify_end();
        match &self.update {
            UpdateState::Available(_) | UpdateState::Failed { .. } => {
                let page = release.page_url.clone();
                actions = actions.child(
                    Button::new("update-notes")
                        .ghost()
                        .label(tr!("update.release_page"))
                        .on_click(move |_: &ClickEvent, _, cx| cx.open_url(&page)),
                );
                if release.installable() {
                    let retry = matches!(self.update, UpdateState::Failed { .. });
                    actions = actions.child(
                        Button::new("update-install")
                            .primary()
                            .label(if retry { tr!("update.retry") } else { tr!("update.download") })
                            .on_click(cx.listener(|this, _, _, cx| this.install_update(cx))),
                    );
                }
            }
            UpdateState::Ready { .. } => {
                actions = actions
                    .child(
                        Button::new("update-later")
                            .ghost()
                            .label(tr!("update.later"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.update_hidden = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("update-restart")
                            .primary()
                            .label(if portable { tr!("update.run_installer") } else { tr!("update.restart") })
                            .on_click(cx.listener(|this, _, _, cx| this.restart_after_update(cx))),
                    );
            }
            _ => {}
        }

        let card = v_flex()
            .id("update-notice")
            // Explicit insets (see the shell comment in app.rs).
            .absolute()
            .bottom_4()
            .right_4()
            .w(theme::scaled(320.))
            .occlude()
            .gap_2()
            .p_3()
            .bg(p.card)
            .border_1()
            .border_color(p.border)
            .rounded(px(8.))
            .shadow_lg()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(IconName::ArrowDown)
                            .size_4()
                            .text_color(p.primary),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::scaled(theme::TEXT_MD))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.foreground)
                            .child(title),
                    )
                    .when(!downloading, |this| {
                        this.child(
                            Button::new("update-close")
                                .ghost()
                                .icon(Icon::new(IconName::Close))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.update_hidden = true;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(theme::scaled(theme::TEXT_SM))
                    .text_color(p.muted_fg)
                    .child(body),
            )
            .when(downloading, |this| {
                this.child(
                    div().h(px(4.)).w_full().rounded_full().bg(p.border).child(
                        div().h_full().rounded_full().bg(p.primary).w(gpui::relative(fraction)),
                    ),
                )
            })
            .child(actions);
        Some(card.into_any_element())
    }

    /// "Updates" block of the Settings dialog.
    pub fn render_update_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let auto = crate::ui::settings::check_updates(cx);
        let checking = matches!(self.update, UpdateState::Checking);
        let status: Option<gpui::SharedString> = match &self.update {
            UpdateState::Checking => Some(tr!("update.checking")),
            UpdateState::UpToDate => Some(tr!("update.up_to_date")),
            UpdateState::Available(r) => Some(tr!("update.available_short", new = r.version)),
            UpdateState::Downloading { .. } => Some(tr!("update.downloading")),
            UpdateState::Ready { release: r, .. } => Some(tr!("update.ready_body", new = r.version)),
            UpdateState::Failed { error, .. } => Some(error.clone().into()),
            UpdateState::Idle => None,
        };
        let failed = matches!(self.update, UpdateState::Failed { .. });

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::scaled(theme::TEXT_MD))
                            .text_color(p.foreground)
                            .child(tr!("update.version", current = update::current_version())),
                    )
                    .child(
                        Button::new("update-check-now")
                            .outline()
                            .label(tr!("update.check_now"))
                            .disabled(checking)
                            .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(true, cx))),
                    ),
            )
            .when_some(status, |this, s| {
                this.child(
                    div()
                        .text_size(theme::scaled(theme::TEXT_SM))
                        .text_color(if failed { p.destructive } else { p.muted_fg })
                        .child(s),
                )
            })
            .child(
                h_flex()
                    .id("update-auto")
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py(px(7.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if auto { p.primary } else { p.border })
                    .bg(if auto { p.accent } else { gpui::transparent_black() })
                    .hover(move |st| st.bg(p.accent))
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::scaled(theme::TEXT_MD))
                            .text_color(p.foreground)
                            .child(tr!("update.auto_check")),
                    )
                    .when(auto, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size_4()
                                .text_color(p.primary),
                        )
                    })
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        crate::ui::settings::set_check_updates(!auto, cx);
                        cx.notify();
                    })),
            )
    }
}
