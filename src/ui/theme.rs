//! DataBridge theme: dark and light palettes ported 1:1
//! from the web version's `style.css` (OKLCH tokens converted to sRGB).

use gpui::{px, rgb, App, Hsla, Window};
use gpui_component::{ActiveTheme, Theme, ThemeColor, ThemeMode};

/// The UI typeface for app chrome (buttons, menus, tabs, inputs).
pub const SANS_FONT: &str = "Public Sans";
/// The monospace typeface for data: grid cells, numbers, SQL-ish strings.
pub const MONO_FONT: &str = "JetBrains Mono";

/// App-specific palette beyond what gpui-component's `ThemeColor` covers.
/// Mirrors the CSS variables of the original web app.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    // base
    pub background: Hsla,
    pub foreground: Hsla,
    pub card: Hsla,
    pub primary: Hsla,
    pub primary_fg: Hsla,
    pub primary_hover: Hsla,
    pub secondary: Hsla,
    pub muted_fg: Hsla,
    pub accent: Hsla,
    pub accent_fg: Hsla,
    pub destructive: Hsla,
    pub destructive_hover: Hsla,
    pub border: Hsla,
    pub input: Hsla,
    // charts
    pub chart_1: Hsla,
    pub chart_2: Hsla,
    pub chart_3: Hsla,
    pub chart_4: Hsla,
    pub chart_5: Hsla,
    // app chrome surfaces
    pub toolbar: Hsla,
    pub toolbar_border: Hsla,
    pub active_tab: Hsla,
    pub inactive_tab: Hsla,
    pub tree_bg: Hsla,
    pub tree_hover: Hsla,
    pub tree_selected: Hsla,
    pub tree_fg: Hsla,
    pub table_header: Hsla,
    pub table_border: Hsla,
    pub table_row_alt: Hsla,
    pub table_selected: Hsla,
    pub statusbar: Hsla,
    pub statusbar_fg: Hsla,
    pub conn_ok: Hsla,
    pub conn_err: Hsla,
    pub keyword: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub comment: Hsla,
    // Type tokens for the data grid (DrawSQL-style colored kinds).
    pub date: Hsla,
    pub json: Hsla,
    pub uuid: Hsla,
    pub sidebar: Hsla,
    /// Multiplier for hand-tuned `px()` sizes (interface scale setting).
    /// Rem-based utilities scale via `theme.font_size` on their own.
    pub scale: f32,
}

fn p(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn dark_palette() -> Palette {
    let background = p(0x0E0F12);
    let foreground = p(0xE7E9EC);
    let card = p(0x181A1F);
    let primary = p(0x3E8BFF);
    let muted_fg = p(0x8B909A);
    let toolbar = p(0x14161A);
    let toolbar_border = p(0x25282E);
    Palette {
        background,
        foreground,
        card,
        primary,
        primary_fg: p(0xFFFFFF),
        primary_hover: p(0x5C9DFF),
        secondary: p(0x1F2126),
        muted_fg,
        accent: p(0x262930),
        accent_fg: p(0xE7E9EC),
        destructive: p(0xE5484D),
        destructive_hover: p(0xFF5C55),
        border: p(0x25282E),
        input: p(0x121317),
        chart_1: p(0x3E8BFF),
        chart_2: p(0x3FB950),
        chart_3: p(0xE8873C),
        chart_4: p(0xA371F7),
        chart_5: p(0xE5484D),
        toolbar,
        toolbar_border,
        active_tab: p(0x1F2126),
        inactive_tab: toolbar,
        tree_bg: toolbar,
        tree_hover: p(0x1D2025),
        tree_selected: primary,
        tree_fg: foreground,
        table_header: toolbar,
        table_border: p(0x1E2126),
        table_row_alt: p(0x101115),
        table_selected: primary,
        statusbar: p(0x14161A),
        statusbar_fg: muted_fg,
        conn_ok: p(0x3FB950),
        conn_err: p(0xFF6369),
        keyword: p(0x3FB950),
        string: p(0xE8873C),
        number: p(0xE3B341),
        comment: p(0x8B909A),
        date: p(0x58A6FF),
        json: p(0xA371F7),
        uuid: p(0x39C5CF),
        sidebar: p(0x14161A),
        scale: 1.0,
    }
}

pub fn light_palette() -> Palette {
    let background = p(0xF7F8FA);
    let foreground = p(0x1A2129);
    let toolbar = p(0xFFFFFF);
    let toolbar_border = p(0xE4E7EB);
    let primary = p(0x2E7CF6);
    Palette {
        background,
        foreground,
        card: p(0xFFFFFF),
        primary,
        primary_fg: p(0xFFFFFF),
        primary_hover: p(0x4A8EF8),
        secondary: p(0xF0F2F5),
        muted_fg: p(0x5C636E),
        accent: p(0xE9EEF5),
        accent_fg: p(0x1A2129),
        destructive: p(0xD93A49),
        destructive_hover: p(0xE5484D),
        border: p(0xE4E7EB),
        input: p(0xFFFFFF),
        chart_1: p(0x2E7CF6),
        chart_2: p(0x1A7F37),
        chart_3: p(0xC76A1F),
        chart_4: p(0x8250DF),
        chart_5: p(0xCF222E),
        toolbar,
        toolbar_border,
        active_tab: p(0xF0F2F5),
        inactive_tab: p(0xFFFFFF),
        tree_bg: p(0xFFFFFF),
        tree_hover: p(0xEEF2F6),
        tree_selected: primary,
        tree_fg: foreground,
        table_header: p(0xF6F8FA),
        table_border: p(0xEAECEF),
        table_row_alt: p(0xFBFCFD),
        table_selected: primary,
        statusbar: p(0xF0F2F5),
        statusbar_fg: p(0x5C636E),
        conn_ok: p(0x1A7F37),
        conn_err: p(0xD1242F),
        keyword: p(0x1A7F37),
        string: p(0xBF4A0A),
        number: p(0x9A6700),
        comment: p(0x5C636E),
        date: p(0x0A62D0),
        json: p(0x8250DF),
        uuid: p(0x0E7C8C),
        sidebar: p(0xFCFCFD),
        scale: 1.0,
    }
}

/// The palette for the currently active theme mode.
pub fn palette(cx: &App) -> Palette {
    let mut pal = if cx.theme().is_dark() {
        dark_palette()
    } else {
        light_palette()
    };
    pal.scale = crate::ui::settings::current(cx).factor();
    pal
}

/// Token color for a column kind — shared by the grid header, grid cells
/// and the schema-diagram cards.
pub fn kind_color(kind: &crate::core::model::ColumnKind, p: &Palette) -> Hsla {
    use crate::core::model::ColumnKind as K;
    match kind {
        K::Int | K::Float => p.number,
        K::Text => p.string,
        K::Date => p.date,
        K::Json => p.json,
        K::Uuid => p.uuid,
        K::Binary => p.comment,
        K::Bool => p.keyword,
    }
}

/// Re-apply the current theme mode with the active interface scale.
pub fn reapply(window: Option<&mut Window>, cx: &mut App) {
    let mode = if cx.theme().is_dark() {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    apply(mode, window, cx);
}

/// Overlay the DataBridge palette onto gpui-component's ThemeColor.
fn theme_colors(base: ThemeColor, pal: &Palette) -> ThemeColor {
    let mut c = base;
    c.background = pal.background;
    c.foreground = pal.foreground;
    c.popover = pal.card;
    c.popover_foreground = pal.foreground;
    c.list = pal.card;
    c.list_hover = pal.tree_hover;
    c.list_active = pal.tree_selected;
    c.list_active_border = pal.tree_selected;
    c.list_head = pal.table_header;
    c.list_even = pal.table_row_alt;
    c.primary = pal.primary;
    c.primary_foreground = pal.primary_fg;
    c.primary_hover = pal.primary_hover;
    c.primary_active = pal.primary_hover;
    c.secondary = pal.secondary;
    c.secondary_foreground = pal.foreground;
    c.secondary_hover = pal.tree_hover;
    c.secondary_active = pal.tree_hover;
    c.muted = pal.secondary;
    c.muted_foreground = pal.muted_fg;
    c.accent = pal.accent;
    c.accent_foreground = pal.foreground;
    c.danger = pal.destructive;
    c.danger_foreground = p(0xFFFFFF);
    c.danger_hover = pal.destructive_hover;
    c.danger_active = pal.destructive_hover;
    c.border = pal.border;
    c.input = pal.input;
    c.ring = pal.primary;
    c.caret = pal.primary;
    c.selection = pal.primary.opacity(0.35);
    c.scrollbar = pal.background;
    c.scrollbar_thumb = pal.toolbar_border;
    c.scrollbar_thumb_hover = pal.muted_fg;
    c.sidebar = pal.sidebar;
    c.sidebar_foreground = pal.foreground;
    c.sidebar_border = pal.border;
    c.sidebar_accent = pal.accent;
    c.sidebar_accent_foreground = pal.foreground;
    c.tab = pal.inactive_tab;
    c.tab_active = pal.active_tab;
    c.tab_bar = pal.inactive_tab;
    c.tab_foreground = pal.muted_fg;
    c.tab_active_foreground = pal.foreground;
    c.table = pal.background;
    c.table_head = pal.table_header;
    c.table_head_foreground = pal.muted_fg;
    c.table_even = pal.table_row_alt;
    // The crate paints the selected row as an overlay ON TOP of the cells, so
    // its bg must be invisible — the solid row background is drawn by our
    // `render_tr` instead. Only the outline stays.
    c.table_active = p(0x000000).opacity(0.0);
    c.table_active_border = pal.primary.opacity(0.55);
    c.table_hover = pal.tree_hover;
    c.table_row_border = pal.table_border;
    c.title_bar = pal.toolbar;
    c.title_bar_border = pal.toolbar_border;
    c.chart_1 = pal.chart_1;
    c.chart_2 = pal.chart_2;
    c.chart_3 = pal.chart_3;
    c.chart_4 = pal.chart_4;
    c.chart_5 = pal.chart_5;
    c.success = pal.conn_ok;
    c.success_foreground = p(0xFFFFFF);
    c.warning = pal.number;
    c.warning_foreground = p(0xFFFFFF);
    c.link = pal.primary;
    c.link_hover = pal.primary_hover;
    c.link_active = pal.primary_hover;
    c.overlay = p(0x000000).opacity(0.5);
    c.window_border = pal.border;
    c
}

/// Switch theme mode and re-apply the DataBridge palette.
pub fn apply(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    Theme::change(mode, None, cx);
    let pal = if mode.is_dark() {
        dark_palette()
    } else {
        light_palette()
    };
    let base = if mode.is_dark() {
        *ThemeColor::dark()
    } else {
        *ThemeColor::light()
    };
    let font_size = crate::ui::settings::font_size(cx);
    let theme = Theme::global_mut(cx);
    theme.colors = theme_colors(base, &pal);
    theme.font_family = SANS_FONT.into();
    theme.font_size = font_size;
    theme.mono_font_family = MONO_FONT.into();
    theme.mono_font_size = font_size;
    theme.radius = px(6.0);
    theme.radius_lg = px(10.0);
    theme.shadow = true;
    if let Some(window) = window {
        window.refresh();
    }
}

/// Toggle between light and dark.
pub fn toggle(window: &mut Window, cx: &mut App) {
    let next = if cx.theme().is_dark() {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    apply(next, Some(window), cx);
}
