//! Document stores (MongoDB) in the records view: a JSON view of the page
//! (one card per document with an expandable field tree, like Compass),
//! the same tree in the detail panel, and a whole-document editor that
//! works on mongosh text (`model::format_document` → the driver's
//! `save_document`). Rows/columns stay available through the Table view.

use gpui::prelude::*;
use gpui::{AnyElement, ClickEvent, Context, Entity, FontWeight, SharedString, Window, div, px};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState, TabSize};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::{Disableable as _, Icon, IconName, Sizable as _, Size, h_flex, v_flex};

use super::{PAGE, RecordsView};
use crate::core::model::{CellChange, DocNode, TableRef, format_document};
use crate::ui::theme;

/// What the document editor was opened for.
pub(super) enum DocEdit {
    /// Replace the document at this page index.
    Edit(usize),
    /// A copy of the document at this index, without `_id`.
    Duplicate(usize),
    New,
}

pub(super) struct DocEditor {
    input: Entity<InputState>,
    /// Identity of the edited document; empty = insert.
    pk: Vec<CellChange>,
    title: String,
    error: Option<String>,
    saving: bool,
    save_id: u64,
}

impl RecordsView {
    /// The page carries full documents (a document store).
    pub(super) fn has_docs(&self, cx: &gpui::App) -> bool {
        self.state.read(cx).delegate().docs.is_some()
    }

    fn doc(&self, ix: usize, cx: &gpui::App) -> Option<DocNode> {
        self.state.read(cx).delegate().docs.as_ref()?.get(ix).cloned()
    }

    pub(super) fn open_doc_editor(&mut self, what: DocEdit, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.state.read(cx).delegate().page;
        let (text, pk, title) = match what {
            DocEdit::Edit(ix) => {
                let (Some(doc), Some(row)) = (self.doc(ix, cx), self.state.read(cx).delegate().rows.get(ix).cloned())
                else {
                    return;
                };
                let pk = self.pk_cells(&row);
                if pk.is_empty() {
                    return;
                }
                let no = page * PAGE + ix as u64 + 1;
                (format_document(&doc), pk, format!("Edit document {no} · {}", self.table.name))
            }
            DocEdit::Duplicate(ix) => {
                let Some(doc) = self.doc(ix, cx) else { return };
                (
                    format_document(&doc.without("_id")),
                    Vec::new(),
                    format!("New document · {}", self.table.name),
                )
            }
            DocEdit::New => (
                "{\n  \n}".to_string(),
                Vec::new(),
                format!("New document · {}", self.table.name),
            ),
        };
        let input = cx.new(|cx| {
            let mut s = InputState::new(window, cx)
                .code_editor("javascript")
                .tab_size(TabSize { tab_size: 2, hard_tabs: false });
            s.set_value(text, window, cx);
            s
        });
        self.doc_editor = Some(DocEditor {
            input,
            pk,
            title,
            error: None,
            saving: false,
            save_id: 0,
        });
        cx.notify();
    }

    pub(super) fn close_doc_editor(&mut self, cx: &mut Context<Self>) {
        self.doc_editor = None;
        cx.notify();
    }

    /// Insert / replace through the session, then reload the page
    /// (stale-guarded by `save_id`, like the row editor).
    fn save_doc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.doc_editor.as_mut() else { return };
        let text = ed.input.read(cx).value().to_string();
        let pk = ed.pk.clone();
        ed.saving = true;
        ed.save_id += 1;
        ed.error = None;
        let id = ed.save_id;
        let table = TableRef::of(&self.table);
        let rx = self.state.read(cx).delegate().session.save_document(table, pk, text);
        cx.notify();
        let view = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            let result = rx.await;
            cx.update(|_window, cx| {
                view.update(cx, |v, cx| {
                    let Some(ed) = v.doc_editor.as_mut() else { return };
                    if ed.save_id != id {
                        return;
                    }
                    ed.saving = false;
                    match result {
                        Ok(Ok(_)) => {
                            v.doc_editor = None;
                            v.reload(cx);
                        }
                        Ok(Err(e)) => ed.error = Some(e.message().to_string()),
                        Err(_) => ed.error = Some("save was cancelled".into()),
                    }
                    cx.notify();
                })
            })
            .ok();
        })
        .detach();
    }

    // ---------------- tree ----------------

    /// Rows of a field tree: one line per field, containers collapsible
    /// (`doc_open` holds the expanded paths; collapsed by default).
    fn tree_rows(
        &self,
        key: Option<String>,
        node: &DocNode,
        path: String,
        depth: usize,
        out: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let p = theme::palette(cx);
        let is_container = !matches!(node, DocNode::Scalar { .. });
        let open = is_container && self.doc_open.contains(&path);
        let value: AnyElement = match node {
            DocNode::Scalar { text, kind } => div()
                .flex_1()
                .min_w_0()
                .text_color(match kind {
                    Some(k) => theme::kind_color(k, &p),
                    None => p.keyword,
                })
                .child(text.clone())
                .into_any_element(),
            container => div()
                .text_color(p.muted_fg)
                .child(container.summary())
                .into_any_element(),
        };
        let toggle_path = path.clone();
        let line = h_flex()
            .id(SharedString::from(format!("doc-line:{path}")))
            .items_start()
            .gap(px(4.))
            .pl(px(depth as f32 * 16.))
            .py(px(1.))
            .font_family(theme::MONO_FONT)
            .text_size(theme::scaled(theme::TEXT_MD))
            .child(
                div()
                    .w(px(14.))
                    .flex_none()
                    .pt(px(2.))
                    .when(is_container, |t| {
                        t.child(
                            Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                                .with_size(Size::Size(px(12.)))
                                .text_color(p.muted_fg),
                        )
                    }),
            )
            .when_some(key, |t, k| {
                t.child(
                    div()
                        .flex_none()
                        .max_w(theme::scaled(260.))
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.foreground)
                        .child(k),
                )
                .child(div().flex_none().text_color(p.muted_fg).child(":"))
            })
            .child(value)
            .when(is_container, |t| {
                t.cursor_pointer()
                    .rounded(px(3.))
                    .hover(move |s| s.bg(p.tree_hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        if !this.doc_open.remove(&toggle_path) {
                            this.doc_open.insert(toggle_path.clone());
                        }
                        cx.notify();
                    }))
            });
        out.push(line.into_any_element());
        if !open {
            return;
        }
        match node {
            DocNode::Object(fields) => {
                for (k, v) in fields {
                    self.tree_rows(Some(k.clone()), v, format!("{path}/{k}"), depth + 1, out, cx);
                }
            }
            DocNode::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    self.tree_rows(Some(i.to_string()), v, format!("{path}/{i}"), depth + 1, out, cx);
                }
            }
            DocNode::Scalar { .. } => {}
        }
    }

    /// The top-level fields of one document as tree lines (`skip` hides a
    /// field the caller shows elsewhere, e.g. `_id` in the card header).
    fn doc_fields(&self, ix: usize, doc: &DocNode, skip: Option<&str>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let DocNode::Object(fields) = doc {
            for (k, v) in fields.iter().filter(|(k, _)| Some(k.as_str()) != skip) {
                self.tree_rows(Some(k.clone()), v, format!("{ix}/{k}"), 0, &mut out, cx);
            }
        }
        out
    }

    /// The detail panel's body for the selected document.
    pub(super) fn render_doc_detail(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let doc = self.doc(ix, cx)?;
        let lines = self.doc_fields(ix, &doc, None, cx);
        Some(
            v_flex()
                .flex_1()
                .min_h_0()
                .px_3()
                .py_1()
                .overflow_y_scrollbar()
                .children(lines)
                .into_any_element(),
        )
    }

    /// The JSON view: one card per document of the page. Click selects
    /// (toolbar edit / delete act on it), double-click edits.
    pub(super) fn render_doc_cards(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = theme::palette(cx);
        let st = self.state.read(cx);
        let d = st.delegate();
        let docs = d.docs.clone().unwrap_or_default();
        let first = d.page * PAGE;
        let selected = st.selected_row();
        if docs.is_empty() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme::scaled(theme::TEXT_SM))
                .text_color(p.muted_fg)
                .child(if d.loading { "Loading documents…" } else { "No documents." })
                .into_any_element();
        }
        let cards: Vec<AnyElement> = docs
            .iter()
            .enumerate()
            .map(|(i, doc)| {
                let is_sel = selected == Some(i);
                let id = doc.field("_id").map(DocNode::summary).unwrap_or_else(|| "(no _id)".into());
                let fields = self.doc_fields(i, doc, Some("_id"), cx);
                // Spacing via margin: the scroll wrapper of
                // `overflow_y_scrollbar` does not carry the list's `gap`.
                v_flex()
                    .id(("doc-card", i))
                    .flex_none()
                    .mb(theme::scaled(10.))
                    .gap(px(2.))
                    .px_3()
                    .py(theme::scaled(10.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if is_sel { p.primary } else { p.border })
                    .bg(p.card)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_size(theme::scaled(theme::TEXT_XS))
                            .child(div().text_color(p.muted_fg).child(format!("#{}", first + i as u64 + 1)))
                            .child(
                                div()
                                    .font_family(theme::MONO_FONT)
                                    .text_color(p.uuid)
                                    .min_w_0()
                                    .truncate()
                                    .child(format!("_id: {id}")),
                            ),
                    )
                    .children(fields)
                    .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                        this.state.update(cx, |s, cx| s.set_selected_row(i, cx));
                        if e.click_count() >= 2 && this.doc_editor.is_none() && this.editor.is_none() {
                            this.open_doc_editor(DocEdit::Edit(i), window, cx);
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .size_full()
            .p_3()
            .overflow_y_scrollbar()
            .children(cards)
            .into_any_element()
    }

    /// The whole-document editor modal (same backdrop/card structure as
    /// the row editor — pinned insets, occluding card).
    pub(super) fn render_doc_editor(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = theme::palette(cx);
        let Some(ed) = self.doc_editor.as_ref() else {
            return div().into_any_element();
        };
        let saving = ed.saving;
        let is_insert = ed.pk.is_empty();
        let backdrop = div()
            .id("doc-editor-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::black().opacity(0.35))
            .when(!saving, |t| t.on_click(cx.listener(|this, _, _, cx| this.close_doc_editor(cx))));
        let card = v_flex()
            .id("doc-editor")
            .absolute()
            .top_1_2()
            .left_1_2()
            .mt(px(-310.))
            .ml(px(-360.))
            .w(theme::scaled(720.))
            .occlude()
            .bg(p.card)
            .border_1()
            .border_color(p.border)
            .rounded(px(10.))
            .shadow_lg()
            .overflow_hidden()
            .child(
                v_flex()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        div()
                            .text_size(theme::scaled(theme::TEXT_XL))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.foreground)
                            .child(ed.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(theme::scaled(theme::TEXT_SM))
                            .text_color(p.muted_fg)
                            .child(if is_insert {
                                "mongosh syntax: ObjectId(\"…\"), ISODate(\"…\"), NumberLong(…) … — _id is generated when omitted."
                            } else {
                                "The whole document is replaced; _id cannot change."
                            }),
                    ),
            )
            .child(div().p_2().child(Input::new(&ed.input).h(px(440.))))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(p.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(theme::scaled(theme::TEXT_MD))
                            .text_color(p.conn_err)
                            .when_some(ed.error.clone(), |t, e| t.child(e)),
                    )
                    .child(
                        Button::new("doc-cancel")
                            .ghost()
                            .label("Cancel")
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.close_doc_editor(cx))),
                    )
                    .child(
                        Button::new("doc-save")
                            .primary()
                            .label(match (saving, is_insert) {
                                (true, _) => "Saving…",
                                (false, true) => "Insert document",
                                (false, false) => "Replace document",
                            })
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, window, cx| this.save_doc(window, cx))),
                    ),
            );
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(backdrop)
            .child(card)
            .into_any_element()
    }
}
