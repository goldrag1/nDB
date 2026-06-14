//! A small, properly-named demo dataset spanning three domains — **proteins**,
//! **exoplanets**, and **species** — in one nDB. It exercises every Studio
//! differentiator: named kinds with typed properties, **N-ary hyperedges** with
//! named roles and their own properties, **vector embeddings** for find-similar,
//! and a handful of post-hoc edits so the **history / time-travel / diff** views
//! have something to show.
//!
//! Reproducible: `ndb-studio --seed-demo <path>` builds the same shape every run
//! (only the random UUIDs differ). This replaces the prebuilt `.demo-data` DBs,
//! which were built name-blind (raw type-id constants, no dictionary records).

use uuid::Uuid;

use ndb_engine::value::Value;

use crate::store::{Store, StoreError};

/// Author attribution stamped on every seeded record.
const AUTHOR: &str = "curator";

fn s(x: &str) -> Value {
    Value::String(x.to_string())
}
fn i(n: i64) -> Value {
    Value::I64(n)
}
fn f(x: f64) -> Value {
    Value::F64(x)
}
fn vector(xs: &[f32]) -> Value {
    Value::Vector(xs.to_vec())
}

/// Create one entity of `kind` under a fresh UUID and return that UUID so it can
/// fill hyperedge roles. (Uses `create_with_id` because plain `create` only
/// returns a tx id, not the new entity's UUID.)
fn entity(store: &Store, kind: &str, props: Vec<(String, Value)>) -> Result<Uuid, StoreError> {
    let id = Uuid::now_v7();
    store.create_with_id(id, kind, &props, Some(AUTHOR))?;
    Ok(id)
}

/// Build the full demo dataset into `store` (expected to be a fresh database).
///
/// # Errors
/// Propagates the first engine/write error.
pub fn seed_demo(store: &Store) -> Result<(), StoreError> {
    // Embeddings become searchable only if the property is registered first.
    store.register_vector("embedding")?;

    // ---- Proteins (AlphaFold-flavoured) ---------------------------------
    let hemoglobin = entity(
        store,
        "Protein",
        vec![
            ("name".into(), s("Hemoglobin subunit alpha")),
            ("uniprot_id".into(), s("P69905")),
            ("organism".into(), s("Homo sapiens")),
            ("residue_count".into(), i(142)),
            ("confidence".into(), f(0.98)),
            ("embedding".into(), vector(&[0.91, 0.12, 0.05, 0.40])),
        ],
    )?;
    let myoglobin = entity(
        store,
        "Protein",
        vec![
            ("name".into(), s("Myoglobin")),
            ("uniprot_id".into(), s("P02144")),
            ("organism".into(), s("Homo sapiens")),
            ("residue_count".into(), i(154)),
            ("confidence".into(), f(0.96)),
            ("embedding".into(), vector(&[0.88, 0.18, 0.07, 0.44])),
        ],
    )?;
    let insulin = entity(
        store,
        "Protein",
        vec![
            ("name".into(), s("Insulin")),
            ("uniprot_id".into(), s("P01308")),
            ("organism".into(), s("Homo sapiens")),
            ("residue_count".into(), i(110)),
            ("confidence".into(), f(0.97)),
            ("embedding".into(), vector(&[0.10, 0.93, 0.22, 0.08])),
        ],
    )?;
    let p53 = entity(
        store,
        "Protein",
        vec![
            ("name".into(), s("Cellular tumor antigen p53")),
            ("uniprot_id".into(), s("P04637")),
            ("organism".into(), s("Homo sapiens")),
            ("residue_count".into(), i(393)),
            ("confidence".into(), f(0.92)),
            ("embedding".into(), vector(&[0.20, 0.30, 0.90, 0.15])),
        ],
    )?;
    let mdm2 = entity(
        store,
        "Protein",
        vec![
            ("name".into(), s("E3 ubiquitin-protein ligase Mdm2")),
            ("uniprot_id".into(), s("Q00987")),
            ("organism".into(), s("Homo sapiens")),
            ("residue_count".into(), i(491)),
            ("confidence".into(), f(0.89)),
            ("embedding".into(), vector(&[0.24, 0.34, 0.86, 0.19])),
        ],
    )?;
    let gfp = entity(
        store,
        "Protein",
        vec![
            ("name".into(), s("Green fluorescent protein")),
            ("uniprot_id".into(), s("P42212")),
            ("organism".into(), s("Aequorea victoria")),
            ("residue_count".into(), i(238)),
            ("confidence".into(), f(0.95)),
            ("embedding".into(), vector(&[0.05, 0.07, 0.11, 0.97])),
        ],
    )?;

    // ---- Exoplanets ------------------------------------------------------
    let proxima_b = entity(
        store,
        "Exoplanet",
        vec![
            ("name".into(), s("Proxima Centauri b")),
            ("host_star".into(), s("Proxima Centauri")),
            ("mass_earth".into(), f(1.07)),
            ("radius_earth".into(), f(1.03)),
            ("orbital_days".into(), f(11.19)),
            ("discovery_year".into(), i(2016)),
        ],
    )?;
    let trappist_e = entity(
        store,
        "Exoplanet",
        vec![
            ("name".into(), s("TRAPPIST-1e")),
            ("host_star".into(), s("TRAPPIST-1")),
            ("mass_earth".into(), f(0.69)),
            ("radius_earth".into(), f(0.92)),
            ("orbital_days".into(), f(6.10)),
            ("discovery_year".into(), i(2017)),
        ],
    )?;
    let kepler_22b = entity(
        store,
        "Exoplanet",
        vec![
            ("name".into(), s("Kepler-22b")),
            ("host_star".into(), s("Kepler-22")),
            ("mass_earth".into(), f(9.1)),
            ("radius_earth".into(), f(2.40)),
            ("orbital_days".into(), f(289.9)),
            ("discovery_year".into(), i(2011)),
        ],
    )?;
    let pegasi_b = entity(
        store,
        "Exoplanet",
        vec![
            ("name".into(), s("51 Pegasi b")),
            ("host_star".into(), s("51 Pegasi")),
            ("mass_earth".into(), f(149.0)),
            ("radius_earth".into(), f(19.0)),
            ("orbital_days".into(), f(4.23)),
            ("discovery_year".into(), i(1995)),
        ],
    )?;

    // ---- Species (biodiversity) -----------------------------------------
    let tiger = entity(
        store,
        "Species",
        vec![
            ("scientific_name".into(), s("Panthera tigris")),
            ("common_name".into(), s("Tiger")),
            ("kingdom".into(), s("Animalia")),
            ("iucn_status".into(), s("Endangered")),
            ("habitat".into(), s("Forest")),
        ],
    )?;
    let bee = entity(
        store,
        "Species",
        vec![
            ("scientific_name".into(), s("Apis mellifera")),
            ("common_name".into(), s("Western honey bee")),
            ("kingdom".into(), s("Animalia")),
            ("iucn_status".into(), s("Least Concern")),
            ("habitat".into(), s("Grassland")),
        ],
    )?;
    let oak = entity(
        store,
        "Species",
        vec![
            ("scientific_name".into(), s("Quercus robur")),
            ("common_name".into(), s("English oak")),
            ("kingdom".into(), s("Plantae")),
            ("iucn_status".into(), s("Least Concern")),
            ("habitat".into(), s("Forest")),
        ],
    )?;
    let panda = entity(
        store,
        "Species",
        vec![
            ("scientific_name".into(), s("Ailuropoda melanoleuca")),
            ("common_name".into(), s("Giant panda")),
            ("kingdom".into(), s("Animalia")),
            ("iucn_status".into(), s("Endangered")),
            ("habitat".into(), s("Forest")),
        ],
    )?;

    // ---- N-ary hyperedges (no relational equivalent) --------------------
    // A pathway connecting several proteins by the role they play in it — one
    // fact spanning N entities, carrying its own properties.
    store.create_hyperedge(
        "Pathway",
        &[
            ("regulator".into(), mdm2),
            ("substrate".into(), p53),
        ],
        &[],
        &[
            ("name".into(), s("p53 regulation")),
            ("process".into(), s("Ubiquitination / apoptosis control")),
        ],
    )?;
    store.create_hyperedge(
        "Pathway",
        &[
            ("member".into(), hemoglobin),
            ("member".into(), myoglobin),
        ],
        &[],
        &[
            ("name".into(), s("Oxygen transport")),
            ("process".into(), s("Reversible O2 binding via heme")),
        ],
    )?;
    // A cross-domain ecological interaction: pollinator ↔ plant by role.
    store.create_hyperedge(
        "Interaction",
        &[("pollinator".into(), bee), ("plant".into(), oak)],
        &[],
        &[("kind".into(), s("Pollination"))],
    )?;
    // A habitat-sharing fact spanning three species — arity 3, where SQL needs
    // a junction table.
    store.create_hyperedge(
        "SharesHabitat",
        &[
            ("species".into(), tiger),
            ("species".into(), panda),
            ("species".into(), oak),
        ],
        &[],
        &[("habitat".into(), s("Temperate forest"))],
    )?;

    // ---- Edits → version history for time-travel / diff -----------------
    // IUCN reclassified the giant panda Endangered→Vulnerable (2016).
    store.set(panda, "iucn_status", &s("Vulnerable"), Some("iucn-2016"))?;
    // Tighten an early radial-velocity mass estimate.
    store.set(pegasi_b, "mass_earth", &f(146.0), Some("revisions"))?;
    // A later AlphaFold release nudged a confidence score.
    store.set(p53, "confidence", &f(0.94), Some("af-v4"))?;
    // GFP's organism corrected with the full binomial.
    store.set(gfp, "organism", &s("Aequorea victoria (jellyfish)"), Some("curator"))?;

    // Silence unused-binding warnings for entities referenced only by id above.
    let _ = (insulin, proxima_b, trappist_e, kepler_22b);

    Ok(())
}
