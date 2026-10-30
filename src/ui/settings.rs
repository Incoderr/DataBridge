//! Interface scale settings (font / UI size), shared app-wide via a Global.

use gpui::{px, App, Global, Pixels, Window};

/// Preset interface scales shown in the Settings dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiScale {
    Compact,
    Normal,
    Large,
}

impl UiScale {
    #[allow(dead_code)]
    pub const ALL: [UiScale; 3] = [UiScale::Compact, UiScale::Normal, UiScale::Large];

    pub fn factor(self) -> f32 {
        match self {
            UiScale::Compact => 0.85,
            UiScale::Normal => 1.0,
            UiScale::Large => 1.2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            UiScale::Compact => "Compact",
            UiScale::Normal => "Normal",
            UiScale::Large => "Large",
        }
    }

    #[allow(dead_code)]
    pub fn percent(self) -> u32 {
        (self.factor() * 100.).round() as u32
    }
}

/// Process-wide active scale.
pub struct UiScaleGlobal(pub UiScale);
impl Global for UiScaleGlobal {}

pub fn current(cx: &App) -> UiScale {
    cx.try_global::<UiScaleGlobal>()
        .map(|g| g.0)
        .unwrap_or(UiScale::Normal)
}

/// Base design font size before scaling (`theme.font_size = base * factor`).
/// All rem-based utilities (gap_1, size_4, text_sm…) scale with it because
/// `Root` feeds `theme.font_size` into `window.set_rem_size`.
pub const BASE_FONT_SIZE: f32 = 14.0;

pub fn font_size(cx: &App) -> Pixels {
    px(BASE_FONT_SIZE * current(cx).factor())
}

/// Switch the active scale and re-apply the theme so every surface repaints.
pub fn set(scale: UiScale, window: Option<&mut Window>, cx: &mut App) {
    if current(cx) == scale {
        return;
    }
    cx.set_global(UiScaleGlobal(scale));
    crate::ui::theme::reapply(window, cx);
}

/// SQL editor font size in design px at the Normal interface scale
/// (rendered through `theme::scaled`); Ctrl+wheel over an editor changes
/// it for every SQL tab. Not persisted yet, like the interface scale.
pub struct EditorFontSize(pub f32);
impl Global for EditorFontSize {}

pub const EDITOR_FONT_DEFAULT: f32 = 14.;
pub const EDITOR_FONT_MIN: f32 = 9.;
pub const EDITOR_FONT_MAX: f32 = 32.;

pub fn editor_font_size(cx: &App) -> f32 {
    cx.try_global::<EditorFontSize>()
        .map(|g| g.0)
        .unwrap_or(EDITOR_FONT_DEFAULT)
}

/// Step the editor font by `delta` px (clamped) and repaint.
pub fn bump_editor_font_size(delta: f32, cx: &mut App) {
    let size = (editor_font_size(cx) + delta).clamp(EDITOR_FONT_MIN, EDITOR_FONT_MAX);
    cx.set_global(EditorFontSize(size));
    cx.refresh_windows();
}
