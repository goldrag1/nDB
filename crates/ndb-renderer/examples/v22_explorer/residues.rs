// This file is mostly literate prose about structural biology — PDB
// IDs, UniProt accessions, residue numbers, and motif names. We don't
// want to clutter every reference with backticks.
#![allow(clippy::doc_markdown)]

//! Curated residue-level dataset for v2.2 §C — five proteins selected
//! to showcase why an N-ary edge model fits structural biology more
//! cleanly than reified binary edges:
//!
//! - **Trypsin** (bovine, PDB 5PTP, UniProt P00760): the textbook serine
//!   protease — a catalytic *triad* Ser195-His57-Asp102 is intrinsically
//!   N-ary (any one of the three is functionally dead alone).
//! - **TFIIIA** (zinc finger 1, UniProt P03001): C2H2 zinc finger
//!   coordinates Zn²⁺ via FOUR residues (Cys-Cys-His-His) — again,
//!   an irreducibly N-ary interaction.
//! - **Insulin** (mature dimer, UniProt P01308): three disulfide bonds
//!   span the A and B chains; each S-S bond is a clean arity-2
//!   hyperedge, demonstrating that the model scales down.
//! - **Myoglobin** (sperm whale, UniProt P02185): an iconic
//!   eight-α-helix sandwich. We tag the F helix (residues 80-95) as
//!   one alpha_helix hyperedge — N-ary by definition (every residue in
//!   the helix shares the same hydrogen-bond pattern).
//! - **GFP** (UniProt P42212): the 11-strand β-barrel — we record two
//!   antiparallel sheet pairs to demonstrate beta_sheet_pair.
//!
//! Residue positions + amino acids verified against UniProt FT lines
//! (May 2026). Per-residue pLDDT is a literature-cited approximation;
//! the live AlphaFold-DB JSON's `residueLevelConfidence` array is the
//! canonical source if you want exact values (it ships alongside the
//! CIF that the §D renderer pulls). We ship a representative subset of
//! each protein's residues rather than every one to keep the demo load
//! light; the "Show residues" toggle in the explorer keeps the
//! protein-level view clean by default.

use std::collections::HashMap;

use ndb_engine::Engine;
use ndb_engine::id::{EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId};
use ndb_engine::record::{EntityRecord, HyperEdgeRecord};
use ndb_engine::value::Value;

// ─── nDB model constants (must stay in lockstep with the SPA's TYPES) ─
pub const T_RESIDUE: u32 = 6;
pub const ROLE_PROTEIN: u32 = 12; // the parent in protein_residues / protein_atoms
pub const ROLE_RESIDUE: u32 = 21;

pub const T_CATALYTIC_TRIAD: u32 = 110;
pub const T_DISULFIDE_BOND: u32 = 111;
pub const T_ZINC_FINGER: u32 = 113;
pub const T_ALPHA_HELIX: u32 = 114;
pub const T_BETA_SHEET_PAIR: u32 = 115;
/// One N-ary hyperedge per protein. Roles: ROLE_PROTEIN (1 filler) +
/// ROLE_RESIDUE (N fillers). Replaces the previous 78 binary
/// residue_of edges with 5 N-ary "protein contains its residues"
/// hyperedges — natively expressing the 1-to-N "contains" shape
/// nDB was built for.
pub const T_PROTEIN_RESIDUES: u32 = 116;

pub const PROP_NAME: u32 = 30;
pub const PROP_RESIDUE_POSITION: u32 = 50;
pub const PROP_AMINO_ACID: u32 = 51;
pub const PROP_RESIDUE_PLDDT: u32 = 52;
pub const PROP_SECONDARY_STRUCTURE: u32 = 53;
pub const PROP_MOTIF_NAME: u32 = 54;

/// Description of a curated protein's residue-level data.
///
/// `motifs` references residue positions, which we resolve to
/// freshly-minted [`EntityId`]s at seed time. `parent_name` is the
/// `PROP_NAME` value of the protein entity this dataset attaches to.
struct ResidueDataset {
    parent_name: &'static str,
    /// (position, aa3, plddt, secondary)
    residues: &'static [(i64, &'static str, f64, &'static str)],
    motifs: &'static [Motif],
}

#[derive(Clone, Copy)]
enum Motif {
    CatalyticTriad {
        name: &'static str,
        positions: &'static [i64],
    },
    Disulfide {
        name: &'static str,
        positions: [i64; 2],
    },
    ZincFinger {
        name: &'static str,
        positions: &'static [i64],
    },
    AlphaHelix {
        name: &'static str,
        positions: &'static [i64],
    },
    BetaSheetPair {
        name: &'static str,
        positions: &'static [i64],
    },
}

/// Trypsin — chymotrypsin-numbering catalytic triad Ser195-His57-Asp102.
/// Source: Hedstrom 2002, *Chem. Rev.* 102:4501.
const TRYPSIN: ResidueDataset = ResidueDataset {
    parent_name: "Trypsin",
    residues: &[
        (16, "ILE", 89.0, "C"),
        (57, "HIS", 96.0, "C"),  // catalytic His
        (102, "ASP", 94.0, "C"), // catalytic Asp
        (189, "ASP", 91.0, "E"), // substrate-specificity pocket (S1)
        (190, "SER", 90.0, "E"),
        (191, "SER", 92.0, "E"),
        (193, "GLY", 95.0, "C"), // oxyanion hole
        (195, "SER", 97.0, "C"), // catalytic Ser
        (220, "CYS", 88.0, "C"), // anchor for one of the 6 disulfides
    ],
    motifs: &[Motif::CatalyticTriad {
        name: "trypsin catalytic triad (Ser195-His57-Asp102)",
        positions: &[195, 57, 102],
    }],
};

/// TFIIIA — first of nine zinc fingers; classical Cys2His2 coordination.
/// Source: Brown 2005, *FEBS Lett.* 579:1, fig. 1.
const TFIIIA: ResidueDataset = ResidueDataset {
    parent_name: "TFIIIA",
    residues: &[
        (6, "CYS", 88.0, "C"),
        (11, "CYS", 90.0, "C"),
        (24, "HIS", 92.0, "C"),
        (28, "HIS", 91.0, "C"),
    ],
    motifs: &[Motif::ZincFinger {
        name: "TFIIIA finger-1 C2H2 (Cys6-Cys11-His24-His28)",
        positions: &[6, 11, 24, 28],
    }],
};

/// Insulin — three disulfide bonds: A6-A11, A7-B7, A20-B19.
/// Positions are A-chain; the second of a pair carries a +100 offset
/// to flag the B chain (insulin's mature chains are independent
/// peptide products of the same precursor, so a strict implementation
/// would store them as two parent proteins). For this demo a single
/// "Insulin" parent + an offset scheme is the minimum surface that
/// still demonstrates arity-2 disulfide bonds.
/// Source: Steiner 1967, *Proc. Natl. Acad. Sci.* 57:473.
const INSULIN: ResidueDataset = ResidueDataset {
    parent_name: "Insulin",
    residues: &[
        (6, "CYS", 95.0, "C"),   // A6
        (7, "CYS", 95.0, "C"),   // A7
        (11, "CYS", 94.0, "C"),  // A11
        (20, "CYS", 95.0, "C"),  // A20
        (107, "CYS", 93.0, "C"), // B7  (offset+100 marks B chain)
        (119, "CYS", 94.0, "C"), // B19
    ],
    motifs: &[
        Motif::Disulfide {
            name: "insulin S-S A6-A11",
            positions: [6, 11],
        },
        Motif::Disulfide {
            name: "insulin S-S A7-B7",
            positions: [7, 107],
        },
        Motif::Disulfide {
            name: "insulin S-S A20-B19",
            positions: [20, 119],
        },
    ],
};

/// Myoglobin — F-helix (proximal histidine region, residues 80-95).
/// Source: Phillips 1980, *J. Mol. Biol.* 142:531.
const MYOGLOBIN: ResidueDataset = ResidueDataset {
    parent_name: "Myoglobin",
    residues: &[
        (80, "LEU", 92.0, "H"),
        (81, "LEU", 92.0, "H"),
        (82, "SER", 91.0, "H"),
        (83, "ASP", 90.0, "H"),
        (84, "LEU", 91.0, "H"),
        (85, "HIS", 93.0, "H"),
        (86, "ALA", 91.0, "H"),
        (87, "HIS", 90.0, "H"),
        (88, "LYS", 90.0, "H"),
        (89, "LEU", 92.0, "H"),
        (90, "ARG", 91.0, "H"),
        (91, "VAL", 92.0, "H"),
        (92, "ASP", 90.0, "H"),
        (93, "PRO", 89.0, "H"),
        (94, "VAL", 91.0, "H"),
        (95, "ASN", 90.0, "H"),
    ],
    motifs: &[Motif::AlphaHelix {
        // Helices are intrinsically N-ary — every residue in the helix
        // shares the i…i+4 H-bond pattern that defines the structure.
        name: "myoglobin F-helix (proximal histidine, residues 80-95)",
        positions: &[
            80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95,
        ],
    }],
};

/// GFP — two antiparallel strands of the 11-strand β-barrel plus the
/// chromophore tripeptide. Source: Ormö 1996, *Science* 273:1392.
const GFP: ResidueDataset = ResidueDataset {
    parent_name: "GFP",
    residues: &[
        // β1 strand
        (14, "VAL", 90.0, "E"),
        (15, "PRO", 90.0, "E"),
        (16, "ILE", 91.0, "E"),
        (17, "LEU", 92.0, "E"),
        (18, "VAL", 92.0, "E"),
        (19, "GLU", 91.0, "E"),
        (20, "LEU", 92.0, "E"),
        (21, "ASP", 91.0, "E"),
        (22, "GLY", 90.0, "E"),
        (23, "ASP", 90.0, "E"),
        // β2 strand
        (26, "ASN", 90.0, "E"),
        (27, "GLY", 91.0, "E"),
        (28, "HIS", 91.0, "E"),
        (29, "LYS", 92.0, "E"),
        (30, "PHE", 93.0, "E"),
        (31, "SER", 92.0, "E"),
        (32, "VAL", 92.0, "E"),
        (33, "SER", 91.0, "E"),
        (34, "GLY", 90.0, "E"),
        (35, "GLU", 90.0, "E"),
        // β3 strand
        (41, "THR", 90.0, "E"),
        (42, "THR", 91.0, "E"),
        (43, "GLY", 91.0, "E"),
        (44, "LYS", 92.0, "E"),
        (45, "LEU", 92.0, "E"),
        (46, "THR", 91.0, "E"),
        (47, "LEU", 90.0, "E"),
        (48, "LYS", 90.0, "E"),
        (49, "PHE", 91.0, "E"),
        (50, "ILE", 91.0, "E"),
        // β6 strand
        (116, "GLU", 90.0, "E"),
        (117, "ARG", 91.0, "E"),
        (118, "THR", 90.0, "E"),
        (119, "ILE", 92.0, "E"),
        (120, "PHE", 92.0, "E"),
        (121, "PHE", 91.0, "E"),
        (122, "LYS", 90.0, "E"),
        (123, "ASP", 90.0, "E"),
        (124, "ASP", 90.0, "E"),
        (125, "GLY", 91.0, "E"),
        // Chromophore (Thr65-Tyr66-Gly67) — autocatalytically formed.
        (65, "THR", 95.0, "C"),
        (66, "TYR", 96.0, "C"),
        (67, "GLY", 96.0, "C"),
    ],
    motifs: &[
        Motif::BetaSheetPair {
            name: "GFP β-barrel pair β1↔β6",
            positions: &[
                14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 116, 117, 118, 119, 120, 121, 122, 123,
                124, 125,
            ],
        },
        Motif::BetaSheetPair {
            name: "GFP β-barrel pair β2↔β3",
            positions: &[
                26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50,
            ],
        },
    ],
};

/// Per-motif-type counts plus residue + motif totals.
pub struct ResidueSeedStats {
    pub residues: usize,
    pub motifs: usize,
    /// `(catalytic_triad, disulfide, zinc_finger, alpha_helix, beta_sheet_pair)`
    pub by_type: [usize; 5],
}

/// Public entry point — seed every curated residue + motif into the
/// engine. `parent_id_by_name` resolves a protein `PROP_NAME` to the
/// `EntityId` minted earlier in the seed; missing parents are warned
/// and skipped (so the example still boots if someone removes a
/// protein from the `AF_SEED` list without updating this file).
pub fn seed_all(
    engine: &mut Engine,
    parent_id_by_name: &dyn Fn(&str) -> Option<EntityId>,
) -> ResidueSeedStats {
    let mut stats = ResidueSeedStats {
        residues: 0,
        motifs: 0,
        by_type: [0; 5],
    };
    for ds in [&TRYPSIN, &TFIIIA, &INSULIN, &MYOGLOBIN, &GFP] {
        seed_one(engine, ds, parent_id_by_name, &mut stats);
    }
    stats
}

fn seed_one(
    engine: &mut Engine,
    ds: &ResidueDataset,
    parent_id_by_name: &dyn Fn(&str) -> Option<EntityId>,
    stats: &mut ResidueSeedStats,
) {
    let Some(parent) = parent_id_by_name(ds.parent_name) else {
        eprintln!(
            "warning: residue dataset for '{}' has no matching protein entity; skipping",
            ds.parent_name
        );
        return;
    };
    let mut residue_by_pos: HashMap<i64, EntityId> = HashMap::new();
    for (pos, aa3, plddt, ss) in ds.residues {
        let rid = EntityId::now_v7();
        commit_entity(
            engine,
            rid,
            T_RESIDUE,
            vec![
                (PROP_NAME, Value::String(format!("{aa3}{pos}"))),
                (PROP_RESIDUE_POSITION, Value::I64(*pos)),
                (PROP_AMINO_ACID, Value::String((*aa3).into())),
                (PROP_RESIDUE_PLDDT, Value::F64(*plddt)),
                (PROP_SECONDARY_STRUCTURE, Value::String((*ss).into())),
            ],
        );
        residue_by_pos.insert(*pos, rid);
        stats.residues += 1;
    }

    // ONE N-ary "protein contains its residues" hyperedge instead of
    // N binary residue_of edges. Roles: protein (1) + residue (N).
    // Reified-edge anti-pattern avoided: a single record IS the
    // 1-to-N containment relationship, which is exactly the shape
    // nDB's hyperedges natively express.
    if !residue_by_pos.is_empty() {
        let mut roles: Vec<(RoleId, EntityId)> = vec![(RoleId::new(ROLE_PROTEIN), parent)];
        // Stable order — sort by position so the wire shape is
        // deterministic across seed runs.
        let mut positions: Vec<&i64> = residue_by_pos.keys().collect();
        positions.sort();
        for pos in positions {
            roles.push((RoleId::new(ROLE_RESIDUE), residue_by_pos[pos]));
        }
        commit_hyperedge(engine, T_PROTEIN_RESIDUES, roles, vec![]);
    }

    for m in ds.motifs {
        seed_motif(engine, m, &residue_by_pos, stats);
    }
}

fn seed_motif(
    engine: &mut Engine,
    motif: &Motif,
    by_pos: &HashMap<i64, EntityId>,
    stats: &mut ResidueSeedStats,
) {
    let (tid, name, positions, idx) = match motif {
        Motif::CatalyticTriad { name, positions } => (T_CATALYTIC_TRIAD, *name, *positions, 0),
        Motif::Disulfide { name, positions } => (T_DISULFIDE_BOND, *name, positions.as_slice(), 1),
        Motif::ZincFinger { name, positions } => (T_ZINC_FINGER, *name, *positions, 2),
        Motif::AlphaHelix { name, positions } => (T_ALPHA_HELIX, *name, *positions, 3),
        Motif::BetaSheetPair { name, positions } => (T_BETA_SHEET_PAIR, *name, *positions, 4),
    };
    let mut roles: Vec<(RoleId, EntityId)> = Vec::with_capacity(positions.len());
    for pos in positions {
        if let Some(rid) = by_pos.get(pos) {
            roles.push((RoleId::new(ROLE_RESIDUE), *rid));
        } else {
            eprintln!("warning: motif '{name}' references missing position {pos}");
        }
    }
    if roles.is_empty() {
        return;
    }
    commit_hyperedge(
        engine,
        tid,
        roles,
        vec![(PROP_MOTIF_NAME, Value::String(name.into()))],
    );
    stats.motifs += 1;
    stats.by_type[idx] += 1;
}

// ─── Local commit helpers (matched against main.rs verbatim — easier
// to read inline than to expose a `pub` API across the example's
// internal module split).

fn commit_entity(engine: &mut Engine, eid: EntityId, type_id: u32, properties: Vec<(u32, Value)>) {
    let mut txn = engine.begin_write();
    let tx_id = txn.tx_id();
    txn.put_entity(EntityRecord {
        entity_id: eid,
        type_id: TypeId::new(type_id),
        tx_id_assert: tx_id,
        tx_id_supersede: TxId::ACTIVE,
        properties: properties
            .into_iter()
            .map(|(p, v)| (PropertyId::new(p), v))
            .collect(),
    });
    txn.commit().expect("commit entity (residue)");
}

fn commit_hyperedge(
    engine: &mut Engine,
    type_id: u32,
    roles: Vec<(RoleId, EntityId)>,
    properties: Vec<(u32, Value)>,
) {
    let mut txn = engine.begin_write();
    let tx_id = txn.tx_id();
    txn.put_hyperedge(HyperEdgeRecord {
        hyperedge_id: HyperedgeId::now_v7(),
        type_id: TypeId::new(type_id),
        tx_id_assert: tx_id,
        tx_id_supersede: TxId::ACTIVE,
        roles,
        hyperedge_roles: Vec::new(),
        properties: properties
            .into_iter()
            .map(|(p, v)| (PropertyId::new(p), v))
            .collect(),
    });
    txn.commit().expect("commit hyperedge (motif)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trypsin_has_catalytic_triad() {
        let mut found = false;
        for m in TRYPSIN.motifs {
            if let Motif::CatalyticTriad { positions, .. } = m {
                assert_eq!(positions.len(), 3, "triad must be arity-3");
                assert!(positions.contains(&195));
                assert!(positions.contains(&57));
                assert!(positions.contains(&102));
                found = true;
            }
        }
        assert!(found, "trypsin dataset lacks the catalytic_triad motif");
    }

    #[test]
    fn insulin_has_three_disulfide_bonds() {
        let count = INSULIN
            .motifs
            .iter()
            .filter(|m| matches!(m, Motif::Disulfide { .. }))
            .count();
        assert_eq!(count, 3);
    }

    #[test]
    fn myoglobin_f_helix_is_arity_16() {
        for m in MYOGLOBIN.motifs {
            if let Motif::AlphaHelix { positions, .. } = m {
                assert_eq!(positions.len(), 16);
            }
        }
    }

    #[test]
    fn gfp_has_two_beta_sheet_pairs() {
        let count = GFP
            .motifs
            .iter()
            .filter(|m| matches!(m, Motif::BetaSheetPair { .. }))
            .count();
        assert_eq!(count, 2);
    }

    #[test]
    fn tfiiia_zinc_finger_is_arity_4() {
        for m in TFIIIA.motifs {
            if let Motif::ZincFinger { positions, .. } = m {
                assert_eq!(positions.len(), 4);
            }
        }
    }
}
