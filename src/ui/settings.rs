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
