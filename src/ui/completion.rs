//! Editor adapter for SQL autocompletion: plugs `core::completion` into
//! gpui-component's code editor as its `CompletionProvider`. The schema
//! tree is read from the app on every request, so tables created or
//! dropped (DDL runs reload it) show up immediately.
//!
//! The menu keeps only labels: gpui-component's completion popover does
//! not grow with its content (min 120 px), so descriptions go into the
//! item `documentation`, which it shows beside the selected item. Ghost
//! text comes from `core::completion::predict`; the last one offered is
//! kept in [`GhostSlot`] so the query view can accept it with →.
//!
//! `anyhow` appears here only because the provider trait's signature
//! requires it.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, Task, WeakEntity, Window};
use gpui_component::input::{CompletionProvider, Rope, RopeExt};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    Documentation, InlineCompletionContext, InlineCompletionItem, InlineCompletionResponse, Range,
    TextEdit,
};

use crate::core::completion::{self, SuggestionKind};
use crate::core::model::DbType;
use crate::ui::app::DataBridge;

pub struct SqlCompletions {
    pub app: WeakEntity<DataBridge>,
    pub dialect: DbType,
    /// The ghost text last offered (shared with the query view).
    pub ghost: GhostSlot,
    /// Where the completion menu was last opened with items (shared too).
    pub menu: MenuSlot,
}

/// The document and cursor the completion menu last opened for (with at
/// least one item). Like [`Ghost`], valid only while both are unchanged:
/// gpui-component does not say whether its menu is open.
pub struct MenuAt {
    pub text: String,
    pub offset: usize,
}

pub type MenuSlot = Rc<RefCell<Option<MenuAt>>>;

/// A ghost-text suggestion as offered: valid only while the document and
/// the cursor are still exactly what it was computed for. gpui-component
/// keeps its own copy private, so the view tracks this one to accept it
/// with → (Tab is the editor's own key).
pub struct Ghost {
    pub text: String,
    pub offset: usize,
    pub insert: String,
}

pub type GhostSlot = Rc<RefCell<Option<Ghost>>>;

impl CompletionProvider for SqlCompletions {
    fn completions(
        &self,
        rope: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let empty = || Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        self.menu.borrow_mut().take();
        let Some(app) = self.app.upgrade() else {
            return empty();
        };
        let app = app.read(cx);
        let text = rope.to_string();
        let result = completion::complete(
            &text,
            offset,
            &app.schemas,
            app.default_schema().as_deref(),
            self.dialect,
        );
        if result.items.is_empty() {
            return empty();
        }
        *self.menu.borrow_mut() = Some(MenuAt { text: text.clone(), offset });
        let range = Range {
            start: rope.offset_to_position(result.start),
            end: rope.offset_to_position(result.end),
        };
        let items = result
            .items
            .into_iter()
            .map(|s| CompletionItem {
                // The menu highlights `filter_text.len()` leading bytes.
                filter_text: Some(if s.prefix_match {
                    result.prefix.clone()
                } else {
                    String::new()
                }),
                kind: Some(match s.kind {
                    SuggestionKind::Keyword => CompletionItemKind::KEYWORD,
                    SuggestionKind::Function => CompletionItemKind::FUNCTION,
                    SuggestionKind::Schema => CompletionItemKind::MODULE,
                    SuggestionKind::Table | SuggestionKind::View => CompletionItemKind::CLASS,
                    SuggestionKind::Column => CompletionItemKind::FIELD,
                    SuggestionKind::Alias => CompletionItemKind::VARIABLE,
                }),
                documentation: s.detail.map(Documentation::String),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: s.insert,
                })),
                label: s.label,
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn inline_completion(
        &self,
        rope: &Rope,
        offset: usize,
        _: InlineCompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<InlineCompletionResponse>> {
        let text = rope.to_string();
        let ghost = self.app.upgrade().and_then(|app| {
            let app = app.read(cx);
            completion::predict(
                &text,
                offset,
                &app.schemas,
                app.default_schema().as_deref(),
                self.dialect,
            )
        });
        *self.ghost.borrow_mut() = ghost
            .as_ref()
            .filter(|g| !g.is_empty())
            .map(|insert| Ghost { text, offset, insert: insert.clone() });
        let items = ghost
            .map(|insert_text| InlineCompletionItem {
                insert_text,
                filter_text: None,
                range: None,
                command: None,
                insert_text_format: None,
            })
            .into_iter()
            .collect();
        Task::ready(Ok(InlineCompletionResponse::Array(items)))
    }

    /// Every edit re-queries; the engine returns nothing where the menu
    /// should stay closed (spaces, strings, comments, numbers).
    fn is_completion_trigger(&self, _: usize, _: &str, _: &mut App) -> bool {
        true
    }
}
