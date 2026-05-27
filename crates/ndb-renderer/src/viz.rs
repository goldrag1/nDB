//! N-dimensional visualization renderers (v2.1 §2.10–§2.12).
#![allow(
    clippy::format_push_string,     // out.push_str(&format!(...)) reads cleanly for SVG/HTML assembly
    clippy::too_many_lines,         // SVG-emitting fns naturally have many lines; splitting hurts readability
    clippy::cast_precision_loss,    // f64 axis math knowingly widens i64/usize → f64
)]
//!
//! Each renderer emits **one self-contained HTML file** — inline CSS,
//! inline SVG, inline JS for tooltips, zero external assets. Open in
//! any browser. Email a teammate. Embed in a doc. No build step.
//!
//! Three renderers:
//! - [`render_pivot`] — 4-5 dimensional data via row/column compound
//!   labels (Excel-style pivot, no JS).
//! - [`render_parallel_coords`] — 5-20 dims as polylines crossing N
//!   vertical axes (SVG + hover JS).
//! - [`render_hypergraph`] — entities as nodes, each hyperedge as a
//!   polygon/starburst connecting its N role-fillers (SVG + hover JS,
//!   deterministic Fruchterman-Reingold layout).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use ndb_engine::value::Value;
use ndb_slicer::{Aggregate, Table};

use crate::{format_cell, push_html_escaped};

// ---------------------------------------------------------------------------
// §2.10 Pivot table — N-dim row + N-dim col compound labels
// ---------------------------------------------------------------------------

/// Render `t` as a pivot table: every distinct tuple of values across
/// `rows` becomes a row label; every distinct tuple across `cols`
/// becomes a column label; the cell at the intersection holds the
/// aggregate of `value` for the rows matching that (row-key, col-key)
/// pair.
///
/// Output is HTML — `<table>` with `<thead>` carrying the column
/// header band and `<tbody>` carrying row label bands + aggregated
/// cells. No CSS, no JS. Cells with no matching input rows render as
/// `&nbsp;` to keep the grid visually consistent — distinguishable
/// from "rows that exist but sum to 0".
///
/// Multi-dimensional headers use compound labels joined with ` / ` —
/// e.g. `2024 / Q1` for a 2-dim column group. v2.2+ may add native
/// rowspan/colspan nesting.
///
/// Panics if any index in `rows`, `cols`, or `value` is out of bounds
/// for the table's header set.
#[must_use]
pub fn render_pivot(
    t: &Table,
    rows: &[usize],
    cols: &[usize],
    value: usize,
    agg: Aggregate,
) -> String {
    // Bucket: (row_key, col_key) → Vec<Value> (the value-column entries).
    type Bucket = BTreeMap<(Vec<String>, Vec<String>), Vec<Vec<Value>>>;
    let mut buckets: Bucket = BTreeMap::new();
    let mut row_keys_set: std::collections::BTreeSet<Vec<String>> =
        std::collections::BTreeSet::new();
    let mut col_keys_set: std::collections::BTreeSet<Vec<String>> =
        std::collections::BTreeSet::new();

    for row in &t.rows {
        let row_key: Vec<String> = rows.iter().map(|&i| format_cell(&row[i])).collect();
        let col_key: Vec<String> = cols.iter().map(|&i| format_cell(&row[i])).collect();
        row_keys_set.insert(row_key.clone());
        col_keys_set.insert(col_key.clone());
        // Wrap the value cell in a 1-element row so `compute_aggregate`
        // (re-exported via the public Slicer surface) can fold it.
        buckets
            .entry((row_key, col_key))
            .or_default()
            .push(vec![row[value].clone()]);
    }

    let row_keys: Vec<Vec<String>> = row_keys_set.into_iter().collect();
    let col_keys: Vec<Vec<String>> = col_keys_set.into_iter().collect();

    let mut out = String::new();
    out.push_str("<table>\n");

    // Header band:
    //   <thead>
    //     <tr><th>row dim 1</th>...<th>col dim joined</th>...</tr>
    //   </thead>
    out.push_str("<thead><tr>");
    let row_header_titles: Vec<String> = rows.iter().map(|&i| t.headers[i].clone()).collect();
    for h in &row_header_titles {
        out.push_str("<th>");
        push_html_escaped(&mut out, h);
        out.push_str("</th>");
    }
    for col_key in &col_keys {
        let label = col_key.join(" / ");
        out.push_str("<th>");
        push_html_escaped(&mut out, &label);
        out.push_str("</th>");
    }
    out.push_str("</tr></thead>\n");

    // Body
    out.push_str("<tbody>\n");
    for row_key in &row_keys {
        out.push_str("<tr>");
        for part in row_key {
            out.push_str("<th>");
            push_html_escaped(&mut out, part);
            out.push_str("</th>");
        }
        for col_key in &col_keys {
            out.push_str("<td>");
            match buckets.get(&(row_key.clone(), col_key.clone())) {
                Some(rows_in_bucket) => {
                    // compute_aggregate lives in ndb-slicer as private; we
                    // fold inline so we don't widen the public API surface.
                    let v = fold_aggregate(agg, rows_in_bucket);
                    push_html_escaped(&mut out, &format_cell(&v));
                }
                None => out.push_str("&nbsp;"),
            }
            out.push_str("</td>");
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody>\n");
    out.push_str("</table>\n");
    out
}

/// Fold an aggregate over a per-bucket Vec of single-value rows.
/// Re-implements the Sum/Avg/Min/Max/Count branch from `ndb-slicer`
/// to avoid exposing the slicer's private `compute_aggregate`. Pivot's
/// per-cell budget is small (typically <1000 rows), so a fresh loop
/// per cell is fine.
fn fold_aggregate(agg: Aggregate, rows: &[Vec<Value>]) -> Value {
    match agg {
        Aggregate::Count => {
            let n = rows
                .iter()
                .filter(|r| !matches!(r[0], Value::Null))
                .count();
            Value::I64(i64::try_from(n).unwrap_or(i64::MAX))
        }
        Aggregate::Sum => fold_sum(rows),
        Aggregate::Avg => fold_avg(rows),
        Aggregate::Min => fold_minmax(rows, true),
        Aggregate::Max => fold_minmax(rows, false),
        Aggregate::Percentile { p } => fold_percentile(rows, p),
    }
}

fn fold_sum(rows: &[Vec<Value>]) -> Value {
    let mut acc_i: i64 = 0;
    let mut acc_f: f64 = 0.0;
    let mut as_float = false;
    let mut any = false;
    for r in rows {
        match r[0] {
            Value::I64(n) => {
                any = true;
                if as_float {
                    #[allow(clippy::cast_precision_loss)]
                    let n_f = n as f64;
                    acc_f += n_f;
                } else {
                    acc_i = acc_i.saturating_add(n);
                }
            }
            Value::F64(f) => {
                any = true;
                if !as_float {
                    #[allow(clippy::cast_precision_loss)]
                    let i_f = acc_i as f64;
                    acc_f = i_f;
                    as_float = true;
                }
                acc_f += f;
            }
            _ => {}
        }
    }
    if !any {
        Value::Null
    } else if as_float {
        Value::F64(acc_f)
    } else {
        Value::I64(acc_i)
    }
}

fn fold_avg(rows: &[Vec<Value>]) -> Value {
    let mut n: i64 = 0;
    let mut acc: f64 = 0.0;
    for r in rows {
        match r[0] {
            Value::I64(v) => {
                #[allow(clippy::cast_precision_loss)]
                let v_f = v as f64;
                acc += v_f;
                n += 1;
            }
            Value::F64(v) => {
                acc += v;
                n += 1;
            }
            _ => {}
        }
    }
    if n == 0 {
        Value::Null
    } else {
        #[allow(clippy::cast_precision_loss)]
        let n_f = n as f64;
        Value::F64(acc / n_f)
    }
}

fn fold_minmax(rows: &[Vec<Value>], want_min: bool) -> Value {
    let mut best: Option<Value> = None;
    for r in rows {
        let v = &r[0];
        if matches!(v, Value::Null) {
            continue;
        }
        best = Some(match &best {
            None => v.clone(),
            Some(prev) => match (prev, v) {
                (Value::I64(a), Value::I64(b)) => {
                    if (want_min && b < a) || (!want_min && b > a) {
                        v.clone()
                    } else {
                        prev.clone()
                    }
                }
                (Value::F64(a), Value::F64(b)) => {
                    let take = if want_min { b < a } else { b > a };
                    if take { v.clone() } else { prev.clone() }
                }
                (Value::String(a), Value::String(b)) => {
                    let take = if want_min { b < a } else { b > a };
                    if take { v.clone() } else { prev.clone() }
                }
                _ => prev.clone(),
            },
        });
    }
    best.unwrap_or(Value::Null)
}

fn fold_percentile(rows: &[Vec<Value>], p: f64) -> Value {
    if !(p > 0.0 && p <= 1.0) {
        return Value::Null;
    }
    let mut xs: Vec<f64> = Vec::with_capacity(rows.len());
    for r in rows {
        match r[0] {
            Value::I64(n) => {
                #[allow(clippy::cast_precision_loss)]
                xs.push(n as f64);
            }
            Value::F64(f) => xs.push(f),
            Value::Timestamp(t) => {
                #[allow(clippy::cast_precision_loss)]
                xs.push(t as f64);
            }
            _ => {}
        }
    }
    if xs.is_empty() {
        return Value::Null;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n == 1 {
        return Value::F64(xs[0]);
    }
    #[allow(clippy::cast_precision_loss)]
    let h = p * (n as f64 - 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = h.floor() as usize;
    let hi = lo + 1;
    if hi >= n {
        return Value::F64(xs[n - 1]);
    }
    let frac = h - h.floor();
    Value::F64(xs[lo] + (xs[hi] - xs[lo]) * frac)
}

// ---------------------------------------------------------------------------
// §2.11 Parallel coordinates — N-axis polyline visualization
// ---------------------------------------------------------------------------

/// Options for [`render_parallel_coords`].
#[derive(Debug, Clone)]
pub struct ParallelCoordsOpts {
    /// Overall SVG canvas width in pixels.
    pub width: u32,
    /// Overall SVG canvas height in pixels.
    pub height: u32,
    /// Columns to use as axes, in left-to-right order. Numeric columns
    /// scale linearly between observed (min, max); categorical columns
    /// use ordinal alphabetical positions.
    pub axis_cols: Vec<usize>,
    /// Optional column to colour-code polylines by. Numeric → viridis
    /// gradient; categorical → 10-colour palette.
    pub color_by: Option<usize>,
    /// Optional title displayed above the chart.
    pub title: Option<String>,
}

impl Default for ParallelCoordsOpts {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 600,
            axis_cols: Vec::new(),
            color_by: None,
            title: None,
        }
    }
}

/// Render `t` as a parallel-coordinates SVG embedded in a
/// self-contained HTML document. One vertical axis per
/// `opts.axis_cols` entry; each row becomes a polyline crossing
/// every axis at its normalised position.
///
/// Numeric axes scale linearly between observed `(min, max)`.
/// Categorical (String/Bool/EntityRef/...) axes use alphabetical
/// ordinal positions. Null on any axis → polyline gaps that axis.
#[must_use]
pub fn render_parallel_coords(t: &Table, opts: &ParallelCoordsOpts) -> String {
    let axis_cols = if opts.axis_cols.is_empty() {
        (0..t.headers.len()).collect::<Vec<_>>()
    } else {
        opts.axis_cols.clone()
    };
    if axis_cols.is_empty() {
        return format!(
            "<!DOCTYPE html><html><body><svg width=\"{}\" height=\"{}\"></svg></body></html>",
            opts.width, opts.height
        );
    }

    // Per-axis scale (numeric or categorical).
    let scales: Vec<AxisScale> = axis_cols
        .iter()
        .map(|&i| AxisScale::from_column(t, i))
        .collect();

    // Color buckets per row (if color_by configured).
    let row_colors: Vec<String> = compute_row_colors(t, opts.color_by);

    // Layout constants.
    let pad_left: f64 = 80.0;
    let pad_right: f64 = 40.0;
    let pad_top: f64 = if opts.title.is_some() { 60.0 } else { 30.0 };
    let pad_bottom: f64 = 60.0;
    let canvas_w = f64::from(opts.width);
    let canvas_h = f64::from(opts.height);
    let inner_w = canvas_w - pad_left - pad_right;
    let inner_h = canvas_h - pad_top - pad_bottom;
    let n_axes = axis_cols.len();
    #[allow(clippy::cast_precision_loss)]
    let axis_gap = if n_axes > 1 {
        inner_w / (n_axes as f64 - 1.0)
    } else {
        0.0
    };

    let mut out = String::new();
    out.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>\n");
    out.push_str(
        "body{margin:0;font-family:system-ui,-apple-system,sans-serif;background:#fff;}\n",
    );
    out.push_str(".pc-line{fill:none;stroke-width:1.2;stroke-opacity:0.6;}\n");
    out.push_str(".pc-line:hover{stroke-opacity:1;stroke-width:2.4;}\n");
    out.push_str(".pc-axis{stroke:#444;stroke-width:1;}\n");
    out.push_str(".pc-axis-label{font-size:13px;font-weight:600;text-anchor:middle;fill:#222;}\n");
    out.push_str(".pc-tick{font-size:11px;fill:#666;text-anchor:end;}\n");
    out.push_str(".pc-title{font-size:18px;font-weight:600;fill:#222;text-anchor:middle;}\n");
    out.push_str("#pc-tooltip{position:fixed;pointer-events:none;background:#222;color:#fff;padding:6px 8px;border-radius:4px;font-size:12px;display:none;max-width:320px;z-index:9;}\n");
    out.push_str("</style></head><body>\n");
    out.push_str(&format!(
        "<svg width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" xmlns=\"http://www.w3.org/2000/svg\">\n",
        opts.width, opts.height, opts.width, opts.height,
    ));

    // Title
    if let Some(title) = &opts.title {
        out.push_str(&format!(
            "<text class=\"pc-title\" x=\"{}\" y=\"{}\">",
            canvas_w / 2.0,
            pad_top / 2.0,
        ));
        push_html_escaped(&mut out, title);
        out.push_str("</text>\n");
    }

    // Axes
    for (i, &col) in axis_cols.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = pad_left + (i as f64) * axis_gap;
        out.push_str(&format!(
            "<line class=\"pc-axis\" x1=\"{x}\" y1=\"{pad_top}\" x2=\"{x}\" y2=\"{}\"/>\n",
            pad_top + inner_h,
        ));
        // Axis label
        out.push_str(&format!(
            "<text class=\"pc-axis-label\" x=\"{x}\" y=\"{}\">",
            pad_top + inner_h + 40.0,
        ));
        push_html_escaped(&mut out, &t.headers[col]);
        out.push_str("</text>\n");
        // Tick labels: min/mid/max for numeric, all values for categorical
        match &scales[i] {
            AxisScale::Numeric { min, max } => {
                let mid = (min + max) / 2.0;
                let ticks = [(0.0, *max), (0.5, mid), (1.0, *min)];
                for (frac, val) in ticks {
                    let y = pad_top + frac * inner_h;
                    out.push_str(&format!(
                        "<text class=\"pc-tick\" x=\"{}\" y=\"{y}\">{val:.2}</text>\n",
                        x - 6.0,
                    ));
                }
            }
            AxisScale::Categorical(cats) => {
                #[allow(clippy::cast_precision_loss)]
                let n = cats.len().max(1) as f64;
                for (idx, cat) in cats.iter().enumerate() {
                    #[allow(clippy::cast_precision_loss)]
                    let frac = if cats.len() == 1 {
                        0.5
                    } else {
                        (idx as f64) / (n - 1.0)
                    };
                    let y = pad_top + (1.0 - frac) * inner_h;
                    out.push_str(&format!(
                        "<text class=\"pc-tick\" x=\"{}\" y=\"{y}\">",
                        x - 6.0,
                    ));
                    push_html_escaped(&mut out, cat);
                    out.push_str("</text>\n");
                }
            }
        }
    }

    // Polylines — one per row
    for (row_idx, row) in t.rows.iter().enumerate() {
        // Build path. Skip the row if no axis has a value (all null).
        let mut points: Vec<(f64, f64)> = Vec::with_capacity(axis_cols.len());
        for (i, &col) in axis_cols.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let x = pad_left + (i as f64) * axis_gap;
            if let Some(frac) = scales[i].normalize(&row[col]) {
                let y = pad_top + (1.0 - frac) * inner_h;
                points.push((x, y));
            }
        }
        if points.len() < 2 {
            continue;
        }
        let mut d = String::new();
        for (i, (x, y)) in points.iter().enumerate() {
            d.push(if i == 0 { 'M' } else { 'L' });
            let _ = write!(d, "{x:.2} {y:.2} ");
        }
        let color = row_colors
            .get(row_idx)
            .cloned()
            .unwrap_or_else(|| "#4a90e2".into());
        let tooltip = build_row_tooltip(t, row);
        out.push_str(&format!(
            "<path class=\"pc-line\" stroke=\"{color}\" d=\"{d}\" data-tip=\""
        ));
        push_html_escaped(&mut out, &tooltip);
        out.push_str("\"/>\n");
    }

    out.push_str("</svg>\n");
    out.push_str("<div id=\"pc-tooltip\"></div>\n");
    out.push_str("<script>\n");
    out.push_str(
        "(function(){var tip=document.getElementById('pc-tooltip');document.querySelectorAll('.pc-line').forEach(function(p){p.addEventListener('mousemove',function(e){tip.style.display='block';tip.style.left=(e.clientX+12)+'px';tip.style.top=(e.clientY+12)+'px';tip.textContent=p.getAttribute('data-tip')||'';});p.addEventListener('mouseleave',function(){tip.style.display='none';});});})();\n"
    );
    out.push_str("</script>\n");
    out.push_str("</body></html>\n");
    out
}

#[derive(Debug, Clone)]
enum AxisScale {
    Numeric { min: f64, max: f64 },
    Categorical(Vec<String>),
}

impl AxisScale {
    fn from_column(t: &Table, col: usize) -> Self {
        // Decide numeric vs categorical based on first non-null cell.
        let mut any_numeric = false;
        let mut any_categorical = false;
        for r in &t.rows {
            match &r[col] {
                Value::Null => {}
                Value::I64(_) | Value::F64(_) | Value::Timestamp(_) => any_numeric = true,
                _ => any_categorical = true,
            }
        }
        // Mixed → fall back to categorical (treat numbers as strings).
        if any_numeric && !any_categorical {
            let mut min = f64::INFINITY;
            let mut max = f64::NEG_INFINITY;
            for r in &t.rows {
                let v = numeric_value(&r[col]);
                if let Some(x) = v {
                    if x < min {
                        min = x;
                    }
                    if x > max {
                        max = x;
                    }
                }
            }
            if !min.is_finite() {
                Self::Numeric { min: 0.0, max: 1.0 }
            } else if (min - max).abs() < f64::EPSILON {
                Self::Numeric {
                    min: min - 0.5,
                    max: max + 0.5,
                }
            } else {
                Self::Numeric { min, max }
            }
        } else {
            let mut cats: Vec<String> = t
                .rows
                .iter()
                .filter_map(|r| match &r[col] {
                    Value::Null => None,
                    other => Some(format_cell(other)),
                })
                .collect();
            cats.sort();
            cats.dedup();
            Self::Categorical(cats)
        }
    }

    fn normalize(&self, v: &Value) -> Option<f64> {
        match self {
            Self::Numeric { min, max } => {
                let x = numeric_value(v)?;
                let span = max - min;
                if span.abs() < f64::EPSILON {
                    Some(0.5)
                } else {
                    Some(((x - *min) / span).clamp(0.0, 1.0))
                }
            }
            Self::Categorical(cats) => {
                if matches!(v, Value::Null) {
                    return None;
                }
                let s = format_cell(v);
                let idx = cats.iter().position(|c| c == &s)?;
                if cats.len() == 1 {
                    Some(0.5)
                } else {
                    #[allow(clippy::cast_precision_loss)]
                    let frac = idx as f64 / (cats.len() as f64 - 1.0);
                    Some(frac)
                }
            }
        }
    }
}

fn numeric_value(v: &Value) -> Option<f64> {
    match v {
        Value::I64(n) => {
            #[allow(clippy::cast_precision_loss)]
            Some(*n as f64)
        }
        Value::F64(f) if f.is_finite() => Some(*f),
        Value::Timestamp(t) => {
            #[allow(clippy::cast_precision_loss)]
            Some(*t as f64)
        }
        _ => None,
    }
}

fn compute_row_colors(t: &Table, color_by: Option<usize>) -> Vec<String> {
    // d3.schemeCategory10 (categorical) — 10 colours.
    const CATEGORICAL: &[&str] = &[
        "#1f77b4", "#ff7f0e", "#2ca02c", "#d62728", "#9467bd",
        "#8c564b", "#e377c2", "#7f7f7f", "#bcbd22", "#17becf",
    ];
    // 8-stop viridis approximation.
    const VIRIDIS: &[&str] = &[
        "#440154", "#482878", "#3e4989", "#31688e", "#26828e",
        "#1f9e89", "#35b779", "#fde725",
    ];

    let Some(col) = color_by else {
        return t.rows.iter().map(|_| "#4a90e2".into()).collect();
    };

    let scale = AxisScale::from_column(t, col);
    t.rows
        .iter()
        .map(|r| match &scale {
            AxisScale::Numeric { .. } => scale.normalize(&r[col]).map_or_else(
                || "#888".into(),
                |frac| {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let idx = ((frac * (VIRIDIS.len() as f64 - 1.0)).round() as usize)
                        .min(VIRIDIS.len() - 1);
                    VIRIDIS[idx].to_string()
                },
            ),
            AxisScale::Categorical(cats) => {
                let s = format_cell(&r[col]);
                cats.iter().position(|c| c == &s).map_or_else(
                    || "#888".into(),
                    |i| CATEGORICAL[i % CATEGORICAL.len()].to_string(),
                )
            }
        })
        .collect()
}

fn build_row_tooltip(t: &Table, row: &[Value]) -> String {
    let mut parts = Vec::with_capacity(t.headers.len());
    for (h, v) in t.headers.iter().zip(row.iter()) {
        parts.push(format!("{h}: {}", format_cell(v)));
    }
    parts.join(" • ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn t_4d_sample() -> Table {
        // 2 regions × 2 quarters × 2 years; 8 rows. value = revenue.
        let mut rows = Vec::new();
        for region in ["NA", "EU"] {
            for year in [2024i64, 2025] {
                for quarter in ["Q1", "Q2"] {
                    rows.push(vec![
                        Value::String(region.into()),
                        Value::I64(year),
                        Value::String(quarter.into()),
                        Value::I64(100 * (year - 2023) + i64::try_from(quarter.len()).unwrap()),
                    ]);
                }
            }
        }
        Table {
            headers: vec!["region".into(), "year".into(), "quarter".into(), "revenue".into()],
            rows,
        }
    }

    #[test]
    fn pivot_2x2_sum_renders_cross_tab() {
        // 2 row dims × 2 col dims × Sum on revenue.
        let s = render_pivot(&t_4d_sample(), &[0], &[1, 2], 3, Aggregate::Sum);
        // Header band has the row-dim title + 4 compound col labels.
        assert!(s.contains("<th>region</th>"));
        assert!(s.contains("<th>2024 / Q1</th>"));
        assert!(s.contains("<th>2024 / Q2</th>"));
        assert!(s.contains("<th>2025 / Q1</th>"));
        assert!(s.contains("<th>2025 / Q2</th>"));
        // Two body rows — NA and EU.
        let body_start = s.find("<tbody>").unwrap();
        let body = &s[body_start..];
        assert!(body.contains("<th>NA</th>"));
        assert!(body.contains("<th>EU</th>"));
    }

    #[test]
    fn pivot_single_row_single_col_simple_shape() {
        let t = Table {
            headers: vec!["color".into(), "size".into(), "n".into()],
            rows: vec![
                vec![Value::String("red".into()), Value::String("S".into()), Value::I64(1)],
                vec![Value::String("red".into()), Value::String("L".into()), Value::I64(2)],
                vec![Value::String("blue".into()), Value::String("S".into()), Value::I64(3)],
            ],
        };
        let s = render_pivot(&t, &[0], &[1], 2, Aggregate::Sum);
        assert!(s.contains("<th>color</th>"));
        assert!(s.contains("<th>S</th>"));
        assert!(s.contains("<th>L</th>"));
        assert!(s.contains("<th>red</th>"));
        assert!(s.contains("<th>blue</th>"));
    }

    #[test]
    fn pivot_missing_combos_render_nbsp_not_zero() {
        let t = Table {
            headers: vec!["r".into(), "c".into(), "v".into()],
            rows: vec![
                vec![Value::String("a".into()), Value::String("x".into()), Value::I64(7)],
                // (a, y), (b, x), (b, y) all missing
                vec![Value::String("b".into()), Value::String("y".into()), Value::I64(9)],
            ],
        };
        let s = render_pivot(&t, &[0], &[1], 2, Aggregate::Sum);
        // 2 row keys × 2 col keys = 4 cells; 2 with data, 2 with &nbsp;
        let nbsp_count = s.matches("&nbsp;").count();
        assert_eq!(nbsp_count, 2, "expected 2 empty cells, output: {s}");
        // The two present values appear.
        assert!(s.contains("<td>7</td>"));
        assert!(s.contains("<td>9</td>"));
    }

    #[test]
    fn pivot_3_row_dims_compound_label_per_dim() {
        // 3 row dims, 1 col dim, Count of any one cell.
        let t = Table {
            headers: vec!["a".into(), "b".into(), "c".into(), "k".into(), "v".into()],
            rows: vec![
                vec![Value::String("a1".into()), Value::String("b1".into()), Value::String("c1".into()), Value::String("k1".into()), Value::I64(1)],
                vec![Value::String("a1".into()), Value::String("b1".into()), Value::String("c2".into()), Value::String("k1".into()), Value::I64(1)],
                vec![Value::String("a2".into()), Value::String("b1".into()), Value::String("c1".into()), Value::String("k1".into()), Value::I64(1)],
            ],
        };
        let s = render_pivot(&t, &[0, 1, 2], &[3], 4, Aggregate::Count);
        // Each row's leading cells emit one <th> per row-dim — distinct
        // labels per row (compound headers go in DIFFERENT <th> cells).
        assert!(s.contains("<th>a1</th>"));
        assert!(s.contains("<th>b1</th>"));
        assert!(s.contains("<th>c1</th>"));
        assert!(s.contains("<th>c2</th>"));
        assert!(s.contains("<th>a2</th>"));
        // Three body rows expected (3 distinct row tuples).
        let body_start = s.find("<tbody>").unwrap();
        let body_end = s.find("</tbody>").unwrap();
        let body = &s[body_start..body_end];
        let row_count = body.matches("<tr>").count();
        assert_eq!(row_count, 3);
    }

    // ---------------------------------------------------------------------
    // §2.11 Parallel coordinates
    // ---------------------------------------------------------------------

    fn pc_sample() -> Table {
        // 5 columns; mix of numeric + categorical. 4 rows.
        Table {
            headers: vec!["a".into(), "b".into(), "c".into(), "d".into(), "cat".into()],
            rows: vec![
                vec![Value::I64(1), Value::F64(10.0), Value::I64(100), Value::F64(0.1), Value::String("x".into())],
                vec![Value::I64(2), Value::F64(20.0), Value::I64(200), Value::F64(0.2), Value::String("y".into())],
                vec![Value::I64(3), Value::F64(30.0), Value::I64(300), Value::F64(0.3), Value::String("x".into())],
                vec![Value::I64(4), Value::F64(40.0), Value::I64(400), Value::F64(0.4), Value::String("z".into())],
            ],
        }
    }

    #[test]
    fn parallel_coords_renders_self_contained_html() {
        let t = pc_sample();
        let opts = ParallelCoordsOpts {
            axis_cols: vec![0, 1, 2, 3, 4],
            color_by: Some(4),
            title: Some("Parallel sample".into()),
            ..Default::default()
        };
        let s = render_parallel_coords(&t, &opts);
        // Self-contained HTML — no external resources.
        assert!(s.starts_with("<!DOCTYPE html>"));
        assert!(s.contains("<svg "));
        assert!(s.contains("</svg>"));
        assert!(!s.contains("<script src="));
        assert!(!s.contains("<link rel="));
        // Title rendered.
        assert!(s.contains("Parallel sample"));
        // 5 axis labels present.
        for header in &t.headers {
            assert!(s.contains(&format!(">{header}<")), "missing axis label: {header}");
        }
    }

    #[test]
    fn parallel_coords_one_polyline_per_row() {
        let t = pc_sample();
        let opts = ParallelCoordsOpts {
            axis_cols: vec![0, 1, 2, 3],
            ..Default::default()
        };
        let s = render_parallel_coords(&t, &opts);
        // 4 rows → 4 polylines.
        assert_eq!(s.matches("class=\"pc-line\"").count(), 4);
    }

    #[test]
    fn parallel_coords_color_by_produces_distinct_strokes() {
        let t = pc_sample();
        let opts = ParallelCoordsOpts {
            axis_cols: vec![0, 1, 2, 3],
            color_by: Some(4), // categorical column with 3 distinct values
            ..Default::default()
        };
        let s = render_parallel_coords(&t, &opts);
        // Categorical d3-palette colors.
        let mut found_colors = std::collections::HashSet::new();
        for cap in s.split("stroke=\"").skip(1) {
            if let Some(end) = cap.find('"') {
                found_colors.insert(cap[..end].to_owned());
            }
        }
        // At least 3 distinct stroke colors (one per category).
        assert!(found_colors.len() >= 3, "found colors: {found_colors:?}");
    }

    #[test]
    fn parallel_coords_empty_axes_emits_blank_svg() {
        let t = pc_sample();
        let opts = ParallelCoordsOpts {
            axis_cols: vec![],
            ..Default::default()
        };
        let s = render_parallel_coords(&t, &opts);
        // Falls through to all columns — should still produce a non-empty SVG.
        // (Empty-axis-cols path defaults to every column.)
        assert!(s.contains("<svg"));
        assert!(s.contains("</svg>"));
    }

    #[test]
    fn parallel_coords_numeric_only_axes_scale_correctly() {
        let t = Table {
            headers: vec!["x".into(), "y".into()],
            rows: vec![
                vec![Value::I64(0), Value::F64(0.0)],
                vec![Value::I64(10), Value::F64(100.0)],
            ],
        };
        let opts = ParallelCoordsOpts {
            axis_cols: vec![0, 1],
            ..Default::default()
        };
        let s = render_parallel_coords(&t, &opts);
        // 2 polylines emitted.
        assert_eq!(s.matches("class=\"pc-line\"").count(), 2);
        // Tick labels for min/max numeric scaling.
        assert!(s.contains("0.00") || s.contains("10.00") || s.contains("100.00"));
    }

    #[test]
    fn pivot_avg_aggregate_works() {
        let t = Table {
            headers: vec!["g".into(), "k".into(), "v".into()],
            rows: vec![
                vec![Value::String("a".into()), Value::String("k1".into()), Value::I64(10)],
                vec![Value::String("a".into()), Value::String("k1".into()), Value::I64(20)],
                vec![Value::String("a".into()), Value::String("k1".into()), Value::I64(30)],
            ],
        };
        let s = render_pivot(&t, &[0], &[1], 2, Aggregate::Avg);
        // Avg of 10/20/30 = 20.0
        assert!(s.contains("<td>20</td>") || s.contains("<td>20.0</td>"));
    }
}
