//! nDB renderers — turn a slicer [`Table`] into a textual output (§17.1).
#![warn(missing_docs)]
#![allow(clippy::doc_markdown)] // "SSTable", "JSONL" used liberally.
//!
//! v1 surface: three render targets, all text-only.
//!
//! - `render_text(t)`   — bordered ASCII table for humans
//! - `render_tsv(t)`    — tab-separated rows for piping
//! - `render_csv(t)`    — comma-separated, RFC-4180-style quoting
//!
//! Image / chart / interactive renderers (scatter, bar, line, etc.) are
//! the 2D dimensional renderer family from §17.4 / §17.1 and come in
//! later crates. v1 keeps the surface narrow and the output streamable
//! to a terminal.

use std::fmt::Write;

use ndb_engine::value::Value;
use ndb_slicer::Table;

/// Format a `Value` as a single display string. Bytes / Vector /
/// Extension are summarised — full base64 would blow up tables.
#[must_use]
pub fn format_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::F64(f) => format!("{f}"),
        Value::String(s) => s.clone(),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        Value::Timestamp(us) => format!("ts={us}"),
        Value::EntityRef(id) => id.into_uuid().to_string(),
        Value::Decimal { scale, mantissa } => format_decimal(*scale, *mantissa),
        Value::Vector(v) => format!("<f32 vector len={}>", v.len()),
        Value::Extension(b) => format!("<ext {} bytes>", b.len()),
    }
}

fn format_decimal(scale: u8, mantissa: i128) -> String {
    if scale == 0 {
        return mantissa.to_string();
    }
    let s = mantissa.abs().to_string();
    let scale = usize::from(scale);
    let (int_part, frac_part) = if s.len() > scale {
        let split = s.len() - scale;
        (&s[..split], &s[split..])
    } else {
        ("0", s.as_str())
    };
    // Left-pad the fractional component with zeros when the integer
    // part borrowed from it.
    let frac = if frac_part.len() < scale {
        format!("{frac_part:0>scale$}")
    } else {
        frac_part.to_owned()
    };
    let sign = if mantissa < 0 { "-" } else { "" };
    format!("{sign}{int_part}.{frac}")
}

/// Render `t` as a bordered ASCII table.
///
/// ```text
/// ┌────────┬───────┐
/// │ color  │     n │
/// ├────────┼───────┤
/// │ red    │     2 │
/// │ blue   │     1 │
/// └────────┴───────┘
/// ```
///
/// Numeric cells right-align, everything else left-aligns. Column
/// widths fit the widest cell (header included).
#[must_use]
pub fn render_text(t: &Table) -> String {
    if t.headers.is_empty() {
        return String::new();
    }
    // Pre-format every cell.
    let formatted: Vec<Vec<String>> = t
        .rows
        .iter()
        .map(|row| row.iter().map(format_cell).collect())
        .collect();

    let n_cols = t.headers.len();
    let mut widths = vec![0usize; n_cols];
    for (i, h) in t.headers.iter().enumerate() {
        widths[i] = h.chars().count();
    }
    for row in &formatted {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    // Detect numeric columns to right-align.
    let numeric: Vec<bool> = (0..n_cols)
        .map(|i| t.rows.iter().all(|r| is_numeric(&r[i])))
        .collect();

    let mut out = String::new();
    write_border(&mut out, &widths, "┌", "┬", "┐");
    write_row(&mut out, &t.headers.clone(), &widths, &numeric);
    write_border(&mut out, &widths, "├", "┼", "┤");
    for row in &formatted {
        write_row(&mut out, row, &widths, &numeric);
    }
    write_border(&mut out, &widths, "└", "┴", "┘");
    out
}

fn write_border(out: &mut String, widths: &[usize], l: &str, m: &str, r: &str) {
    out.push_str(l);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            out.push_str(m);
        }
        out.push_str(&"─".repeat(w + 2));
    }
    out.push_str(r);
    out.push('\n');
}

fn write_row(out: &mut String, cells: &[String], widths: &[usize], numeric: &[bool]) {
    out.push('│');
    for (i, cell) in cells.iter().enumerate() {
        let width = widths[i];
        if numeric[i] {
            let pad = width - cell.chars().count();
            let _ = write!(out, " {}{} │", " ".repeat(pad), cell);
        } else {
            let pad = width - cell.chars().count();
            let _ = write!(out, " {}{} │", cell, " ".repeat(pad));
        }
    }
    out.push('\n');
}

fn is_numeric(v: &Value) -> bool {
    matches!(
        v,
        Value::I64(_) | Value::F64(_) | Value::Decimal { .. } | Value::Null
    )
}

/// Render `t` as tab-separated rows, with a header row first. Cells
/// containing tabs/newlines are dropped — TSV has no escaping; callers
/// who need that should use CSV.
#[must_use]
pub fn render_tsv(t: &Table) -> String {
    let mut out = String::new();
    out.push_str(&t.headers.join("\t"));
    out.push('\n');
    for row in &t.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|c| format_cell(c).replace(['\t', '\n'], " "))
            .collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
}

/// Render `t` as CSV (RFC-4180-style). Cells containing commas, quotes,
/// or newlines are quoted; embedded quotes are doubled.
#[must_use]
pub fn render_csv(t: &Table) -> String {
    let mut out = String::new();
    out.push_str(
        &t.headers
            .iter()
            .map(|s| csv_quote(s))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for row in &t.rows {
        let cells: Vec<String> = row.iter().map(|c| csv_quote(&format_cell(c))).collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}

fn csv_quote(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_owned()
    }
}

// ---------------------------------------------------------------------------
// v2.1 §2.7 — Markdown table renderer
// ---------------------------------------------------------------------------

/// GitHub-flavored Markdown table — header row, alignment row, body
/// rows. Cells that contain pipes / newlines / leading `-` / leading
/// `+` get backtick-wrapped to keep the table machine-parseable.
///
/// Used for paste-into-issue and paste-into-doc workflows.
#[must_use]
pub fn render_markdown(t: &Table) -> String {
    let mut out = String::new();
    // Header row.
    out.push_str("| ");
    out.push_str(
        &t.headers
            .iter()
            .map(|h| md_escape(h))
            .collect::<Vec<_>>()
            .join(" | "),
    );
    out.push_str(" |\n");
    // Alignment row — left-align everything (GFM `|---|` syntax).
    out.push('|');
    for _ in &t.headers {
        out.push_str(" --- |");
    }
    out.push('\n');
    // Body rows.
    for row in &t.rows {
        out.push_str("| ");
        let cells: Vec<String> = row.iter().map(|c| md_escape(&format_cell(c))).collect();
        out.push_str(&cells.join(" | "));
        out.push_str(" |\n");
    }
    out
}

/// Escape a cell for GFM tables. Cells containing `|` / `\n` / leading
/// `-` / leading `+` are backtick-wrapped; embedded backticks get
/// doubled inside the wrap (per CommonMark).
fn md_escape(s: &str) -> String {
    let needs_wrap = s.contains('|')
        || s.contains('\n')
        || s.starts_with('-')
        || s.starts_with('+');
    if needs_wrap {
        // CommonMark code spans: more backticks outside than inside.
        // Find the longest run of backticks in `s` and use one more.
        let max_run = max_backtick_run(s);
        let fence: String = "`".repeat(max_run + 1);
        // GFM converts `\n` inside table cells to `<br>` — fold here.
        let body = s.replace('\n', "<br>");
        format!("{fence}{body}{fence}")
    } else {
        s.to_owned()
    }
}

fn max_backtick_run(s: &str) -> usize {
    let mut max = 0;
    let mut cur = 0;
    for ch in s.chars() {
        if ch == '`' {
            cur += 1;
            if cur > max {
                max = cur;
            }
        } else {
            cur = 0;
        }
    }
    max
}

// ---------------------------------------------------------------------------
// v2.1 §2.8 — JSON-lines renderer
// ---------------------------------------------------------------------------

/// JSON-lines (newline-delimited JSON) output — one JSON object per row.
/// Header cell names become the object keys. Drop-in for streaming pipes
/// into `jq`, DuckDB's `read_json`, Polars `scan_ndjson`, etc.
///
/// Value mapping:
/// - `Null`               → `null`
/// - `Bool(b)`            → JSON `true` / `false`
/// - `I64(n)`             → JSON number (no exponent)
/// - `F64(f)`             → JSON number; `NaN` / `±Inf` → `null` (not legal JSON)
/// - `String(s)`          → JSON string (escaped)
/// - `Bytes(b)`           → JSON string of base64
/// - `Timestamp(t)`       → JSON number (microseconds since Unix epoch)
/// - `EntityRef(u)`       → JSON string of the UUID
/// - `Decimal { … }`      → JSON string in `mantissa.scale` form (avoid lossy f64)
/// - `Vector(v)`          → JSON array of numbers
/// - `Extension(b)`       → JSON string of base64 (forward-compat)
#[must_use]
pub fn render_jsonl(t: &Table) -> String {
    let mut out = String::new();
    for row in &t.rows {
        out.push('{');
        for (i, (header, cell)) in t.headers.iter().zip(row.iter()).enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_json_string(&mut out, header);
            out.push(':');
            push_json_value(&mut out, cell);
        }
        out.push_str("}\n");
    }
    out
}

fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_json_value(out: &mut String, v: &Value) {
    use base64::Engine as _;
    use std::fmt::Write as _;
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::I64(n) => {
            let _ = write!(out, "{n}");
        }
        Value::F64(f) => {
            if f.is_finite() {
                let _ = write!(out, "{f}");
            } else {
                // NaN/±Inf aren't valid JSON — emit null. (Same call as
                // serde_json's default behaviour.)
                out.push_str("null");
            }
        }
        Value::String(s) => push_json_string(out, s),
        Value::Bytes(b) | Value::Extension(b) => {
            push_json_string(out, &base64::engine::general_purpose::STANDARD.encode(b));
        }
        Value::Timestamp(t) => {
            let _ = write!(out, "{t}");
        }
        Value::EntityRef(eid) => push_json_string(out, &eid.into_uuid().to_string()),
        Value::Decimal { scale, mantissa } => {
            // Emit as a JSON STRING in `mantissa_int.fraction` form to
            // preserve every decimal digit (JSON numbers go through f64
            // in most parsers — round-trip-lossy).
            push_json_string(out, &format_decimal(*scale, *mantissa));
        }
        Value::Vector(v) => {
            out.push('[');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if x.is_finite() {
                    let _ = write!(out, "{x}");
                } else {
                    out.push_str("null");
                }
            }
            out.push(']');
        }
    }
}

// ---------------------------------------------------------------------------
// v2.1 §2.9 — HTML table renderer
// ---------------------------------------------------------------------------

/// Minimal `<table>` output — `<thead>` + `<tbody>` with HTML-escaped
/// cells. No CSS, no classes, no JS — keeps the output small and
/// paste-into-email / paste-into-Confluence friendly.
#[must_use]
pub fn render_html(t: &Table) -> String {
    let mut out = String::new();
    out.push_str("<table>\n");
    out.push_str("<thead><tr>");
    for h in &t.headers {
        out.push_str("<th>");
        push_html_escaped(&mut out, h);
        out.push_str("</th>");
    }
    out.push_str("</tr></thead>\n");
    out.push_str("<tbody>\n");
    for row in &t.rows {
        out.push_str("<tr>");
        for cell in row {
            out.push_str("<td>");
            push_html_escaped(&mut out, &format_cell(cell));
            out.push_str("</td>");
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody>\n");
    out.push_str("</table>\n");
    out
}

fn push_html_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndb_engine::value::Value;

    fn sample_table() -> Table {
        Table {
            headers: vec!["color".into(), "n".into()],
            rows: vec![
                vec![Value::String("red".into()), Value::I64(2)],
                vec![Value::String("blue".into()), Value::I64(1)],
            ],
        }
    }

    #[test]
    fn text_render_has_borders_and_cells() {
        let s = render_text(&sample_table());
        assert!(s.contains("color"));
        assert!(s.contains("red"));
        assert!(s.contains("blue"));
        assert!(s.contains("┌") && s.contains("┐") && s.contains("│"));
        // numeric column right-aligns: row "n" cell "2" appears after spaces
        assert!(s.contains(" 2 │"));
    }

    #[test]
    fn empty_table_renders_empty_text() {
        let t = Table {
            headers: vec![],
            rows: vec![],
        };
        assert_eq!(render_text(&t), "");
    }

    #[test]
    fn tsv_round_trip_shape() {
        let s = render_tsv(&sample_table());
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "color\tn");
        assert_eq!(lines[1], "red\t2");
        assert_eq!(lines[2], "blue\t1");
    }

    #[test]
    fn csv_quotes_special_characters() {
        let t = Table {
            headers: vec!["a".into(), "b".into()],
            rows: vec![vec![
                Value::String("comma, in cell".into()),
                Value::String("quote\"in cell".into()),
            ]],
        };
        let s = render_csv(&t);
        assert!(s.contains("\"comma, in cell\""));
        assert!(s.contains("\"quote\"\"in cell\""));
    }

    #[test]
    fn decimal_formats_correctly() {
        assert_eq!(format_decimal(2, 1234), "12.34");
        assert_eq!(format_decimal(2, -1234), "-12.34");
        assert_eq!(format_decimal(0, 42), "42");
        assert_eq!(format_decimal(4, 5), "0.0005");
        assert_eq!(format_decimal(3, 1_000_000), "1000.000");
    }

    #[test]
    fn null_cell_renders_as_empty() {
        let t = Table {
            headers: vec!["x".into()],
            rows: vec![vec![Value::Null]],
        };
        let s = render_csv(&t);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[1], "");
    }

    // ---------------------------------------------------------------------
    // v2.1 §2.7 — Markdown renderer
    // ---------------------------------------------------------------------

    #[test]
    fn markdown_basic_table_shape() {
        let s = render_markdown(&sample_table());
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "| color | n |");
        assert_eq!(lines[1], "| --- | --- |");
        // Body rows present
        assert!(lines.iter().any(|l| l.contains("red") && l.contains('2')));
        assert!(lines.iter().any(|l| l.contains("blue") && l.contains('1')));
    }

    #[test]
    fn markdown_escapes_pipes_in_cells() {
        let t = Table {
            headers: vec!["k".into(), "v".into()],
            rows: vec![vec![Value::String("a|b".into()), Value::I64(1)]],
        };
        let s = render_markdown(&t);
        // The pipe-bearing cell must be backtick-wrapped.
        assert!(s.contains("`a|b`"), "expected backtick-wrapped pipe; got: {s}");
    }

    #[test]
    fn markdown_escapes_newline_to_br() {
        let t = Table {
            headers: vec!["k".into()],
            rows: vec![vec![Value::String("line1\nline2".into())]],
        };
        let s = render_markdown(&t);
        // Newline inside a cell becomes <br>, wrapped in backticks.
        assert!(s.contains("`line1<br>line2`"), "got: {s}");
        // Result remains parseable line-by-line (no raw newlines inside cells).
        let body_line = s.lines().nth(2).unwrap();
        assert!(body_line.contains("<br>"));
    }

    #[test]
    fn markdown_empty_table_emits_header_and_alignment_only() {
        let t = Table {
            headers: vec!["only_col".into()],
            rows: vec![],
        };
        let s = render_markdown(&t);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines, vec!["| only_col |", "| --- |"]);
    }

    #[test]
    fn markdown_escapes_leading_dash_and_plus() {
        let t = Table {
            headers: vec!["k".into()],
            rows: vec![
                vec![Value::String("-leading".into())],
                vec![Value::String("+leading".into())],
            ],
        };
        let s = render_markdown(&t);
        assert!(s.contains("`-leading`"));
        assert!(s.contains("`+leading`"));
    }

    // ---------------------------------------------------------------------
    // v2.1 §2.8 — JSON-lines renderer
    // ---------------------------------------------------------------------

    #[test]
    fn jsonl_one_object_per_row() {
        let s = render_jsonl(&sample_table());
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line must be a JSON object with both header keys.
        for line in &lines {
            assert!(line.starts_with('{') && line.ends_with('}'), "line: {line}");
            assert!(line.contains("\"color\":"));
            assert!(line.contains("\"n\":"));
        }
        // Values present.
        assert!(s.contains("\"red\""));
        assert!(s.contains("\"blue\""));
    }

    #[test]
    fn jsonl_escapes_special_chars_in_string_values() {
        let t = Table {
            headers: vec!["k".into()],
            rows: vec![vec![Value::String("a\"b\\c\nd\te".into())]],
        };
        let s = render_jsonl(&t);
        // Each problematic byte must be backslash-escaped per JSON spec.
        let trimmed = s.trim();
        assert!(trimmed.contains("\\\""), "missing escaped quote: {trimmed}");
        assert!(trimmed.contains("\\\\"), "missing escaped backslash: {trimmed}");
        assert!(trimmed.contains("\\n"), "missing escaped newline: {trimmed}");
        assert!(trimmed.contains("\\t"), "missing escaped tab: {trimmed}");
        // No raw control chars survived.
        assert!(!trimmed.contains('\n') || trimmed.ends_with('\n'));
    }

    #[test]
    fn jsonl_null_value_serializes_as_null() {
        let t = Table {
            headers: vec!["k".into()],
            rows: vec![vec![Value::Null]],
        };
        let s = render_jsonl(&t);
        assert_eq!(s.trim(), "{\"k\":null}");
    }

    #[test]
    fn jsonl_bool_and_numbers_unquoted() {
        let t = Table {
            headers: vec!["b".into(), "n".into(), "f".into()],
            rows: vec![vec![Value::Bool(true), Value::I64(42), Value::F64(1.5)]],
        };
        let s = render_jsonl(&t);
        // Numbers + bools must not be quoted.
        assert!(s.contains("\"b\":true"));
        assert!(s.contains("\"n\":42"));
        assert!(s.contains("\"f\":1.5"));
    }

    #[test]
    fn jsonl_nan_and_infinity_serialize_as_null() {
        let t = Table {
            headers: vec!["x".into(), "y".into()],
            rows: vec![vec![Value::F64(f64::NAN), Value::F64(f64::INFINITY)]],
        };
        let s = render_jsonl(&t);
        // Both non-finite floats fall back to JSON null.
        assert!(s.contains("\"x\":null"));
        assert!(s.contains("\"y\":null"));
        // And the line stays valid JSON shape.
        assert!(s.trim().starts_with('{') && s.trim().ends_with('}'));
    }

    // ---------------------------------------------------------------------
    // v2.1 §2.9 — HTML renderer
    // ---------------------------------------------------------------------

    #[test]
    fn html_basic_structure() {
        let s = render_html(&sample_table());
        assert!(s.starts_with("<table>"));
        assert!(s.contains("<thead><tr>"));
        assert!(s.contains("<tbody>"));
        assert!(s.contains("<th>color</th>"));
        assert!(s.contains("<th>n</th>"));
        assert!(s.contains("<td>red</td>"));
        assert!(s.contains("<td>blue</td>"));
        assert!(s.trim_end().ends_with("</table>"));
    }

    #[test]
    fn html_escapes_special_chars() {
        let t = Table {
            headers: vec!["<k>".into()],
            rows: vec![vec![Value::String("a<b & c>\"d'e".into())]],
        };
        let s = render_html(&t);
        // None of the raw special chars must survive in cell content.
        assert!(s.contains("&lt;k&gt;"));
        assert!(s.contains("a&lt;b &amp; c&gt;&quot;d&#39;e"));
        // And no raw < or > should appear inside <td>… contents.
        let body_start = s.find("<tbody>").unwrap();
        let body = &s[body_start..];
        assert!(!body.contains("a<b"));
    }

    #[test]
    fn html_empty_table_structure() {
        let t = Table {
            headers: vec!["only".into()],
            rows: vec![],
        };
        let s = render_html(&t);
        assert!(s.contains("<th>only</th>"));
        // No body rows.
        let body_start = s.find("<tbody>").unwrap();
        let body_end = s.find("</tbody>").unwrap();
        let body = &s[body_start + "<tbody>".len()..body_end];
        assert!(body.trim().is_empty(), "expected empty body, got: {body:?}");
    }

    #[test]
    fn markdown_handles_embedded_backticks() {
        let t = Table {
            headers: vec!["k".into()],
            rows: vec![vec![Value::String("a|`b`".into())]],
        };
        let s = render_markdown(&t);
        // Embedded `b` has 1 backtick; outer fence must use ≥2.
        assert!(s.contains("``a|`b```"), "got: {s}");
    }
}
