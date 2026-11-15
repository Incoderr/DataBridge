//! Interface settings (scale, theme, language, SQL editor font), shared app-wide via
//! Globals and persisted in `settings.json` (`core::storage::AppSettings`):
//! loaded once in `main` (`restore`), written on every change (`persist`).

use gpui::{px, App, Global, Pixels, Window};

/// Preset interface scales shown in the Settings dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiScale {
    Compact,
    Normal,
    Large,
}

impl UiScale {
    pub const ALL: [UiScale; 3] = [UiScale::Compact, UiScale::Normal, UiScale::Large];

    pub fn factor(self) -> f32 {
        match self {
            UiScale::Compact => 0.85,
            UiScale::Normal => 1.0,
            UiScale::Large => 1.2,
        }
    }

    /// The `settings.json` spelling.
    pub fn key(self) -> &'static str {
        match self {
            UiScale::Compact => "compact",
            UiScale::Normal => "normal",
            UiScale::Large => "large",
        }
    }

    /// Translated name (Settings dialog).
    pub fn label(self) -> String {
        match self {
            UiScale::Compact => rust_i18n::t!("settings.scale_compact"),
            UiScale::Normal => rust_i18n::t!("settings.scale_normal"),
            UiScale::Large => rust_i18n::t!("settings.scale_large"),
        }
        .into_owned()
    }

    pub fn percent(self) -> u32 {
        (self.factor() * 100.).round() as u32
    }
}

/// UI language. The active one is rust-i18n's process-wide locale (shared
/// with gpui-component's own strings); `Language::current` reads it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    En,
    Ru,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::En, Language::Ru];

    /// Locale code (`locales/app.yml`, `settings.json`).
    pub fn key(self) -> &'static str {
        match self {
            Language::En => "en",
            Language::Ru => "ru",
        }
    }

    /// Native name — shown untranslated so it is findable in any locale.
    pub fn label(self) -> &'static str {
        match self {
            Language::En => "English",
            Language::Ru => "Русский",
        }
    }

    pub fn current() -> Language {
        let loc = rust_i18n::locale();
        Language::ALL.into_iter().find(|l| l.key() == &*loc).unwrap_or(Language::En)
    }
}

/// Switch the UI language and repaint every window.
pub fn set_language(lang: Language, cx: &mut App) {
    if Language::current() == lang {
        return;
    }
    rust_i18n::set_locale(lang.key());
    cx.refresh_windows();
    persist(cx);
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
    persist(cx);
}

/// Apply the saved settings (before the first window opens); returns
/// whether the dark theme was chosen.
pub fn restore(cx: &mut App) -> bool {
    let s = crate::core::storage::load_settings();
    let scale = UiScale::ALL
        .into_iter()
        .find(|u| u.key() == s.ui_scale)
        .unwrap_or(UiScale::Normal);
    cx.set_global(UiScaleGlobal(scale));
    cx.set_global(EditorFontSize(s.editor_font.clamp(EDITOR_FONT_MIN, EDITOR_FONT_MAX)));
    let lang = Language::ALL
        .into_iter()
        .find(|l| l.key() == s.language)
        .unwrap_or(Language::En);
    rust_i18n::set_locale(lang.key());
    s.dark
}

/// Write the current settings to `settings.json`.
pub fn persist(cx: &App) {
    use gpui_component::ActiveTheme as _;
    crate::core::storage::save_settings(&crate::core::storage::AppSettings {
        ui_scale: current(cx).key().into(),
        dark: cx.theme().is_dark(),
        editor_font: editor_font_size(cx),
        language: Language::current().key().into(),
    });
}

/// SQL editor font size in design px at the Normal interface scale
/// (rendered through `theme::scaled`); Ctrl+wheel over an editor changes
/// it for every SQL tab.
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
    persist(cx);
}

#[cfg(test)]
mod tests {
    /// Both languages are compiled in from locales/app.yml; an unknown
    /// locale falls back to English.
    #[test]
    fn locales_are_embedded() {
        assert_eq!(rust_i18n::t!("settings.title", locale = "ru"), "Настройки");
        assert_eq!(rust_i18n::t!("settings.title", locale = "en"), "Settings");
        assert_eq!(rust_i18n::t!("settings.title", locale = "de"), "Settings");
        assert_eq!(rust_i18n::t!("tree.tables", locale = "ru", n = 3), "Таблицы (3)");
    }

    /// Every `tr!("key")` / `t!("key")` under src/ui resolves in both
    /// languages — a typo or a forgotten key shows up as a raw key otherwise.
    #[test]
    fn every_ui_key_is_translated() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui"), &mut files);
        let mut missing = Vec::new();
        let mut seen = 0;
        for f in files {
            let src = std::fs::read_to_string(&f).unwrap();
            // Product code only (this test mentions the macros itself).
            let src = src.split("#[cfg(test)]").next().unwrap_or("");
            for marker in ["tr!(\"", "t!(\""] {
                for (ix, _) in src.match_indices(marker) {
                    // `format!("` also ends in `t!("`: a macro name starts
                    // after a non-identifier character.
                    let prev = src[..ix].chars().next_back().unwrap_or(' ');
                    if prev.is_alphanumeric() || prev == '_' {
                        continue;
                    }
                    let rest = &src[ix + marker.len()..];
                    let key = &rest[..rest.find('"').unwrap()];
                    seen += 1;
                    for loc in ["en", "ru"] {
                        if rust_i18n::t!(key, locale = loc) == key {
                            missing.push(format!("{loc}: {key} ({})", f.display()));
                        }
                    }
                }
            }
        }
        assert!(seen > 100, "only {seen} keys found — scanner broken?");
        assert!(missing.is_empty(), "untranslated keys:\n{}", missing.join("\n"));
    }
}
