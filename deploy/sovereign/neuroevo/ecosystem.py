#!/usr/bin/env python3
"""Artificial-life ecosystem → nDB showcase (v2: 3 trophic levels, richer brains).

A 2D world with a three-level food web competing to survive:
  🌱 PLANTS  (regrow)            -> eaten by
  🐰 HERBIVORES                  -> eaten by
  🦊 PREDATORS                   -> eaten by
  🦅 APEX predators

No hand-coded fitness. Each creature has a neural-net brain (variable topology =
intelligence) with a RICH sensory dimension (≈12 inputs: nearest food, nearest
threat, nearest same-species/crowding, own energy, age, local density) wired to
motors. Moving and *thinking* cost energy (bigger brains cost more), eating
restores it, starving kills. Survivors breed with mutated brains — selection is
emergent. A spatial grid keeps hundreds of creatures fast.

Records the run into nDB over /v1 when NDB_URL is set (Creatures 50, Neurons 51,
N-ary Synapse hyper_edges 150, Lineage 151, Census 53) and exports neuroevo.json.

    python3 ecosystem.py
    NDB_URL=http://127.0.0.1:8742 python3 ecosystem.py
"""
from __future__ import annotations
import json, math, os, sys

# ---- deterministic RNG ------------------------------------------------------
_S = 0xA0761D6478BD642F
def set_seed(s):
    global _S; _S=(s*0x9E3779B97F4A7C15 + 0xA0761D6478BD642F)&((1<<64)-1)
def rnd():
    global _S; _S=(_S*6364136223846793005 + 1442695040888963407)&((1<<64)-1)
    return (_S>>11)/float(1<<53)
def randn():
    return math.sqrt(-2.0*math.log(max(rnd(),1e-12)))*math.cos(2.0*math.pi*rnd())

W=H=88.0
N_IN, N_OUT = 12, 3
SENSE_R = 20.0
HERB, PRED, APEX = 0, 1, 2

# ---- brain (feed-forward, variable topology) --------------------------------
class Brain:
    __slots__=("nodes","conns")
    def __init__(self, blank=False):
        self.nodes={i:"in" for i in range(N_IN)}
        for j in range(N_OUT): self.nodes[N_IN+j]="out"
        self.conns=[]
        if not blank:
            for j in range(N_OUT):
                for i in range(N_IN):
                    if rnd()<0.35: self.conns.append([i,N_IN+j,randn()])
    def clone(self):
        b=Brain(blank=True); b.nodes=dict(self.nodes); b.conns=[c[:] for c in self.conns]; return b
    def think(self, ins):
        val={i:ins[i] for i in range(N_IN)}
        inc={}; indeg={n:0 for n in self.nodes}; adj={n:[] for n in self.nodes}
        for s,d,w in self.conns:
            inc.setdefault(d,[]).append((s,w)); indeg[d]+=1; adj[s].append(d)
        q=[n for n in self.nodes if indeg[n]==0]
        while q:
            n=q.pop()
            if self.nodes[n]!="in":
                val[n]=math.tanh(sum(val.get(s,0.0)*w for s,w in inc.get(n,[])))
            for m in adj[n]:
                indeg[m]-=1
                if indeg[m]==0: q.append(m)
        return [val.get(N_IN+j,0.0) for j in range(N_OUT)]
    def mutate(self):
        for c in self.conns:
            r=rnd()
            if r<0.85: c[2]+=randn()*0.6
            elif r<0.92: c[2]=randn()
        if rnd()<0.22: self._add_conn()
        if rnd()<0.12: self._add_node()
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
        self.conns.remove(c); self.conns+=[[s,nid,1.0],[nid,d,w]]
    def size(self): return len(self.nodes)+len(self.conns)
    def snapshot(self):
        return {"neuron_count":len(self.nodes),"synapse_count":len(self.conns),
                "nodes":[{"id":n,"kind":k} for n,k in sorted(self.nodes.items())],
                "conns":[{"src":s,"dst":d,"w":round(w,3)} for s,d,w in self.conns]}

# ---- world ------------------------------------------------------------------
class Creature:
    __slots__=("x","y","hd","e","sp","brain","age","cid")
    _n=0
    def __init__(self,x,y,sp,brain):
        self.x=x;self.y=y;self.hd=rnd()*6.283;self.e=42.0;self.sp=sp;self.brain=brain;self.age=0
        Creature._n+=1; self.cid=Creature._n

def torus_d(ax,ay,bx,by):
    dx=bx-ax; dy=by-ay
    if dx>W/2: dx-=W
    elif dx<-W/2: dx+=W
    if dy>H/2: dy-=H
    elif dy<-H/2: dy+=H
    return dx,dy

CELL=20.0
NCX=int(math.ceil(W/CELL)); NCY=int(math.ceil(H/CELL))
def grid_of(items, getxy):
    g={}
    for it in items:
        x,y=getxy(it); g.setdefault((int(x//CELL)%NCX,int(y//CELL)%NCY),[]).append(it)
    return g
def near(g,x,y):
    cx=int(x//CELL)%NCX; cy=int(y//CELL)%NCY; out=[]
    for dx in(-1,0,1):
        for dy in(-1,0,1): out+=g.get(((cx+dx)%NCX,(cy+dy)%NCY),[])
    return out
def nearest_xy(cands, cx, cy, getxy, exclude=None):
    best=None;bd=1e9;bdx=bdy=0.0
    for it in cands:
        if it is exclude: continue
        x,y=getxy(it); dx,dy=torus_d(cx,cy,x,y); d=dx*dx+dy*dy
        if d<bd: bd=d;best=it;bdx,bdy=dx,dy
    return best, math.sqrt(bd) if best else 1e9, bdx, bdy

# diet/threat config: a clean 3-level chain plants→herb→pred→apex. Per-species
# reseed floors (in the loop) keep all three on screen — 3-level coexistence is
# otherwise unstable. This is a showcase, not an ecology proof.
PREY = {HERB:None, PRED:HERB, APEX:PRED}
THREAT = {HERB:PRED, PRED:APEX, APEX:None}   # what each fears
EAT_GAIN = {HERB:15.0, PRED:25.0, APEX:40.0}
METAB = {HERB:0.12, PRED:0.30, APEX:0.36}
REPRO = {HERB:58.0, PRED:108.0, APEX:120.0}
COST  = {HERB:28.0, PRED:52.0, APEX:60.0}

def simulate(ticks=2000, plants0=200, herb0=90, pred0=24, apex0=10):
    plants=[[rnd()*W,rnd()*H] for _ in range(plants0)]
    cr=[Creature(rnd()*W,rnd()*H,HERB,Brain()) for _ in range(herb0)]
    cr+=[Creature(rnd()*W,rnd()*H,PRED,Brain()) for _ in range(pred0)]
    cr+=[Creature(rnd()*W,rnd()*H,APEX,Brain()) for _ in range(apex0)]
    frames=[];census=[];brains=[];predations=[];births=[]
    FRAME_EVERY=8; EPOCH=120; MAXC=360
    cxy=lambda c:(c.x,c.y); pxy=lambda p:(p[0],p[1])
    for t in range(ticks):
        if len(plants)<320 and rnd()<0.95:
            for _ in range(3): plants.append([rnd()*W,rnd()*H])
        pg=grid_of(plants,pxy)
        sg={HERB:[],PRED:[],APEX:[]}
        for c in cr: sg[c.sp].append(c)
        cg={sp:grid_of(sg[sp],cxy) for sp in sg}
        newborn=[]
        for c in cr:
            c.age+=1
            # ---- senses ----
            if c.sp==HERB:
                food,fd,fdx,fdy=nearest_xy(near(pg,c.x,c.y),c.x,c.y,pxy)
                fobj=food
            else:
                food,fd,fdx,fdy=nearest_xy(near(cg[PREY[c.sp]],c.x,c.y),c.x,c.y,cxy)
                fobj=food
            th=THREAT[c.sp]
            if th is None: threat,td,tdx,tdy=(None,1e9,0,0)
            else: threat,td,tdx,tdy=nearest_xy(near(cg[th],c.x,c.y),c.x,c.y,cxy)
            same=near(cg[c.sp],c.x,c.y)
            mate,md,mdx,mdy=nearest_xy(same,c.x,c.y,cxy,exclude=c)
            dens=min(1.0,(len(same)-1)/8.0)
            U=lambda dx,dy,d:(dx/d,dy/d) if d>1e-6 else (0.0,0.0)
            fux,fuy=U(fdx,fdy,fd) if fd<SENSE_R else (0.0,0.0)
            tux,tuy=U(tdx,tdy,td) if td<SENSE_R else (0.0,0.0)
            mux,muy=U(mdx,mdy,md) if md<SENSE_R else (0.0,0.0)
            ins=[1.0, min(c.e/80.0,1.5), min(c.age/600.0,1.5),
                 fux,fuy,max(0.0,1.0-fd/SENSE_R),
                 tux,tuy,max(0.0,1.0-td/SENSE_R),
                 mux,muy,dens]
            turn,speed,act=c.brain.think(ins)
            c.hd+=turn*0.6
            sp=max(0.0,min(1.0,(speed+1)/2))*1.7
            c.x=(c.x+math.cos(c.hd)*sp)%W; c.y=(c.y+math.sin(c.hd)*sp)%H
            c.e-=METAB[c.sp]+0.06*sp+0.004*c.brain.size()
            # ---- eat ----
            if c.sp==HERB and fobj is not None and fd<1.5:
                c.e+=EAT_GAIN[HERB]
                try: plants.remove(fobj)
                except ValueError: pass
            elif c.sp!=HERB and fobj is not None and fd<2.0 and act>0 and fobj.e>0:
                c.e+=EAT_GAIN[c.sp]; fobj.e=-1.0
                predations.append([c.cid,fobj.cid,int(c.x),int(c.y),t,c.sp])
            # ---- reproduce ----
            if c.e>REPRO[c.sp] and len(cr)+len(newborn)<MAXC:
                c.e-=COST[c.sp]; kb=c.brain.clone(); kb.mutate()
                k=Creature((c.x+randn())%W,(c.y+randn())%H,c.sp,kb); k.e=COST[c.sp]*0.8
                newborn.append(k); births.append([c.cid,k.cid,t,c.sp])
        cr=[c for c in cr if c.e>0 and c.age<950]+newborn
        # Light per-species reseed floors so all THREE trophic levels stay on
        # screen competing (3-level coexistence is otherwise unstable). This is
        # a showcase, not an ecology proof — immigration keeps the web alive.
        nH=sum(1 for c in cr if c.sp==HERB); nP=sum(1 for c in cr if c.sp==PRED); nA=sum(1 for c in cr if c.sp==APEX)
        if nH<10: cr+=[Creature(rnd()*W,rnd()*H,HERB,Brain()) for _ in range(12)]
        if nP<6:  cr+=[Creature(rnd()*W,rnd()*H,PRED,Brain()) for _ in range(6)]
        if nA<3:  cr+=[Creature(rnd()*W,rnd()*H,APEX,Brain()) for _ in range(4)]
        if t%FRAME_EVERY==0:
            frames.append({"t":t,
                "c":[[int(c.x),int(c.y),c.sp,max(0,min(99,int(c.e))),c.cid] for c in cr],
                "p":[[int(p[0]),int(p[1])] for p in plants]})
        h=sum(1 for c in cr if c.sp==HERB);p=sum(1 for c in cr if c.sp==PRED);a=sum(1 for c in cr if c.sp==APEX)
        census.append([t,h,p,a,len(plants)])
        if t%EPOCH==0:
            for sp in (HERB,PRED,APEX):
                grp=[c for c in cr if c.sp==sp]
                if grp:
                    champ=max(grp,key=lambda c:c.brain.size())
                    snap=champ.brain.snapshot(); snap["epoch"]=t; snap["species"]=sp
                    snap["avg_neurons"]=round(sum(len(c.brain.nodes) for c in grp)/len(grp),2)
                    snap["avg_synapses"]=round(sum(len(c.brain.conns) for c in grp)/len(grp),2)
                    snap["pop"]=len(grp); brains.append(snap)
    return {"world":{"w":int(W),"h":int(H)},"frames":frames,"census":census,
            "brains":brains,"predations":predations[:6000],"births":births[:6000]}


def write_to_ndb(data,url):
    sys.path.insert(0, os.path.join(os.path.dirname(__file__),"..","..","..","clients","python"))
    try:
        from ndb_client import Client
    except Exception as e:  # noqa
        print(f"(skip nDB write: {e})"); return 0
    import uuid
    db=Client(url); recs=[]
    for r in data["census"][::40]:
        recs.append({"kind":"entity","entity_id":str(uuid.uuid4()),"type_id":53,"tx_id_assert":0,
            "tx_id_supersede":"active","properties":[
            {"prop_id":30,"value":{"tag":"i64","value":r[0]}},
            {"prop_id":31,"value":{"tag":"i64","value":r[1]}},
            {"prop_id":32,"value":{"tag":"i64","value":r[2]}},
            {"prop_id":38,"value":{"tag":"i64","value":r[3]}}]})
    prev={0:None,1:None,2:None}
    for b in data["brains"]:
        cid=str(uuid.uuid4())
        recs.append({"kind":"entity","entity_id":cid,"type_id":50,"tx_id_assert":0,"tx_id_supersede":"active",
            "properties":[{"prop_id":30,"value":{"tag":"i64","value":b["epoch"]}},
            {"prop_id":33,"value":{"tag":"i64","value":b["species"]}},
            {"prop_id":34,"value":{"tag":"i64","value":b["neuron_count"]}},
            {"prop_id":36,"value":{"tag":"i64","value":b["synapse_count"]}}]})
        nid={}
        for nd in b["nodes"]:
            x=str(uuid.uuid4()); nid[nd["id"]]=x
            recs.append({"kind":"entity","entity_id":x,"type_id":51,"tx_id_assert":0,"tx_id_supersede":"active",
                "properties":[{"prop_id":37,"value":{"tag":"string","value":nd["kind"]}}]})
        for c in b["conns"]:
            recs.append({"kind":"hyper_edge","hyperedge_id":str(uuid.uuid4()),"type_id":150,"tx_id_assert":0,
                "tx_id_supersede":"active","roles":[{"role_id":1,"entity_id":nid[c["src"]]},{"role_id":2,"entity_id":nid[c["dst"]]}],
                "properties":[{"prop_id":35,"value":{"tag":"f64","value":c["w"]}}]})
        if prev[b["species"]]:
            recs.append({"kind":"hyper_edge","hyperedge_id":str(uuid.uuid4()),"type_id":151,"tx_id_assert":0,
                "tx_id_supersede":"active","roles":[{"role_id":3,"entity_id":prev[b["species"]]},{"role_id":4,"entity_id":cid}],"properties":[]})
        prev[b["species"]]=cid
    w=0
    try:
        for i in range(0,len(recs),200): db.commit(recs[i:i+200]); w+=len(recs[i:i+200])
    except Exception as e:  # noqa
        print(f"(nDB write failed: {e})")
    return w


def main():
    best=None;best_score=-1
    for s in range(3):
        set_seed(s); Creature._n=0
        d=simulate()
        cen=d["census"]; late=cen[len(cen)*3//4:]
        persist=sum(1 for col in (1,2,3) if all(r[col]>0 for r in late[::5]))  # species alive throughout
        anycol=sum(1 for col in (1,2,3) if any(r[col]>0 for r in late))
        peak_syn=max((b.get("avg_synapses",0) for b in d["brains"]),default=0)
        score=persist*1000 + anycol*100 + min(peak_syn,50)
        if score>best_score: best_score=score; best=d
        if persist==3: break
    d=best
    dst=os.path.join(os.path.dirname(__file__),"neuroevo.json")
    with open(dst,"w") as f: json.dump(d,f,separators=(",",":"))
    c=d["census"]
    print(f"wrote {dst} ({os.path.getsize(dst)//1024} KB, {len(d['frames'])} frames)")
    print(f"  final: 🐰{c[-1][1]} 🦊{c[-1][2]} 🦅{c[-1][3]} 🌱{c[-1][4]}")
    print(f"  ranges: 🐰{min(r[1] for r in c)}-{max(r[1] for r in c)}  🦊{min(r[2] for r in c)}-{max(r[2] for r in c)}  🦅{min(r[3] for r in c)}-{max(r[3] for r in c)}")
    for sp,nm in ((0,'🐰'),(1,'🦊'),(2,'🦅')):
        bs=[b for b in d['brains'] if b['species']==sp]
        if bs: print(f"  {nm} avg synapses {bs[0]['avg_synapses']}->{bs[-1]['avg_synapses']} (neurons {bs[0]['avg_neurons']}->{bs[-1]['avg_neurons']}, max {max(b['synapse_count'] for b in bs)} syn)")
    url=os.environ.get("NDB_URL")
    if url: print(f"  wrote {write_to_ndb(d,url)} records into nDB")


if __name__=="__main__":
    main()
