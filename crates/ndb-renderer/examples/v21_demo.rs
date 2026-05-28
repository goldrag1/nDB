//! v2.1 demo — seed a real nDB database with biology data
#![allow(
    clippy::format_push_string,        // HTML stitching reads cleanly with push_str(&format!(...))
    clippy::too_many_lines,            // The seed + render flow is one linear story
)]
//! (protein complexes), then render the three N-dim visualizations
//! into a single self-contained HTML file.
//!
//! Run from the repo root:
//!
//! ```sh
//! cargo run -p ndb-renderer --example v21_demo
//! ```
//!
//! Outputs:
//! - `/tmp/v21-demo-ndb/` — the seeded database (can be queried via
//!   `cargo run -p ndb-mcp-server -- --path /tmp/v21-demo-ndb`)
//! - `docs/v2.1-demo.html` — the interactive demo. Open it in any
//!   browser; hover over nodes/edges to see metadata.
//!
//! The demo dataset:
//! - 15 protein entities (`TYPE_PROTEIN` = 1)
//! - 6 protein-complex hyperedges (`TYPE_COMPLEX` = 100), each connecting
//!   3-4 proteins via role bindings (`ROLE_MEMBER` = 10)
//! - One pathway property per complex (`signaling/dna_repair/autophagy`)

use std::path::Path;

use ndb_engine::id::{EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId};
use ndb_engine::record::{EntityRecord, HyperEdgeRecord, Record};
use ndb_engine::value::Value;
use ndb_engine::Engine;
use ndb_renderer::viz::{
    HypergraphOpts, HyperedgeStyle, ParallelCoordsOpts, render_hypergraph, render_parallel_coords,
    render_pivot,
};
use ndb_slicer::{AggSpec, Aggregate, Column, Pipeline, Table};

// Reserved ids for the demo schema. Real applications would allocate
// these via dictionary records; here we hardcode for brevity.
const TYPE_PROTEIN: u32 = 1;
const TYPE_COMPLEX: u32 = 100;
const ROLE_MEMBER: u32 = 10;
const PROP_NAME: u32 = 30;
const PROP_FUNCTION: u32 = 31;
const PROP_YEAR_DISCOVERED: u32 = 32;
const PROP_PATHWAY: u32 = 33;
const PROP_ORGANISM: u32 = 34;
const PROP_COMPLEX_NAME: u32 = 35;

fn main() {
    let db_dir = Path::new("/tmp/v21-demo-ndb");
    // Start fresh every run.
    if db_dir.exists() {
        std::fs::remove_dir_all(db_dir).expect("cleanup old demo db");
    }
    std::fs::create_dir_all(db_dir).expect("mkdir demo db");
    let mut engine = Engine::create(db_dir).expect("create engine");

    // ---- Seed protein entities ----
    let proteins: Vec<(&str, &str, i64)> = vec![
        ("P53", "tumor suppressor", 1979),
        ("MDM2", "ubiquitin ligase", 1991),
        ("ATM", "kinase, DNA damage response", 1995),
        ("CHK2", "checkpoint kinase", 1998),
        ("BRCA1", "DNA repair", 1994),
        ("BRCA2", "DNA repair", 1995),
        ("AKT1", "kinase, survival", 1987),
        ("MTOR", "kinase, growth", 1994),
        ("ULK1", "kinase, autophagy init", 1998),
        ("BECN1", "autophagy regulator", 1998),
        ("LC3", "autophagy elongation", 2000),
        ("ATG7", "E1-like enzyme", 1999),
        ("RAB7", "late autophagy GTPase", 1990),
        ("PI3K", "kinase, signaling", 1988),
        ("PTEN", "phosphatase, tumor suppressor", 1997),
    ];
    let protein_ids: Vec<(EntityId, &str)> = proteins
        .into_iter()
        .map(|(name, func, year)| {
            let eid = EntityId::now_v7();
            let mut txn = engine.begin_write();
            let tx_id = txn.tx_id();
            txn.put_entity(EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(TYPE_PROTEIN),
                tx_id_assert: tx_id,
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(PROP_NAME), Value::String(name.into())),
                    (PropertyId::new(PROP_FUNCTION), Value::String(func.into())),
                    (PropertyId::new(PROP_YEAR_DISCOVERED), Value::I64(year)),
                ],
            });
            txn.commit().expect("commit protein");
            (eid, name)
        })
        .collect();

    // Lookup by name for hyperedge construction.
    let by_name = |n: &str| -> EntityId {
        protein_ids
            .iter()
            .find(|(_, name)| *name == n)
            .map_or_else(
                || panic!("missing protein {n}"),
                |(eid, _)| *eid,
            )
    };

    // ---- Seed protein-complex hyperedges ----
    let complexes: Vec<(&str, &str, Vec<&str>)> = vec![
        ("p53 surveillance", "dna_repair", vec!["P53", "MDM2", "ATM", "CHK2"]),
        ("BRCA repair", "dna_repair", vec!["BRCA1", "BRCA2", "ATM"]),
        ("mTOR growth", "signaling", vec!["AKT1", "MTOR", "PI3K", "PTEN"]),
        ("autophagy init", "autophagy", vec!["ULK1", "BECN1", "ATG7"]),
        ("autophagy elongation", "autophagy", vec!["LC3", "ATG7", "BECN1"]),
        ("late autophagy", "autophagy", vec!["LC3", "RAB7"]),
    ];
    for (cname, pathway, members) in &complexes {
        let mut txn = engine.begin_write();
        let tx_id = txn.tx_id();
        txn.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(TYPE_COMPLEX),
            tx_id_assert: tx_id,
            tx_id_supersede: TxId::ACTIVE,
            roles: members
                .iter()
                .map(|name| (RoleId::new(ROLE_MEMBER), by_name(name)))
                .collect(),
            hyperedge_roles: Vec::new(),
            properties: vec![
                (PropertyId::new(PROP_COMPLEX_NAME), Value::String((*cname).into())),
                (PropertyId::new(PROP_PATHWAY), Value::String((*pathway).into())),
                (PropertyId::new(PROP_ORGANISM), Value::String("H. sapiens".into())),
            ],
        });
        txn.commit().expect("commit complex");
    }
    engine.flush().expect("flush");

    // ---- Fetch every record at the latest snapshot ----
    let snapshot = TxId::new(engine.manifest().last_tx_id);
    let records: Vec<Record> = engine
        .snapshot_iter(snapshot)
        .expect("snapshot iter")
        .into_iter()
        // Filter out internal metadata kinds (TxTimestamp, RetentionPolicy)
        // that the engine writes as part of commit bookkeeping.
        .filter(|r| matches!(r, Record::Entity(_) | Record::HyperEdge(_)))
        .collect();

    println!(
        "Seeded {} entities + {} hyperedges. Database at {}",
        records.iter().filter(|r| matches!(r, Record::Entity(_))).count(),
        records.iter().filter(|r| matches!(r, Record::HyperEdge(_))).count(),
        db_dir.display(),
    );

    // ---- Render the three viz outputs ----
    let hypergraph_html = render_hypergraph(
        &records,
        &HypergraphOpts {
            width: 1100,
            height: 700,
            hyperedge_style: HyperedgeStyle::Polygon,
            max_nodes: Some(200),
            iterations: 250,
            seed: 0x00C0_FFEE,
            title: Some("Protein complexes — hover any node or polygon".into()),
        },
    );

    // For parallel-coords + pivot we need a tabular view of the proteins.
    // Build a small Pipeline that projects (name, function, year) per protein.
    let table_proteins = Pipeline::new()
        .select(Column::typed_entity_property(
            "name",
            TypeId::new(TYPE_PROTEIN),
            PropertyId::new(PROP_NAME),
        ))
        .select(Column::typed_entity_property(
            "function",
            TypeId::new(TYPE_PROTEIN),
            PropertyId::new(PROP_FUNCTION),
        ))
        .select(Column::typed_entity_property(
            "year_discovered",
            TypeId::new(TYPE_PROTEIN),
            PropertyId::new(PROP_YEAR_DISCOVERED),
        ))
        .filter(|r| matches!(r, Record::Entity(e) if e.type_id == TypeId::new(TYPE_PROTEIN)))
        .run(records.iter().cloned());

    let parallel_html = render_parallel_coords(
        &table_proteins,
        &ParallelCoordsOpts {
            width: 1100,
            height: 360,
            axis_cols: vec![2, 0, 1], // year, name, function
            color_by: Some(1),         // colour by function
            title: Some("Proteins — year discovered × function".into()),
        },
    );

    // For pivot: build a (protein × complex) table from the hyperedges,
    // pivoted by pathway. We synthesize one row per (complex, protein,
    // pathway) tuple.
    let mut pivot_rows = Vec::new();
    for (cname, pathway, members) in &complexes {
        for m in members {
            pivot_rows.push(vec![
                Value::String((*pathway).into()),
                Value::String((*m).into()),
                Value::String((*cname).into()),
                Value::I64(1),
            ]);
        }
    }
    let pivot_input = Table {
        headers: vec![
            "pathway".into(),
            "protein".into(),
            "complex".into(),
            "membership".into(),
        ],
        rows: pivot_rows,
    };
    let pivot_html = render_pivot(&pivot_input, &[1], &[0, 2], 3, Aggregate::Sum);

    // Demonstrate the percentile aggregate over year_discovered, grouped
    // by function: shows P50/P95 of discovery years per function class.
    let percentile_table = Pipeline::new()
        .select(Column::typed_entity_property(
            "function",
            TypeId::new(TYPE_PROTEIN),
            PropertyId::new(PROP_FUNCTION),
        ))
        .select(Column::typed_entity_property(
            "year",
            TypeId::new(TYPE_PROTEIN),
            PropertyId::new(PROP_YEAR_DISCOVERED),
        ))
        .group_by([0])
        .aggregate(AggSpec {
            header: "p50_year".into(),
            column: 1,
            agg: Aggregate::P50,
        })
        .aggregate(AggSpec {
            header: "n".into(),
            column: 1,
            agg: Aggregate::Count,
        })
        .filter(|r| matches!(r, Record::Entity(e) if e.type_id == TypeId::new(TYPE_PROTEIN)))
        .run(records.iter().cloned());

    let summary_html = ndb_renderer::render_html(&percentile_table);

    // ---- Stitch everything into one self-contained HTML file ----
    // CARGO_MANIFEST_DIR resolves to `<repo>/crates/ndb-renderer` at
    // compile time; walk up two levels to the workspace root.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("walk up to workspace root");
    let out_path = workspace_root.join("docs/v2.1-demo.html");
    std::fs::create_dir_all(out_path.parent().unwrap()).expect("mkdir docs");
    let combined = stitch_demo_html(
        &hypergraph_html,
        &parallel_html,
        &pivot_html,
        &summary_html,
        db_dir,
    );
    std::fs::write(&out_path, combined).expect("write demo html");
    println!("Wrote {}", out_path.display());
    println!();
    println!("Open it directly in your browser:");
    println!("  file://{}", out_path.display());
    println!();
    println!("Or attach the MCP server to the same database:");
    println!("  cargo run -p ndb-mcp-server -- --path /tmp/v21-demo-ndb");
}

/// Extract the inner `<body>` content from a self-contained HTML page
/// — the three viz renderers each emit a full `<!DOCTYPE html>...</html>`
/// wrapper, but we want to stitch their inner content under one shared
/// root in the demo file.
fn extract_body(html: &str) -> String {
    if let Some(body_start) = html.find("<body>") {
        let inner = &html[body_start + "<body>".len()..];
        if let Some(body_end) = inner.find("</body>") {
            return inner[..body_end].to_string();
        }
    }
    html.to_string()
}

/// Extract the contents of every `<style>` block — we hoist all CSS to
/// a single `<style>` in the stitched document head.
fn extract_styles(html: &str) -> String {
    let mut out = String::new();
    let mut cursor = html;
    while let Some(open) = cursor.find("<style>") {
        let rest = &cursor[open + "<style>".len()..];
        if let Some(close) = rest.find("</style>") {
            out.push_str(&rest[..close]);
            out.push('\n');
            cursor = &rest[close + "</style>".len()..];
        } else {
            break;
        }
    }
    out
}

fn stitch_demo_html(
    hypergraph: &str,
    parallel: &str,
    pivot: &str,
    summary: &str,
    db_dir: &Path,
) -> String {
    let mut out = String::new();
    out.push_str("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    out.push_str("<title>nDB v2.1 — hypergraph demo</title>\n");
    out.push_str("<style>\n");
    out.push_str("body{margin:0;font-family:system-ui,-apple-system,sans-serif;background:#f7f7f9;color:#222;}\n");
    out.push_str(".wrap{max-width:1180px;margin:0 auto;padding:24px;}\n");
    out.push_str("h1{font-size:28px;margin:0 0 8px;}\n");
    out.push_str("h2{font-size:18px;margin:28px 0 6px;color:#444;border-bottom:1px solid #ddd;padding-bottom:4px;}\n");
    out.push_str("p.lead{color:#555;line-height:1.55;}\n");
    out.push_str(".card{background:#fff;border:1px solid #e5e5ea;border-radius:8px;padding:14px;margin:12px 0;box-shadow:0 1px 2px rgba(0,0,0,0.03);}\n");
    out.push_str(".meta{font-size:13px;color:#777;}\n");
    out.push_str(".meta code{background:#eee;padding:2px 4px;border-radius:3px;}\n");
    out.push_str("table{border-collapse:collapse;margin:4px 0;}\n");
    out.push_str("th,td{border:1px solid #ddd;padding:6px 10px;text-align:left;}\n");
    out.push_str("th{background:#f3f3f6;font-weight:600;}\n");
    out.push_str(".legend{font-size:13px;color:#555;margin:6px 0 0;}\n");
    // Inline the viz-specific CSS rules.
    out.push_str(&extract_styles(hypergraph));
    out.push_str(&extract_styles(parallel));
    out.push_str("</style></head><body>\n");
    out.push_str("<div class=\"wrap\">\n");
    out.push_str("<h1>nDB v2.1 demo — protein complexes</h1>\n");
    out.push_str("<p class=\"lead\">A small biology dataset (15 proteins, 6 protein complexes) seeded into a real nDB database, then visualised through the three v2.1 N-dim renderers. Hover any node or polygon for metadata.</p>\n");
    out.push_str(&format!(
        "<p class=\"meta\">Source database: <code>{}</code> &middot; rendered by <code>cargo run -p ndb-renderer --example v21_demo</code></p>\n",
        db_dir.display(),
    ));

    // 1. Hypergraph
    out.push_str("<h2>1 · Hypergraph diagram (§2.12) — the showcase</h2>\n");
    out.push_str("<p>Entities are labelled nodes; each protein complex is a polygon connecting its members. <strong>This is what a hyperedge looks like</strong> — a single fact connecting <em>N</em> entities, not a binary edge with reified properties. Hover any node or polygon for properties.</p>\n");
    out.push_str("<p class=\"legend\">Node colour = entity type · polygon colour = hyperedge identity · layout = deterministic Fruchterman-Reingold</p>\n");
    out.push_str("<div class=\"card\">");
    out.push_str(&extract_body(hypergraph));
    out.push_str("</div>\n");

    // 2. Parallel coordinates
    out.push_str("<h2>2 · Parallel coordinates (§2.11) — proteins across discovery year × function</h2>\n");
    out.push_str("<p>One axis per dimension; each protein becomes a polyline crossing every axis. Useful for spotting clusters + outliers in N-dim numeric/categorical data. Colour = function class.</p>\n");
    out.push_str("<div class=\"card\">");
    out.push_str(&extract_body(parallel));
    out.push_str("</div>\n");

    // 3. Pivot
    out.push_str("<h2>3 · Pivot table (§2.10) — protein × (pathway / complex) membership</h2>\n");
    out.push_str("<p>Compound column labels (pathway / complex) expand 4-5 dimensional data into a single tabular view. Empty cells = no membership.</p>\n");
    out.push_str("<div class=\"card\">");
    out.push_str(pivot);
    out.push_str("</div>\n");

    // 4. Slicer aggregates
    out.push_str("<h2>4 · Slicer aggregates (§2.3) — discovery-year P50 per function</h2>\n");
    out.push_str("<p>Per-group median of the year-discovered property, computed by the slicer's R-7 percentile aggregate.</p>\n");
    out.push_str("<div class=\"card\">");
    out.push_str(summary);
    out.push_str("</div>\n");

    out.push_str("<p class=\"meta\">Generated by nDB v2.1.0 · <a href=\"https://github.com/goldrag1/nDB\">github.com/goldrag1/nDB</a></p>\n");
    out.push_str("</div>\n");

    // Inline the JS that powers tooltips. Each viz emitted its own
    // hover handler scoped to its CSS classes — concatenate them all
    // (their selectors are disjoint).
    out.push_str("<div id=\"hg-tooltip\"></div>\n");
    out.push_str("<div id=\"pc-tooltip\"></div>\n");
    out.push_str("<script>\n");
    // Hypergraph hover handler
    out.push_str("(function(){var tip=document.getElementById('hg-tooltip');function bind(sel){document.querySelectorAll(sel).forEach(function(n){n.addEventListener('mousemove',function(e){tip.style.display='block';tip.style.left=(e.clientX+12)+'px';tip.style.top=(e.clientY+12)+'px';tip.textContent=n.getAttribute('data-tip')||'';});n.addEventListener('mouseleave',function(){tip.style.display='none';});});}bind('.hg-node');bind('.hg-edge');})();\n");
    // Parallel coords hover handler
    out.push_str("(function(){var tip=document.getElementById('pc-tooltip');document.querySelectorAll('.pc-line').forEach(function(p){p.addEventListener('mousemove',function(e){tip.style.display='block';tip.style.left=(e.clientX+12)+'px';tip.style.top=(e.clientY+12)+'px';tip.textContent=p.getAttribute('data-tip')||'';});p.addEventListener('mouseleave',function(){tip.style.display='none';});});})();\n");
    out.push_str("</script>\n");
    out.push_str("</body></html>\n");
    out
}
