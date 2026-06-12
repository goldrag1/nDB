#!/usr/bin/env python3
"""Generate illustration charts for the nDB vs MariaDB storage comparison post.

Numbers come straight from the byte-level analysis in
docs/ndb-vs-mariadb-storage.md (nDB on-disk format v3 + InnoDB DYNAMIC rows).
Outputs PNGs into docs/assets/.
"""
import os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

OUT = os.path.dirname(os.path.abspath(__file__))

# Brand-ish palette
NDB = "#6C4DF6"      # purple
MARIA = "#F0883E"    # orange
GRID = "#E3E3E8"
INK = "#1A1A22"
plt.rcParams.update({
    "font.size": 12,
    "axes.edgecolor": "#C9C9D2",
    "axes.titleweight": "bold",
    "axes.titlesize": 15,
    "figure.facecolor": "white",
    "axes.facecolor": "white",
})


def style(ax):
    ax.grid(axis="y", color=GRID, linewidth=1, zorder=0)
    for s in ("top", "right"):
        ax.spines[s].set_visible(False)
    ax.tick_params(length=0)


# ---------------------------------------------------------------------------
# Chart 1 — fixed overhead drowns in payload (overhead % vs payload size)
# ---------------------------------------------------------------------------
fig, ax = plt.subplots(figsize=(9, 5.2))
payload = np.linspace(1, 8000, 1000)
overhead_pct = 48.0 / (48.0 + payload) * 100.0
ax.plot(payload, overhead_pct, color=NDB, linewidth=3, zorder=3)
ax.fill_between(payload, overhead_pct, color=NDB, alpha=0.08, zorder=2)

pts = [
    (33, "Tiny entity\n(3 scalars)\n59%", 9),
    (2048, "Text 2 KB\n2.3%", -18),
    (3072, "768-d vector\n1.5%", 18),
    (6144, "1536-d vector\n0.77%", 12),
]
for px, label, dx in pts:
    py = 48.0 / (48.0 + px) * 100.0
    ax.scatter([px], [py], color=INK, zorder=5, s=42)
    ax.annotate(label, (px, py), textcoords="offset points",
                xytext=(dx, 22), ha="center", fontsize=10.5,
                color=INK, fontweight="bold")

ax.set_title("nDB's 48-byte fixed overhead drowns in payload", color=INK)
ax.set_xlabel("Payload size per record (bytes)")
ax.set_ylabel("Fixed overhead as % of record")
ax.set_xlim(0, 8000)
ax.set_ylim(0, 65)
ax.yaxis.set_major_formatter(lambda v, _: f"{v:.0f}%")
style(ax)
fig.text(0.5, -0.01,
         "overhead  =  48 / (48 + payload)   →   approaches 0 as payload grows",
         ha="center", fontsize=10.5, color="#666", style="italic")
fig.tight_layout()
fig.savefig(os.path.join(OUT, "chart-overhead-curve.png"), dpi=150,
            bbox_inches="tight")
plt.close(fig)


# ---------------------------------------------------------------------------
# Chart 2 — small record vs large record (who is leaner)
# ---------------------------------------------------------------------------
fig, axes = plt.subplots(1, 2, figsize=(11, 4.8))

# small record
ax = axes[0]
labels = ["MariaDB", "nDB"]
vals = [34, 81]
bars = ax.bar(labels, vals, color=[MARIA, NDB], width=0.55, zorder=3)
ax.set_title("Small row  {name, age, active}", color=INK, fontsize=13)
ax.set_ylabel("bytes per record")
for b, v in zip(bars, vals):
    ax.text(b.get_x() + b.get_width() / 2, v + 1.5, f"{v} B",
            ha="center", fontweight="bold", color=INK)
ax.text(0.5, 0.92, "MariaDB wins ~2.4x", transform=ax.transAxes,
        ha="center", color=MARIA, fontweight="bold", fontsize=11)
ax.set_ylim(0, 95)
style(ax)

# large record (768-d embedding)
ax = axes[1]
vals = [3098, 3129]
bars = ax.bar(labels, vals, color=[MARIA, NDB], width=0.55, zorder=3)
ax.set_title("768-d embedding record", color=INK, fontsize=13)
ax.set_ylabel("bytes per record")
for b, v in zip(bars, vals):
    ax.text(b.get_x() + b.get_width() / 2, v + 25, f"{v:,} B",
            ha="center", fontweight="bold", color=INK)
ax.text(0.5, 0.92, "≈ tie (within ~1%)", transform=ax.transAxes,
        ha="center", color="#444", fontweight="bold", fontsize=11)
ax.set_ylim(0, 3700)
style(ax)

fig.suptitle("Per-record storage: it flips with payload size",
             fontweight="bold", fontsize=15, color=INK, y=1.02)
fig.tight_layout()
fig.savefig(os.path.join(OUT, "chart-record-size.png"), dpi=150,
            bbox_inches="tight")
plt.close(fig)


# ---------------------------------------------------------------------------
# Chart 3 — cost of an n-ary relationship vs arity
# ---------------------------------------------------------------------------
fig, ax = plt.subplots(figsize=(9, 5.2))
k = np.arange(2, 33)
ndb_cost = 56 + 20 * k                 # one hyperedge record
maria_cost = 92 * k                    # membership rows + reverse index (UUID keys)
ax.plot(k, ndb_cost, color=NDB, linewidth=3, marker="o", markersize=4,
        label="nDB — one hyperedge record (56 + 20k)", zorder=4)
ax.plot(k, maria_cost, color=MARIA, linewidth=3, marker="s", markersize=4,
        label="MariaDB — membership table + reverse index (~92k)", zorder=3)
ax.set_title("Storing one n-ary relationship of arity k", color=INK)
ax.set_xlabel("Arity k  (number of participants in the relationship)")
ax.set_ylabel("bytes")
ax.legend(frameon=False, fontsize=10.5, loc="upper left")
ax.set_xlim(2, 32)
ax.set_ylim(0, 3050)
style(ax)
# annotate the multiplier at k=6
ax.annotate("at k=6:\nnDB 176 B vs MariaDB ~552 B\n(~3.1x)",
            (6, 92 * 6), textcoords="offset points", xytext=(35, -8),
            fontsize=10, color=INK,
            arrowprops=dict(arrowstyle="->", color="#888"))
fig.tight_layout()
fig.savefig(os.path.join(OUT, "chart-relationship-arity.png"), dpi=150,
            bbox_inches="tight")
plt.close(fig)


# ---------------------------------------------------------------------------
# Chart 4 — end-to-end knowledge graph total (stacked)
# ---------------------------------------------------------------------------
fig, ax = plt.subplots(figsize=(8, 5))
labels = ["nDB", "MariaDB"]
entities = [41.8, 41.5]
relations = [0.88, 2.30]
b1 = ax.bar(labels, entities, color=[NDB, MARIA], width=0.5, zorder=3,
            label="Entities / rows (payload-dominated)")
b2 = ax.bar(labels, relations, bottom=entities, color=["#B9A9FB", "#F7C79B"],
            width=0.5, zorder=3, label="Relationships")
for i, (e, r) in enumerate(zip(entities, relations)):
    ax.text(i, e / 2, f"{e:.1f} MB", ha="center", color="white",
            fontweight="bold")
    ax.text(i, e + r + 0.4, f"+{r:.2f} MB rel\n= {e + r:.1f} MB total",
            ha="center", fontweight="bold", color=INK, fontsize=10)
ax.set_title("Chemistry knowledge graph\n10k molecules + 5k reactions (arity ~6)",
             color=INK, fontsize=13)
ax.set_ylabel("total storage (MB)")
ax.set_ylim(0, 47)
ax.legend(frameon=False, fontsize=10, loc="lower center")
style(ax)
fig.tight_layout()
fig.savefig(os.path.join(OUT, "chart-knowledge-graph.png"), dpi=150,
            bbox_inches="tight")
plt.close(fig)

print("wrote 4 charts to", OUT)
