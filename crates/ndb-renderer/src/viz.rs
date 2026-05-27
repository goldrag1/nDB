//! N-dimensional visualization renderers (v2.1 §2.10–§2.12).
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
