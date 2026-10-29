//! Connection wizard (splash + dialog). Port of the web app's
//! `connectDialog.tsx`. Also the landing zone for SQLite files: OS
//! drag & drop (`on_drop::<ExternalPaths>`) and the native file picker
//! (rfd) both end up in `open_sqlite_file`. New SQLite databases are
//! created from a native save dialog (`create_sqlite_file`).

use std::path::Path;

use gpui::prelude::*;
use gpui::{App, ClickEvent, Context, Div, ExternalPaths, IntoElement, ParentElement, SharedString, Styled, Window, div, px, rgb, svg};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::Input;
use gpui_component::{Disableable, Icon, IconName, Sizable, Size, h_flex, v_flex};

use crate::core::model::{ConnectionStatus, DbConnection, DbType};
use crate::core::registry;
use crate::ui::app::{DataBridge, Draft, Phase};
use crate::ui::theme;

impl DataBridge {
    /// Full-screen view while no connection is active: splash + wizard.
    pub fn render_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        div()
            .id("wizard-root")
            .relative()
            .size_full()
            .bg(p.background)
            .text_color(p.foreground)
            .font_family(theme::SANS_FONT)
            // splash behind the dialog
            .child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .opacity(0.4)
                    .child(
                        v_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(p.muted_fg)
                                    .child("DataBridge"),
                            )
                            .child(div().h(px(1.)).w(px(128.)).bg(p.toolbar_border))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(p.muted_fg)
                                    .child("Database Explorer"),
                            ),
                    ),
            )
            // the dialog card
            .child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        v_flex()
                            .id("wizard")
                            .w(px(600.))
                            .max_h(px(700.))
                            .bg(p.card)
                            .border_1()
                            .border_color(p.border)
                            .rounded(px(10.))
                            .shadow_md()
                            .overflow_hidden()
                            .child(self.wizard_header(cx))
                            .child(self.wizard_body(window, cx))
                            .child(self.wizard_footer(cx)),
                    ),
            )
    }

    fn wizard_header(&mut self, cx: &mut Context<Self>) -> Div {
        let p = theme::palette(cx);
        let desc = match self.wizard.phase {
            Phase::Choose => {
                "Choose a database type to connect, pick an existing connection, or drop a SQLite file onto the window."
            }
            Phase::Form => "Configure connection properties. Click Test Connection to verify.",
            Phase::Testing => "Establishing connection…",
        };
        v_flex()
            .flex_none()
            .gap_1()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(p.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_size(px(16.))
                    .child(
                        svg()
                            .path("icons/cube.svg")
                            .size_4()
                            .flex_none()
                            .text_color(p.primary),
                    )
                    .child("New Connection — DataBridge"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(p.muted_fg)
                    .child(desc),
            )
    }

    fn wizard_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        match self.wizard.phase {
            Phase::Choose => self.wizard_choose(cx),
            Phase::Form => self.wizard_form(window, cx),
            Phase::Testing => self.wizard_testing(cx),
        }
    }

    fn wizard_choose(&mut self, cx: &mut Context<Self>) -> Div {
        let p = theme::palette(cx);
        // Every known type; ones without a driver in this build are shown
        // dimmed and inert (they connect only in `--mock`).
        let types = [
            DbType::PostgreSql,
            DbType::MySql,
            DbType::MariaDb,
            DbType::Sqlite,
            DbType::MongoDb,
            DbType::Redis,
            DbType::ClickHouse,
            DbType::Cassandra,
            DbType::ScyllaDb,
        ];
        const PER_ROW: usize = 5;
        let card = |i: usize, t: DbType, cx: &mut Context<Self>| {
            let supported = registry::is_supported(t);
            div()
                .id(("type", i))
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(6.))
                .p_3()
                .border_1()
                .border_color(p.border)
                .rounded(px(8.))
                .child(
                    svg()
                        .path(t.brand_icon())
                        .size(px(26.))
                        .flex_none()
                        .text_color(rgb(t.brand_color())),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(px(14.))
                        .text_color(p.foreground)
                        .child(t.label()),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(p.muted_fg)
                        .child(if !supported {
                            "coming soon".to_string()
                        } else if t.default_port() > 0 {
                            format!("port {}", t.default_port())
                        } else {
                            "embedded file".to_string()
                        }),
                )
                .when(!supported, |this| this.opacity(0.4).cursor_not_allowed())
                .when(supported, |this| {
                    this.cursor_pointer()
                        .hover(move |s| s.border_color(p.primary.opacity(0.7)).bg(p.accent))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.pick_type(t, window, cx);
                        }))
                })
        };
        let rows: Vec<Div> = types
            .chunks(PER_ROW)
            .enumerate()
            .map(|(r, chunk)| {
                let mut row = h_flex().gap_2();
                for (j, t) in chunk.iter().enumerate() {
                    row = row.child(card(r * PER_ROW + j, *t, cx));
                }
                // Pad the last row so its cards keep the same width.
                for _ in chunk.len()..PER_ROW {
                    row = row.child(div().flex_1().min_w_0());
                }
                row
            })
            .collect();
        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .px_4()
            .py_3()
            .gap_5()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.muted_fg)
                            .child("Database type"),
                    )
                    .children(rows)
                    // Standing hint that drag & drop works, too.
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                Icon::new(IconName::File)
                                    .with_size(Size::Size(px(13.)))
                                    .text_color(p.muted_fg),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(p.muted_fg)
                                    .child("…or drag & drop a SQLite file onto this window, or"),
                            )
                            .child(
                                div()
                                    .id("create-sqlite")
                                    .text_size(px(12.))
                                    .text_color(p.primary)
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .child("create a new SQLite database")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.create_sqlite_file(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.muted_fg)
                            .child("Existing connections"),
                    )
                    .child(
                        v_flex()
                            .id("existing-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .gap(px(6.))
                            .when(self.connections.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(p.muted_fg)
                                        .child(
                                            "No saved connections yet — create one above.",
                                        ),
                                )
                            })
                            .children(
                                self.connections
                                    .clone()
                                    .iter()
                                    .enumerate()
                                    .map(|(i, c)| {
                                        let id = c.id.clone();
                                        let id_forget = c.id.clone();
                                        let status_color = match c.status {
                                            ConnectionStatus::Connected => p.conn_ok,
                                            ConnectionStatus::Error => p.conn_err,
                                            ConnectionStatus::Disconnected => p.muted_fg,
                                            ConnectionStatus::Connecting => p.primary,
                                        };
                                        h_flex()
                                            .id(("existing", i))
                                            .w_full()
                                            .items_center()
                                            .gap_3()
                                            .px_3()
                                            .py_2()
                                            .rounded(px(8.))
                                            .cursor_pointer()
                                            .hover(move |s| s.bg(p.accent))
                                            .child(
                                                div()
                                                    .size_2()
                                                    .flex_none()
                                                    .rounded_full()
                                                    .bg(status_color),
                                            )
                                            .child(
                                                svg()
                                                    .path(c.db_type.brand_icon())
                                                    .size_4()
                                                    .flex_none()
                                                    .text_color(rgb(c.db_type.brand_color())),
                                            )
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .overflow_x_hidden()
                                                    .gap(px(2.))
                                                    .child(
                                                        div()
                                                            .w_full()
                                                            .text_size(px(14.))
                                                            .text_color(p.foreground)
                                                            .child(c.name.clone()),
                                                    )
                                                    .child(
                                                        div()
                                                            .w_full()
                                                            .text_size(px(12.))
                                                            .text_color(p.muted_fg)
                                                            .child(format!(
                                                                "{} · {}{} · {}",
                                                                c.db_type.label(),
                                                                c.host,
                                                                if c.port > 0 {
                                                                    format!(":{}", c.port)
                                                                } else {
                                                                    String::new()
                                                                },
                                                                c.database
                                                            )),
                                                    ),
                                            )
                                            // Forget a saved connection (list +
                                            // store + keyring); the row itself
                                            // still opens the form.
                                            .child(
                                                div()
                                                    .id(("existing-forget", i))
                                                    .size(px(24.))
                                                    .flex()
                                                    .flex_none()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(px(4.))
                                                    .cursor_pointer()
                                                    .text_color(p.muted_fg)
                                                    .hover(
                                                        move |s| s
                                                            .bg(p.tree_hover)
                                                            .text_color(p.destructive),
                                                    )
                                                    .child(
                                                        Icon::new(IconName::Delete)
                                                            .with_size(Size::Size(px(13.))),
                                                    )
                                                    .on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            cx.stop_propagation();
                                                            this.forget_connection(&id_forget, cx);
                                                        },
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .child(
                                                        Icon::new(IconName::ArrowRight)
                                                            .with_size(Size::Size(px(15.)))
                                                            .text_color(p.muted_fg),
                                                    ),
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.start_existing(&id, window, cx);
                                            }))
                                    }),
                            ),
                    ),
            )
    }

    fn wizard_form(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let p = theme::palette(cx);
        let d = &self.wizard.draft;
        let is_embedded = d.db_type == DbType::Sqlite;
        let url_mode = d.url_mode;
        let error = self.wizard.error.clone();
        let chosen = self
            .wizard
            .chosen_existing
            .as_ref()
            .map(|id| {
                self.connections
                    .iter()
                    .find(|c| &c.id == id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default()
            });

        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .px_4()
            .py_3()
            .gap_3()
            .when_some(chosen, |this, name| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(p.primary.opacity(0.3))
                        .bg(p.primary.opacity(0.1))
                        .text_size(px(12.))
                        .text_color(p.primary.opacity(0.9))
                        .child(
                            svg()
                                .path("icons/cube.svg")
                                .size_3()
                                .flex_none()
                                .text_color(p.primary),
                        )
                        .child(format!("editing connection: {name}")),
                )
            })
            .child(field("Name", &d.name))
            // Fields ↔ URL mode switch
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(mode_btn(
                        "mode-fields",
                        "Fields",
                        !url_mode,
                        &p,
                        cx.listener(|this, _, _, cx| {
                            this.wizard.draft.url_mode = false;
                            cx.notify();
                        }),
                    ))
                    .child(mode_btn(
                        "mode-url",
                        "URL",
                        url_mode,
                        &p,
                        cx.listener(|this, _, _, cx| {
                            this.wizard.draft.url_mode = true;
                            cx.notify();
                        }),
                    )),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .items_center()
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .bg(p.accent)
                    .text_size(px(12.))
                    .text_color(p.primary)
                    .child(
                        svg()
                            .path(d.db_type.brand_icon())
                            .size_3()
                            .flex_none()
                            .text_color(rgb(d.db_type.brand_color())),
                    )
                    .child(d.db_type.label()),
            )
            .when(url_mode, |this| {
                this.child(field("Connection URL", &d.url))
            })
            .when(!url_mode, |this| {
                this.when(is_embedded, |this| {
                        this.child(file_field(&d.database, cx))
                    })
                    .when(!is_embedded, |this| {
                        this.child(
                            h_flex()
                                .gap(px(10.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(field("Host", &d.host)),
                                )
                                .child(
                                    div().flex_1().min_w_0().child(field("Port", &d.port)),
                                ),
                        )
                        .child(field("Database", &d.database))
                        .child(
                            h_flex().gap(px(10.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(field("User", &d.user)),
                                )
                                .child(
                                    div().flex_1().min_w_0().child(password_field(&d.password)),
                                ),
                        )
                    })
            })
            .when_some(error, |this, e| {
                let success = e.to_lowercase().contains("success");
                let (border, bg, fg) = if success {
                    (p.conn_ok, p.conn_ok, p.conn_ok)
                } else {
                    (p.conn_err, p.conn_err, p.conn_err)
                };
                this.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(border.opacity(0.4))
                        .bg(bg.opacity(0.1))
                        .text_color(fg)
                        .text_size(px(13.))
                        .child(
                            if success {
                                Icon::new(IconName::CircleCheck)
                            } else {
                                Icon::new(IconName::TriangleAlert)
                            }
                            .with_size(Size::Size(px(15.))),
                        )
                        .child(e),
                )
            })
    }

    fn wizard_testing(&mut self, cx: &mut Context<Self>) -> Div {
        let p = theme::palette(cx);
        let d = &self.wizard.draft;
        let host = if d.db_type == DbType::Sqlite {
            "local file".to_string()
        } else {
            let val = d.host.read(cx).value().to_string();
            if val.is_empty() {
                "local file".to_string()
            } else {
                val
            }
        };
        let port = d.port.read(cx).value().to_string();
        let port_suffix = if port.is_empty() || port == "0" {
            String::new()
        } else {
            format!(":{port}")
        };
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .py_10()
            .child(
                Icon::new(IconName::LoaderCircle)
                    .with_size(Size::Size(px(30.)))
                    .text_color(p.primary),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(p.muted_fg)
                    .child(format!("Connecting to {host}{port_suffix} …")),
            )
    }

    fn wizard_footer(&mut self, cx: &mut Context<Self>) -> Div {
        let p = theme::palette(cx);
        let testing = self.wizard.testing;
        let show = self.wizard.phase == Phase::Form;
        h_flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(p.border)
            .when(show, |this| {
                this.child(
                    Button::new("back")
                        .ghost()
                        .label("Back")
                        .disabled(testing)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.wizard.phase = Phase::Choose;
                            this.wizard.chosen_existing = None;
                            this.wizard.error = None;
                            cx.notify();
                        })),
                )
                .child(div().flex_1())
                .child(
                    Button::new("test")
                        .outline()
                        .label(if testing { "Testing…" } else { "Test Connection" })
                        .disabled(testing)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.run_test(false, window, cx);
                        })),
                )
                .child(
                    Button::new("connect")
                        .primary()
                        .label("Connect")
                        .disabled(testing)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.run_test(true, window, cx);
                        })),
                )
            })
    }

    // ---------------- wizard actions ----------------

    pub fn pick_type(&mut self, t: DbType, window: &mut Window, cx: &mut Context<Self>) {
        self.wizard.draft = Draft::new(t, window, cx);
        self.wizard.error = None;
        self.wizard.phase = Phase::Form;
        cx.notify();
    }

    pub fn start_existing(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.connections.iter().find(|c| c.id == id) else {
            return;
        };
        let mut draft = Draft::new(conn.db_type, window, cx);
        draft.id = conn.id.clone();
        draft.set(
            &conn.name,
            &conn.host,
            &conn.port.to_string(),
            &conn.database,
            &conn.user,
            window,
            cx,
        );
        // Prefill the stored password (from the OS keyring) so a saved
        // connection reconnects with a single Connect; `Draft::set` clears
        // it for fresh drafts.
        if !conn.password.is_empty() {
            draft
                .password
                .update(cx, |s, cx| s.set_value(conn.password.clone(), window, cx));
        }
        // A URL-created connection keeps its URL when re-edited.
        if let Some(url) = &conn.url {
            draft.url_mode = true;
            draft
                .url
                .update(cx, |s, cx| s.set_value(url.clone(), window, cx));
        }
        self.wizard.draft = draft;
        self.wizard.chosen_existing = Some(id.to_string());
        self.wizard.error = None;
        self.wizard.phase = Phase::Form;
        cx.notify();
    }

    /// Validate the draft, open a real session and ping it. On success
    /// either report back or enter the explorer (`final_connect`).
    pub fn run_test(&mut self, final_connect: bool, _window: &mut Window, cx: &mut Context<Self>) {
        // Cheap field validation first — fails fast, before any I/O.
        let conn = match self.validate_draft(cx) {
            Ok(c) => c,
            Err(e) => {
                self.wizard.error = Some(e);
                cx.notify();
                return;
            }
        };
        self.wizard.testing = true;
        self.wizard.error = None;
        if final_connect {
            self.wizard.phase = Phase::Testing;
        }
        cx.notify();

        // Real probe: the mock sleeps its latency, the sqlx drivers
        // actually connect and run `SELECT 1`.
        let session = match registry::open(&conn) {
            Ok(s) => s,
            Err(e) => {
                self.wizard.testing = false;
                self.wizard.error = Some(e.message().to_string());
                cx.notify();
                return;
            }
        };
        let rx = session.ping();
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |db, cx| {
                db.wizard.testing = false;
                match result {
                    Ok(Ok(_)) => {
                        if final_connect {
                            db.on_connected(conn, cx);
                        } else {
                            db.wizard.error = Some("Test connection: SUCCESS".into());
                        }
                    }
                    Ok(Err(e)) => {
                        db.wizard.error = Some(e.message().to_string());
                        db.wizard.phase = Phase::Form;
                    }
                    Err(_) => {
                        db.wizard.error = Some("Test connection was cancelled".into());
                        db.wizard.phase = Phase::Form;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------------- SQLite file opening (drop + file picker) ----------------

    /// Entry point for files dragged in from the OS file manager. Only
    /// SQLite databases are recognized; they connect immediately.
    pub fn open_dropped_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.wizard.testing {
            return; // a connect is already in flight
        }
        let Some(path) = paths.paths().iter().find(|p| is_sqlite_path(p)) else {
            // Unknown payload: hint in the wizard, stay quiet in the
            // explorer (dropping a photo there shouldn't disturb anything).
            if self.active_conn.is_none() {
                self.wizard.error = Some(
                    "Only SQLite database files can be dropped here (.db, .sqlite, .sqlite3, .db3)."
                        .into(),
                );
                cx.notify();
            }
            return;
        };
        self.open_sqlite_file(path, window, cx);
    }

    /// Create a SQLite draft for `path`, then connect right away — the
    /// form (with the path filled in) shows the error if the file turns
    /// out not to be a database.
    pub fn open_sqlite_file(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        // Dropping a file while exploring replaces the current connection.
        if self.active_conn.is_some() {
            self.terminate(cx);
        }
        let path_str = path.to_string_lossy().to_string();
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "sqlite".into());
        let draft = Draft::new(DbType::Sqlite, window, cx);
        draft
            .database
            .update(cx, |s, cx| s.set_value(path_str, window, cx));
        draft.name.update(cx, |s, cx| s.set_value(name, window, cx));
        self.wizard.draft = draft;
        self.wizard.chosen_existing = None;
        self.wizard.error = None;
        self.wizard.phase = Phase::Form;
        cx.notify();
        self.run_test(true, window, cx);
    }

    /// The SQLite form's Browse… button: the native OS file picker. Async
    /// (rfd runs the dialog off the UI thread); the picked path lands in
    /// the draft's file-path field, the name is suggested when still empty.
    pub fn browse_sqlite_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let file = rfd::AsyncFileDialog::new()
                .add_filter("SQLite database", &["db", "sqlite", "sqlite3", "db3"])
                .set_title("Open SQLite database")
                .pick_file()
                .await;
            let Some(file) = file else {
                return; // cancelled
            };
            let path = file.path().to_string_lossy().to_string();
            let stem = file
                .path()
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    this.wizard
                        .draft
                        .database
                        .update(cx, |s, cx| s.set_value(path, window, cx));
                    // Suggest a name for fresh drafts; never overwrite a
                    // typed one.
                    if this.wizard.draft.name.read(cx).value().is_empty() {
                        this.wizard
                            .draft
                            .name
                            .update(cx, |s, cx| s.set_value(stem, window, cx));
                    }
                    cx.notify();
                })
            })
            .ok();
        })
        .detach();
    }

    /// "Create a new SQLite database": native save dialog, then an empty
    /// file at the picked path — a zero-byte file is a valid, empty SQLite
    /// database (the header is written on the first write) — and the usual
    /// `open_sqlite_file` connect. Existing files are never truncated.
    pub fn create_sqlite_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.wizard.testing {
            return; // a connect is already in flight
        }
        cx.spawn_in(window, async move |this, cx| {
            let file = rfd::AsyncFileDialog::new()
                .add_filter("SQLite database", &["db", "sqlite", "sqlite3", "db3"])
                .set_title("Create SQLite database")
                .set_file_name("database.db")
                .save_file()
                .await;
            let Some(file) = file else {
                return; // cancelled
            };
            let mut path = file.path().to_path_buf();
            if path.extension().is_none() {
                path.set_extension("db");
            }
            let created = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path);
            cx.update(|window, cx| {
                this.update(cx, |this, cx| match created {
                    Ok(_) => this.open_sqlite_file(&path, window, cx),
                    Err(e) => {
                        // Errors render in the form only: land on a SQLite one.
                        if this.wizard.phase != Phase::Form
                            || this.wizard.draft.db_type != DbType::Sqlite
                        {
                            this.wizard.draft = Draft::new(DbType::Sqlite, window, cx);
                            this.wizard.chosen_existing = None;
                            this.wizard.phase = Phase::Form;
                        }
                        this.wizard.error = Some(if e.kind() == std::io::ErrorKind::AlreadyExists {
                            format!(
                                "{} already exists — open it with Browse… instead.",
                                path.display()
                            )
                        } else {
                            format!("Could not create {}: {e}", path.display())
                        });
                        cx.notify();
                    }
                })
            })
            .ok();
        })
        .detach();
    }

    fn validate_draft(&self, cx: &App) -> Result<DbConnection, String> {
        let d = &self.wizard.draft;
        let val = |e: &gpui::Entity<gpui_component::input::InputState>| {
            e.read(cx).value().to_string()
        };
        let name = val(&d.name);
        let name = if name.is_empty() { "new_connection".into() } else { name };
        if d.url_mode {
            let url = val(&d.url);
            if url.trim().is_empty() {
                return Err("Connection URL is required".into());
            }
            // Display fields are best-effort; the driver re-parses the URL
            // natively (keeping sslmode & friends).
            let (host, port, database, user) = match crate::core::model::parse_db_url(url.trim())
            {
                Some(x) => (
                    x.host,
                    if x.port > 0 {
                        x.port
                    } else {
                        d.db_type.default_port()
                    },
                    x.database,
                    x.user,
                ),
                None => (
                    String::new(),
                    d.db_type.default_port(),
                    String::new(),
                    String::new(),
                ),
            };
            return Ok(DbConnection {
                id: d.id.clone(),
                name,
                db_type: d.db_type,
                host,
                port,
                database,
                user,
                password: String::new(),
                url: Some(url.trim().to_string()),
                status: ConnectionStatus::Connected,
                latency_ms: 24,
            });
        }
        let host = val(&d.host);
        let port = val(&d.port).parse::<u16>().unwrap_or_else(|_| d.db_type.default_port());
        let database = val(&d.database);
        let user = val(&d.user);
        let password = val(&d.password);
        if host.is_empty() && d.db_type != DbType::Sqlite {
            return Err("Host is required".into());
        }
        if database.is_empty() {
            return Err("Database name/path is required".into());
        }
        Ok(DbConnection {
            id: d.id.clone(),
            name,
            db_type: d.db_type,
            host,
            port,
            database,
            user,
            password,
            url: None,
            status: ConnectionStatus::Connected,
            latency_ms: 24,
        })
    }
}

fn field(
    label: &str,
    state: &gpui::Entity<gpui_component::input::InputState>,
) -> Div {
    let label = label.to_string();
    v_flex()
        .gap_1()
        .child(
            div()
                .text_size(px(12.))
                .child(label),
        )
        .child(Input::new(state).large().h(px(38.)))
}

/// Like [`field`], but masked with an eye toggle — the value can be
/// prefilled from the OS keyring and must not render in clear text.
fn password_field(state: &gpui::Entity<gpui_component::input::InputState>) -> Div {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_size(px(12.))
                .child("Password"),
        )
        .child(Input::new(state).large().h(px(38.)).mask_toggle())
}

/// The SQLite "File path" field: input plus a Browse… button opening the
/// native OS file picker.
fn file_field(
    state: &gpui::Entity<gpui_component::input::InputState>,
    cx: &mut Context<DataBridge>,
) -> Div {
    let p = theme::palette(cx);
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .items_center()
                .child(
                    div()
                        .text_size(px(12.))
                        .child("File path"),
                )
                .child(div().flex_1())
                .child(
                    Button::new("new-sqlite")
                        .ghost()
                        .small()
                        .label("New…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.create_sqlite_file(window, cx);
                        })),
                )
                .child(
                    Button::new("browse-sqlite")
                        .ghost()
                        .small()
                        .label("Browse…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.browse_sqlite_file(window, cx);
                        })),
                ),
        )
        .child(Input::new(state).large().h(px(38.)))
        // Standing hint that dropping the file works, too.
        .child(
            div()
                .text_size(px(11.5))
                .text_color(p.muted_fg)
                .child("…or drop the file anywhere on the window."),
        )
}

/// The drop zone's gate: recognize SQLite database files by extension.
fn is_sqlite_path(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "db" | "sqlite" | "sqlite3" | "db3"
                )
            })
}

/// One half of the wizard's Fields / URL mode switch.
fn mode_btn(
    id: &'static str,
    label: &'static str,
    active: bool,
    p: &crate::ui::theme::Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let hover_bg = p.accent;
    div()
        .id(id)
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .px(px(12.))
        .py(px(8.))
        .rounded(px(6.))
        .border_1()
        .border_color(if active {
            p.primary.opacity(0.6)
        } else {
            p.border
        })
        .when(active, |this| this.bg(p.primary.opacity(0.12)))
        .text_size(px(12.))
        .text_color(if active { p.primary } else { p.muted_fg })
        .cursor_pointer()
        .when(!active, move |this| {
            this.hover(move |s| s.bg(hover_bg))
        })
        .child(label)
        .on_click(on_click)
}

impl Draft {
    pub fn new(t: DbType, window: &mut Window, cx: &mut Context<DataBridge>) -> Self {
        let mk = |v: &str, window: &mut Window, cx: &mut Context<DataBridge>| {
            let v = v.to_string();
            cx.new(move |cx| {
                let mut s = gpui_component::input::InputState::new(window, cx);
                s.set_value(v, window, cx);
                s
            })
        };
        let host = if t == DbType::Sqlite { "" } else { "localhost" };
        let database = if t == DbType::Sqlite { "" } else { "my_database" };
        let user = if t == DbType::Sqlite { "" } else { "my_user" };
        let name = mk("", window, cx);
        let url = mk("", window, cx);
        let url_placeholder = match t {
            DbType::PostgreSql => "postgres://user:password@localhost:5432/database",
            DbType::MySql => "mysql://user:password@localhost:3306/database",
            DbType::Sqlite => "sqlite:///path/to/file.db",
            DbType::MongoDb => "mongodb://user:password@localhost:27017/database",
            DbType::Redis => "redis://localhost:6379",
            DbType::MariaDb => "mariadb://user:password@localhost:3306/database",
            DbType::ClickHouse => "clickhouse://user:password@localhost:9000/database",
            DbType::Cassandra => "cassandra://user:password@localhost:9042/keyspace",
            DbType::ScyllaDb => "scylla://user:password@localhost:9042/keyspace",
        };
        url.update(cx, |s, cx| s.set_placeholder(url_placeholder, window, cx));
        let host = mk(host, window, cx);
        let port = mk(&t.default_port().to_string(), window, cx);
        let database = mk(database, window, cx);
        let user = mk(user, window, cx);
        // The password is masked from the start (it can be prefilled from
        // the OS keyring when re-editing a saved connection).
        let password = cx.new(|cx| {
            let mut s = gpui_component::input::InputState::new(window, cx);
            s.set_masked(true, window, cx);
            s
        });
        password.update(cx, |s, cx| s.set_placeholder("••••••••", window, cx));
        Self {
            id: next_id(),
            db_type: t,
            url_mode: false,
            name,
            url,
            host,
            port,
            database,
            user,
            password,
        }
    }

    pub fn set(
        &mut self,
        name: &str,
        host: &str,
        port: &str,
        database: &str,
        user: &str,
        window: &mut Window,
        cx: &mut Context<DataBridge>,
    ) {
        for (e, v) in [
            (&self.name, name),
            (&self.host, host),
            (&self.port, port),
            (&self.database, database),
            (&self.user, user),
        ] {
            let v = v.to_string();
            e.update(cx, |s, cx| s.set_value(v.clone(), window, cx));
        }
        self.password
            .update(cx, |s, cx| s.set_value(String::new(), window, cx));
    }
}

/// Stable unique id for a new draft — unique across app launches, not
/// just per session: persisted connections are upserted by id and their
/// keyring entries are keyed by it, so a session counter would collide
/// with a stored connection on the next run.
fn next_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// keep SharedString import used even if future edits drop call sites
#[allow(dead_code)]
fn _unused(s: SharedString) -> SharedString {
    s
}
