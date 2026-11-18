//! Draws a `core::chart::ChartData` with gpui-component's charts (bar,
//! line, area, pie). Colors come from the palette's `chart_1..5`; the
//! data arrives already aggregated and capped, so this is presentation
//! only. Used by the SQL tab's results pane (`ui/query.rs`).

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, Hsla, SharedString, div, px};
use gpui_component::chart::{AreaChart, BarChart, LineChart, PieChart};
use gpui_component::h_flex;

use crate::core::chart::{ChartData, ChartKind, short_number};
use crate::ui::theme;

/// One point: its label, every series' value, and its position (pie colors).
#[derive(Clone)]
struct Pt {
    label: SharedString,
    vals: Rc<[f64]>,
    ix: usize,
}

pub fn series_colors(cx: &App) -> [Hsla; 5] {
    let p = theme::palette(cx);
    [p.chart_1, p.chart_2, p.chart_3, p.chart_4, p.chart_5]
}

/// The chart itself (fills its parent). `key` replays the draw-in
/// animation when the data changes.
pub fn chart_element(data: &ChartData, kind: ChartKind, key: u64, cx: &App) -> AnyElement {
    let colors = series_colors(cx);
    let pts: Vec<Pt> = data
        .labels
        .iter()
        .enumerate()
        .map(|(i, l)| Pt {
            label: l.clone().into(),
            vals: data.series.iter().map(|s| s.values.get(i).copied().unwrap_or(0.)).collect(),
            ix: i,
        })
        .collect();
    let n = pts.len();
    // Thin out X labels on long axes (≈ 12 visible).
    let tick_margin = (n / 12).max(1);
    let fmt = |v: f64| SharedString::from(short_number(v));

    let chart = match kind {
        ChartKind::Bar => {
            let c = colors[0];
            BarChart::new(pts)
                .appear_key(key)
                .band(|p: &Pt| p.label.clone())
                .value(|p: &Pt| p.vals[0])
                .fill(move |_, _, _, _| c)
                // A handful of bars must not turn into slabs.
                .max_band_width(px(56.))
                .tick_margin(tick_margin)
                .value_tick_format(fmt)
                .tooltip_title(|p: &Pt| p.label.clone())
                .tooltip_value(|_, v| short_number(v).into())
                .into_any_element()
        }
        ChartKind::Line => {
            let mut chart = LineChart::new(pts)
                .appear_key(key)
                .x(|p: &Pt| p.label.clone())
                .tick_margin(tick_margin)
                .y_tick_format(fmt)
                .tooltip_title(|p: &Pt| p.label.clone());
            for (s, series) in data.series.iter().enumerate() {
                chart = chart
                    .y(move |p: &Pt| p.vals[s])
                    .stroke(colors[s % colors.len()])
                    .name(series.name.clone());
            }
            if n <= 60 {
                chart = chart.dot();
            }
            chart.into_any_element()
        }
        ChartKind::Area => {
            let mut chart = AreaChart::new(pts)
                .appear_key(key)
                .x(|p: &Pt| p.label.clone())
                .tick_margin(tick_margin)
                .y_tick_format(fmt)
                .tooltip_title(|p: &Pt| p.label.clone());
            for (s, series) in data.series.iter().enumerate() {
                let c = colors[s % colors.len()];
                chart = chart
                    .y(move |p: &Pt| p.vals[s])
                    .stroke(c)
                    .fill(c.opacity(0.25))
                    .name(series.name.clone());
            }
            chart.into_any_element()
        }
        ChartKind::Pie => PieChart::new(pts)
            .appear_key(key)
            .value(|p: &Pt| p.vals[0].max(0.) as f32)
            .color(move |p: &Pt| colors[p.ix % colors.len()])
            .inner_radius_ratio(0.55)
            .pad_angle(0.01)
            .label(|p: &Pt| p.label.clone())
            .tooltip_name(|p: &Pt| p.label.clone())
            .tooltip_value(|_, v, _| short_number(v as f64).into())
            .into_any_element(),
    };
    div().size_full().p_3().child(chart).into_any_element()
}

/// Series legend (line / area with more than one series).
pub fn legend(data: &ChartData, cx: &App) -> AnyElement {
    let p = theme::palette(cx);
    let colors = series_colors(cx);
    let mut row = h_flex().gap_3().flex_wrap();
    for (s, series) in data.series.iter().enumerate() {
        row = row.child(
            h_flex()
                .gap_1()
                .items_center()
                .child(div().size(px(8.)).rounded_full().bg(colors[s % colors.len()]))
                .child(
                    div()
                        .text_size(theme::scaled(theme::TEXT_XS))
                        .text_color(p.muted_fg)
                        .font_family(theme::MONO_FONT)
                        .child(series.name.clone()),
                ),
        );
    }
    row.into_any_element()
}
