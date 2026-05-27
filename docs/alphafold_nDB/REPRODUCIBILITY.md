# Reproducibility notes — alphafold_nDB

Concrete steps so a structural biologist (or peer reviewer) can
verify every claim in the demo without trusting the seed code.

---

## 1. Verify the pLDDT mean values against AlphaFold-DB

These are fetched May 2026 from the live AF-DB API and pinned in
`crates/ndb-renderer/examples/v22_explorer/main.rs` (see `AF_SEED`).

```sh
for acc in P04637 Q00987 Q13315 O96017 P38398 P51587 P31749 P42345 \
           O75385 Q14457 Q9GZQ8 O95352 P42336 P60484 P01116 \
           P00760 P03001 P01308 P02185 P42212; do
  printf "%-8s pLDDT=" "$acc"
  curl -s "https://alphafold.ebi.ac.uk/api/prediction/$acc" \
    | jq -r '.[0].globalMetricValue // "(no AF-DB record)"'
done
```

Expected values (rounded):

| Accession | Name      | pLDDT mean | Bucket      |
| --------- | --------- | ---------- | ----------- |
| P04637    | P53       | 75.06      | confident   |
| Q00987    | MDM2      | 62.59      | low         |
| Q13315    | ATM       | (retired)  | no record   |
| O96017    | CHK2      | 76.19      | confident   |
| P38398    | BRCA1     | 41.59      | very_low    |
| P51587    | BRCA2     | (retired)  | no record   |
| P31749    | AKT1      | 83.06      | confident   |
| P42345    | MTOR      | 78.00      | confident   |
| O75385    | ULK1      | 59.41      | low         |
| Q14457    | BECN1     | 76.56      | confident   |
| Q9GZQ8    | LC3       | 91.44      | very_high   |
| O95352    | ATG7      | 87.62      | confident   |
| P42336    | PI3K      | 92.38      | very_high   |
| P60484    | PTEN      | 83.00      | confident   |
| P01116    | KRAS      | 91.50      | very_high   |
| P00760    | Trypsin   | 93.12      | very_high   |
| P03001    | TFIIIA    | 71.00      | confident   |
| P01308    | Insulin   | 52.91      | low         |
| P02185    | Myoglobin | 97.50      | very_high   |
| P42212    | GFP       | 96.62      | very_high   |

The bucket boundaries are from the
[AlphaFold-DB FAQ section 5](https://alphafold.ebi.ac.uk/faq#faq-5):
> 90 = very_high, > 70 = confident, > 50 = low, ≤ 50 = very_low.

---

## 2. Verify the motif residue lists against the literature

| Motif | Verification |
| --- | --- |
| Trypsin catalytic triad Ser195-His57-Asp102 | UniProt P00760 features `ACT_SITE` 57 / 102 / 195 → matches Hedstrom 2002 *Chem. Rev.* fig 2. |
| TFIIIA finger-1 Cys6-Cys11-His24-His28 | UniProt P03001 `ZN_FING` 6-28 → matches Brown 2005 *FEBS Lett.* fig 1. |
| Insulin disulfides A6-A11 / A7-B7 / A20-B19 | UniProt P01308 `DISULFID` records (A-chain numbering). |
| Myoglobin F-helix residues 80-95 | UniProt P02185 `HELIX` 86-95 plus surrounding loop; Phillips 1980 *JMB* fig 3. |
| GFP β-barrel strands β1/β2/β3/β6 | UniProt P42212 `STRAND` annotations matching PDB 1EMA. |

Cross-check any of these by pulling the UniProt flat file:

```sh
curl -s 'https://www.uniprot.org/uniprot/P00760.txt' | grep '^FT' | head -40
```

---

## 3. Verify the engine actually stores what it says it stores

```sh
cargo test -p ndb-renderer --example v22_explorer -- --nocapture
```

Expected output:

```
running 8 tests
test residues::tests::trypsin_has_catalytic_triad ... ok
test residues::tests::insulin_has_three_disulfide_bonds ... ok
test residues::tests::myoglobin_f_helix_is_arity_16 ... ok
test residues::tests::tfiiia_zinc_finger_is_arity_4 ... ok
test residues::tests::gfp_has_two_beta_sheet_pairs ... ok
test tests::plddt_bucket_matches_alphafold_db_thresholds ... ok
test tests::af_seed_has_20_proteins_with_consistent_metadata ... ok
test tests::seeded_engine_has_plddt_properties ... ok

test result: ok. 8 passed; 0 failed
```

The final test boots a fresh engine, runs the seed, and asserts:
- 20 protein entities (15 cancer/signalling + 5 showcase)
- 18 carry `PROP_PLDDT_MEAN` (ATM and BRCA2 are skipped because
  AF-DB has retired their predictions)
- 4 of the 6 protein complexes carry `PROP_COMPLEX_CONFIDENCE`
  (the two that touch ATM or BRCA2 are skipped — no fabrication)
- Exactly 78 residue entities + 78 `residue_of` arity-2 hyperedges
- Exactly 1 catalytic triad (arity 3), 3 disulfide bonds (arity 2),
  1 zinc finger (arity 4), 1 alpha-helix (arity 16), 2 β-sheet pairs

Any drift from these numbers is a real bug — either the dataset
changed (intentional) or the engine misclassified something.

---

## 4. Verify the 3D viewer pipeline manually

1. `cargo run -p ndb-renderer --example v22_explorer`
2. Open <http://127.0.0.1:9876/>
3. Click the **KRAS** node (small dark-blue dot, very_high bucket)
4. Click **Load AlphaFold 3D structure**
5. Confirm: the cartoon backbone is mostly dark blue (G-domain core)
   with a yellow/orange C-terminal segment around residues 169-189
   (the hypervariable region — famously disordered, low pLDDT)
6. Toggle **Show residues + structural motifs**
7. Click any protein in the residue cluster (e.g. **Trypsin**)
8. Click **Load AlphaFold 3D structure** for Trypsin
9. Find the `catalytic_triad` hyperedge node (red dot, very small)
   in the upper hypergraph and click it
10. Confirm: in the 3D pane, 3 residues light up as pink ball+stick
    near the active site of trypsin's two-β-barrel fold, with labels
    showing `SER195`, `HIS57`, `ASP102`

Any deviation here is a 3D-viewer-side bug, not an engine bug.

---

## 5. The local nDB filesystem

```
/tmp/v22-explorer-ndb/
├── CURRENT
├── LOCK
├── MANIFEST-000000
├── 000001.ndb            # SSTable (proteins + genes + …)
├── 000002.ndb            # SSTable (residues + motifs)
└── 000001.ndb.idx        # block index sidecar
```

The whole database is human-inspectable with the `ndb` CLI:

```sh
cargo run -p ndb-cli -- --url http://127.0.0.1:8742 iter | head -3
# JSONL — one record per line
```

---

## 6. CIF file source

For the 3D viewer we use AlphaFold-DB's directly-served CIF files at
`https://alphafold.ebi.ac.uk/files/AF-<acc>-F1-model_v6.cif`. As of
May 2026 these are CORS-accessible from any origin (verified via
preflight; see the commit message for §B).

If you want a **PDB-experimental** structure (e.g. trypsin PDB
`5PTP`, GFP PDB `1EMA`) instead of the AF prediction, swap the URL
to `https://files.rcsb.org/download/<id>.cif` — NGL accepts both
formats identically. The pLDDT colouring will then be replaced by
B-factor (real crystallographic uncertainty), still using the same
NGL "bfactor" scheme.
