//! Editor adapter for SQL autocompletion: plugs `core::completion` into
//! gpui-component's code editor as its `CompletionProvider`. The schema
//! tree is read from the app on every request, so tables created or
//! dropped (DDL runs reload it) show up immediately.
//!
//! The menu keeps only labels: gpui-component's completion popover does
//! not grow with its content (min 120 px), so descriptions go into the
//! item `documentation`, which it shows beside the selected item. Ghost
//! text comes from `core::completion::predict`.
//!
//! `anyhow` appears here only because the provider trait's signature
//! requires it.

use gpui::{Context, Task, WeakEntity, Window};
use gpui_component::input::{CompletionProvider, InputState, Rope, RopeExt};
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
}

impl CompletionProvider for SqlCompletions {
    fn completions(
        &self,
        rope: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let empty = || Task::ready(Ok(CompletionResponse::Array(Vec::new())));
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
        cx: &mut Context<InputState>,
    ) -> Task<anyhow::Result<InlineCompletionResponse>> {
        let ghost = self.app.upgrade().and_then(|app| {
            let app = app.read(cx);
            completion::predict(
                &rope.to_string(),
                offset,
                &app.schemas,
                app.default_schema().as_deref(),
                self.dialect,
            )
        });
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
    fn is_completion_trigger(&self, _: usize, _: &str, _: &mut Context<InputState>) -> bool {
        true
    }
}
