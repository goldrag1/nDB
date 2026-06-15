#!/usr/bin/env python3
"""Run the REAL larval-Drosophila connectome as a spiking network → nDB showcase.

Loads the complete 3,016-neuron larval brain connectome (Winding et al., Science
2023; data: github.com/brain-networks/larval-drosophila-connectome), wires it as
a leaky integrate-and-fire (LIF) network with synapse-count weights normalised by
post-synaptic input (the same bounding trick as Shiu et al.'s whole-adult-brain
LIF model, eLife 2024), then *stimulates a sensory modality* and watches the
spike cascade sweep sensory → projection → mushroom-body → descending neurons.

Descending neurons (DN-SEZ → feeding/head motor centre, DN-VNC → body motor
centre in the ventral nerve cord) are the brain's command output. Reading which
DN population fires — and the left/right asymmetry of DN-VNC — yields a behaviour:
feed / crawl / turn-left / turn-right / escape. So: real wiring + real dynamics
→ a fly action.

HONEST CAVEATS (same class the authors state): forward LIF only — no learning, no
neuromodulation, single-compartment, and the larval data lacks neurotransmitter
identity, so signs are approximated by cell class (local neurons = inhibitory).
This is real wiring run with simplified dynamics, not a conscious larva.

nDB (local write to verify the path): Neurons (type 70: skid, celltype, side,
modality), Synapse hyper_edges (170: roles pre/post + weight), time-stamped Spike
events (171: neuron + tick + scenario), Action events (172: behaviour + tick).

    python3 flybrain.py prepare   # raw Supplementary CSVs  -> connectome.json (compact, vendored)
    python3 flybrain.py           # connectome.json -> flybrain.json (viewer payload, all scenarios)
    NDB_URL=http://127.0.0.1:8742 python3 flybrain.py   # + write one scenario into nDB
"""
from __future__ import annotations
import csv, json, math, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
RAW = os.environ.get("LARVAL_DATA", "/tmp/larval-conn/data/Supplementary-Data-S1")
COMPACT = os.path.join(HERE, "connectome.json")
VIEWER = os.path.join(HERE, "flybrain.json")

# ---- deterministic RNG (no global Date/random) -------------------------------
_S = 0x2545F4914F6CDD1D
def seed(s):
    global _S; _S = (s * 0x9E3779B97F4A7C15 + 0xA0761D6478BD642F) & ((1 << 64) - 1)
def rnd():
    global _S; _S = (_S * 6364136223846793005 + 1442695040888963407) & ((1 << 64) - 1)
    return (_S >> 11) / float(1 << 53)

# ---- cell-class -> processing tier (sensory left .. descending right) --------
TIER = {
    "sensory": 0,
    "PN": 1, "PN-somato": 1,
    "LHN": 2, "KC": 2, "MBIN": 2, "LN": 2,
    "MBON": 3, "MB-FBN": 3, "MB-FFN": 3, "CN": 3,
    "pre-DN-VNC": 4, "pre-DN-SEZ": 4, "RGN": 4,
    "DN-VNC": 5, "DN-SEZ": 5, "ascending": 5,
}
MAXTIER = 5
INHIBITORY = {"LN"}                       # local neurons ~ GABAergic (approx.)
OUTPUT_SEZ = {"DN-SEZ"}                    # feeding / head motor command
OUTPUT_VNC = {"DN-VNC"}                    # body locomotion command


def prepare():
    """Parse the raw Supplementary CSVs into a compact connectome.json."""
    ann, modal, side = {}, {}, {}
    with open(os.path.join(RAW, "annotations.csv")) as f:
        for row in csv.DictReader(f):
            lid, rid = row["left_id"].strip(), row["right_id"].strip()
            ct, md = row["celltype"], row["additional_annotations"]
            if lid and lid != "no pair": ann[lid] = ct; modal[lid] = md; side[lid] = "L"
            if rid and rid != "no pair": ann[rid] = ct; modal[rid] = md; side[rid] = "R"
    # axo-dendritic adjacency (synapse counts) = directed signal flow,
    # axon of presynaptic -> dendrite of postsynaptic. Cleaner feed-forward
    # cascade than all-all (which also has axo-axonic + dendro-dendritic contacts).
    with open(os.path.join(RAW, "ad_connectivity_matrix.csv")) as f:
        r = csv.reader(f); cols = next(r)[1:]
        edges = []          # (src_idx, dst_idx, w)
        idx = {sid: i for i, sid in enumerate(cols)}
        in_tot = [0.0] * len(cols)
        for line in r:
            si = idx.get(line[0])
            if si is None: continue
            for j, val in enumerate(line[1:]):
                if val and val != "0":
                    w = float(val)
                    if w > 0:
                        edges.append((si, j, w)); in_tot[j] += w
    neurons = []
    for sid in cols:
        ct = ann.get(sid, "other")
        neurons.append({
            "skid": sid, "ct": ct, "side": side.get(sid, "U"),
            "mod": modal.get(sid, "") if ct == "sensory" else "",
            "tier": TIER.get(ct, 2),
        })
    out = {"neurons": neurons, "edges": edges, "in_tot": in_tot}
    with open(COMPACT, "w") as f: json.dump(out, f, separators=(",", ":"))
    print(f"prepared {COMPACT}: {len(neurons)} neurons, {len(edges)} edges "
          f"({os.path.getsize(COMPACT)//1024} KB)")


# ---- layout: x = processing tier, y = spread (left top / right bottom) -------
def layout(neurons):
    W, H = 1000.0, 620.0
    by_tier = {}
    for i, n in enumerate(neurons):
        by_tier.setdefault(n["tier"], []).append(i)
    pos = [None] * len(neurons)
    for tier, ids in by_tier.items():
        # sensory: sub-group by modality so a stimulated modality is a visible band
        if tier == 0:
            ids.sort(key=lambda i: (neurons[i]["mod"], neurons[i]["side"]))
        else:
            ids.sort(key=lambda i: (neurons[i]["side"], neurons[i]["ct"]))
        x = 40 + (tier / MAXTIER) * (W - 80)
        m = len(ids)
        for k, i in enumerate(ids):
            seed(i * 2654435761 + 7)
            jx = (rnd() - 0.5) * 34
            y = 30 + (m == 1 and 0.5 or k / (m - 1)) * (H - 60)
            pos[i] = (round(x + jx, 1), round(y, 1))
    return pos, int(W), int(H)


# ---- LIF simulation over the real wiring -------------------------------------
# Weights normalised by post-synaptic input (Shiu-style bounding); a gain term +
# moderate threshold lets even sparse sensory populations seed a visible cascade.
TICKS = 170
THRESH = 0.18          # fire when normalised input integral crosses this
GAIN = 2.4             # synaptic gain on top of in-degree normalisation
INH_FACTOR = -2.0      # inhibitory (local-neuron) synapse strength
GINH = 6.0             # global inhibition: competitive E/I balance (no runaway)
LEAK = 0.84
REFRAC = 4
DELAY = 1
STIM_ON, STIM_OFF = 10, 58   # one clean sensory pulse -> transient cascade
DRIVE = 0.9

def build_adj(C, gain=GAIN):
    N = len(C["neurons"]); in_tot = C["in_tot"]
    inh = [1 if C["neurons"][i]["ct"] in INHIBITORY else 0 for i in range(N)]
    out_adj = [[] for _ in range(N)]
    for (s, d, w) in C["edges"]:
        denom = in_tot[d] or 1.0
        wn = gain * (w / denom) * (INH_FACTOR if inh[s] else 1.0)
        out_adj[s].append((d, wn))
    return out_adj

def simulate(C, stim_ids, out_adj=None, thresh=THRESH, ginh=GINH):
    N = len(C["neurons"])
    if out_adj is None: out_adj = build_adj(C)
    v = [0.0] * N; refr = [0] * N
    pending = [[0.0] * N for _ in range(DELAY + 1)]
    stim = set(stim_ids)
    spikes = []                 # [neuron, tick]
    fired_log = []              # per-tick list of fired ids (for action readout + glow)
    prev_n = 0                  # neurons fired last tick (drives global inhibition)
    for t in range(TICKS):
        arrive = pending.pop(0); pending.append([0.0] * N)
        gi = ginh * (prev_n / N)            # E/I balance: more firing -> more damping
        fired = []
        for i in range(N):
            if refr[i] > 0:
                refr[i] -= 1; v[i] = 0.0; continue
            drive = DRIVE if (i in stim and STIM_ON <= t < STIM_OFF) else 0.0
            v[i] = max(0.0, v[i] * LEAK + arrive[i] + drive - gi)
            if v[i] >= thresh:
                fired.append(i); v[i] = 0.0; refr[i] = REFRAC
        buf = pending[DELAY - 1]
        for i in fired:
            for (d, wn) in out_adj[i]: buf[d] += wn
        for i in fired: spikes.append([i, t])
        fired_log.append(fired); prev_n = len(fired)
    return spikes, fired_log


def action_readout(C, fired_log):
    """Per-tick behaviour from descending-neuron firing RATES (EMA-smoothed,
    normalised per population). Feed = SEZ command dominant; locomotion = VNC
    command, split into crawl / turn (L-R asymmetry) / escape (high synchronous)."""
    N = len(C["neurons"]); neu = C["neurons"]
    sez = set(i for i in range(N) if neu[i]["ct"] in OUTPUT_SEZ)
    vncL = set(i for i in range(N) if neu[i]["ct"] in OUTPUT_VNC and neu[i]["side"] == "L")
    vncR = set(i for i in range(N) if neu[i]["ct"] in OUTPUT_VNC and neu[i]["side"] == "R")
    nS, nL, nR = len(sez) or 1, len(vncL) or 1, len(vncR) or 1
    DK = 0.6                       # EMA decay
    es = el = er = 0.0
    out = []
    for t, fired in enumerate(fired_log):
        s = sum(1 for i in fired if i in sez) / nS
        l = sum(1 for i in fired if i in vncL) / nL
        r = sum(1 for i in fired if i in vncR) / nR
        es = DK * es + (1 - DK) * s
        el = DK * el + (1 - DK) * l
        er = DK * er + (1 - DK) * r
        loco = el + er
        if es < 0.03 and loco < 0.04:
            label = "rest"
        elif es >= 1.15 * loco:                        # SEZ feeding command dominant
            label = "feed"
        elif loco >= 0.55:                             # strong synchronous body burst
            label = "escape"
        else:
            asym = (el - er) / (loco + 1e-9)
            if asym > 0.22:    label = "turn-left"
            elif asym < -0.22: label = "turn-right"
            else:              label = "crawl"
        out.append({"t": t, "sez": round(es, 3), "vl": round(el, 3),
                    "vr": round(er, 3), "label": label})
    return out


# (modality key, stimulus side, label, emoji). Unilateral odour -> asymmetric
# descending command -> a turn; bilateral taste -> feeding; light/heat -> locomotion.
SCENARIOS = [
    ("gustatory-external", "both", "taste (sugar)", "🍬"),
    ("olfactory",          "L",    "odour, left",   "👃"),
    ("visual",             "both", "light",         "💡"),
    ("thermo",             "both", "heat / cold",   "🌡"),
]

def stim_for(neu, key, side):
    out = []
    for i, n in enumerate(neu):
        if n["ct"] != "sensory": continue
        modok = n["mod"].startswith("thermo") if key == "thermo" else (n["mod"] == key)
        if modok and (side == "both" or n["side"] == side):
            out.append(i)
    return out

def build_viewer(C):
    pos, W, H = layout(C["neurons"])
    neurons = [{"i": i, "x": pos[i][0], "y": pos[i][1], "ct": n["ct"],
                "side": n["side"], "mod": n["mod"], "tier": n["tier"]}
               for i, n in enumerate(C["neurons"])]
    # edges for the view: keep the stronger ones so the canvas stays legible
    THR = 8
    vedges = [[s, d] for (s, d, w) in C["edges"] if w >= THR]
    out_adj = build_adj(C)
    runs = {}
    for key, side, label, emoji in SCENARIOS:
        stim = stim_for(C["neurons"], key, side)
        if not stim: continue
        spikes, flog = simulate(C, stim, out_adj)
        act = action_readout(C, flog)
        runs[key] = {"label": label, "emoji": emoji, "side": side, "stim": stim,
                     "spikes": spikes, "action": act, "ticks": TICKS}
        nf = len(spikes)
        dom = {}
        for a in act: dom[a["label"]] = dom.get(a["label"], 0) + 1
        print(f"  {key:20s}: stim {len(stim):3d} → {nf:5d} spikes "
              f"({nf/(len(C['neurons'])*TICKS)*100:.1f}%), "
              f"behaviour: {dict(sorted(dom.items(), key=lambda x:-x[1]))}")
    out = {"world": {"w": W, "h": H}, "neurons": neurons, "edges": vedges,
           "edge_thr": THR, "runs": runs, "n_edges_total": len(C["edges"]),
           "ticks": TICKS, "stim_window": [STIM_ON, STIM_OFF]}
    with open(VIEWER, "w") as f: json.dump(out, f, separators=(",", ":"))
    print(f"wrote {VIEWER} ({os.path.getsize(VIEWER)//1024} KB) "
          f"— {len(neurons)} neurons, {len(vedges)} view-edges (≥{THR} syn), "
          f"{len(runs)} scenarios")
    return out


def write_to_ndb(C, view, url, scenario="gustatory-external"):
    sys.path.insert(0, os.path.join(HERE, "..", "..", "..", "clients", "python"))
    try:
        from ndb_client import Client
    except Exception as e:
        print(f"(skip nDB write: {e})"); return 0
    import uuid
    db = Client(url); recs = []; nid = {}
    neu = C["neurons"]
    # Neurons (type 70). Cap synapse edges written so the demo write stays bounded.
    for i, n in enumerate(neu):
        x = str(uuid.uuid4()); nid[i] = x
        recs.append({"kind": "entity", "entity_id": x, "type_id": 70,
            "tx_id_assert": 0, "tx_id_supersede": "active", "properties": [
                {"prop_id": 40, "value": {"tag": "string", "value": n["ct"]}},
                {"prop_id": 41, "value": {"tag": "string", "value": n["side"]}},
                {"prop_id": 44, "value": {"tag": "string", "value": n["skid"]}},
                {"prop_id": 45, "value": {"tag": "string", "value": n.get("mod", "")}}]})
    EDGE_CAP = 8000
    strong = sorted(C["edges"], key=lambda e: -e[2])[:EDGE_CAP]
    for (s, d, w) in strong:
        recs.append({"kind": "hyper_edge", "hyperedge_id": str(uuid.uuid4()),
            "type_id": 170, "tx_id_assert": 0, "tx_id_supersede": "active",
            "roles": [{"role_id": 1, "entity_id": nid[s]}, {"role_id": 2, "entity_id": nid[d]}],
            "properties": [{"prop_id": 42, "value": {"tag": "f64", "value": float(w)}}]})
    run = view["runs"][scenario]
    for (i, t) in run["spikes"][:6000]:
        recs.append({"kind": "hyper_edge", "hyperedge_id": str(uuid.uuid4()),
            "type_id": 171, "tx_id_assert": 0, "tx_id_supersede": "active",
            "roles": [{"role_id": 5, "entity_id": nid[i]}],
            "properties": [{"prop_id": 43, "value": {"tag": "i64", "value": t}},
                           {"prop_id": 46, "value": {"tag": "string", "value": scenario}}]})
    # Action events: standalone facts (behaviour + tick), one per behaviour change.
    # Not a link between neurons -> an entity, not a hyper_edge.
    prev = None
    for a in run["action"]:
        if a["label"] != prev:
            prev = a["label"]
            recs.append({"kind": "entity", "entity_id": str(uuid.uuid4()),
                "type_id": 172, "tx_id_assert": 0, "tx_id_supersede": "active",
                "properties": [
                    {"prop_id": 43, "value": {"tag": "i64", "value": a["t"]}},
                    {"prop_id": 47, "value": {"tag": "string", "value": a["label"]}},
                    {"prop_id": 46, "value": {"tag": "string", "value": scenario}}]})
    w = 0
    try:
        for k in range(0, len(recs), 200):
            db.commit(recs[k:k + 200]); w += len(recs[k:k + 200])
    except Exception as e:
        print(f"(nDB write failed at {w}: {e})")
    return w


def tune(C):
    """Sweep global-inhibition strength; report per-scenario firing % + DN reach.
    Goal: low, stimulus-SPECIFIC activity (different DN subsets per modality)."""
    neu = C["neurons"]
    dn_sez = [i for i in range(len(neu)) if neu[i]["ct"] == "DN-SEZ"]
    dn_vncL = [i for i in range(len(neu)) if neu[i]["ct"] == "DN-VNC" and neu[i]["side"] == "L"]
    dn_vncR = [i for i in range(len(neu)) if neu[i]["ct"] == "DN-VNC" and neu[i]["side"] == "R"]
    sezS, vlS, vrS = set(dn_sez), set(dn_vncL), set(dn_vncR)
    oa = build_adj(C, GAIN)
    # report DN SPIKE-SHARE: total spikes from each DN group (the behaviour signal)
    for ginh in (4.0, 6.0, 9.0):
        line = f"ginh={ginh:4.1f}: "
        for key, sd, _, _ in SCENARIOS:
            stim = stim_for(neu, key, sd)
            sp, fl = simulate(C, stim, oa, THRESH, ginh)
            pct = len(sp) / (len(neu) * TICKS) * 100
            sz = sum(1 for i, _ in sp if i in sezS)
            l = sum(1 for i, _ in sp if i in vlS)
            r = sum(1 for i, _ in sp if i in vrS)
            line += f"{key[:4]}={pct:3.1f}%[SEZ{sz} L{l} R{r}] "
        print(line)

def main():
    if len(sys.argv) > 1 and sys.argv[1] == "prepare":
        prepare(); return
    if len(sys.argv) > 1 and sys.argv[1] == "tune":
        with open(COMPACT) as f: C = json.load(f)
        tune(C); return
    if not os.path.exists(COMPACT):
        print("connectome.json missing — running prepare first"); prepare()
    with open(COMPACT) as f: C = json.load(f)
    print(f"loaded connectome: {len(C['neurons'])} neurons, {len(C['edges'])} edges")
    view = build_viewer(C)
    url = os.environ.get("NDB_URL")
    if url:
        n = write_to_ndb(C, view, url)
        print(f"  wrote {n} records into nDB at {url}")


if __name__ == "__main__":
    main()
