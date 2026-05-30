//! langgraph-ingest — build a "language graph" (scholarly knowledge graph)
//! directly inside nDB, Rust-native.
//!
//! The demo corpus is an OpenAlex citation subgraph. OpenAlex metadata is
//! **CC0 (public domain)** — freely redistributable — so `--fetch` pulls a
//! connected slice once and caches it into the repo; every later run (and
//! the 3D explorer) reads that static cache, never a live API.
//!
//! Why a paper graph: it is the richest **5-dimensional** structure a demo
//! can stand on, and every dimension maps to an nDB feature:
//!   1-3. x/y/z force layout over the CITES topology      (adjacency)
//!   4.   publication year → `as_of` time scrubber        (MVCC time-travel)
//!   5.   abstract/title embedding → semantic projection  (vector_search)
//!   (+)  research field → node colour                    (property)
//!   (+)  citation count → node size / centrality         (property)
//!
//! It also exercises the graph shapes relational engines handle worst:
//!   - **N-ary** `AUTHORED` hyperedges (one paper + N author role-fillers),
//!   - **recursive** CITES chains (the `relation+` traversal the bench
//!     already shows nDB beating `WITH RECURSIVE` on).
//!
//! Writes go straight through the embedded engine (`begin_write` /
//! `put_entity` / `put_hyperedge`) — no seed.json hop, no Python scraper.
//! One transaction per publication year, oldest first, so `as_of(year)`
//! shows the field as it stood that year.
//!
//! Run:
//!     cargo run -p langgraph -- --fetch            # OpenAlex → cached JSON
//!     cargo run -p langgraph                       # ingest cache → .demo-data/langgraph-ndb
//!     cargo run -p langgraph -- /tmp/lg            # custom db dir
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation,
         clippy::cast_sign_loss, clippy::too_many_lines)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use ndb_engine::record::{
    EntityRecord, HyperEdgeRecord, PropertyKeyRecord, Record, RoleNameRecord, TypeNameRecord,
};
use ndb_engine::{
    Distance, Engine, EngineConfig, EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId, Value,
};

// ─── Schema ────────────────────────────────────────────────────────────
const TYPE_PAPER: u32 = 310;
const TYPE_AUTHOR: u32 = 320;
const TYPE_CITES: u32 = 410; // binary hyperedge: citing → cited
const TYPE_AUTHORED: u32 = 420; // N-ary hyperedge: paper + N authors

const PROP_NAME: u32 = 50; // lookup-key indexed (paper title / author name)
const PROP_KIND: u32 = 51; // "paper" | "author"
const PROP_EMBED: u32 = 52; // vector indexed
const PROP_YEAR: u32 = 55;
const PROP_CITATIONS: u32 = 56;
const PROP_FIELD: u32 = 57; // property-btree indexed (top concept)
const PROP_OAID: u32 = 58; // OpenAlex work id (e.g. W2163605009) — "see more" link
const PROP_DOI: u32 = 59; // full DOI URL or "" — "read the paper" link

const ROLE_CITING: u32 = 30;
const ROLE_CITED: u32 = 31;
const ROLE_PAPER: u32 = 32;
const ROLE_AUTHOR: u32 = 33;

const EMBED_DIM: usize = 16;
const MAX_AUTHORS: usize = 8; // cap arity so AUTHORED stays readable
const CACHE_PATH: &str = "tools/langgraph/data/openalex-ml.json";

// ─── Cached corpus shape (committed JSON; CC0 metadata) ─────────────────
#[derive(Serialize, Deserialize, Clone)]
struct Paper {
    id: String,
    title: String,
    year: i64,
    citations: i64,
    field: String,
    /// Full DOI URL (https://doi.org/…) or "" — the "read the paper" link.
    #[serde(default)]
    doi: String,
    authors: Vec<String>,
    /// Ids of cited papers — kept only when the target is also in the set,
    /// so the cache is an internally-connected subgraph.
    cites: Vec<String>,
}

// ─── Deterministic embedding (offline, reproducible) ────────────────────
// Char unigram + bigram hashing into EMBED_DIM buckets, L2-normalised.
// Fed title + field, so papers sharing a field/topic land near each other
// — enough topical signal for the kNN demo without an embedding API.
fn embed(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; EMBED_DIM];
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    for c in &chars {
        let h = (*c as u32).wrapping_mul(2_654_435_761) as usize % EMBED_DIM;
        v[h] += 1.0;
    }
    for w in chars.windows(2) {
        let h = (w[0] as u32)
            .wrapping_mul(31)
            .wrapping_add(w[1] as u32)
            .wrapping_mul(2_654_435_761) as usize
            % EMBED_DIM;
        v[h] += 1.0;
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

// ─── OpenAlex fetch (ureq; CC0 data) ────────────────────────────────────
fn short_id(openalex_url: &str) -> String {
    openalex_url.rsplit('/').next().unwrap_or(openalex_url).to_string()
}

/// Pull the top-cited connected slice of a topic into `Vec<Paper>`. Keeps
/// only CITES edges whose target is also in the slice, so the result is an
/// internally-connected citation subgraph.
fn fetch_openalex(query: &str, target: usize) -> Result<Vec<Paper>, Box<dyn std::error::Error>> {
    let ua = "langgraph-demo (mailto:demo@nDB.example)";
    let filter = format!("default.search:{query},from_publication_date:2012-01-01");
    let select = "id,doi,display_name,publication_year,cited_by_count,referenced_works,concepts,authorships";

    // OpenAlex caps a page at 200; cursor-paginate to reach `target`. (The
    // 200-per-page cap is the only reason the first cut of this demo had
    // exactly 200 papers — nДB itself has no such limit.)
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut cursor = "*".to_string();
    while results.len() < target {
        let resp: serde_json::Value = ureq::get("https://api.openalex.org/works")
            .query("filter", &filter)
            .query("sort", "cited_by_count:desc")
            .query("per-page", "200")
            .query("cursor", &cursor)
            .query("select", select)
            .set("User-Agent", ua)
            .call()?
            .into_json()?;
        let page = resp["results"].as_array().cloned().unwrap_or_default();
        if page.is_empty() {
            break;
        }
        results.extend(page);
        match resp["meta"]["next_cursor"].as_str() {
            Some(c) if !c.is_empty() => cursor = c.to_string(),
            _ => break,
        }
    }
    results.truncate(target);
    // First pass: collect the id set so we can intersect references.
    let id_set: std::collections::HashSet<String> = results
        .iter()
        .filter_map(|w| w["id"].as_str().map(short_id))
        .collect();

    let mut papers = Vec::new();
    for w in &results {
        let Some((id, title, year, citations, field, doi, authors, cites_all)) = work_fields(w)
        else { continue };
        // Keep only references whose target is also in this set (internally-
        // connected subgraph) and never a self-loop.
        let cites = cites_all.into_iter().filter(|rid| id_set.contains(rid) && rid != &id).collect();
        papers.push(Paper { id, title, year, citations, field, doi, authors, cites });
    }
    Ok(papers)
}

/// Decode one OpenAlex `works` JSON object into the graph's fields. The
/// returned `cites_all` is EVERY referenced short-id (not yet intersected
/// with any kept set — the caller decides which targets exist). `None` for a
/// work with no publication year (incomplete record). Shared by the API
/// fetch path and the `--from-spool` ingest so both parse identically.
#[allow(clippy::type_complexity)]
fn work_fields(
    w: &serde_json::Value,
) -> Option<(String, String, i64, i64, String, String, Vec<String>, Vec<String>)> {
    let id = w["id"].as_str().map(short_id)?;
    let year = w["publication_year"].as_i64().unwrap_or(0);
    if year == 0 {
        return None;
    }
    let title = w["display_name"].as_str().unwrap_or("(untitled)").to_string();
    let citations = w["cited_by_count"].as_i64().unwrap_or(0);
    // First concept whose level >= 1 reads as a usable field label.
    let field = w["concepts"]
        .as_array()
        .and_then(|cs| {
            cs.iter().find(|c| c["level"].as_i64().unwrap_or(0) >= 1).or_else(|| cs.first())
        })
        .and_then(|c| c["display_name"].as_str())
        .unwrap_or("Unknown")
        .to_string();
    let authors: Vec<String> = w["authorships"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["author"]["display_name"].as_str().map(str::to_string))
                .take(MAX_AUTHORS)
                .collect()
        })
        .unwrap_or_default();
    let cites_all: Vec<String> = w["referenced_works"]
        .as_array()
        .map(|r| r.iter().filter_map(|x| x.as_str().map(short_id)).collect())
        .unwrap_or_default();
    let doi = w["doi"].as_str().unwrap_or("").to_string();
    Some((id, title, year, citations, field, doi, authors, cites_all))
}

fn load_cache() -> Option<Vec<Paper>> {
    let bytes = std::fs::read(CACHE_PATH).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ─── Resumable spool fetch (the real ~10GB acquisition) ─────────────────
// The full OpenAlex works snapshot is 492M works / 639GB compressed — and
// S3 has no server-side filter, so building a coherent slice from it means
// downloading all 639GB (~60-110h on this link). The API filters
// server-side, so we download ONLY the slice we keep (e.g.
// `cited_by_count:>50` → ~12.3M works, the citation backbone of science),
// projected to the fields the schema needs, gzip-compressed on the wire.
// That's ~12.6GB transferred, not 639GB.
//
// Records are spooled as gzipped JSONL parts (~1KB/record gzip vs ~7KB
// plain → fits the disk budget). The cursor is checkpointed to state.json
// only at part boundaries, so a kill/restart re-fetches at most one
// in-flight batch — no gaps, no dups, no corrupt gz members.
const OA_SELECT: &str =
    "id,doi,display_name,publication_year,cited_by_count,referenced_works,concepts,authorships";
const OA_MAIL: &str = "nguyenhoanglong1@gmail.com";
// Works buffered per shard before a part is flushed. Kept small so the 10
// parallel shard threads stay well under the app's ~2 GB RAM cap: each shard's
// in-flight buffer is ≤ this × ~5 KB/work (10k → ~50 MB/shard → ~500 MB across
// 10 shards). Smaller parts also mean more frequent cursor checkpoints (less
// re-fetch on kill). RAM is bounded by THIS, not by the slice size.
const SPOOL_BATCH_WORKS: u64 = 10_000;

#[derive(Serialize, Deserialize)]
struct SpoolState {
    cursor: String,
    pages: u64,
    works: u64,
    part: u32,
    done: bool,
}
impl Default for SpoolState {
    fn default() -> Self {
        Self { cursor: "*".into(), pages: 0, works: 0, part: 0, done: false }
    }
}

fn load_spool_state(dir: &str) -> SpoolState {
    std::fs::read(format!("{dir}/state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_spool_state(dir: &str, st: &SpoolState) -> std::io::Result<()> {
    let tmp = format!("{dir}/state.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(st)?)?;
    std::fs::rename(tmp, format!("{dir}/state.json")) // atomic checkpoint
}

/// gzip a JSONL buffer to `part_NNNNN.jsonl.gz` atomically (temp + rename),
/// so a half-written file can never be mistaken for a complete part.
fn write_gz_part(dir: &str, part: u32, data: &str) -> std::io::Result<()> {
    let tmp = format!("{dir}/part_{part:05}.jsonl.gz.tmp");
    let fin = format!("{dir}/part_{part:05}.jsonl.gz");
    let f = std::fs::File::create(&tmp)?;
    let mut enc = flate2::write::GzEncoder::new(std::io::BufWriter::new(f), flate2::Compression::default());
    enc.write_all(data.as_bytes())?;
    enc.finish()?; // flush deflate + write gzip trailer
    std::fs::rename(tmp, fin)
}

/// Resolver that returns ONLY IPv4 addresses. The native IPv6 path to
/// Cloudflare (which fronts OpenAlex) on this link stalls long-lived
/// connections mid-read; IPv4 is reliably ~1.2-1.7s/request. ureq otherwise
/// happy-eyeballs onto the flaky v6 address and wedges.
struct V4Only;
impl ureq::Resolver for V4Only {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        Ok(netloc.to_socket_addrs()?.filter(std::net::SocketAddr::is_ipv4).collect())
    }
}

fn build_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(120)) // congested VN→CF link: large pages need >45s to finish
        .timeout_write(Duration::from_secs(30))
        .resolver(V4Only)
        .build()
}

/// One cursor page, with retry/backoff for transient drops on a slow link.
/// Reuses one `Agent` (connection pool + keep-alive) across the whole walk.
fn fetch_oa_page(
    agent: &ureq::Agent,
    filter: &str,
    cursor: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let ua = format!("langgraph-demo (mailto:{OA_MAIL})");
    let mut delay = 1u64;
    for attempt in 0..6 {
        let r = agent
            .get("https://api.openalex.org/works")
            .query("filter", filter)
            .query("select", OA_SELECT)
            .query("per-page", "200")
            .query("cursor", cursor)
            .query("mailto", OA_MAIL)
            .set("User-Agent", &ua)
            .call();
        match r {
            Ok(resp) => return Ok(resp.into_json()?),
            Err(e) => {
                eprintln!("  request error ({e}); retry {}/6 in {delay}s", attempt + 1);
                std::thread::sleep(Duration::from_secs(delay));
                delay = (delay * 2).min(30);
            }
        }
    }
    Err("exhausted retries on a page".into())
}

fn spool_fetch(dir: &str, filter: &str, cap: Option<u64>) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let mut st = load_spool_state(dir);
    if st.done {
        println!("spool already complete: {} works in {} parts → {dir}", st.works, st.part);
        return Ok(());
    }
    if st.cursor.is_empty() {
        st.cursor = "*".into();
    }
    eprintln!(
        "spool: filter={filter:?}  resume={}  works so far={}",
        if st.cursor == "*" { "<start>".into() } else { format!("{}…", &st.cursor[..st.cursor.len().min(16)]) },
        st.works
    );

    let agent = build_agent();
    let mut cursor = st.cursor.clone(); // working cursor (advances every page)
    let mut buf = String::new();
    let mut batch_works = 0u64;
    let t0 = Instant::now();

    loop {
        let data = fetch_oa_page(&agent, filter, &cursor)?;
        let results = data["results"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        if results.is_empty() {
            st.done = true;
        } else {
            for w in results {
                buf.push_str(&serde_json::to_string(w)?);
                buf.push('\n');
            }
            let n = results.len() as u64;
            st.works += n;
            st.pages += 1;
            batch_works += n;
        }
        let nxt = data["meta"]["next_cursor"].as_str().map(str::to_string);
        cursor = nxt.clone().unwrap_or_default();
        let no_more = nxt.as_deref().map_or(true, str::is_empty);
        let hit_cap = cap.is_some_and(|c| st.works >= c);

        if batch_works >= SPOOL_BATCH_WORKS || no_more || hit_cap {
            if !buf.is_empty() {
                write_gz_part(dir, st.part, &buf)?;
                st.part += 1;
                buf = String::new(); // drop capacity, not just len → frees the ~50 MB
                batch_works = 0;
            }
            st.cursor = cursor.clone(); // checkpoint resume point = next batch start
            if no_more {
                st.done = true;
            }
            save_spool_state(dir, &st)?;
        }
        if st.done || hit_cap {
            break;
        }
        if st.pages % 50 == 0 {
            let rate = st.works as f64 / t0.elapsed().as_secs_f64().max(1e-6);
            eprintln!("  {} works ({} pages, {rate:.0}/s this run, part {})", st.works, st.pages, st.part);
        }
        // Per-shard inter-page delay. This is PER SHARD — with N parallel
        // shards the aggregate is N×(this rate). At 1000ms + ~1.2s RTT each
        // shard does ~0.45 req/s → ~4.5 req/s across 10 shards, comfortably
        // under OpenAlex's 10/s polite cap (the old 120ms × 10 shards = ~80
        // req/s burst is what tripped the 429 storm).
        std::thread::sleep(Duration::from_millis(1000));
    }
    println!("spool DONE: {} works across {} parts → {dir}", st.works, st.part);
    Ok(())
}

/// One `group_by=publication_year` call → per-year counts (sorted), used to
/// split the slice into balanced year ranges.
fn fetch_year_histogram(
    agent: &ureq::Agent,
    base_filter: &str,
) -> Result<Vec<(i32, u64)>, Box<dyn std::error::Error>> {
    // Retry/backoff — a transient 429 here must NOT kill the whole sharded
    // run (it's the first call; without this a single rate-limit bounce sends
    // the supervisor into a restart→429 storm). 429 = slow down → back off
    // hard (starts 5s, doubles to 60s).
    let ua = format!("langgraph-demo (mailto:{OA_MAIL})");
    let mut v: Option<serde_json::Value> = None;
    let mut delay = 5u64;
    for attempt in 0..7 {
        match agent
            .get("https://api.openalex.org/works")
            .query("filter", base_filter)
            .query("group_by", "publication_year")
            .query("mailto", OA_MAIL)
            .set("User-Agent", &ua)
            .call()
        {
            Ok(resp) => {
                v = Some(resp.into_json()?);
                break;
            }
            Err(e) => {
                eprintln!("  histogram error ({e}); retry {}/7 in {delay}s", attempt + 1);
                std::thread::sleep(Duration::from_secs(delay));
                delay = (delay * 2).min(60);
            }
        }
    }
    let v = v.ok_or("histogram: exhausted retries")?;
    let mut hist: Vec<(i32, u64)> = v["group_by"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|g| {
            let y = g["key"].as_str()?.parse::<i32>().ok()?;
            Some((y, g["count"].as_u64().unwrap_or(0)))
        })
        .collect();
    hist.sort_unstable();
    Ok(hist)
}

/// Partition the year histogram into `n` contiguous ranges of ~equal work
/// count (cumulative greedy). Balanced shards → the slowest shard (= total
/// wall time, since they run in parallel) is minimised.
fn partition_year_shards(hist: &[(i32, u64)], n: usize) -> Vec<(i32, i32)> {
    if hist.is_empty() {
        return vec![(0, 9999)];
    }
    let total: u64 = hist.iter().map(|(_, c)| c).sum();
    let target = (total / n as u64).max(1);
    let mut buckets = Vec::new();
    let mut lo = hist[0].0;
    let mut acc = 0u64;
    for (y, c) in hist {
        acc += c;
        if acc >= target && buckets.len() < n - 1 {
            buckets.push((lo, *y));
            lo = *y + 1;
            acc = 0;
        }
    }
    buckets.push((lo, hist.last().unwrap().0));
    buckets
}

/// Sharded bulk fetch: split the slice by publication year into `n_shards`
/// balanced ranges and walk them on parallel threads (each an independent,
/// resumable `spool_fetch` into its own subdir). Cursor paging is sequential
/// PER filter, so parallel year-ranges are the only way to beat the
/// single-stream RTT wall (~53h → ~5-7h). Combined request rate (~n×0.3/s)
/// stays well under OpenAlex's 10/s polite cap. Year-bucketed spools also
/// give the ingest its oldest-first ordering for free.
fn spool_sharded(
    dir: &str,
    base_filter: &str,
    n_shards: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    // Cache the shard ranges in shards.json: they're stable across the whole
    // run, so a supervisor restart must NOT re-call the histogram (that's the
    // request that 429-storms). Compute once (with retry), persist, then every
    // resume loads it and goes straight to the shard threads.
    let shards_path = format!("{dir}/shards.json");
    let shards: Vec<(i32, i32)> = match std::fs::read(&shards_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<(i32, i32)>>(&b).ok())
    {
        Some(s) if !s.is_empty() => {
            eprintln!("sharded spool: loaded {} cached shard ranges from shards.json", s.len());
            s
        }
        _ => {
            let agent = build_agent();
            let hist = fetch_year_histogram(&agent, base_filter)?;
            let total: u64 = hist.iter().map(|(_, c)| c).sum();
            let s = partition_year_shards(&hist, n_shards);
            eprintln!("sharded spool: {total} works over {} year-shards (computed + cached)", s.len());
            let _ = std::fs::write(&shards_path, serde_json::to_vec(&s).unwrap_or_default());
            s
        }
    };
    for (lo, hi) in &shards {
        eprintln!("  shard {lo}-{hi}");
    }

    let mut handles = Vec::new();
    for (i, (lo, hi)) in shards.clone().into_iter().enumerate() {
        let dir = dir.to_string();
        let bf = base_filter.to_string();
        handles.push(std::thread::spawn(move || {
            // Stagger starts so 10 shards don't fire their first request in
            // the same instant (avoids the startup burst that trips the 429).
            std::thread::sleep(Duration::from_millis(3000 * i as u64));
            let shard_dir = format!("{dir}/shard_{lo:04}_{hi:04}");
            let filter = format!("{bf},publication_year:{lo}-{hi}");
            if let Err(e) = spool_fetch(&shard_dir, &filter, None) {
                eprintln!("shard {lo}-{hi} error: {e}");
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }

    let (mut done, mut works) = (0usize, 0u64);
    for (lo, hi) in &shards {
        let st = load_spool_state(&format!("{dir}/shard_{lo:04}_{hi:04}"));
        if st.done {
            done += 1;
        }
        works += st.works;
    }
    println!("sharded spool: {done}/{} shards done, {works} works total → {dir}", shards.len());
    if done < shards.len() {
        return Err("some shards incomplete — rerun to resume".into());
    }
    Ok(())
}

// ─── Schema registration + ingest ──────────────────────────────────────
/// Index registrations that decide which sidecars (.pidx / .vidx / .lkp) get
/// emitted by `write_index_sidecars` — at flush AND at compaction. EVERY
/// writer that flushes or compacts this DB (ingest + `--compact`) MUST apply
/// these first, or the matching sidecar is silently dropped and the reader
/// falls back to a RAM rebuild (defeats low-memory serving). Single source of
/// truth — keep `--compact` and `register_schema` in sync via this helper.
fn register_index_props(engine: &mut Engine) {
    engine.register_lookup_key(PropertyId::new(PROP_NAME));
    // citations drives langgraph-server's /view/top (property_top_k); field
    // is kept for completeness. A reader can't index pairs the sidecar omits.
    engine.register_property_btree(TypeId::new(TYPE_PAPER), PropertyId::new(PROP_CITATIONS));
    engine.register_property_btree(TypeId::new(TYPE_PAPER), PropertyId::new(PROP_FIELD));
    engine.register_vector_property(PropertyId::new(PROP_EMBED));
}

fn register_schema(engine: &mut Engine) {
    register_index_props(engine);

    let mut tx = engine.begin_write();
    for (id, name) in [
        (TYPE_PAPER, "paper"),
        (TYPE_AUTHOR, "author"),
        (TYPE_CITES, "cites"),
        (TYPE_AUTHORED, "authored"),
    ] {
        tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(id), name: name.into() }));
    }
    for (id, name) in [
        (ROLE_CITING, "citing"),
        (ROLE_CITED, "cited"),
        (ROLE_PAPER, "paper"),
        (ROLE_AUTHOR, "author"),
    ] {
        tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(id), name: name.into() }));
    }
    for (id, name) in [
        (PROP_NAME, "name"),
        (PROP_KIND, "kind"),
        (PROP_EMBED, "embedding"),
        (PROP_YEAR, "year"),
        (PROP_CITATIONS, "citations"),
        (PROP_FIELD, "field"),
        (PROP_OAID, "openalex_id"),
        (PROP_DOI, "doi"),
    ] {
        tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(id), name: name.into() }));
    }
    tx.commit().unwrap();
}

fn ensure_author(
    tx: &mut ndb_engine::WriteTxn<'_>,
    authors: &mut HashMap<String, EntityId>,
    name: &str,
) -> EntityId {
    if let Some(eid) = authors.get(name) {
        return *eid;
    }
    let eid = EntityId::now_v7();
    tx.put_entity(EntityRecord {
        entity_id: eid,
        type_id: TypeId::new(TYPE_AUTHOR),
        tx_id_assert: TxId::new(0),
        tx_id_supersede: TxId::ACTIVE,
        properties: vec![
            (PropertyId::new(PROP_NAME), Value::String(name.to_string())),
            (PropertyId::new(PROP_KIND), Value::String("author".into())),
            (PropertyId::new(PROP_EMBED), Value::Vector(embed(name))),
        ],
    });
    authors.insert(name.to_string(), eid);
    eid
}

struct Ingested {
    papers: usize,
    authors: usize,
    cites: usize,
    authored: usize,
    /// (year, committed tx) ascending — the time-travel timeline.
    timeline: Vec<(i64, TxId)>,
    /// OpenAlex id → entity id (consumed by the explorer wiring + tests).
    #[allow(dead_code)]
    paper_ids: HashMap<String, EntityId>,
}

/// Ingest a paper set, one transaction per publication year (oldest
/// first). A paper, its authors, its AUTHORED edge, and its CITES edges to
/// already-created targets all land in that year's tx — so `as_of(year)`
/// is an honest snapshot of the field at that year.
fn ingest_papers(engine: &mut Engine, papers: &[Paper]) -> Ingested {
    let mut by_year: BTreeMap<i64, Vec<&Paper>> = BTreeMap::new();
    for p in papers {
        by_year.entry(p.year).or_default().push(p);
    }

    let mut paper_ids: HashMap<String, EntityId> = HashMap::new();
    let mut author_ids: HashMap<String, EntityId> = HashMap::new();
    let mut timeline = Vec::new();
    let (mut n_cites, mut n_authored) = (0usize, 0usize);
    // Flush the memtable to an SSTable every ~50k records so ingest RAM
    // stays bounded — without this the whole graph lives in the memtable
    // and a large (multi-GB) ingest OOMs before it finishes.
    let mut since_flush = 0usize;

    for (year, group) in by_year {
        let mut tx = engine.begin_write();
        let cites0 = n_cites;
        for p in &group {
            // Paper entity (dedup by OpenAlex id).
            let pid = *paper_ids.entry(p.id.clone()).or_insert_with(EntityId::now_v7);
            tx.put_entity(EntityRecord {
                entity_id: pid,
                type_id: TypeId::new(TYPE_PAPER),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(PROP_NAME), Value::String(p.title.clone())),
                    (PropertyId::new(PROP_KIND), Value::String("paper".into())),
                    (PropertyId::new(PROP_YEAR), Value::I64(p.year)),
                    (PropertyId::new(PROP_CITATIONS), Value::I64(p.citations)),
                    (PropertyId::new(PROP_FIELD), Value::String(p.field.clone())),
                    (PropertyId::new(PROP_OAID), Value::String(p.id.clone())),
                    (PropertyId::new(PROP_DOI), Value::String(p.doi.clone())),
                    (PropertyId::new(PROP_EMBED), Value::Vector(embed(&format!("{} {}", p.title, p.field)))),
                ],
            });

            // N-ary AUTHORED: paper + each author.
            if !p.authors.is_empty() {
                let mut roles = vec![(RoleId::new(ROLE_PAPER), pid)];
                for a in &p.authors {
                    let aid = ensure_author(&mut tx, &mut author_ids, a);
                    roles.push((RoleId::new(ROLE_AUTHOR), aid));
                }
                tx.put_hyperedge(HyperEdgeRecord {
                    hyperedge_id: HyperedgeId::now_v7(),
                    type_id: TypeId::new(TYPE_AUTHORED),
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    roles,
                    hyperedge_roles: Vec::new(),
                    properties: vec![],
                });
                n_authored += 1;
            }

            // CITES edges to targets already created (older or same-year-
            // earlier papers). Forward refs are skipped — citations point
            // backward in time.
            for cited in &p.cites {
                if let Some(&cid) = paper_ids.get(cited) {
                    tx.put_hyperedge(HyperEdgeRecord {
                        hyperedge_id: HyperedgeId::now_v7(),
                        type_id: TypeId::new(TYPE_CITES),
                        tx_id_assert: TxId::new(0),
                        tx_id_supersede: TxId::ACTIVE,
                        roles: vec![
                            (RoleId::new(ROLE_CITING), pid),
                            (RoleId::new(ROLE_CITED), cid),
                        ],
                        hyperedge_roles: Vec::new(),
                        properties: vec![],
                    });
                    n_cites += 1;
                }
            }
        }
        let tx_id = tx.commit().unwrap();
        timeline.push((year, tx_id));
        // ~5 records/paper (entity + authors + authored) + cites added.
        since_flush += group.len() * 5 + (n_cites - cites0);
        if since_flush >= 50_000 {
            engine.flush().unwrap();   // promote memtable → SSTable, free RAM
            since_flush = 0;
        }
    }
    engine.flush().unwrap();           // final flush

    Ingested {
        papers: paper_ids.len(),
        authors: author_ids.len(),
        cites: n_cites,
        authored: n_authored,
        timeline,
        paper_ids,
    }
}

/// Sorted list of `part_*.jsonl.gz` files under `dir` (recurses into the
/// per-shard subdirs a `--spool-sharded` run creates). Sorted so a
/// year-bucketed sharded spool is visited oldest-range-first.
fn spool_parts(dir: &str) -> std::io::Result<Vec<PathBuf>> {
    fn collect(d: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for e in std::fs::read_dir(d)? {
            let p = e?.path();
            if p.is_dir() {
                collect(&p, out)?;
            } else if p.extension().is_some_and(|x| x == "gz")
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("part_"))
            {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut parts = Vec::new();
    collect(Path::new(dir), &mut parts)?;
    parts.sort();
    Ok(parts)
}

/// Stream the decoded works of one gzipped JSONL part, calling `f` per work.
/// `MultiGzDecoder` handles a part written as several concatenated gz members
/// (the spool flushes one member per part, but this is robust either way).
fn for_each_work_in_part(
    path: &Path,
    mut f: impl FnMut(serde_json::Value),
) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    let dec = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(file));
    for line in std::io::BufReader::new(dec).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            f(v);
        }
    }
    Ok(())
}

/// Numeric part of an OpenAlex short id ("W2029397690" → 2029397690). The
/// numeric id is stable + unique, so we DON'T store an id→EntityId map: the
/// EntityId is DERIVED from it (`eid_for`). `None` if not a `W<digits>` id.
fn work_num(short_id: &str) -> Option<u64> {
    short_id.strip_prefix('W').and_then(|d| d.parse::<u64>().ok())
}

/// Deterministic EntityId for a work number — collision-free across works
/// (distinct numbers → distinct u128 → distinct uuid). Lets `--from-spool`
/// recompute any paper's EntityId on the fly instead of holding a 12.3M-entry
/// id→EntityId map: the only retained set is a `HashSet<u64>` membership
/// (~200 MB at full scale) — the difference between fitting the 2 GB cap and not.
fn eid_for(num: u64) -> EntityId {
    EntityId::from_uuid(uuid::Uuid::from_u128(u128::from(num)))
}

/// Two-pass streaming ingest of a `--spool` directory into nDB under the app's
/// ~2 GB RAM cap. Pass 1 builds a `HashSet<u64>` of kept (year != 0) work
/// numbers — membership only, ~16 B/entry. Pass 2 streams works, deriving every
/// EntityId via `eid_for` (no stored map) and wiring EVERY in-slice citation,
/// flushing the memtable every ~50k records. **Citation backbone only — papers
/// + CITES, no authors** (the explorer renders papers/clusters/cites, not
/// authors; the author map was the unbounded RAM term). Run `--compact` after.
fn ingest_from_spool(
    engine: &mut Engine,
    dir: &str,
) -> Result<Ingested, Box<dyn std::error::Error>> {
    let parts = spool_parts(dir)?;
    if parts.is_empty() {
        return Err(format!("no part_*.jsonl.gz under {dir}").into());
    }
    eprintln!("from-spool: {} parts under {dir}", parts.len());

    // Pass 1 — membership set of kept work numbers (bounded ~16 B/entry).
    let t0 = Instant::now();
    let mut kept: HashSet<u64> = HashSet::new();
    let mut scanned = 0u64;
    for path in &parts {
        for_each_work_in_part(path, |w| {
            scanned += 1;
            if w["publication_year"].as_i64().unwrap_or(0) == 0 {
                return;
            }
            if let Some(n) = w["id"].as_str().and_then(|s| work_num(&short_id(s))) {
                kept.insert(n);
            }
        })?;
    }
    eprintln!(
        "  pass 1/2: {scanned} works scanned, {} kept papers ({:.0}s)",
        kept.len(), t0.elapsed().as_secs_f64()
    );

    // Pass 2 — stream entities + CITES, flushing every ~50k records.
    let (mut n_cites, mut n_papers) = (0usize, 0usize);
    let mut since_flush = 0usize;
    let mut last_tx = TxId::new(0);
    let mut tx = engine.begin_write();
    let mut commit_and_flush = false;
    for path in &parts {
        for_each_work_in_part(path, |w| {
            let Some((id, title, year, citations, field, doi, _authors, cites_all)) = work_fields(&w)
            else { return };
            let Some(num) = work_num(&id) else { return };
            let pid = eid_for(num);
            tx.put_entity(EntityRecord {
                entity_id: pid,
                type_id: TypeId::new(TYPE_PAPER),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(PROP_NAME), Value::String(title.clone())),
                    (PropertyId::new(PROP_KIND), Value::String("paper".into())),
                    (PropertyId::new(PROP_YEAR), Value::I64(year)),
                    (PropertyId::new(PROP_CITATIONS), Value::I64(citations)),
                    (PropertyId::new(PROP_FIELD), Value::String(field.clone())),
                    (PropertyId::new(PROP_OAID), Value::String(id.clone())),
                    (PropertyId::new(PROP_DOI), Value::String(doi.clone())),
                    (PropertyId::new(PROP_EMBED), Value::Vector(embed(&format!("{title} {field}")))),
                ],
            });
            n_papers += 1;
            since_flush += 1;
            // Every EntityId is derivable → wire every in-slice citation.
            for cited in &cites_all {
                if let Some(cnum) = work_num(cited)
                    && cnum != num
                    && kept.contains(&cnum)
                {
                    tx.put_hyperedge(HyperEdgeRecord {
                        hyperedge_id: HyperedgeId::now_v7(),
                        type_id: TypeId::new(TYPE_CITES),
                        tx_id_assert: TxId::new(0),
                        tx_id_supersede: TxId::ACTIVE,
                        roles: vec![
                            (RoleId::new(ROLE_CITING), pid),
                            (RoleId::new(ROLE_CITED), eid_for(cnum)),
                        ],
                        hyperedge_roles: Vec::new(),
                        properties: vec![],
                    });
                    n_cites += 1;
                    since_flush += 1;
                }
            }
            commit_and_flush = since_flush >= 50_000;
        })?;
        // Commit + flush at part boundaries once the window is full (keeps the
        // memtable bounded; tx can't be committed inside the borrow closure).
        if commit_and_flush {
            last_tx = tx.commit()?;
            engine.flush()?;
            tx = engine.begin_write();
            since_flush = 0;
            commit_and_flush = false;
            eprintln!("  pass 2/2: {n_papers} papers, {n_cites} cites…");
        }
    }
    last_tx = tx.commit().unwrap_or(last_tx);
    engine.flush()?;
    eprintln!(
        "  pass 2/2: done — {n_papers} papers, {n_cites} cites ({:.0}s total)",
        t0.elapsed().as_secs_f64()
    );

    Ok(Ingested {
        papers: n_papers,
        authors: 0,
        cites: n_cites,
        authored: 0,
        timeline: vec![(0, last_tx)],
        paper_ids: HashMap::new(),
    })
}

/// Export the whole graph to a viz-ready JSON the static 3D explorer
/// reads. nDB produces the feed: nodes carry the five demo dimensions
/// (year / field / citations / embedding + kind); links flatten CITES
/// (paper→paper) and the N-ary AUTHORED edge (paper→each author) so a
/// force layout can render them. Authors inherit the earliest year of
/// any paper they wrote, so the time scrubber reveals them with their
/// debut.
/// Derive the small cluster aggregate (top-18 fields + "Other", counts,
/// max citations, year range) by a bounded streaming scan and write it to
/// `<db_dir>/clusters.json`. The server reads this instead of materialising
/// every paper. Keys mirror the in-server lean reader.
/// Stream-ingest `n` synthetic papers (+ a CITES chain) in batches so a
/// large langgraph-schema nDB can be built for the scale/RSS test without
/// holding millions of `Paper` structs in RAM. Uses the same schema as
/// `ingest_papers`, so `langgraph-server` reads it identically.
fn synthetic_ingest(engine: &mut Engine, n: usize) {
    const FIELDS: [&str; 20] = [
        "Artificial intelligence", "Machine translation", "Computer vision", "Reinforcement learning",
        "Natural language processing", "Speech recognition", "Robotics", "Optimization",
        "Graph theory", "Information retrieval", "Bioinformatics", "Cryptography",
        "Distributed systems", "Databases", "Computer graphics", "Quantum computing",
        "Statistics", "Signal processing", "Recommender systems", "Knowledge graphs",
    ];
    const BATCH: usize = 5000;
    const FLUSH_EVERY: usize = 250_000;
    let mut prev: Option<EntityId> = None;
    let mut since_flush = 0usize;
    let mut i = 0usize;
    while i < n {
        let mut tx = engine.begin_write();
        let end = (i + BATCH).min(n);
        for j in i..end {
            let eid = EntityId::now_v7();
            let field = FIELDS[j % FIELDS.len()];
            let title = format!("Synthetic paper {j} on {field}");
            tx.put_entity(EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(TYPE_PAPER),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(PROP_NAME), Value::String(title.clone())),
                    (PropertyId::new(PROP_KIND), Value::String("paper".into())),
                    (PropertyId::new(PROP_YEAR), Value::I64(2012 + (j % 14) as i64)),
                    (PropertyId::new(PROP_CITATIONS), Value::I64(((j.wrapping_mul(2_654_435_761)) % 100_000) as i64)),
                    (PropertyId::new(PROP_FIELD), Value::String(field.into())),
                    (PropertyId::new(PROP_OAID), Value::String(format!("W{j}"))),
                    (PropertyId::new(PROP_DOI), Value::String(String::new())),
                    (PropertyId::new(PROP_EMBED), Value::Vector(embed(&title))),
                ],
            });
            if let Some(p) = prev {
                tx.put_hyperedge(HyperEdgeRecord {
                    hyperedge_id: HyperedgeId::now_v7(),
                    type_id: TypeId::new(TYPE_CITES),
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    roles: vec![(RoleId::new(ROLE_CITING), eid), (RoleId::new(ROLE_CITED), p)],
                    hyperedge_roles: vec![],
                    properties: vec![],
                });
            }
            prev = Some(eid);
            since_flush += 1;
        }
        tx.commit().unwrap();
        if since_flush >= FLUSH_EVERY {
            engine.flush().unwrap();
            since_flush = 0;
            eprintln!("  {} papers ingested", end);
        }
        i = end;
    }
    engine.flush().unwrap();
}

fn write_clusters_meta(engine: &Engine, db_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;
    let mut field_count: HashMap<String, usize> = HashMap::new();
    let mut max_cit: i64 = 1;
    let mut min_year = i64::MAX;
    let mut max_year = i64::MIN;
    let mut total = 0usize;
    for item in engine.snapshot_iter_streaming(TxId::ACTIVE) {
        let Ok(Record::Entity(e)) = item else { continue };
        if e.type_id != TypeId::new(TYPE_PAPER) {
            continue;
        }
        total += 1;
        let f = e.properties.iter().find(|(p, _)| p.get() == PROP_FIELD)
            .and_then(|(_, v)| if let Value::String(s) = v { Some(s.clone()) } else { None })
            .unwrap_or_default();
        *field_count.entry(f).or_default() += 1;
        for (p, v) in &e.properties {
            match (p.get(), v) {
                (PROP_CITATIONS, Value::I64(n)) => max_cit = max_cit.max(*n),
                (PROP_YEAR, Value::I64(y)) if *y > 0 => {
                    min_year = min_year.min(*y);
                    max_year = max_year.max(*y);
                }
                _ => {}
            }
        }
    }
    let mut fc: Vec<(String, usize)> = field_count.into_iter().collect();
    fc.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top: std::collections::HashSet<String> = fc.iter().take(18).map(|(f, _)| f.clone()).collect();
    let mut coarse: BTreeMap<String, usize> = BTreeMap::new();
    for (f, c) in &fc {
        *coarse.entry(if top.contains(f) { f.clone() } else { "Other".into() }).or_default() += c;
    }
    let clusters: Vec<(String, usize)> = coarse.into_iter().collect();
    if min_year == i64::MAX { min_year = 2020; max_year = 2020; }
    let meta = serde_json::json!({
        "clusters": clusters, "max_cit": max_cit,
        "min_year": min_year, "max_year": max_year, "total": total,
    });
    std::fs::write(format!("{db_dir}/clusters.json"), serde_json::to_vec(&meta)?)?;
    Ok(())
}

fn export_graph(engine: &Engine, path: &str) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    use serde_json::json;
    let records = engine.snapshot_iter(TxId::ACTIVE)?;

    let uuid = |e: EntityId| e.into_uuid().to_string();
    let str_prop = |props: &[(PropertyId, Value)], pid: u32| -> Option<String> {
        props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
    };
    let i64_prop = |props: &[(PropertyId, Value)], pid: u32| -> i64 {
        props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
            Value::I64(n) => Some(*n),
            _ => None,
        }).unwrap_or(0)
    };
    let vec_prop = |props: &[(PropertyId, Value)], pid: u32| -> Vec<f32> {
        props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
            Value::Vector(x) => Some(x.clone()),
            _ => None,
        }).unwrap_or_default()
    };

    // author id → display name; paper id → year.
    let author_name: HashMap<String, String> = records.iter().filter_map(|r| match r {
        Record::Entity(e) if e.type_id == TypeId::new(TYPE_AUTHOR) =>
            Some((uuid(e.entity_id), str_prop(&e.properties, PROP_NAME).unwrap_or_default())),
        _ => None,
    }).collect();
    let paper_year: HashMap<String, i64> = records.iter().filter_map(|r| match r {
        Record::Entity(e) if e.type_id == TypeId::new(TYPE_PAPER) =>
            Some((uuid(e.entity_id), i64_prop(&e.properties, PROP_YEAR))),
        _ => None,
    }).collect();

    // Walk AUTHORED once: per-paper author ids, per-author paper count,
    // per-author debut year. nDB stores every author; the VIZ renders only
    // "bridge" authors (≥2 papers in the set) — the co-authorship structure
    // that actually connects the graph — while a paper's details panel
    // still lists all of its authors by name. Without this, ~4.5k
    // single-paper leaf authors swamp the force layout.
    let mut paper_authors: HashMap<String, Vec<String>> = HashMap::new(); // paper → author ids
    let mut author_count: HashMap<String, usize> = HashMap::new();
    let mut author_year: HashMap<String, i64> = HashMap::new();
    for r in &records {
        if let Record::HyperEdge(h) = r {
            if h.type_id != TypeId::new(TYPE_AUTHORED) { continue; }
            let paper = h.roles.iter().find(|(r, _)| r.get() == ROLE_PAPER).map(|(_, e)| uuid(*e));
            let Some(paper) = paper else { continue };
            let py = paper_year.get(&paper).copied().unwrap_or(0);
            for (rid, aid) in &h.roles {
                if rid.get() != ROLE_AUTHOR { continue; }
                let a = uuid(*aid);
                paper_authors.entry(paper.clone()).or_default().push(a.clone());
                *author_count.entry(a.clone()).or_default() += 1;
                let y = author_year.entry(a).or_insert(i64::MAX);
                if py > 0 { *y = (*y).min(py); }
            }
        }
    }
    let is_bridge = |aid: &str| author_count.get(aid).copied().unwrap_or(0) >= 2;

    let mut nodes = Vec::new();
    let mut links = Vec::new();

    // Nodes: every paper (carrying the full author-name list for its
    // details panel) + only bridge-author entities.
    for r in &records {
        if let Record::Entity(e) = r {
            let id = uuid(e.entity_id);
            if e.type_id == TypeId::new(TYPE_PAPER) {
                let authors: Vec<String> = paper_authors.get(&id).map(|ids|
                    ids.iter().filter_map(|a| author_name.get(a).cloned()).collect()).unwrap_or_default();
                nodes.push(json!({
                    "id": id,
                    "label": str_prop(&e.properties, PROP_NAME).unwrap_or_default(),
                    "kind": "paper",
                    "year": i64_prop(&e.properties, PROP_YEAR),
                    "field": str_prop(&e.properties, PROP_FIELD).unwrap_or_else(|| "Unknown".into()),
                    "citations": i64_prop(&e.properties, PROP_CITATIONS),
                    "oaid": str_prop(&e.properties, PROP_OAID).unwrap_or_default(),
                    "doi": str_prop(&e.properties, PROP_DOI).unwrap_or_default(),
                    "authors": authors,
                    "embedding": vec_prop(&e.properties, PROP_EMBED),
                }));
            } else if e.type_id == TypeId::new(TYPE_AUTHOR) && is_bridge(&id) {
                let y = author_year.get(&id).copied().unwrap_or(0);
                nodes.push(json!({
                    "id": id,
                    "label": str_prop(&e.properties, PROP_NAME).unwrap_or_default(),
                    "kind": "author",
                    "year": if y == i64::MAX { 0 } else { y },
                    "field": "Author",
                    "citations": 0,
                    "embedding": vec_prop(&e.properties, PROP_EMBED),
                }));
            }
        }
    }

    // Links: all CITES; AUTHORED only to bridge authors (the only ones
    // with a node).
    for r in &records {
        if let Record::HyperEdge(h) = r {
            let role = |rid: u32| h.roles.iter().find(|(r, _)| r.get() == rid).map(|(_, e)| uuid(*e));
            if h.type_id == TypeId::new(TYPE_CITES) {
                if let (Some(s), Some(t)) = (role(ROLE_CITING), role(ROLE_CITED)) {
                    links.push(json!({"source": s, "target": t, "kind": "cites"}));
                }
            } else if h.type_id == TypeId::new(TYPE_AUTHORED) {
                if let Some(paper) = role(ROLE_PAPER) {
                    for (rid, aid) in &h.roles {
                        if rid.get() == ROLE_AUTHOR {
                            let a = uuid(*aid);
                            if is_bridge(&a) {
                                links.push(json!({"source": paper.clone(), "target": a, "kind": "authored"}));
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Cluster super-nodes (Step 2 / far-zoom tier) ──────────────────
    // Coarsen fields to the top-K by paper count (rest → "Other"), make one
    // fixed super-node per coarse field on a ring, and add invisible
    // paper→cluster "member" springs so the force layout pulls papers into
    // field "galaxies". When zoomed out the explorer shows only these
    // clusters; zoom in and they give way to the papers. This is what lets
    // the view stay bounded — and meaningful — at any nДB size.
    use std::collections::BTreeMap;
    let mut field_count: HashMap<String, usize> = HashMap::new();
    for n in &nodes {
        if n["kind"] == "paper" {
            *field_count.entry(n["field"].as_str().unwrap_or("Unknown").to_string()).or_default() += 1;
        }
    }
    let mut fc: Vec<(String, usize)> = field_count.into_iter().collect();
    fc.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top_fields: std::collections::HashSet<String> = fc.iter().take(18).map(|(f, _)| f.clone()).collect();
    let coarse = |f: &str| if top_fields.contains(f) { f.to_string() } else { "Other".to_string() };

    let mut cluster_count: BTreeMap<String, usize> = BTreeMap::new();
    for n in &nodes {
        if n["kind"] == "paper" {
            *cluster_count.entry(coarse(n["field"].as_str().unwrap_or("Unknown"))).or_default() += 1;
        }
    }
    let k = cluster_count.len().max(1);
    let ring = 850.0_f64;
    for (i, (field, count)) in cluster_count.iter().enumerate() {
        let ang = std::f64::consts::TAU * (i as f64) / (k as f64);
        nodes.push(json!({
            "id": format!("cluster:{field}"), "label": field, "kind": "cluster",
            "field": field, "count": count, "year": 0, "citations": 0,
            "fx": ring * ang.cos(), "fy": ring * ang.sin(), "fz": 0.0,
        }));
    }
    // Tag papers with their coarse cluster (for colour) + emit member springs.
    for n in &mut nodes {
        if n["kind"] == "paper" {
            let f = coarse(n["field"].as_str().unwrap_or("Unknown"));
            n["cluster"] = json!(f);
            links.push(json!({"source": n["id"].clone(), "target": format!("cluster:{f}"), "kind": "member"}));
        }
    }

    let papers_n = nodes.iter().filter(|n| n["kind"] == "paper").count();
    let authors_shown = nodes.iter().filter(|n| n["kind"] == "author").count();
    let doc = json!({
        "nodes": nodes,
        "links": links,
        // The viz renders a sample; nDB holds the full set. Surfaced so the
        // UI can say "514 of 4,583 authors" honestly.
        "meta": { "papers": papers_n, "authors_total": author_name.len(), "authors_shown": authors_shown },
    });
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec(&doc)?)?;
    Ok((doc["nodes"].as_array().map_or(0, Vec::len), doc["links"].as_array().map_or(0, Vec::len)))
}

/// Count `PAPER` entities visible at a snapshot — the time-travel probe.
fn count_papers_at(engine: &Engine, tx: TxId) -> usize {
    engine
        .snapshot_iter(tx)
        .unwrap()
        .iter()
        .filter(|r| matches!(r, Record::Entity(e) if e.type_id == TypeId::new(TYPE_PAPER)))
        .count()
}

// ─── Tiny network-free corpus for tests + offline fallback ──────────────
fn synthetic_papers() -> Vec<Paper> {
    let mk = |id: &str, title: &str, year: i64, cites_n: i64, field: &str, authors: &[&str], cites: &[&str]| Paper {
        id: id.into(),
        title: title.into(),
        year,
        citations: cites_n,
        field: field.into(),
        doi: String::new(),
        authors: authors.iter().map(|s| (*s).to_string()).collect(),
        cites: cites.iter().map(|s| (*s).to_string()).collect(),
    };
    vec![
        mk("W1", "A Neural Probabilistic Language Model", 2012, 9000, "NLP", &["Bengio", "Ducharme"], &[]),
        mk("W2", "ImageNet Classification with Deep CNNs", 2012, 90000, "Computer Vision", &["Krizhevsky", "Sutskever", "Hinton"], &["W1"]),
        mk("W3", "Sequence to Sequence Learning", 2014, 30000, "NLP", &["Sutskever", "Vinyals", "Le"], &["W1", "W2"]),
        mk("W4", "Deep Residual Learning", 2016, 220000, "Computer Vision", &["He", "Zhang", "Ren", "Sun"], &["W2"]),
        mk("W5", "Attention Is All You Need", 2017, 130000, "NLP", &["Vaswani", "Shazeer", "Parmar"], &["W1", "W3", "W4"]),
        mk("W6", "BERT", 2019, 80000, "NLP", &["Devlin", "Chang", "Lee", "Toutanova"], &["W3", "W5"]),
    ]
}

fn demo_reads(engine: &Engine, ing: &Ingested) {
    println!(
        "ingested {} papers + {} authors, {} CITES + {} AUTHORED edges across {} years",
        ing.papers, ing.authors, ing.cites, ing.authored, ing.timeline.len()
    );

    // (5) semantic kNN — the GraphRAG retrieval dimension.
    let id_to_title: HashMap<EntityId, String> = engine
        .snapshot_iter(TxId::ACTIVE)
        .unwrap()
        .into_iter()
        .filter_map(|r| match r {
            Record::Entity(e) if e.type_id == TypeId::new(TYPE_PAPER) => {
                let t = e.properties.iter().find(|(p, _)| p.get() == PROP_NAME)
                    .and_then(|(_, v)| if let Value::String(s) = v { Some(s.clone()) } else { None })?;
                Some((e.entity_id, t))
            }
            _ => None,
        })
        .collect();
    let probe = "transformer attention language model";
    let hits = engine.vector_search(PropertyId::new(PROP_EMBED), &embed(probe), 5, Distance::Cosine);
    println!("\nsemantic kNN nearest to \"{probe}\":");
    for (eid, dist) in hits {
        if let Some(t) = id_to_title.get(&eid) {
            println!("  {dist:.4}  {}", &t[..t.len().min(60)]);
        }
    }

    // (4) time travel — papers visible at the earliest vs latest year.
    if let (Some(first), Some(last)) = (ing.timeline.first(), ing.timeline.last()) {
        println!(
            "\ntime travel: {} papers as_of {} → {} papers as_of {}",
            count_papers_at(engine, first.1), first.0,
            count_papers_at(engine, last.1), last.0
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // --spool <dir> [--filter <openalex-filter>] [--max N]: resumable bulk
    // fetch of a server-side-filtered OpenAlex slice into a gzipped JSONL
    // spool. The long-running acquisition step (hours on a slow link);
    // `--from-spool` later ingests it into nDB at bounded RAM.
    if args.first().map(String::as_str) == Some("--spool") {
        let flag_val = |name: &str| {
            args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
        };
        let dir = args
            .iter()
            .skip(1)
            .find(|a| !a.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| ".demo-data/oa-spool".to_string());
        let filter = flag_val("--filter").unwrap_or_else(|| "cited_by_count:>50".to_string());
        let cap = flag_val("--max").and_then(|s| s.parse::<u64>().ok());
        spool_fetch(&dir, &filter, cap)?;
        return Ok(());
    }

    // --spool-sharded <dir> [--filter F] [--shards N]: parallel year-range
    // cursor walks (the fast path — beats the single-stream RTT wall).
    if args.first().map(String::as_str) == Some("--spool-sharded") {
        let flag_val = |name: &str| {
            args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
        };
        let dir = args
            .iter()
            .skip(1)
            .find(|a| !a.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| ".demo-data/oa-spool".to_string());
        let filter = flag_val("--filter").unwrap_or_else(|| "cited_by_count:>50".to_string());
        let shards = flag_val("--shards").and_then(|s| s.parse::<usize>().ok()).unwrap_or(10);
        spool_sharded(&dir, &filter, shards)?;
        return Ok(());
    }

    if args.first().map(String::as_str) == Some("--fetch") {
        let target = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
        let papers = fetch_openalex("deep learning neural network transformer", target)?;
        let connected = papers.iter().filter(|p| !p.cites.is_empty()).count();
        std::fs::create_dir_all("tools/langgraph/data")?;
        std::fs::write(CACHE_PATH, serde_json::to_vec_pretty(&papers)?)?;
        println!(
            "fetched {} papers ({} with internal citations) → {CACHE_PATH}",
            papers.len(), connected
        );
        return Ok(());
    }

    // --compact <db>: merge the DB's SSTables (and their per-SSTable index
    // sidecars) down to one. A low-memory ingest leaves one SSTable + one set
    // of .pidx/.vidx sidecars PER FLUSH — hundreds at 10 GB. `property_top_k`
    // / `vector_search` then iterate every sidecar (the 21.8 s `/view/top`
    // wall). Compaction collapses them to a single sidecar, so those become
    // a single-source top-k. ONE-TIME offline maintenance: it builds the
    // record set in RAM during the merge (spikes to ~DB-size), unlike the
    // bounded serving path — run it after ingest, before serving, NOT on a
    // live low-RAM server. Must precede the db-dir wipe below.
    if args.first().map(String::as_str) == Some("--compact") {
        let db_dir = args.iter().skip(1).find(|a| !a.starts_with("--")).cloned()
            .unwrap_or_else(|| ".demo-data/langgraph-ndb".to_string());
        let cache_mb: usize = args.iter().position(|a| a == "--cache-mb")
            .and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(2048);
        eprintln!("opening {db_dir} (low-memory) for compaction…");
        let mut engine = Engine::open_with_config(
            &db_dir, EngineConfig::low_memory(cache_mb * 1024 * 1024))?;
        // Re-declare the indexed (type, prop) pairs so the post-compaction
        // SSTable's .pidx/.vidx/.lkp sidecars are rewritten — without this,
        // compaction deletes the old sidecars and emits none (reader then
        // RAM-rebuilds, breaking bounded-memory serving).
        register_index_props(&mut engine);
        let before = engine.sstable_count();
        let t = std::time::Instant::now();
        let stats = engine.compact()?;
        // Stale top-k cache (built against the pre-compaction layout) must be
        // dropped so the server rebuilds it from the single compacted sidecar.
        let _ = std::fs::remove_file(format!("{db_dir}/top.json"));
        println!(
            "compacted {db_dir}: {before} → {} SSTable(s) ({} → {} records) in {:.0}s",
            engine.sstable_count(), stats.records_in, stats.records_out, t.elapsed().as_secs_f64()
        );
        return Ok(());
    }

    // --from-spool <spool-dir> [db-dir]: build a low-memory nDB from a
    // (possibly partial) --spool/--spool-sharded directory. Two-pass
    // streaming ingest at bounded memtable RAM. The OUTPUT db dir is wiped +
    // recreated; the SPOOL dir is only read. Run `--compact` after, then
    // serve with `langgraph-server --low-memory`.
    if args.first().map(String::as_str) == Some("--from-spool") {
        let positional: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
        let spool_dir = positional.first().map(|s| s.as_str()).unwrap_or(".demo-data/oa-spool");
        let db_dir = positional.get(1).map(|s| s.as_str()).unwrap_or(".demo-data/langgraph-oa-ndb");
        if !Path::new(spool_dir).exists() {
            return Err(format!("spool dir not found: {spool_dir}").into());
        }
        eprintln!("from-spool: {spool_dir} → {db_dir} (low-memory)");
        let _ = std::fs::remove_dir_all(db_dir); // OUTPUT only — never the spool
        std::fs::create_dir_all(db_dir)?;
        // 512 MB cache leaves headroom under the ~2 GB app cap for the pass-1
        // membership set (~200 MB at 12.3M) + the per-flush memtable window.
        let mut engine =
            Engine::create_with_config(db_dir, EngineConfig::low_memory(512 * 1024 * 1024))?;
        register_schema(&mut engine);
        let ing = ingest_from_spool(&mut engine, spool_dir)?;
        write_clusters_meta(&engine, db_dir)?;
        let sz: u64 = std::fs::read_dir(db_dir).map(|rd| rd.flatten()
            .filter_map(|e| e.metadata().ok().map(|m| m.len())).sum()).unwrap_or(0);
        println!(
            "from-spool DONE: {} papers, {} authors, {} cites, {} authored — {:.2} GB → {db_dir}\n\
             next: langgraph-ingest --compact {db_dir}  then  langgraph-server --low-memory --db {db_dir}",
            ing.papers, ing.authors, ing.cites, ing.authored, sz as f64 / 1.073_741_824e9
        );
        return Ok(());
    }

    // --bench-knn <db>: engine-level A/B of kNN at scale — exact (multi-sidecar
    // fan-out + MVCC verify) vs the global current-vector snapshot (one mmap'd
    // file, no fan-out/verify). No top-cache, no HTTP — isolates the kNN paths.
    // Reports latency (cold+warm), snapshot build time, COMMITTED RAM (RssAnon,
    // not VmRSS which counts reclaimable mmap), and result-set equality.
    if args.first().map(String::as_str) == Some("--bench-knn") {
        let db = args.iter().skip(1).find(|a| !a.starts_with("--")).cloned()
            .unwrap_or_else(|| ".demo-data/langgraph-ndb".to_string());
        let rss_anon_mb = || -> u64 {
            std::fs::read_to_string("/proc/self/status").ok()
                .and_then(|s| s.lines().find(|l| l.starts_with("RssAnon:"))
                    .and_then(|l| l.split_whitespace().nth(1)).and_then(|kb| kb.parse::<u64>().ok()))
                .map_or(0, |kb| kb / 1024)
        };
        eprintln!("opening {db} (low-memory)…");
        let mut engine = Engine::open_with_config(&db, EngineConfig::low_memory(768 * 1024 * 1024))?;
        register_index_props(&mut engine);
        engine.rebuild_indexes()?;
        let pid = PropertyId::new(PROP_EMBED);
        let q = embed("deep learning neural network");
        println!("RssAnon after open: {} MB", rss_anon_mb());

        let t = Instant::now();
        let ex = engine.vector_search(pid, &q, 20, Distance::Cosine);
        let ex_cold = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let _ = engine.vector_search(pid, &q, 20, Distance::Cosine);
        let ex_warm = t.elapsed().as_secs_f64();
        println!("EXACT (multi-sidecar+verify): {} hits, cold {ex_cold:.3}s, warm {ex_warm:.3}s, RssAnon {} MB",
            ex.len(), rss_anon_mb());

        let t = Instant::now();
        let n = engine.build_vector_snapshot(pid)?;
        println!("SNAPSHOT build: {n} vectors in {:.1}s, RssAnon {} MB (peak during build)",
            t.elapsed().as_secs_f64(), rss_anon_mb());
        let t = Instant::now();
        let sn = engine.vector_search_snapshot(pid, &q, 20, Distance::Cosine).unwrap_or_default();
        let sn_cold = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let _ = engine.vector_search_snapshot(pid, &q, 20, Distance::Cosine);
        let sn_warm = t.elapsed().as_secs_f64();
        println!("SNAPSHOT (.vsnap, no fan-out): {} hits, cold {sn_cold:.3}s, warm {sn_warm:.3}s, RssAnon {} MB",
            sn.len(), rss_anon_mb());

        let exs: HashSet<_> = ex.iter().map(|(e, _)| *e).collect();
        let sns: HashSet<_> = sn.iter().map(|(e, _)| *e).collect();
        println!("RESULT same_set={} overlap={}/{}  |  speedup cold {:.1}x warm {:.1}x",
            exs == sns, exs.intersection(&sns).count(), exs.union(&sns).count(),
            if sn_cold > 0.0 { ex_cold / sn_cold } else { 0.0 },
            if sn_warm > 0.0 { ex_warm / sn_warm } else { 0.0 });
        return Ok(());
    }

    // --low-memory creates the nDB with on-disk index sidecars so a
    // (possibly large) graph can later be SERVED with bounded RAM
    // (`langgraph-server --low-memory`). The db dir is the first non-flag arg.
    let low_memory = args.iter().any(|a| a == "--low-memory");
    // db dir = first positional arg, skipping flags AND the value that
    // follows --synthetic (the only flag that takes a value).
    let synth_val_idx = args.iter().position(|a| a == "--synthetic").map(|i| i + 1);
    let db_dir = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with("--") && Some(*i) != synth_val_idx)
        .map(|(_, a)| a.clone())
        .unwrap_or_else(|| ".demo-data/langgraph-ndb".to_string());
    let _ = std::fs::remove_dir_all(&db_dir);
    std::fs::create_dir_all(&db_dir)?;

    // --synthetic N: stream-ingest N synthetic papers for the scale/RSS
    // test (no cache, no graph.json export — doesn't touch the committed
    // demo). Implies low-memory so the on-disk sidecars are written.
    if let Some(n) = args.iter().position(|a| a == "--synthetic")
        .and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<usize>().ok())
    {
        let mut engine =
            Engine::create_with_config(&db_dir, EngineConfig::low_memory(2 * 1024 * 1024 * 1024))?;
        register_schema(&mut engine);
        let t = std::time::Instant::now();
        synthetic_ingest(&mut engine, n);
        write_clusters_meta(&engine, &db_dir)?;
        let sz: u64 = std::fs::read_dir(&db_dir).map(|rd| rd.flatten()
            .filter_map(|e| e.metadata().ok().map(|m| m.len())).sum()).unwrap_or(0);
        println!("synthetic ingest: {n} papers, {:.2} GB on disk, {:.0}s → {db_dir}",
            sz as f64 / 1.073_741_824e9, t.elapsed().as_secs_f64());
        return Ok(());
    }

    let papers = load_cache().unwrap_or_else(|| {
        eprintln!("no cache at {CACHE_PATH} — run `--fetch` first; using synthetic fallback");
        synthetic_papers()
    });

    let mut engine = if low_memory {
        Engine::create_with_config(&db_dir, EngineConfig::low_memory(2 * 1024 * 1024 * 1024))?
    } else {
        Engine::create(&db_dir)?
    };
    register_schema(&mut engine);
    let ing = ingest_papers(&mut engine, &papers);
    println!("→ {db_dir}");
    demo_reads(&engine, &ing);

    // Tiny cluster-aggregate sidecar so langgraph-server can serve
    // /view/clusters + compute galaxy positions WITHOUT scanning the whole
    // DB or holding per-paper metadata in RAM (bounded server at scale).
    write_clusters_meta(&engine, &db_dir)?;

    // Emit the viz feed for the static 3D explorer (docs/langgraph/).
    let (n, l) = export_graph(&engine, "docs/langgraph/graph.json")?;
    println!("\nexported {n} nodes + {l} links → docs/langgraph/graph.json");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_engine() -> (Engine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "langgraph-test-{}-{}",
            std::process::id(),
            EntityId::now_v7().into_uuid().simple()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut engine = Engine::create(&dir).unwrap();
        register_schema(&mut engine);
        (engine, dir)
    }

    #[test]
    fn ingest_builds_citation_and_authorship_graph() {
        let (mut engine, dir) = temp_engine();
        let papers = synthetic_papers();
        let ing = ingest_papers(&mut engine, &papers);

        assert_eq!(ing.papers, 6, "every paper becomes an entity");
        assert!(ing.authors >= 12, "deduped authors across papers");
        assert!(ing.cites >= 6, "internal CITES edges preserved");
        assert_eq!(ing.authored, 6, "one N-ary AUTHORED edge per paper");
        assert!(ing.paper_ids.contains_key("W5"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn vector_knn_returns_self_first() {
        let (mut engine, dir) = temp_engine();
        let papers = synthetic_papers();
        let _ = ingest_papers(&mut engine, &papers);
        // Query with the exact embed text used at ingest for W5.
        let probe = format!("{} {}", "Attention Is All You Need", "NLP");
        let hits = engine.vector_search(PropertyId::new(PROP_EMBED), &embed(&probe), 3, Distance::Cosine);
        assert!(!hits.is_empty());
        assert!(hits[0].1 < 1e-4, "self distance ~0, got {}", hits[0].1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn time_travel_shows_the_field_growing() {
        let (mut engine, dir) = temp_engine();
        let ing = ingest_papers(&mut engine, &synthetic_papers());
        let early = count_papers_at(&engine, ing.timeline.first().unwrap().1);
        let late = count_papers_at(&engine, ing.timeline.last().unwrap().1);
        assert!(early < late, "as_of earliest year ({early}) sees fewer papers than latest ({late})");
        assert_eq!(late, ing.papers, "latest snapshot sees every paper");
        let _ = std::fs::remove_dir_all(dir);
    }
}
