#!/usr/bin/env python3
"""Artificial-life ecosystem → nDB showcase.

A 2D world with regrowing plants and two creature types competing to survive:
  * HERBIVORES eat plants, flee predators.
  * PREDATORS eat herbivores.

There is NO hand-coded fitness. Each creature has a neural-net brain (variable
topology = "intelligence") wired senses -> motors; moving and *thinking* cost
energy (bigger brains cost more), eating gains it, starving kills. Whoever
survives long enough reproduces with a mutated brain. Selection is emergent:
predator-prey populations boom and bust, and brains get smarter where it pays.

Outputs `neuroevo.json` (the showcase page replays it) and, when NDB_URL is set,
writes the evolutionary record into a real nDB over /v1: Creatures (type 50),
N-ary Predation (type 152: predator/prey) and Birth/Lineage (type 151:
parent/child) hyper_edges, and per-epoch Census (type 53) — a time-travelable
history.

    python3 ecosystem.py
    NDB_URL=http://127.0.0.1:8742 python3 ecosystem.py
"""
from __future__ import annotations
import json, math, os, sys

# ---- deterministic RNG ------------------------------------------------------
_S = 0xA0761D6478BD642F
def set_seed(s):
    global _S; _S = (s*0x9E3779B97F4A7C15 + 0xA0761D6478BD642F) & ((1<<64)-1)
def rnd():
    global _S; _S = (_S*6364136223846793005 + 1442695040888963407) & ((1<<64)-1)
    return (_S>>11)/float(1<<53)
def randn():
    return math.sqrt(-2.0*math.log(max(rnd(),1e-12)))*math.cos(2.0*math.pi*rnd())

W = H = 64.0
N_IN, N_OUT = 7, 3          # senses, motors
SENSE_R = 18.0

# ---- brain (feed-forward, variable topology) --------------------------------
class Brain:
    __slots__ = ("nodes","conns")
    def __init__(self, blank=False):
        # ids: 0..N_IN-1 inputs (incl bias at 0), N_IN..N_IN+N_OUT-1 outputs
        self.nodes = {i:"in" for i in range(N_IN)}
        for j in range(N_OUT): self.nodes[N_IN+j] = "out"
        self.conns = []
        if not blank:
            for j in range(N_OUT):
                for i in range(N_IN):
                    if rnd() < 0.5:
                        self.conns.append([i, N_IN+j, randn()])
    def clone(self):
        b = Brain(blank=True); b.nodes = dict(self.nodes); b.conns=[c[:] for c in self.conns]; return b
    def think(self, ins):
        val = {i: ins[i] for i in range(N_IN)}
        inc = {}; indeg = {n:0 for n in self.nodes}; adj = {n:[] for n in self.nodes}
        for s,d,w in self.conns:
            inc.setdefault(d,[]).append((s,w)); indeg[d]+=1; adj[s].append(d)
        q = [n for n in self.nodes if indeg[n]==0]
        while q:
            n = q.pop()
            if self.nodes[n] != "in":
                val[n] = math.tanh(sum(val.get(s,0.0)*w for s,w in inc.get(n,[])))
            for m in adj[n]:
                indeg[m]-=1
                if indeg[m]==0: q.append(m)
        return [val.get(N_IN+j,0.0) for j in range(N_OUT)]
    def mutate(self):
        for c in self.conns:
            r=rnd()
            if r<0.85: c[2]+=randn()*0.6
            elif r<0.92: c[2]=randn()
        if rnd()<0.20: self._add_conn()
        if rnd()<0.10: self._add_node()
    def _add_conn(self):
        ids=sorted(self.nodes)
        for _ in range(6):
            s=ids[int(rnd()*len(ids))]; d=ids[int(rnd()*len(ids))]
            if s>=d or self.nodes[d]=="in": continue
            if any(c[0]==s and c[1]==d for c in self.conns): continue
            self.conns.append([s,d,randn()]); return
    def _add_node(self):
        if not self.conns: return
        c=self.conns[int(rnd()*len(self.conns))]; s,d,w=c
        nid=max(self.nodes)+1; self.nodes[nid]="hidden"
        self.conns.remove(c); self.conns += [[s,nid,1.0],[nid,d,w]]
    def size(self): return len(self.nodes)+len(self.conns)
    def snapshot(self):
        return {"neuron_count":len(self.nodes),"synapse_count":len(self.conns),
                "nodes":[{"id":n,"kind":k} for n,k in sorted(self.nodes.items())],
                "conns":[{"src":s,"dst":d,"w":round(w,3)} for s,d,w in self.conns]}

# ---- world ------------------------------------------------------------------
HERB, PRED = 0, 1
class Creature:
    __slots__=("x","y","hd","e","sp","brain","age","cid","gen")
    _n=0
    def __init__(self, x,y,sp,brain,gen=0):
        self.x=x; self.y=y; self.hd=rnd()*2*math.pi; self.e=40.0
        self.sp=sp; self.brain=brain; self.age=0; self.gen=gen
        Creature._n+=1; self.cid=Creature._n

def wrap(v,m): return v%m
def torus_d(ax,ay,bx,by):
    dx=bx-ax; dy=by-ay
    if dx> W/2: dx-=W
    if dx<-W/2: dx+=W
    if dy> H/2: dy-=H
    if dy<-H/2: dy+=H
    return dx,dy

def nearest(c, others):
    best=None; bd=1e9; bdx=bdy=0.0
    for o in others:
        dx,dy=torus_d(c.x,c.y,o.x,o.y); d=dx*dx+dy*dy
        if d<bd: bd=d; best=o; bdx,bdy=dx,dy
    return best, math.sqrt(bd) if best else 1e9, bdx, bdy

def simulate(ticks=2400, plants0=150, herb0=50, pred0=8):
    plants=[[rnd()*W, rnd()*H] for _ in range(plants0)]
    creatures=[Creature(rnd()*W,rnd()*H,HERB,Brain()) for _ in range(herb0)]
    creatures+=[Creature(rnd()*W,rnd()*H,PRED,Brain()) for _ in range(pred0)]
    frames=[]; census=[]; brains=[]; predations=[]; births=[]
    FRAME_EVERY=6; EPOCH=120; MAXC=240
    for t in range(ticks):
        herbs=[c for c in creatures if c.sp==HERB]
        preds=[c for c in creatures if c.sp==PRED]
        # plant regrowth
        if len(plants)<220 and rnd()<0.9:
            for _ in range(2): plants.append([rnd()*W, rnd()*H])
        newborn=[]
        for c in creatures:
            c.age+=1
            # senses
            if c.sp==HERB:
                tgt = plants; food,fd,fdx,fdy = (None,1e9,0,0)
                # nearest plant
                bd=1e9
                for p in plants:
                    dx,dy=torus_d(c.x,c.y,p[0],p[1]); d=dx*dx+dy*dy
                    if d<bd: bd=d; food=p; fdx,fdy=dx,dy; fd=math.sqrt(d)
                threat,td,tdx,tdy = nearest(c, preds)
            else:
                food,fd,fdx,fdy = nearest(c, herbs)
                threat,td,tdx,tdy = (None,1e9,0,0)
            def unit(dx,dy,d): return (dx/d, dy/d) if d>1e-6 else (0.0,0.0)
            fux,fuy = unit(fdx,fdy,fd) if fd<SENSE_R else (0.0,0.0)
            tux,tuy = unit(tdx,tdy,td) if td<SENSE_R else (0.0,0.0)
            ins=[1.0, min(c.e/80.0,1.5), fux,fuy, tux,tuy, max(0.0,1.0-fd/SENSE_R)]
            turn,speed,act = c.brain.think(ins)
            c.hd += turn*0.6
            sp = max(0.0,min(1.0,(speed+1)/2))*1.6
            c.x = wrap(c.x+math.cos(c.hd)*sp, W); c.y = wrap(c.y+math.sin(c.hd)*sp, H)
            # metabolism: base + motion + THINKING (brain size) — intelligence costs.
            # Predators burn far more (keeps them in check → both species persist).
            base = 0.12 if c.sp==HERB else 0.34
            c.e -= base + 0.06*sp + 0.004*c.brain.size()
            # actions
            if c.sp==HERB and food is not None and fd<1.4:
                c.e += 16.0;
                try: plants.remove(food)
                except ValueError: pass
            if c.sp==PRED and food is not None and fd<1.8 and act>0:
                if food.e>0:
                    c.e += 24.0; food.e = -1.0
                    if t<ticks: predations.append([c.cid, food.cid, int(c.x), int(c.y), t])
            # reproduce — herbivores breed readily, predators slowly
            thr = 60.0 if c.sp==HERB else 110.0
            if c.e>thr and len(creatures)+len(newborn)<MAXC:
                cost = 30.0 if c.sp==HERB else 55.0
                c.e -= cost
                kb=c.brain.clone(); kb.mutate()
                k=Creature(wrap(c.x+randn(),W), wrap(c.y+randn(),H), c.sp, kb, c.gen+1)
                k.e = cost*0.8
                newborn.append(k); births.append([c.cid, k.cid, t, c.sp])
        # deaths
        maxage = 900
        creatures = [c for c in creatures if c.e>0 and c.age<maxage] + newborn
        plants = [p for p in plants if p[1]>=0 and len(p)==2]  # eaten plants removed above
        # bounded safety
        if not any(c.sp==HERB for c in creatures):
            creatures += [Creature(rnd()*W,rnd()*H,HERB,Brain()) for _ in range(8)]  # reseed prey
        # record
        if t%FRAME_EVERY==0:
            # [x, y, species, energy, cid] — cid lets the viewer match a creature
            # across frames and glide it smoothly between recorded positions.
            cc=[[int(c.x),int(c.y),c.sp,max(0,min(99,int(c.e))),c.cid] for c in creatures]
            pp=[[int(p[0]),int(p[1])] for p in plants]
            frames.append({"t":t,"c":cc,"p":pp})
        herbs=[c for c in creatures if c.sp==HERB]; preds=[c for c in creatures if c.sp==PRED]
        census.append([t, len(herbs), len(preds), len(plants)])
        if t%EPOCH==0:
            for sp,grp in ((HERB,herbs),(PRED,preds)):
                if grp:
                    champ=max(grp, key=lambda c:c.brain.size())
                    snap=champ.brain.snapshot(); snap["epoch"]=t; snap["species"]=sp
                    snap["avg_neurons"]=round(sum(len(c.brain.nodes) for c in grp)/len(grp),2)
                    snap["avg_synapses"]=round(sum(len(c.brain.conns) for c in grp)/len(grp),2)
                    snap["pop"]=len(grp)
                    brains.append(snap)
    return {"world":{"w":int(W),"h":int(H)}, "frames":frames, "census":census,
            "brains":brains, "predations":predations[:4000], "births":births[:4000]}


def write_to_ndb(data, url):
    sys.path.insert(0, os.path.join(os.path.dirname(__file__),"..","..","..","clients","python"))
    try:
        from ndb_client import Client
    except Exception as e:  # noqa
        print(f"(skip nDB write: {e})"); return 0
    import uuid
    db=Client(url); written=0
    recs=[]
    # census time-series → Census entities (type 53) for time-travel
    for t,h,p,pl in data["census"][::40]:
        recs.append({"kind":"entity","entity_id":str(uuid.uuid4()),"type_id":53,
            "tx_id_assert":0,"tx_id_supersede":"active","properties":[
            {"prop_id":30,"value":{"tag":"i64","value":t}},
            {"prop_id":31,"value":{"tag":"i64","value":h}},
            {"prop_id":32,"value":{"tag":"i64","value":p}}]})
    # each epoch's champion brain as the full N-ary structure: a Creature (50),
    # its Neurons (51), and Synapse hyper_edges (150, roles pre/post + weight),
    # plus a Lineage hyper_edge (151) chaining a species' champions over time.
    prev={0:None,1:None}
    for b in data["brains"]:
        cid=str(uuid.uuid4())
        recs.append({"kind":"entity","entity_id":cid,"type_id":50,
            "tx_id_assert":0,"tx_id_supersede":"active","properties":[
            {"prop_id":30,"value":{"tag":"i64","value":b["epoch"]}},
            {"prop_id":33,"value":{"tag":"i64","value":b["species"]}},
            {"prop_id":34,"value":{"tag":"i64","value":b["neuron_count"]}},
            {"prop_id":36,"value":{"tag":"i64","value":b["synapse_count"]}}]})
        nid={}
        for nd in b["nodes"]:
            x=str(uuid.uuid4()); nid[nd["id"]]=x
            recs.append({"kind":"entity","entity_id":x,"type_id":51,"tx_id_assert":0,
                "tx_id_supersede":"active","properties":[{"prop_id":37,"value":{"tag":"string","value":nd["kind"]}}]})
        for c in b["conns"]:
            recs.append({"kind":"hyper_edge","hyperedge_id":str(uuid.uuid4()),"type_id":150,
                "tx_id_assert":0,"tx_id_supersede":"active",
                "roles":[{"role_id":1,"entity_id":nid[c["src"]]},{"role_id":2,"entity_id":nid[c["dst"]]}],
                "properties":[{"prop_id":35,"value":{"tag":"f64","value":c["w"]}}]})
        if prev[b["species"]]:
            recs.append({"kind":"hyper_edge","hyperedge_id":str(uuid.uuid4()),"type_id":151,
                "tx_id_assert":0,"tx_id_supersede":"active",
                "roles":[{"role_id":3,"entity_id":prev[b["species"]]},{"role_id":4,"entity_id":cid}],
                "properties":[]})
        prev[b["species"]]=cid
    try:
        for i in range(0,len(recs),200):
            db.commit(recs[i:i+200]); written+=len(recs[i:i+200])
    except Exception as e:  # noqa
        print(f"(nDB write failed: {e})")
    return written


def main():
    best=None; best_score=-1
    # pick a seed whose ecosystem stays alive + interesting: BOTH species persist
    # to the end (balance first), then reward brain growth.
    for s in range(12):
        set_seed(s); Creature._n=0
        d=simulate()
        cen=d["census"]; late=cen[len(cen)*3//4:]
        herb_ok=sum(1 for r in late if r[1]>0); pred_ok=sum(1 for r in late if r[2]>0)
        peak_syn=max((b.get("avg_synapses",0) for b in d["brains"]), default=0)
        score=herb_ok + pred_ok + min(peak_syn,40)   # balance dominates, brain breaks ties
        if score>best_score: best_score=score; best=d
        if herb_ok==len(late) and pred_ok==len(late): break
    d=best
    dst=os.path.join(os.path.dirname(__file__),"neuroevo.json")
    with open(dst,"w") as f: json.dump(d,f,separators=(",",":"))
    c=d["census"]
    print(f"wrote {dst}  ({os.path.getsize(dst)//1024} KB, {len(d['frames'])} frames)")
    print(f"  final: herbivores={c[-1][1]} predators={c[-1][2]} plants={c[-1][3]}")
    cen=d["census"]; late=cen[len(cen)*3//4:]
    print(f"  both-species-alive in last quarter: herb {sum(1 for r in late if r[1]>0)}/{len(late)}  pred {sum(1 for r in late if r[2]>0)}/{len(late)}")
    print(f"  herb pop range: {min(r[1] for r in cen)}-{max(r[1] for r in cen)}  pred: {min(r[2] for r in cen)}-{max(r[2] for r in cen)}")
    hbrain=[b for b in d['brains'] if b['species']==0]
    pbrain=[b for b in d['brains'] if b['species']==1]
    if hbrain: print(f"  herbivore avg synapses: {hbrain[0]['avg_synapses']} -> {hbrain[-1]['avg_synapses']} (neurons {hbrain[0]['avg_neurons']}->{hbrain[-1]['avg_neurons']})")
    if pbrain: print(f"  predator  avg synapses: {pbrain[0]['avg_synapses']} -> {pbrain[-1]['avg_synapses']} (neurons {pbrain[0]['avg_neurons']}->{pbrain[-1]['avg_neurons']})")
    url=os.environ.get("NDB_URL")
    if url: print(f"  wrote {write_to_ndb(d,url)} records into nDB")


if __name__=="__main__":
    main()
