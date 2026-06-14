#!/usr/bin/env python3
"""NEAT-lite neuro-evolution → nDB showcase.

Evolves feed-forward creatures on XOR. "Intelligence" = neuron + synapse count,
which grows as topology mutations (add-node / add-connection) accumulate. Each
generation's champion (its full brain) is recorded, so the result is an
evolution *history* you can time-travel through.

Two outputs:
  1. `neuroevo.json` — the per-generation champions (chart + brain-graph + slider
     data) the static showcase page loads.
  2. (optional) writes the morphology into a real nDB over /v1 when NDB_URL is
     set — Creatures (type 50), Neurons (type 51), N-ary Synapse hyperedges
     (type 150, roles pre/post + weight), and Lineage hyperedges (type 151,
     roles parent/child) — to prove the N-ary model end-to-end.

Pure stdlib + a deterministic RNG so the run is reproducible.

    python3 evolve.py                       # -> neuroevo.json
    NDB_URL=http://127.0.0.1:8742 python3 evolve.py   # + write to a local nDB
"""

from __future__ import annotations

import json
import math
import os
import sys

# ---- deterministic RNG (LCG) so the showcase is reproducible ----------------
_STATE = 0x2545F4914F6CDD1D


def set_seed(s: int) -> None:
    global _STATE
    _STATE = (s * 0x9E3779B97F4A7C15 + 0x2545F4914F6CDD1D) & ((1 << 64) - 1)


def rnd() -> float:
    global _STATE
    _STATE = (_STATE * 6364136223846793005 + 1442695040888963407) & ((1 << 64) - 1)
    return (_STATE >> 11) / float(1 << 53)


def randn() -> float:
    # Box-Muller from two uniforms.
    u1 = max(rnd(), 1e-12)
    u2 = rnd()
    return math.sqrt(-2.0 * math.log(u1)) * math.cos(2.0 * math.pi * u2)


# ---- genome -----------------------------------------------------------------
# Nodes: 0,1 = inputs, 2 = bias(=1), 3 = output, >=4 = hidden.
# Connections: (src, dst, weight) with src "before" dst in node order (DAG).
IN, BIAS, OUT, FIRST_HIDDEN = (0, 1), 2, 3, 4

XOR = [((0, 0), 0), ((0, 1), 1), ((1, 0), 1), ((1, 1), 0)]


class Genome:
    __slots__ = ("nodes", "conns")

    def __init__(self):
        # node id -> kind
        self.nodes = {0: "in", 1: "in", 2: "bias", 3: "out"}
        # list of [src, dst, weight]
        self.conns = [[0, 3, randn()], [1, 3, randn()], [2, 3, randn()]]

    def clone(self) -> "Genome":
        g = Genome.__new__(Genome)
        g.nodes = dict(self.nodes)
        g.conns = [c[:] for c in self.conns]
        return g

    def next_node_id(self) -> int:
        return max(self.nodes) + 1

    def evaluate(self, x0: float, x1: float) -> float:
        # Feed-forward with a real topological order (the output node has a LOW
        # id but depends on higher-id hidden nodes, so id-order is wrong — do
        # Kahn's algorithm over the connection DAG).
        val = {0: x0, 1: x1, 2: 1.0}
        incoming: dict[int, list] = {}
        indeg: dict[int, int] = {n: 0 for n in self.nodes}
        adj: dict[int, list] = {n: [] for n in self.nodes}
        for s, d, w in self.conns:
            incoming.setdefault(d, []).append((s, w))
            indeg[d] += 1
            adj[s].append(d)
        queue = [n for n in self.nodes if indeg[n] == 0]
        while queue:
            n = queue.pop()
            if self.nodes[n] in ("out", "hidden"):
                z = sum(val.get(s, 0.0) * w for s, w in incoming.get(n, []))
                val[n] = math.tanh(z)
            for m in adj[n]:
                indeg[m] -= 1
                if indeg[m] == 0:
                    queue.append(m)
        return val.get(OUT, 0.0)

    def fitness(self) -> float:
        # 4 - SSE on XOR (output squashed to 0..1). 4.0 = perfect.
        sse = 0.0
        for (a, b), target in XOR:
            out = (self.evaluate(float(a), float(b)) + 1.0) / 2.0
            sse += (out - target) ** 2
        return 4.0 - sse

    # mutations --------------------------------------------------------------
    def mutate(self):
        for c in self.conns:
            r = rnd()
            if r < 0.7:
                c[2] += randn() * 1.5  # perturb
            elif r < 0.78:
                c[2] += randn() * 5.0  # occasional big jump (escape basins)
            elif r < 0.85:
                c[2] = randn() * 2.0  # reset
        if rnd() < 0.25:
            self._add_connection()
        if rnd() < 0.15:
            self._add_node()

    def _add_connection(self):
        ids = sorted(self.nodes)
        for _ in range(8):
            s = ids[int(rnd() * len(ids))]
            d = ids[int(rnd() * len(ids))]
            if s >= d:  # forward-only keeps the DAG
                continue
            if self.nodes[d] in ("in", "bias"):
                continue
            if any(c[0] == s and c[1] == d for c in self.conns):
                continue
            self.conns.append([s, d, randn()])
            return

    def crossover(self, other: "Genome") -> "Genome":
        # Child = clone of self (the fitter parent); for connections shared with
        # `other`, inherit the weight from a random parent (NEAT-lite).
        child = self.clone()
        omap = {(s, d): w for s, d, w in other.conns}
        for c in child.conns:
            key = (c[0], c[1])
            if key in omap and rnd() < 0.5:
                c[2] = omap[key]
        return child

    def _add_node(self):
        if not self.conns:
            return
        c = self.conns[int(rnd() * len(self.conns))]
        s, d, w = c
        nid = self.next_node_id()
        self.nodes[nid] = "hidden"
        # split s->d into s->nid (w=1) and nid->d (w=old): preserves behaviour.
        self.conns.remove(c)
        self.conns.append([s, nid, 1.0])
        self.conns.append([nid, d, w])

    # serialisation ----------------------------------------------------------
    def snapshot(self) -> dict:
        return {
            "neuron_count": len(self.nodes),
            "synapse_count": len(self.conns),
            "nodes": [{"id": n, "kind": k} for n, k in sorted(self.nodes.items())],
            "conns": [{"src": s, "dst": d, "w": round(w, 4)} for s, d, w in self.conns],
        }


def _tournament(scored, pop_size):
    picks = [scored[int(rnd() * pop_size)] for _ in range(3)]
    return max(picks, key=lambda g: g.fitness())


def evolve(pop_size: int = 150, generations: int = 150):
    pop = [Genome() for _ in range(pop_size)]
    history = []
    best = None  # best brain seen so far → monotone champion line
    for gen in range(generations):
        scored = sorted(pop, key=lambda g: g.fitness(), reverse=True)
        if best is None or scored[0].fitness() > best.fitness():
            best = scored[0].clone()
        snap = best.snapshot()
        snap["generation"] = gen
        snap["fitness"] = round(best.fitness(), 4)
        history.append(snap)
        # next generation: elitism + crossover + mutation
        nxt = [scored[0].clone(), scored[1].clone()]
        while len(nxt) < pop_size:
            a = _tournament(scored, pop_size)
            b = _tournament(scored, pop_size)
            parent = a if a.fitness() >= b.fitness() else b
            child = parent.crossover(b if parent is a else a)
            child.mutate()
            nxt.append(child)
        pop = nxt
    return history


def best_run(seeds=24, **kw):
    """Run several seeds, keep the history whose peak fitness is highest — so the
    showcase is a genuine *successful* evolution, reproducibly chosen."""
    best_hist, best_peak = None, -1.0
    for s in range(seeds):
        set_seed(s)
        h = evolve(**kw)
        peak = max(x["fitness"] for x in h)
        if peak > best_peak:
            best_peak, best_hist = peak, h
        if best_peak >= 3.97:  # XOR effectively solved — stop early
            break
    return best_hist, best_peak


def write_to_ndb(history, url):
    """Best-effort: write champions + neurons + N-ary synapses + lineage into a
    real nDB over /v1. Failure never blocks the JSON export."""
    sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "..", "clients", "python"))
    try:
        from ndb_client import Client
    except Exception as e:  # noqa: BLE001
        print(f"(skip nDB write — client import failed: {e})")
        return 0
    import uuid

    db = Client(url)
    PROP_GEN, PROP_FIT, PROP_NEUR, PROP_SYN, PROP_KIND, PROP_CONNECTOME = 30, 31, 32, 33, 34, 35
    ROLE_PRE, ROLE_POST, ROLE_PARENT, ROLE_CHILD = 1, 2, 3, 4
    T_CREATURE, T_NEURON, E_SYNAPSE, E_LINEAGE = 50, 51, 150, 151
    prev_champ = None
    written = 0
    for snap in history:
        recs = []
        cid = str(uuid.uuid4())
        recs.append({
            "kind": "entity", "entity_id": cid, "type_id": T_CREATURE,
            "tx_id_assert": 0, "tx_id_supersede": "active",
            "properties": [
                {"prop_id": PROP_GEN, "value": {"tag": "i64", "value": snap["generation"]}},
                {"prop_id": PROP_FIT, "value": {"tag": "f64", "value": snap["fitness"]}},
                {"prop_id": PROP_NEUR, "value": {"tag": "i64", "value": snap["neuron_count"]}},
                {"prop_id": PROP_SYN, "value": {"tag": "i64", "value": snap["synapse_count"]}},
                {"prop_id": PROP_CONNECTOME, "value": {"tag": "string", "value": json.dumps({"nodes": snap["nodes"], "conns": snap["conns"]})}},
            ],
        })
        node_ids = {}
        for nd in snap["nodes"]:
            nid = str(uuid.uuid4())
            node_ids[nd["id"]] = nid
            recs.append({
                "kind": "entity", "entity_id": nid, "type_id": T_NEURON,
                "tx_id_assert": 0, "tx_id_supersede": "active",
                "properties": [{"prop_id": PROP_KIND, "value": {"tag": "string", "value": nd["kind"]}}],
            })
        for c in snap["conns"]:
            recs.append({
                "kind": "hyper_edge", "hyperedge_id": str(uuid.uuid4()), "type_id": E_SYNAPSE,
                "tx_id_assert": 0, "tx_id_supersede": "active",
                "roles": [
                    {"role_id": ROLE_PRE, "entity_id": node_ids[c["src"]]},
                    {"role_id": ROLE_POST, "entity_id": node_ids[c["dst"]]},
                ],
                "properties": [{"prop_id": PROP_GEN, "value": {"tag": "f64", "value": c["w"]}}],
            })
        if prev_champ:
            recs.append({
                "kind": "hyper_edge", "hyperedge_id": str(uuid.uuid4()), "type_id": E_LINEAGE,
                "tx_id_assert": 0, "tx_id_supersede": "active",
                "roles": [
                    {"role_id": ROLE_PARENT, "entity_id": prev_champ},
                    {"role_id": ROLE_CHILD, "entity_id": cid},
                ],
                "properties": [],
            })
        try:
            db.commit(recs)
            written += len(recs)
        except Exception as e:  # noqa: BLE001
            print(f"(nDB write failed at gen {snap['generation']}: {e})")
            return written
        prev_champ = cid
    return written


def main():
    history, peak = best_run()
    print(f"best run peak fitness = {peak:.3f} (4.0 = XOR solved)")
    out = {
        "task": "XOR",
        "generations": len(history),
        "history": history,
        "note": "Each generation's champion brain; intelligence = neuron + synapse count.",
    }
    dst = os.path.join(os.path.dirname(__file__), "neuroevo.json")
    with open(dst, "w") as f:
        json.dump(out, f, separators=(",", ":"))
    first, last = history[0], history[-1]
    print(f"wrote {dst}")
    print(f"  gen 0:  fitness={first['fitness']}  neurons={first['neuron_count']}  synapses={first['synapse_count']}")
    print(f"  gen {last['generation']}: fitness={last['fitness']}  neurons={last['neuron_count']}  synapses={last['synapse_count']}")
    url = os.environ.get("NDB_URL")
    if url:
        n = write_to_ndb(history, url)
        print(f"  wrote {n} records into nDB at {url}")


if __name__ == "__main__":
    main()
