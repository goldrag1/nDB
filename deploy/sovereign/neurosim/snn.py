#!/usr/bin/env python3
"""Spiking neural-network simulation → nDB showcase.

A reservoir of leaky integrate-and-fire (LIF) neurons wired by excitatory and
inhibitory synapses on a 2D sheet. A periodic stimulus drives a few input
neurons; activity then cascades across the net as spikes propagate along
synapses (with a 1-tick delay), leak away, and — via the recurrent +
inhibitory wiring — produce travelling waves and bursts.

This is a *different* nDB showcase from the ecosystem: here nDB stores ONE
network's structure as N-ary facts (Neurons + Synapse hyper_edges) AND its
**temporal activity** — every spike is an event tagged with a tick, so "what
was the network doing at time T" is a native time-travel read. The viewer
replays it as a live network + a spike raster.

Outputs neurosim.json; writes to nDB over /v1 when NDB_URL is set:
Neurons (type 60), N-ary Synapse hyper_edges (type 160, roles pre/post +
weight), and Spike events (type 61: tick + neuron index) — time-travelable.

    python3 snn.py
    NDB_URL=http://127.0.0.1:8742 python3 snn.py
"""
from __future__ import annotations
import json, math, os, sys

_S = 0x12345678 ^ 0xA0761D6478BD642F
def set_seed(s):
    global _S; _S=(s*0x9E3779B97F4A7C15 + 0xA0761D6478BD642F)&((1<<64)-1)
def rnd():
    global _S; _S=(_S*6364136223846793005 + 1442695040888963407)&((1<<64)-1)
    return (_S>>11)/float(1<<53)

W=H=100.0
N=90               # neurons
FRAC_INH=0.22      # fraction inhibitory
K=10               # outgoing synapses per neuron (distance-biased)
THRESH=0.92
LEAK=0.93
REFRAC=3
DELAY=1
TICKS=520

def build():
    pos=[(rnd()*W, rnd()*H) for _ in range(N)]
    kind=[1 if rnd()<FRAC_INH else 0 for _ in range(N)]   # 1=inhibitory, 0=excitatory
    # distance-biased connectivity: mostly short edges + a few long ones → waves
    syn=[]  # (pre,post,w)
    for i in range(N):
        d=sorted(((((pos[i][0]-pos[j][0])**2+(pos[i][1]-pos[j][1])**2), j) for j in range(N) if j!=i))
        cand=[j for _,j in d[:18]]               # 18 nearest
        picks=set()
        for _ in range(K):
            j = cand[int(rnd()*len(cand))] if rnd()<0.8 else int(rnd()*N)
            if j!=i: picks.add(j)
        for j in picks:
            base = 0.78 if kind[i]==0 else -0.9   # inhibitory hits harder
            w = base*(0.6+0.8*rnd())
            syn.append((i,j,round(w,3)))
    return pos, kind, syn

def simulate(seed=0):
    set_seed(seed)
    pos,kind,syn = build()
    out_adj=[[] for _ in range(N)]
    for pi,(s,d,w) in enumerate(syn): out_adj[s].append((d,w))
    v=[0.0]*N; refr=[0]*N
    pending=[[0.0]*N]          # signals arriving at future ticks (delay buffer)
    inputs=[i for i in range(N) if pos[i][0]<25][:6] or list(range(5))   # left-edge input neurons
    spikes=[]                  # [neuron, tick]
    pot=[]                     # per-tick quantized potential 0..99
    for t in range(TICKS):
        arrive = pending.pop(0) if pending else [0.0]*N
        while len(pending)<DELAY: pending.append([0.0]*N)
        drive=[0.0]*N
        # periodic stimulus bursts into input neurons
        if (t % 22) < 3:
            for i in inputs: drive[i]+=0.62
        fired=[]
        for i in range(N):
            if refr[i]>0:
                refr[i]-=1; v[i]=0.0; continue
            v[i] = v[i]*LEAK + arrive[i] + drive[i]
            if v[i]>=THRESH:
                fired.append(i); v[i]=0.0; refr[i]=REFRAC
        # propagate this tick's spikes to arrive after DELAY
        buf=pending[DELAY-1] if DELAY-1<len(pending) else None
        if buf is None:
            pending.append([0.0]*N); buf=pending[-1]
        for i in fired:
            for (d,w) in out_adj[i]: buf[d]+=w
        for i in fired: spikes.append([i,t])
        pot.append([max(0,min(99,int(v[i]/THRESH*99))) for i in range(N)])
    return {
        "world":{"w":int(W),"h":int(H)},
        "neurons":[{"i":i,"x":round(pos[i][0],1),"y":round(pos[i][1],1),
                    "kind":("inh" if kind[i] else "exc"),"input":(i in inputs)} for i in range(N)],
        "synapses":[{"pre":s,"post":d,"w":w} for (s,d,w) in syn],
        "spikes":spikes, "pot":pot, "ticks":TICKS,
    }

def write_to_ndb(data,url):
    sys.path.insert(0, os.path.join(os.path.dirname(__file__),"..","..","..","clients","python"))
    try:
        from ndb_client import Client
    except Exception as e:  # noqa
        print(f"(skip nDB write: {e})"); return 0
    import uuid
    db=Client(url); recs=[]; nid={}
    for nd in data["neurons"]:
        x=str(uuid.uuid4()); nid[nd["i"]]=x
        recs.append({"kind":"entity","entity_id":x,"type_id":60,"tx_id_assert":0,"tx_id_supersede":"active",
            "properties":[{"prop_id":40,"value":{"tag":"string","value":nd["kind"]}},
            {"prop_id":41,"value":{"tag":"i64","value":nd["i"]}}]})
    for sy in data["synapses"]:
        recs.append({"kind":"hyper_edge","hyperedge_id":str(uuid.uuid4()),"type_id":160,"tx_id_assert":0,
            "tx_id_supersede":"active","roles":[{"role_id":1,"entity_id":nid[sy["pre"]]},{"role_id":2,"entity_id":nid[sy["post"]]}],
            "properties":[{"prop_id":42,"value":{"tag":"f64","value":sy["w"]}}]})
    # spike events as N-ary facts: a SpikeEvent hyper_edge links the neuron, tagged with the tick
    for (i,t) in data["spikes"][:5000]:
        recs.append({"kind":"hyper_edge","hyperedge_id":str(uuid.uuid4()),"type_id":161,"tx_id_assert":0,
            "tx_id_supersede":"active","roles":[{"role_id":5,"entity_id":nid[i]}],
            "properties":[{"prop_id":43,"value":{"tag":"i64","value":t}}]})
    w=0
    try:
        for k in range(0,len(recs),200): db.commit(recs[k:k+200]); w+=len(recs[k:k+200])
    except Exception as e:  # noqa
        print(f"(nDB write failed: {e})")
    return w

def main():
    # pick a seed with lively-but-not-saturated activity (good spike rate)
    best=None;best_score=-1
    for s in range(8):
        d=simulate(s)
        ns=len(d["spikes"]); rate=ns/(N*TICKS)
        score=-abs(rate-0.04)         # target ~4% of neuron-ticks spiking
        if score>best_score: best_score=score; best=d
    d=best
    dst=os.path.join(os.path.dirname(__file__),"neurosim.json")
    with open(dst,"w") as f: json.dump(d,f,separators=(",",":"))
    ns=len(d["spikes"]); exc=sum(1 for n in d["neurons"] if n["kind"]=="exc")
    print(f"wrote {dst} ({os.path.getsize(dst)//1024} KB)")
    print(f"  {N} neurons ({exc} exc / {N-exc} inh), {len(d['synapses'])} synapses, {ns} spikes over {TICKS} ticks")
    print(f"  mean firing rate {ns/(N*TICKS)*100:.1f}% of neuron-ticks")
    url=os.environ.get("NDB_URL")
    if url: print(f"  wrote {write_to_ndb(d,url)} records into nDB")

if __name__=="__main__":
    main()
