//! Charts of a result set (pure: columns + rows in, chart data out; the
//! query tab draws it with gpui-component's charts, `ui/query.rs`).
//!
//! [`suggest`] picks a sensible first chart the way DataGrip / Metabase
//! do: the first date-like column (else the first non-numeric one, else
//! the first column) is the X axis, up to [`MAX_SERIES`] numeric columns
//! are the series; dates → line, categories → bar. [`build`] turns a
//! spec into points: repeated X values are aggregated (sum / avg / min /
//! max / count), date and number axes sort ascending, categories keep
//! first-seen order — bar and pie categories are ranked by the first
//! series and cut at [`MAX_CATEGORIES`] (the rest folded into "other").
//! Numbers in text cells (ClickHouse `UInt64`/`Decimal` strings, SQLite
//! text) count; NULL and non-numeric cells are skipped.

use std::collections::HashMap;

use crate::core::model::{ColumnKind, ColumnMeta, Row, Value};

/// Series drawn at most (line / area; bar and pie draw the first).
pub const MAX_SERIES: usize = 5;
/// Bars / pie slices shown before the rest folds into "other".
pub const MAX_CATEGORIES: usize = 30;
/// Points on a line / area chart (longer results keep the first ones).
pub const MAX_POINTS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Bar,
    Line,
    Area,
    Pie,
}

impl ChartKind {
    pub const ALL: [ChartKind; 4] = [ChartKind::Bar, ChartKind::Line, ChartKind::Area, ChartKind::Pie];

    /// One series only (the first Y).
    pub fn single_series(self) -> bool {
        matches!(self, ChartKind::Bar | ChartKind::Pie)
    }
}

/// How rows sharing an X value combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Sum,
    Avg,
    Min,
    Max,
    /// Rows per X value (the Y columns are ignored).
    Count,
}

impl Agg {
    pub const ALL: [Agg; 5] = [Agg::Sum, Agg::Avg, Agg::Min, Agg::Max, Agg::Count];
}

/// What to draw: column indexes into the result set.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSpec {
    pub kind: ChartKind,
    pub x: usize,
    pub ys: Vec<usize>,
    pub agg: Agg,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    pub values: Vec<f64>,
}

/// Ready-to-draw data: one label per point, series parallel to them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChartData {
    pub labels: Vec<String>,
    pub series: Vec<Series>,
    /// Points dropped by the caps (categories folded / tail cut).
    pub clipped: bool,
}

impl ChartData {
    pub fn is_empty(&self) -> bool {
        self.labels.is_empty() || self.series.is_empty()
    }
}

/// A cell as a number, when it is one.
pub fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        Value::Bool(b) => Some(if *b { 1. } else { 0. }),
        Value::Str(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// Numeric column: typed so, or (untyped results) most non-null cells
/// parse as numbers.
pub fn is_numeric(ix: usize, columns: &[ColumnMeta], rows: &[Row]) -> bool {
    if let Some(c) = columns.get(ix) {
        match c.kind {
            ColumnKind::Int | ColumnKind::Float => return true,
            ColumnKind::Date | ColumnKind::Uuid | ColumnKind::Json | ColumnKind::Binary
            | ColumnKind::Bool => return false,
            ColumnKind::Text => {}
        }
    }
    let (mut seen, mut nums) = (0usize, 0usize);
    for r in rows.iter().take(200) {
        match r.get(ix) {
            None | Some(Value::Null) => {}
            Some(v) => {
                seen += 1;
                if !matches!(v, Value::Bool(_)) && number(v).is_some() {
                    nums += 1;
                }
            }
        }
    }
    seen > 0 && nums * 10 >= seen * 9
}

fn is_date(ix: usize, columns: &[ColumnMeta], rows: &[Row]) -> bool {
    if columns.get(ix).is_some_and(|c| c.kind == ColumnKind::Date) {
        return true;
    }
    // Text dates (SQLite, mongosh results): `2024-01-31…`.
    let looks = |s: &str| {
        let b = s.as_bytes();
        b.len() >= 10 && b[4] == b'-' && b[7] == b'-' && b[..4].iter().all(u8::is_ascii_digit)
    };
    let mut seen = 0;
    for r in rows.iter().take(50) {
        match r.get(ix) {
            Some(Value::Str(s)) if looks(s) => seen += 1,
            None | Some(Value::Null) => {}
            _ => return false,
        }
    }
    seen > 0
}

/// The first chart for a result set; `None` when nothing is plottable
/// (no rows, or no numeric column and only one column for counting).
pub fn suggest(columns: &[ColumnMeta], rows: &[Row]) -> Option<ChartSpec> {
    let n = columns.len();
    if n == 0 || rows.is_empty() {
        return None;
    }
    let numeric: Vec<usize> = (0..n).filter(|&i| is_numeric(i, columns, rows)).collect();
    let date = (0..n).find(|&i| is_date(i, columns, rows));
    let category = (0..n).find(|i| !numeric.contains(i));
    let x = date.or(category).unwrap_or(0);
    let ys: Vec<usize> = numeric.iter().copied().filter(|&i| i != x).take(MAX_SERIES).collect();
    let kind = if date == Some(x) { ChartKind::Line } else { ChartKind::Bar };
    if ys.is_empty() {
        // Nothing to sum: count rows per X instead (`SELECT status FROM …`)
        // — unless there is a single row (a status message, `SELECT version()`).
        if rows.len() < 2 {
            return None;
        }
        return Some(ChartSpec { kind: ChartKind::Bar, x, ys: Vec::new(), agg: Agg::Count });
    }
    Some(ChartSpec { kind, x, ys, agg: Agg::Sum })
}

struct Acc {
    sum: f64,
    min: f64,
    max: f64,
    n: usize,
}

impl Acc {
    fn new() -> Self {
        Acc { sum: 0., min: f64::INFINITY, max: f64::NEG_INFINITY, n: 0 }
    }
    fn add(&mut self, v: f64) {
        self.sum += v;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.n += 1;
    }
    fn get(&self, agg: Agg, rows: usize) -> f64 {
        match agg {
            Agg::Sum => self.sum,
            Agg::Avg if self.n > 0 => self.sum / self.n as f64,
            Agg::Min if self.n > 0 => self.min,
            Agg::Max if self.n > 0 => self.max,
            Agg::Count => rows as f64,
            _ => 0.,
        }
    }
}

/// Points for `spec`. `other` labels the folded tail of a bar / pie.
pub fn build(spec: &ChartSpec, columns: &[ColumnMeta], rows: &[Row], other: &str) -> ChartData {
    let ys: Vec<usize> = if spec.agg == Agg::Count {
        Vec::new()
    } else if spec.kind.single_series() {
        spec.ys.iter().copied().take(1).collect()
    } else {
        spec.ys.iter().copied().take(MAX_SERIES).collect()
    };
    if spec.agg != Agg::Count && ys.is_empty() {
        return ChartData::default();
    }

    // Timestamps bucket by day (by month past `MAX_DAYS` days), like a BI
    // tool would — raw timestamps are all distinct.
    let x_is_date = is_date(spec.x, columns, rows);
    let bucket = if x_is_date { date_bucket(spec.x, rows) } else { usize::MAX };

    // Group by the X cell's text, first-seen order.
    let mut order: Vec<(String, Option<f64>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut accs: Vec<(usize, Vec<Acc>)> = Vec::new(); // (row count, per series)
    for r in rows {
        let xv = r.get(spec.x).unwrap_or(&Value::Null);
        let label = match xv {
            Value::Null => "null".to_string(),
            v => {
                let mut s = v.edit_string();
                if s.len() > bucket && s.is_char_boundary(bucket) {
                    s.truncate(bucket);
                }
                s
            }
        };
        let slot = *index.entry(label.clone()).or_insert_with(|| {
            order.push((label, number(xv)));
            accs.push((0, (0..ys.len().max(1)).map(|_| Acc::new()).collect()));
            order.len() - 1
        });
        let (count, series) = &mut accs[slot];
        *count += 1;
        for (s, &col) in ys.iter().enumerate() {
            if let Some(v) = r.get(col).and_then(number) {
                series[s].add(v);
            }
        }
    }

    let names: Vec<String> = if spec.agg == Agg::Count {
        vec!["count".into()]
    } else {
        ys.iter()
            .map(|&i| columns.get(i).map(|c| c.name.clone()).unwrap_or_default())
            .collect()
    };
    let mut points: Vec<(String, Option<f64>, Vec<f64>)> = order
        .into_iter()
        .zip(accs)
        .map(|((label, xnum), (count, series))| {
            let vals = series.iter().take(names.len()).map(|a| a.get(spec.agg, count)).collect();
            (label, xnum, vals)
        })
        .collect();

    let mut clipped = false;
    let x_is_num = is_numeric(spec.x, columns, rows);
    if spec.kind.single_series() {
        if x_is_date || (x_is_num && spec.kind == ChartKind::Bar) {
            sort_axis(&mut points, x_is_num);
        } else {
            // Categories: biggest first.
            points.sort_by(|a, b| b.2[0].total_cmp(&a.2[0]));
        }
        if points.len() > MAX_CATEGORIES {
            clipped = true;
            let tail: Vec<_> = points.split_off(MAX_CATEGORIES - 1);
            let mut folded = vec![0.; names.len()];
            for (_, _, vals) in &tail {
                for (f, v) in folded.iter_mut().zip(vals) {
                    *f += v;
                }
            }
            points.push((other.to_string(), None, folded));
        }
    } else {
        if x_is_date || x_is_num {
            sort_axis(&mut points, x_is_num);
        }
        if points.len() > MAX_POINTS {
            clipped = true;
            points.truncate(MAX_POINTS);
        }
    }

    let labels = points.iter().map(|p| p.0.clone()).collect();
    let series = names
        .into_iter()
        .enumerate()
        .map(|(s, name)| Series { name, values: points.iter().map(|p| p.2[s]).collect() })
        .collect();
    ChartData { labels, series, clipped }
}

/// Distinct days on a date axis above which it buckets by month.
pub const MAX_DAYS: usize = 120;

/// Prefix length of an ISO date label to group by: the day (`YYYY-MM-DD`,
/// 10) or, for long ranges, the month (`YYYY-MM`, 7).
fn date_bucket(x: usize, rows: &[Row]) -> usize {
    let mut days = std::collections::HashSet::new();
    for r in rows {
        if let Some(Value::Str(s)) = r.get(x) {
            if let Some(day) = s.get(..10) {
                days.insert(day);
            }
        }
    }
    if days.len() > MAX_DAYS { 7 } else { 10 }
}

fn sort_axis(points: &mut [(String, Option<f64>, Vec<f64>)], numeric: bool) {
    if numeric {
        points.sort_by(|a, b| a.1.unwrap_or(f64::MAX).total_cmp(&b.1.unwrap_or(f64::MAX)));
    } else {
        // ISO dates sort as text.
        points.sort_by(|a, b| a.0.cmp(&b.0));
    }
}

/// Compact axis / tooltip number: `1.2K`, `3.4M`, `0.25`.
pub fn short_number(v: f64) -> String {
    let a = v.abs();
    let (n, suffix) = if a >= 1e9 {
        (v / 1e9, "B")
    } else if a >= 1e6 {
        (v / 1e6, "M")
    } else if a >= 1e4 {
        (v / 1e3, "K")
    } else {
        (v, "")
    };
    let s = if n.fract() == 0. && suffix.is_empty() {
        format!("{n:.0}")
    } else if n.abs() >= 100. {
        format!("{n:.0}")
    } else {
        format!("{n:.2}").trim_end_matches('0').trim_end_matches('.').to_string()
    };
    format!("{s}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, kind: ColumnKind) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_: String::new(),
            nullable: true,
            primary_key: false,
            auto_increment: false,
            has_default: false,
            kind,
        }
    }
    fn s(v: &str) -> Value {
        Value::Str(v.into())
    }

    #[test]
    fn suggests_line_for_dates_and_bar_for_categories() {
        let cols = [col("day", ColumnKind::Date), col("orders", ColumnKind::Int), col("revenue", ColumnKind::Float)];
        let rows = vec![vec![s("2024-01-02"), Value::Int(3), Value::Float(9.5)]];
        let spec = suggest(&cols, &rows).unwrap();
        assert_eq!((spec.kind, spec.x, spec.ys.clone(), spec.agg), (ChartKind::Line, 0, vec![1, 2], Agg::Sum));

        // Untyped text columns (SQLite / ClickHouse strings).
        let cols = [col("country", ColumnKind::Text), col("n", ColumnKind::Text)];
        let rows = vec![vec![s("DE"), s("18446744073709551615")], vec![s("FR"), s("2")]];
        let spec = suggest(&cols, &rows).unwrap();
        assert_eq!((spec.kind, spec.x, spec.ys), (ChartKind::Bar, 0, vec![1]));

        // Nothing numeric: count rows per value — not for a lone row.
        let rows = vec![vec![s("a"), s("x")], vec![s("a"), s("y")]];
        assert_eq!(suggest(&cols, &rows).unwrap().agg, Agg::Count);
        assert!(suggest(&cols, &rows[..1]).is_none());
        assert!(suggest(&cols, &[]).is_none());
    }

    #[test]
    fn aggregates_sorts_and_folds() {
        let cols = [col("day", ColumnKind::Date), col("v", ColumnKind::Int)];
        let rows = vec![
            vec![s("2024-01-03"), Value::Int(1)],
            vec![s("2024-01-01"), Value::Int(2)],
            vec![s("2024-01-03"), Value::Int(5)],
            vec![s("2024-01-02"), Value::Null],
        ];
        let line = |agg| build(&ChartSpec { kind: ChartKind::Line, x: 0, ys: vec![1], agg }, &cols, &rows, "other");
        let d = line(Agg::Sum);
        assert_eq!(d.labels, ["2024-01-01", "2024-01-02", "2024-01-03"]);
        assert_eq!(d.series[0].values, [2., 0., 6.]);
        assert_eq!(line(Agg::Avg).series[0].values, [2., 0., 3.]);
        assert_eq!(line(Agg::Max).series[0].values, [2., 0., 5.]);
        let c = line(Agg::Count);
        assert_eq!((c.series[0].name.as_str(), c.series[0].values.clone()), ("count", vec![1., 1., 2.]));

        // Categories: ranked by value, the tail folded into "other".
        let cols = [col("k", ColumnKind::Text), col("v", ColumnKind::Int)];
        let rows: Vec<Row> = (0..40).map(|i| vec![s(&format!("k{i}")), Value::Int(i)]).collect();
        let d = build(&ChartSpec { kind: ChartKind::Pie, x: 0, ys: vec![1], agg: Agg::Sum }, &cols, &rows, "other");
        assert_eq!(d.labels.len(), MAX_CATEGORIES);
        assert_eq!(d.labels[0], "k39");
        assert_eq!(d.labels.last().unwrap(), "other");
        assert_eq!(d.series[0].values.iter().sum::<f64>(), (0..40).sum::<i64>() as f64);
        assert!(d.clipped);
    }

    #[test]
    fn timestamps_bucket_by_day_then_month() {
        let cols = [col("ts", ColumnKind::Date), col("v", ColumnKind::Int)];
        let spec = ChartSpec { kind: ChartKind::Line, x: 0, ys: vec![1], agg: Agg::Sum };
        let rows = vec![
            vec![s("2024-01-02 10:00:00"), Value::Int(1)],
            vec![s("2024-01-02 18:30:00"), Value::Int(2)],
            vec![s("2024-01-01 09:00:00"), Value::Int(4)],
        ];
        let d = build(&spec, &cols, &rows, "other");
        assert_eq!(d.labels, ["2024-01-01", "2024-01-02"]);
        assert_eq!(d.series[0].values, [4., 3.]);

        // A year of days → months.
        let rows: Vec<Row> = (0..200)
            .map(|i| {
                let (m, d) = (1 + i / 28, 1 + i % 28);
                vec![s(&format!("2024-{m:02}-{d:02} 12:00:00")), Value::Int(1)]
            })
            .collect();
        let d = build(&spec, &cols, &rows, "other");
        assert_eq!(d.labels.first().map(String::as_str), Some("2024-01"));
        assert_eq!(d.labels.len(), 8);
    }

    #[test]
    fn short_numbers() {
        assert_eq!(short_number(0.25), "0.25");
        assert_eq!(short_number(42.), "42");
        assert_eq!(short_number(12_500.), "12.5K");
        assert_eq!(short_number(-3_400_000.), "-3.4M");
        assert_eq!(short_number(2e9), "2B");
    }
}
