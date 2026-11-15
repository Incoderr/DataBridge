//! AI side chat (Stage 6): a right-hand panel talking to any
//! OpenAI-compatible server (`core::ai`). The active connection's schema
//! (names + types, never rows) goes into the system prompt on every send.
//!
//! Two modes (`AiMode`, saved in `ai.json`):
//! - **Manual** — code blocks get Run / Open in SQL tab / Copy; nothing
//!   executes without a click.
//! - **Auto** — after each answer the assistant's statements run in order
//!   (stop on the first error), the outcomes go back to the model as
//!   `[ok]` / `[error]` messages and it continues, up to
//!   `ai::MAX_AUTO_STEPS` rounds. `ai::needs_confirmation` statements
//!   (DROP, TRUNCATE, UPDATE/DELETE without WHERE) always wait for Run.
//!
//! Conversations persist in `ai_history.json` (history list in the
//! header). Enter sends, Ctrl+Enter inserts a newline. Streaming and
//! execution are stale-guarded by `send_id` (same pattern as
//! `RecordsDelegate::load`); Stop bumps it and drops the stream task,
//! which cancels the HTTP request.

use gpui::prelude::*;
use gpui::{
    ClipboardItem, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement, Render,
    ScrollHandle, SharedString, Styled, Subscription, Task, WeakEntity, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::{Disableable, Icon, IconName, Sizable, Size, h_flex, v_flex};

use crate::core::session::CancelToken;
use crate::core::ai::{
    self, AiConfig, AiEvent, AiMode, AiSettings, ChatMessage, Conversation, Entry, PRESETS,
    Provider, Role, segments,
};

/// Model chips shown at once; the Model field filters the rest.
const MODEL_CHIPS: usize = 30;
use crate::ui::app::DataBridge;
use crate::ui::query::RESULT_LIMIT;
use crate::ui::theme;

pub struct AiChatView {
    app: WeakEntity<DataBridge>,
    /// Providers + mode as saved in `ai.json`.
    settings: AiSettings,
    /// Request config of the active provider (derived from `settings`).
    cfg: AiConfig,
    conv_id: String,
    entries: Vec<Entry>,
    history: Vec<Conversation>,
    show_history: bool,
    input: Entity<TextareaState>,
    scroll: ScrollHandle,
    streaming: bool,
    /// Statements are being executed (manual Run or an Auto round).
    running: bool,
    send_id: u64,
    task: Option<Task<()>>,
    /// Cancels the statement being executed (Stop); dropping `task` alone
    /// would leave it running in the database.
    exec_cancel: Option<CancelToken>,
    /// Auto rounds since the user's last message.
    auto_steps: usize,
    error: Option<String>,
    notice: Option<String>,
    // settings
    show_settings: bool,
    /// The preset picker ("Add provider") is open.
    adding: bool,
    name: Entity<InputState>,
    base_url: Entity<InputState>,
    model: Entity<InputState>,
    api_key: Entity<InputState>,
    models: Vec<String>,
    models_loading: bool,
    models_gen: u64,
    _subs: Vec<Subscription>,
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn is_ddl(sql: &str) -> bool {
    crate::core::model::changes_schema(sql)
}

/// A small muted note (used inside chip rows).
fn label_owned(s: String, color: Hsla) -> gpui::Div {
    div().text_size(theme::scaled(theme::TEXT_XS)).text_color(color).child(s)
}

/// At most `n` lines of `s`, with a marker when something was cut.
fn clip_lines(s: &str, n: usize) -> String {
    let mut lines = s.lines();
    let head: Vec<&str> = lines.by_ref().take(n).collect();
    let rest = lines.count();
    let mut out = head.join("\n");
    if rest > 0 {
        out.push_str(&format!("\n… {}", tr!("ai.more_lines", n = rest)));
    }
    out
}

impl AiChatView {
    pub fn new(app: WeakEntity<DataBridge>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = ai::load_settings();
        let cfg = settings.config();
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 10)
                .placeholder(tr!("ai.placeholder"))
        });
        let field = |v: &str, ph: &str, masked: bool, window: &mut Window, cx: &mut Context<Self>| {
            let (v, ph) = (v.to_string(), ph.to_string());
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(ph).masked(masked);
                s.set_value(v, window, cx);
                s
            })
        };
        let name = field(
            settings.active().map(|p| p.name.as_str()).unwrap_or(""),
            &tr!("common.name"),
            false,
            window,
            cx,
        );
        let base_url = field(&cfg.base_url, "https://…/v1", false, window, cx);
        let model = field(&cfg.model, &tr!("ai.model_ph"), false, window, cx);
        let api_key = field(&cfg.api_key, &tr!("ai.key_ph"), true, window, cx);
        // The model chips filter by the Model field: repaint as it changes.
        let model_sub = cx.subscribe(&model, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });

        // Multi-line inputs insert the newline *before* emitting
        // PressEnter (Ctrl+Enter = `secondary` included, which is exactly
        // the "new line" gesture). Plain Enter: take that newline back out
        // and send.
        let send_sub = cx.subscribe_in(&input, window, |this, state, ev, window, cx| {
            if let InputEvent::PressEnter { secondary: false, shift: false } = ev {
                let (mut text, cursor) = {
                    let s = state.read(cx);
                    (s.value().to_string(), s.cursor())
                };
                if cursor > 0 && cursor <= text.len() && text[..cursor].ends_with('\n') {
                    text.remove(cursor - 1);
                }
                if this.busy() {
                    state.update(cx, |s, cx| s.set_value(text, window, cx));
                } else {
                    this.send_text(text, window, cx);
                }
            }
        });
        let show_settings = cfg.model.trim().is_empty();
        let adding = settings.providers.is_empty();
        Self {
            app,
            settings,
            cfg,
            conv_id: new_id(),
            entries: Vec::new(),
            history: ai::load_history(),
            show_history: false,
            input,
            scroll: ScrollHandle::new(),
            streaming: false,
            running: false,
            send_id: 0,
            exec_cancel: None,
            task: None,
            auto_steps: 0,
            error: None,
            notice: None,
            show_settings,
            adding,
            name,
            base_url,
            model,
            api_key,
            models: Vec::new(),
            models_loading: false,
            models_gen: 0,
            _subs: vec![send_sub, model_sub],
        }
    }

    fn busy(&self) -> bool {
        self.streaming || self.running
    }

    // ---------------- settings ----------------

    fn read_settings(&self, cx: &Context<Self>) -> AiConfig {
        AiConfig {
            base_url: self.base_url.read(cx).value().trim().to_string(),
            model: self.model.read(cx).value().trim().to_string(),
            mode: self.cfg.mode,
            api_key: self.api_key.read(cx).value().trim().to_string(),
        }
    }

    /// Copy the form into the active provider and persist it (file + key).
    fn commit_inputs(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        let form = self.read_settings(cx);
        let Some(p) = self.settings.active_mut() else { return };
        if !name.is_empty() {
            p.name = name;
        }
        p.base_url = form.base_url;
        p.model = form.model;
        p.api_key = form.api_key;
        let p = p.clone();
        ai::save_key(&p);
        ai::save_settings(&self.settings);
        self.cfg = self.settings.config();
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        self.commit_inputs(cx);
        self.show_settings = false;
        self.error = None;
        cx.notify();
    }

    /// Fill the form from the active provider; the model list belongs to
    /// the previous server, so it is dropped (and any fetch in flight).
    fn load_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let p = self.settings.active().cloned();
        let get = |f: fn(&Provider) -> &String| p.as_ref().map(|p| f(p).clone()).unwrap_or_default();
        let vals = [
            (&self.name, get(|p| &p.name)),
            (&self.base_url, get(|p| &p.base_url)),
            (&self.model, get(|p| &p.model)),
            (&self.api_key, get(|p| &p.api_key)),
        ];
        for (input, v) in vals {
            input.update(cx, |s, cx| s.set_value(v, window, cx));
        }
        self.models.clear();
        self.models_gen += 1;
        self.models_loading = false;
        self.error = None;
        cx.notify();
    }

    fn select_provider(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.active().is_some_and(|p| p.id == id) {
            return;
        }
        self.commit_inputs(cx);
        self.settings.active = id;
        ai::save_settings(&self.settings);
        self.cfg = self.settings.config();
        self.load_inputs(window, cx);
    }

    fn add_provider(&mut self, preset: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preset) = PRESETS.get(preset) else { return };
        self.commit_inputs(cx);
        let p = Provider::from_preset(preset, &self.settings.providers);
        self.settings.active = p.id.clone();
        self.settings.providers.push(p);
        ai::save_settings(&self.settings);
        self.cfg = self.settings.config();
        self.adding = false;
        self.load_inputs(window, cx);
        if preset.needs_key {
            self.notice = Some(tr!("ai.paste_key", name = preset.name).to_string());
        } else if !preset.base_url.is_empty() {
            self.notice = Some(tr!("ai.press_list").into());
        }
    }

    fn remove_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.settings.active().cloned() else { return };
        ai::forget_provider(&p);
        self.settings.providers.retain(|x| x.id != p.id);
        self.settings.active = self.settings.providers.first().map(|x| x.id.clone()).unwrap_or_default();
        ai::save_settings(&self.settings);
        self.cfg = self.settings.config();
        self.adding = self.settings.providers.is_empty();
        self.load_inputs(window, cx);
    }

    fn set_mode(&mut self, mode: AiMode, cx: &mut Context<Self>) {
        self.settings.mode = mode;
        self.cfg.mode = mode;
        ai::save_settings(&self.settings);
        cx.notify();
    }

    fn fetch_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.models_gen += 1;
        let id = self.models_gen;
        self.models_loading = true;
        self.error = None;
        cx.notify();
        let rx = ai::list_models(self.read_settings(cx));
        cx.spawn_in(window, async move |this, cx| {
            let res = rx.await;
            let _ = this.update(cx, |v, cx| {
                if v.models_gen != id {
                    return;
                }
                v.models_loading = false;
                match res {
                    Ok(Ok(list)) if list.is_empty() => {
                        v.error = Some(tr!("ai.no_models").into())
                    }
                    Ok(Ok(list)) => v.models = list,
                    Ok(Err(e)) => v.error = Some(e.message().to_string()),
                    Err(_) => v.error = Some(tr!("ai.cancelled").into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn pick_model(&mut self, m: String, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |s, cx| s.set_value(m, window, cx));
        cx.notify();
    }

    // ---------------- history ----------------

    fn persist(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let conv = Conversation {
            id: self.conv_id.clone(),
            title: Conversation::derive_title(&self.entries),
            updated: ai::now_ms(),
            entries: self.entries.clone(),
        };
        ai::upsert(&mut self.history, conv);
        ai::save_history(&self.history);
    }

    fn new_chat(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.persist();
        self.conv_id = new_id();
        self.entries.clear();
        self.error = None;
        self.notice = None;
        self.show_history = false;
        cx.notify();
    }

    fn open_conv(&mut self, id: String, cx: &mut Context<Self>) {
        self.stop(cx);
        self.persist();
        if let Some(c) = self.history.iter().find(|c| c.id == id) {
            self.conv_id = c.id.clone();
            self.entries = c.entries.clone();
        }
        self.error = None;
        self.notice = None;
        self.show_history = false;
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn delete_conv(&mut self, id: String, cx: &mut Context<Self>) {
        self.history.retain(|c| c.id != id);
        ai::save_history(&self.history);
        if self.conv_id == id {
            self.stop(cx);
            self.conv_id = new_id();
            self.entries.clear();
        }
        cx.notify();
    }

    // ---------------- chat ----------------

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.send_id += 1;
        self.task = None; // drops the receiver → the sidecar request stops
        if let Some(cancel) = self.exec_cancel.take() {
            cancel.cancel();
        }
        if self.streaming {
            if matches!(self.entries.last(), Some(Entry::Assistant { text }) if text.is_empty()) {
                self.entries.pop();
            }
        }
        self.streaming = false;
        self.running = false;
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        self.send_text(text, window, cx);
    }

    fn send_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() || self.busy() {
            return;
        }
        if self.cfg.model.trim().is_empty() {
            self.show_settings = true;
            self.error = Some(tr!("ai.choose_model").into());
            cx.notify();
            return;
        }
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        self.show_history = false;
        self.auto_steps = 0;
        self.notice = None;
        self.entries.push(Entry::User { text });
        self.persist();
        self.start_stream(window, cx);
    }

    /// Ask the model for the next answer over the whole transcript.
    fn start_stream(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let system = self
            .app
            .read_with(cx, |a, _| {
                ai::system_prompt(a.active_conn.as_ref().map(|c| c.db_type), &a.schemas)
            })
            .unwrap_or_else(|_| ai::system_prompt(None, &[]));
        let mut wire = vec![ChatMessage { role: Role::System, content: system }];
        wire.extend(self.entries.iter().map(Entry::to_wire));
        // Language nudge on the newest message of the user's own words.
        let hint = self.entries.iter().rev().find_map(|e| match e {
            Entry::User { text } => ai::language_hint(text),
            _ => None,
        });
        if let (Some(hint), Some(last)) = (hint, wire.last_mut()) {
            if last.role == Role::User {
                last.content.push_str("\n\n");
                last.content.push_str(hint);
            }
        }
        self.entries.push(Entry::Assistant { text: String::new() });

        self.send_id += 1;
        let id = self.send_id;
        self.streaming = true;
        self.error = None;
        self.scroll.scroll_to_bottom();
        cx.notify();

        let mut rx = ai::stream_chat(self.cfg.clone(), wire);
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let finished = matches!(ev, AiEvent::Done | AiEvent::Error(_));
                let alive = cx
                    .update(|window, cx| {
                        this.update(cx, |v, cx| {
                            if v.send_id != id {
                                return false;
                            }
                            match ev {
                                AiEvent::Delta(s) => {
                                    if let Some(Entry::Assistant { text }) = v.entries.last_mut() {
                                        text.push_str(&s);
                                    }
                                }
                                AiEvent::Done => v.finish_answer(window, cx),
                                AiEvent::Error(e) => {
                                    v.streaming = false;
                                    v.error = Some(e);
                                    if matches!(v.entries.last(), Some(Entry::Assistant { text }) if text.is_empty()) {
                                        v.entries.pop();
                                    }
                                    v.persist();
                                }
                            }
                            v.scroll.scroll_to_bottom();
                            cx.notify();
                            true
                        })
                        .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if finished || !alive {
                    break;
                }
            }
        }));
    }

    /// Answer complete: save, and in Auto mode run its statements.
    fn finish_answer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.streaming = false;
        self.persist();
        if self.cfg.mode != AiMode::Auto {
            return;
        }
        let sqls = match self.entries.last() {
            Some(Entry::Assistant { text }) => ai::sql_of(text),
            _ => Vec::new(),
        };
        if sqls.is_empty() {
            return;
        }
        if self.auto_steps >= ai::MAX_AUTO_STEPS {
            self.notice = Some(tr!(
                "ai.auto_paused",
                n = ai::MAX_AUTO_STEPS
            ).to_string());
            return;
        }
        // Run up to the first statement that needs a human.
        let safe: Vec<String> = sqls
            .iter()
            .take_while(|s| !ai::needs_confirmation(s))
            .cloned()
            .collect();
        if safe.len() < sqls.len() {
            self.notice = Some(
                tr!("ai.destructive").into(),
            );
        }
        if safe.is_empty() {
            return;
        }
        if safe.iter().any(|q| ai::repeats_failure(&self.entries, q)) {
            self.notice = Some(
                tr!("ai.repeat_pause").into(),
            );
            return;
        }
        self.auto_steps += 1;
        self.execute(safe, true, window, cx);
    }

    /// Run `sqls` in order on the active connection, appending one `Exec`
    /// entry each; stops at the first error. `continue_after` hands the
    /// outcomes back to the model (Auto mode).
    fn execute(
        &mut self,
        sqls: Vec<String>,
        continue_after: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self.app.read_with(cx, |a, _| a.session.clone()).ok().flatten();
        let Some(session) = session else {
            self.error = Some(tr!("ai.connect_first").into());
            cx.notify();
            return;
        };
        self.send_id += 1;
        let id = self.send_id;
        self.running = true;
        self.error = None;
        cx.notify();
        let cancel = CancelToken::new();
        self.exec_cancel = Some(cancel.clone());
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut ddl = false;
            for sql in sqls {
                let res = match session.execute(sql.clone(), RESULT_LIMIT, cancel.clone()).await {
                    Ok(r) => r,
                    Err(_) => return,
                };
                let (ok, outcome) = ai::describe_outcome(&res);
                ddl |= ok && is_ddl(&sql);
                let keep_going = this
                    .update(cx, |v, cx| {
                        if v.send_id != id {
                            return false;
                        }
                        v.entries.push(Entry::Exec { sql, ok, outcome });
                        v.scroll.scroll_to_bottom();
                        cx.notify();
                        ok
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
            let _ = cx.update(|window, cx| {
                let _ = this.update(cx, |v, cx| {
                    if v.send_id != id {
                        return;
                    }
                    v.running = false;
                    v.persist();
                    if ddl {
                        let _ = v.app.update(cx, |a, cx| a.reload_schemas(cx));
                    }
                    if continue_after {
                        v.start_stream(window, cx);
                    }
                    cx.notify();
                });
            });
        }));
    }

    /// Manual Run on a code block. In Auto mode the model also sees the
    /// outcome and continues (this is how a paused round resumes).
    fn run_block(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.notice = None;
        let cont = self.cfg.mode == AiMode::Auto;
        if cont {
            self.auto_steps = 0;
        }
        self.execute(vec![sql], cont, window, cx);
    }

    /// Retry: drop everything after the last user message (the answer and
    /// any statements it ran) and ask the model again. Statements already
    /// executed against the database are *not* undone.
    fn regenerate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(last_user) = self
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::User { .. }))
        else {
            return;
        };
        self.entries.truncate(last_user + 1);
        self.auto_steps = 0;
        self.notice = None;
        self.error = None;
        self.persist();
        self.start_stream(window, cx);
    }

    /// Put the last user message back into the input for editing; the
    /// turn it started is removed.
    fn edit_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(ix) = self
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::User { .. }))
        else {
            return;
        };
        if let Entry::User { text } = self.entries[ix].clone() {
            self.entries.truncate(ix);
            self.persist();
            self.input.update(cx, |s, cx| s.set_value(text, window, cx));
            cx.notify();
        }
    }

    fn open_sql(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.app.update(cx, |a, cx| a.open_query_tab_seeded(sql, window, cx));
    }
}

impl Render for AiChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let connected = self
            .app
            .read_with(cx, |a, _| a.active_conn.is_some())
            .unwrap_or(false);
        let busy = self.busy();
        let auto = self.cfg.mode == AiMode::Auto;

        let icon_btn = |id: &'static str, icon: IconName, tip: SharedString| {
            Button::new(id).ghost().xsmall().icon(Icon::new(icon)).tooltip(tip)
        };
        let mode_btn = |id: &'static str, label: SharedString, on: bool| {
            let b = Button::new(id).xsmall().label(label);
            if on { b.primary() } else { b.ghost() }
        };

        let header = h_flex()
            .h(theme::scaled(38.))
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .border_b_1()
            .border_color(p.toolbar_border)
            .bg(p.toolbar)
            .child(Icon::new(IconName::Bot).with_size(Size::Size(px(14.))).text_color(p.primary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(theme::scaled(theme::TEXT_MD))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if self.cfg.model.is_empty() {
                        tr!("ai.title").to_string()
                    } else {
                        self.cfg.model.clone()
                    }),
            )
            .when_some(self.settings.active().filter(|_| self.settings.providers.len() > 1), |this, prov| {
                this.child(
                    div()
                        .flex_none()
                        .text_size(theme::scaled(theme::TEXT_XS))
                        .text_color(p.muted_fg)
                        .child(prov.name.clone()),
                )
            })
            .child(icon_btn("ai-history", IconName::Inbox, tr!("ai.history")).on_click(cx.listener(
                |this, _, _, cx| {
                    this.show_history = !this.show_history;
                    cx.notify();
                },
            )))
            .child(icon_btn("ai-new", IconName::Plus, tr!("ai.new_chat")).on_click(
                cx.listener(|this, _, _, cx| this.new_chat(cx)),
            ))
            .child(icon_btn("ai-settings", IconName::Settings, tr!("ai.model_settings")).on_click(
                cx.listener(|this, _, _, cx| {
                    this.show_settings = !this.show_settings;
                    cx.notify();
                }),
            ));

        let settings = self.show_settings.then(|| self.render_settings(cx));

        let body = if self.show_history {
            self.render_history(cx)
        } else {
            self.render_transcript(connected, busy, cx)
        };

        let composer = v_flex()
            .flex_none()
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(p.border)
            .when_some(self.notice.clone(), |this, n| {
                this.child(
                    div()
                        .text_size(theme::scaled(theme::TEXT_SM))
                        .text_color(p.muted_fg)
                        .child(n),
                )
            })
            .when_some(self.error.clone(), |this, e| {
                this.child(
                    div()
                        .text_size(theme::scaled(theme::TEXT_SM))
                        .text_color(p.conn_err)
                        .child(e),
                )
            })
            .child(Textarea::new(&self.input))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
            .child(
                h_flex()
                    .gap(px(2.))
                    .p(px(2.))
                    .rounded(px(5.))
                    .bg(p.background)
                    .child(
                        mode_btn("ai-manual", tr!("ai.manual"), !auto)
                            .tooltip(tr!("ai.manual_tip"))
                            .on_click(cx.listener(|this, _, _, cx| this.set_mode(AiMode::Manual, cx))),
                    )
                    .child(
                        mode_btn("ai-auto", tr!("ai.auto"), auto)
                            .tooltip(tr!("ai.auto_tip"))
                            .on_click(cx.listener(|this, _, _, cx| this.set_mode(AiMode::Auto, cx))),
                    ),
            )
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::scaled(theme::TEXT_XS))
                            .text_color(p.muted_fg)
                            .child(if self.running {
                                tr!("ai.running_sql")
                            } else if self.streaming {
                                tr!("ai.answering")
                            } else {
                                SharedString::default()
                            }),
                    )
                    .child(if busy {
                        Button::new("ai-stop")
                            .small()
                            .outline()
                            .icon(Icon::new(IconName::CircleX))
                            .label(tr!("ai.stop"))
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                    } else {
                        Button::new("ai-send")
                            .small()
                            .primary()
                            .icon(Icon::new(IconName::ArrowUp))
                            .label(tr!("ai.send"))
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                    }),
            );

        v_flex()
            .id("ai-panel")
            .size_full()
            .bg(p.card)
            .text_color(p.foreground)
            .child(header)
            .children(settings)
            .child(body)
            .child(composer)
    }
}

impl AiChatView {
    fn render_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = theme::palette(cx);
        let label = |s: SharedString| {
            div()
                .text_size(theme::scaled(theme::TEXT_XS))
                .text_color(p.muted_fg)
                .child(s)
        };
        // Provider chips: one per configured endpoint, plus "Add".
        let active_id = self.settings.active().map(|p| p.id.clone());
        let mut providers = h_flex().flex_wrap().gap_1();
        for (i, prov) in self.settings.providers.iter().enumerate() {
            let id = prov.id.clone();
            let b = Button::new(("ai-provider", i)).xsmall().label(prov.name.clone());
            let b = if active_id.as_deref() == Some(prov.id.as_str()) { b.primary() } else { b.outline() };
            providers = providers.child(b.on_click(cx.listener(move |this, _, window, cx| {
                this.select_provider(id.clone(), window, cx)
            })));
        }
        providers = providers.child(
            Button::new("ai-provider-add")
                .xsmall()
                .ghost()
                .icon(Icon::new(IconName::Plus))
                .label(tr!("ai.add"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.adding = !this.adding || this.settings.providers.is_empty();
                    cx.notify();
                })),
        );

        let presets = self.adding.then(|| {
            let mut row = h_flex().flex_wrap().gap_1();
            for (i, pr) in PRESETS.iter().enumerate() {
                row = row.child(
                    Button::new(("ai-preset", i))
                        .xsmall()
                        .outline()
                        .label(pr.name)
                        .tooltip(if pr.base_url.is_empty() { tr!("ai.custom_tip") } else { SharedString::from(pr.base_url) })
                        .on_click(cx.listener(move |this, _, window, cx| this.add_provider(i, window, cx))),
                );
            }
            v_flex().gap_1().child(label(tr!("ai.add_provider"))).child(row)
        });

        let filter = self.model.read(cx).value().to_string();
        let shown = ai::filter_models(&self.models, &filter);
        // A model already typed in full still shows the whole list.
        let shown = if shown.len() == 1 && *shown[0] == filter.trim() {
            self.models.iter().collect()
        } else {
            shown
        };
        let mut chips = h_flex().flex_wrap().gap_1();
        for (i, m) in shown.iter().take(MODEL_CHIPS).enumerate() {
            let m = (*m).clone();
            chips = chips.child(
                Button::new(("ai-model", i))
                    .xsmall()
                    .outline()
                    .label(m.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick_model(m.clone(), window, cx)
                    })),
            );
        }
        if shown.len() > MODEL_CHIPS {
            chips = chips.child(label_owned(
                tr!("ai.more_models", n = shown.len() - MODEL_CHIPS).to_string(),
                p.muted_fg,
            ));
        } else if shown.is_empty() && !self.models.is_empty() {
            chips = chips.child(label_owned(tr!("ai.no_match", q = filter.trim()).to_string(), p.muted_fg));
        }

        let head = v_flex()
            .flex_none()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(p.border)
            .child(label(tr!("ai.provider")))
            .child(providers)
            .children(presets);
        if self.settings.providers.is_empty() {
            return head.into_any_element();
        }
        head.child(label(tr!("common.name")))
            .child(Input::new(&self.name).small())
            .child(label(tr!("ai.server")))
            .child(Input::new(&self.base_url).small())
            .child(label(tr!("ai.model")))
            .child(
                h_flex()
                    .gap_1()
                    .child(div().flex_1().child(Input::new(&self.model).small()))
                    .child(
                        Button::new("ai-models")
                            .small()
                            .outline()
                            .label(if self.models_loading { SharedString::from("…") } else { tr!("ai.list") })
                            .disabled(self.models_loading)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.fetch_models(window, cx)
                            })),
                    ),
            )
            .child(chips)
            .child(label(tr!("ai.api_key")))
            .child(Input::new(&self.api_key).small().mask_toggle())
            .child(
                h_flex()
                    .justify_between()
                    .pt_1()
                    .child(
                        Button::new("ai-provider-remove")
                            .small()
                            .ghost()
                            .icon(Icon::new(IconName::Delete))
                            .label(tr!("ai.remove"))
                            .tooltip(tr!("ai.remove_tip"))
                            .on_click(cx.listener(|this, _, window, cx| this.remove_provider(window, cx))),
                    )
                    .child(
                        Button::new("ai-save")
                            .small()
                            .primary()
                            .label(tr!("ai.save"))
                            .on_click(cx.listener(|this, _, _, cx| this.save_settings(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_history(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = theme::palette(cx);
        let mut list = v_flex().gap_1().p_2();
        if self.history.is_empty() {
            list = list.child(
                div()
                    .text_size(theme::scaled(theme::TEXT_SM))
                    .text_color(p.muted_fg)
                    .child(tr!("ai.no_history")),
            );
        }
        for (i, c) in self.history.iter().enumerate() {
            let (open_id, del_id) = (c.id.clone(), c.id.clone());
            let when = chrono::DateTime::from_timestamp_millis(c.updated as i64)
                .map(|d| d.with_timezone(&chrono::Local).format("%d.%m %H:%M").to_string())
                .unwrap_or_default();
            let current = c.id == self.conv_id;
            list = list.child(
                h_flex()
                    .id(("ai-conv", i))
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(6.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(current, |d| d.bg(p.accent))
                    .hover(move |s| s.bg(p.tree_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_conv(open_id.clone(), cx)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_size(theme::scaled(theme::TEXT_MD))
                                    .child(c.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(theme::scaled(theme::TEXT_XS))
                                    .text_color(p.muted_fg)
                                    .child(format!("{when} · {}", tr!("ai.messages", n = c.entries.len()))),
                            ),
                    )
                    .child(
                        Button::new(("ai-conv-del", i))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Delete))
                            .tooltip(tr!("ai.delete"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.delete_conv(del_id.clone(), cx)
                            })),
                    ),
            );
        }
        div()
            .id("ai-history-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    fn render_transcript(&self, connected: bool, busy: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = theme::palette(cx);
        let mono_block = |text: String, color: Hsla| {
            div()
                .p_2()
                .font_family(theme::MONO_FONT)
                .text_size(theme::scaled(theme::TEXT_SM))
                .text_color(color)
                .child(text)
        };

        let mut list = v_flex().gap_3().p_2();
        if self.entries.is_empty() {
            list = list.child(
                div()
                    .text_size(theme::scaled(theme::TEXT_SM))
                    .text_color(p.muted_fg)
                    .child(if connected {
                        tr!("ai.intro")
                    } else {
                        tr!("ai.connect_hint")
                    }),
            );
        }
        let last_ix = self.entries.len().saturating_sub(1);
        for (mi, e) in self.entries.iter().enumerate() {
            match e {
                Entry::User { text } => {
                    list = list.child(
                        div().pl_6().child(
                            div()
                                .p_2()
                                .rounded(px(6.))
                                .bg(p.accent)
                                .text_size(theme::scaled(theme::TEXT_MD))
                                .child(text.clone()),
                        ),
                    );
                }
                Entry::Assistant { text } => {
                    let mut body = v_flex().gap_1().min_w_0().p_2();
                    if text.is_empty() && self.streaming && mi == last_ix {
                        body = body.child(
                            div()
                                .text_size(theme::scaled(theme::TEXT_SM))
                                .text_color(p.muted_fg)
                                .child(tr!("ai.thinking")),
                        );
                    }
                    for (si, (code, seg)) in segments(text).into_iter().enumerate() {
                        if !code {
                            body = body.child(
                                div()
                                    .text_size(theme::scaled(theme::TEXT_MD))
                                    .child(seg),
                            );
                            continue;
                        }
                        let (a, b, c) = (seg.clone(), seg.clone(), seg.clone());
                        let danger = ai::needs_confirmation(&seg);
                        body = body.child(
                            v_flex()
                                .rounded(px(4.))
                                .border_1()
                                .border_color(if danger { p.destructive } else { p.border })
                                .bg(p.background)
                                .child(mono_block(seg, p.foreground))
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .px_1()
                                        .pb_1()
                                        .justify_end()
                                        .child(
                                            Button::new(SharedString::from(format!("ai-copy-{mi}-{si}")))
                                                .xsmall()
                                                .ghost()
                                                .icon(Icon::new(IconName::Copy))
                                                .tooltip(tr!("ai.copy"))
                                                .on_click(move |_, _, cx| {
                                                    cx.write_to_clipboard(ClipboardItem::new_string(a.clone()))
                                                }),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!("ai-open-{mi}-{si}")))
                                                .xsmall()
                                                .outline()
                                                .label(tr!("ai.open_sql"))
                                                .disabled(!connected)
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    this.open_sql(b.clone(), window, cx)
                                                })),
                                        )
                                        .child({
                                            let run = Button::new(SharedString::from(format!("ai-run-{mi}-{si}")))
                                                .xsmall()
                                                .label(tr!("common.run"))
                                                .disabled(!connected || busy)
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    this.run_block(c.clone(), window, cx)
                                                }));
                                            if danger { run.danger() } else { run.primary() }
                                        }),
                                ),
                        );
                    }
                    list = list.child(body);
                }
                Entry::Exec { sql, ok, outcome } => {
                    let color = if *ok { p.conn_ok } else { p.conn_err };
                    list = list.child(
                        v_flex()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(color.opacity(0.5))
                            .bg(p.background)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .px_2()
                                    .pt_1()
                                    .items_center()
                                    .text_size(theme::scaled(theme::TEXT_XS))
                                    .text_color(color)
                                    .child(
                                        Icon::new(if *ok { IconName::CircleCheck } else { IconName::TriangleAlert })
                                            .with_size(Size::Size(px(12.))),
                                    )
                                    .child(if *ok { tr!("ai.executed") } else { tr!("ai.failed") }),
                            )
                            .child(mono_block(clip_lines(sql, 4), p.muted_fg))
                            .child(mono_block(clip_lines(outcome, 10), if *ok { p.foreground } else { p.conn_err })),
                    );
                }
            }
        }

        // Retry / edit row under the finished turn.
        if !busy && self.entries.iter().any(|e| matches!(e, Entry::User { .. })) {
            list = list.child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("ai-regenerate")
                            .xsmall()
                            .ghost()
                            .icon(Icon::new(IconName::Redo2))
                            .label(tr!("ai.retry"))
                            .tooltip(tr!("ai.retry_tip"))
                            .on_click(cx.listener(|this, _, window, cx| this.regenerate(window, cx))),
                    )
                    .child(
                        Button::new("ai-edit-last")
                            .xsmall()
                            .ghost()
                            .label(tr!("ai.edit"))
                            .tooltip(tr!("ai.edit_tip"))
                            .on_click(cx.listener(|this, _, window, cx| this.edit_last(window, cx))),
                    ),
            );
        }

        div()
            .id("ai-transcript")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(list)
            .into_any_element()
    }
}
