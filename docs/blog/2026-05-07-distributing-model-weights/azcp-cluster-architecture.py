#!/usr/bin/env python3
"""Generate the publication-quality azcp-cluster architecture diagram.

Renders both SVG (vector, preferred for publication) and PNG (high-DPI
raster, for places that don't accept SVG).

Usage:
    python3 docs/blog/2026-05-07-distributing-model-weights/azcp-cluster-architecture.py
"""
from __future__ import annotations

import matplotlib.pyplot as plt
from matplotlib.patches import FancyArrowPatch, FancyBboxPatch, Rectangle
from matplotlib.patheffects import withStroke
import os

# ---------------------------------------------------------------------------
# Style
# ---------------------------------------------------------------------------
plt.rcParams.update({
    "font.family": "DejaVu Sans",
    "font.size": 10,
    "svg.fonttype": "none",   # keep text as text in the SVG
    "pdf.fonttype": 42,
})

# Palette
AZURE_BLUE   = "#0078D4"   # Microsoft Azure brand blue
AZURE_LIGHT  = "#DEECF9"
RANK_FILL    = "#F5F5F5"
RANK_EDGE    = "#333333"
BCAST_FILL   = "#107C10"   # Microsoft green
BCAST_LIGHT  = "#E1F1E1"
NVME_FILL    = "#FFF4CE"
NVME_EDGE    = "#8A6D00"
ARROW_COLOR  = "#555555"
TEXT_DARK    = "#1A1A1A"
TEXT_MUTED   = "#666666"

# Canvas
FIG_W, FIG_H = 11.5, 6.6
N_RANKS = 4

# ---------------------------------------------------------------------------
# Geometry
# ---------------------------------------------------------------------------
fig, ax = plt.subplots(figsize=(FIG_W, FIG_H))
ax.set_xlim(0, 100)
ax.set_ylim(0, 104)
ax.axis("off")

# Y-bands (top to bottom)
Y_AZURE_TOP    = 96
Y_AZURE_BOT    = 82
Y_RANK_TOP     = 70
Y_RANK_BOT     = 54
Y_BCAST_TOP    = 42
Y_BCAST_BOT    = 30
Y_NVME_TOP     = 18
Y_NVME_BOT     = 4

# Box padding used by rounded_box() — needed so arrows land on the visible
# edge of each box rather than the (smaller) nominal rectangle.
BOX_PAD = 0.6

# Horizontal layout: four rank columns on the left, a fixed label column
# on the right for the "stage 1 / stage 2" annotations.
LEFT_PAD     = 5
INNER_RIGHT  = 76    # right edge of the content area (rank columns end here)
LABEL_X      = 78    # stage labels start here, ha="left"
COL_GAP      = 1.6
COL_W        = (INNER_RIGHT - LEFT_PAD) / N_RANKS
BOX_W        = COL_W - COL_GAP
COL_X        = [LEFT_PAD + i * COL_W + COL_GAP / 2 for i in range(N_RANKS)]
COL_CENTER   = [x + BOX_W / 2 for x in COL_X]

# Wide-row geometry: Azure box (row 1) and bcast bar (row 3) span exactly
# the outer edges of the rank/NVMe columns so all four rows line up.
WIDE_X = COL_X[0]
WIDE_W = COL_X[N_RANKS - 1] + BOX_W - COL_X[0]


def rounded_box(x, y, w, h, fc, ec, lw=1.2, label=None,
                label_color=TEXT_DARK, label_size=10, label_weight="normal",
                sublabel=None, sublabel_color=TEXT_MUTED, sublabel_size=8.5,
                pad=0.6):
    """Draw a rounded rectangle with optional 1-2 line label."""
    box = FancyBboxPatch(
        (x, y), w, h,
        boxstyle=f"round,pad={pad},rounding_size=1.2",
        linewidth=lw, edgecolor=ec, facecolor=fc,
    )
    ax.add_patch(box)
    if label:
        if sublabel:
            ax.text(x + w / 2, y + h * 0.62, label,
                    ha="center", va="center",
                    fontsize=label_size, color=label_color,
                    fontweight=label_weight)
            ax.text(x + w / 2, y + h * 0.30, sublabel,
                    ha="center", va="center",
                    fontsize=sublabel_size, color=sublabel_color)
        else:
            ax.text(x + w / 2, y + h / 2, label,
                    ha="center", va="center",
                    fontsize=label_size, color=label_color,
                    fontweight=label_weight)
    return box


def arrow(x1, y1, x2, y2, color=ARROW_COLOR, lw=1.4, mut=12, style="-|>"):
    a = FancyArrowPatch(
        (x1, y1), (x2, y2),
        arrowstyle=style, color=color, lw=lw,
        mutation_scale=mut, shrinkA=2, shrinkB=2,
    )
    ax.add_patch(a)
    return a


# ---------------------------------------------------------------------------
# Row 1: Azure source (same width as the rank/NVMe row outer edges)
# ---------------------------------------------------------------------------
azure_y = Y_AZURE_BOT
azure_h = Y_AZURE_TOP - Y_AZURE_BOT

rounded_box(
    WIDE_X, azure_y, WIDE_W, azure_h,
    fc=RANK_FILL, ec=RANK_EDGE, lw=1.6,
    label="Azure Blob Storage  ·  source account",
    label_size=12, label_weight="bold", label_color=TEXT_DARK,
    sublabel="(model checkpoint, 524 files / 413 GiB)",
    sublabel_color=TEXT_MUTED, sublabel_size=9,
)

# Stage 1 label — to the right of the rank row (this is the row that's
# actually doing the sharded download).
ax.text(
    LABEL_X, (Y_RANK_BOT + Y_RANK_TOP) / 2,
    "stage 1\nsharded download\nfrom Azure",
    ha="left", va="center",
    fontsize=10, color=AZURE_BLUE, fontweight="bold",
    linespacing=1.25,
)

# ---------------------------------------------------------------------------
# Row 2: per-rank sharded download (now in Azure-blue)
# ---------------------------------------------------------------------------
for i in range(N_RANKS):
    rounded_box(
        COL_X[i], Y_RANK_BOT, BOX_W, Y_RANK_TOP - Y_RANK_BOT,
        fc=AZURE_LIGHT, ec=AZURE_BLUE, lw=1.4,
        label=f"rank {i}  ·  azcp",
        label_weight="bold", label_color=AZURE_BLUE,
        sublabel=f"1/{N_RANKS} of files\n(LPT bin-pack)",
        sublabel_color=AZURE_BLUE,
    )
    # Arrow from Azure box (visible bottom) to this rank (visible top)
    arrow(
        COL_CENTER[i], Y_AZURE_BOT - BOX_PAD,
        COL_CENTER[i], Y_RANK_TOP + BOX_PAD,
        color=AZURE_BLUE, lw=1.6,
    )

# ---------------------------------------------------------------------------
# Row 3: MPI_Ibcast bar (same width as rows 2 & 4)
# ---------------------------------------------------------------------------
bcast_h = Y_BCAST_TOP - Y_BCAST_BOT

rounded_box(
    WIDE_X, Y_BCAST_BOT, WIDE_W, bcast_h,
    fc=BCAST_LIGHT, ec=BCAST_FILL, lw=1.8,
    label="MPI_Ibcast over RDMA / UCX",
    label_size=12, label_weight="bold", label_color=BCAST_FILL,
    sublabel="each owner broadcasts the files it holds; every other rank receives what it doesn't own",
    sublabel_color=BCAST_FILL, sublabel_size=9,
)

# Stage 2 label — to the right of the bcast box, on the same Y-line.
ax.text(
    LABEL_X, (Y_BCAST_BOT + Y_BCAST_TOP) / 2,
    "stage 2\nbroadcast over\nthe fabric",
    ha="left", va="center",
    fontsize=10, color=BCAST_FILL, fontweight="bold",
    linespacing=1.25,
)

# Arrows from rank → bcast bar (visible edges)
for i in range(N_RANKS):
    arrow(
        COL_CENTER[i], Y_RANK_BOT - BOX_PAD,
        COL_CENTER[i], Y_BCAST_TOP + BOX_PAD,
        color=BCAST_FILL, lw=1.4,
    )

# ---------------------------------------------------------------------------
# Row 4: per-node NVMe destinations
# ---------------------------------------------------------------------------
for i in range(N_RANKS):
    rounded_box(
        COL_X[i], Y_NVME_BOT, BOX_W, Y_NVME_TOP - Y_NVME_BOT,
        fc=NVME_FILL, ec=NVME_EDGE, lw=1.3,
        label="/mnt/nvme",
        label_weight="bold",
        sublabel="full dataset",
    )
    # Arrow bcast → NVMe (visible edges)
    arrow(
        COL_CENTER[i], Y_BCAST_BOT - BOX_PAD,
        COL_CENTER[i], Y_NVME_TOP + BOX_PAD,
        color=ARROW_COLOR, lw=1.4,
    )

# ---------------------------------------------------------------------------
# Title (optional — comment out if you'd rather have a clean figure for an
# external caption)
# ---------------------------------------------------------------------------
ax.text(
    (WIDE_X + WIDE_X + WIDE_W) / 2, 102,
    "azcp-cluster: shard once, broadcast everywhere",
    ha="center", va="center",
    fontsize=13, fontweight="bold", color=TEXT_DARK,
)

# ---------------------------------------------------------------------------
# Save
# ---------------------------------------------------------------------------
out_dir = os.path.dirname(os.path.abspath(__file__))
svg_path = os.path.join(out_dir, "azcp-cluster-architecture.svg")
png_path = os.path.join(out_dir, "azcp-cluster-architecture.png")

plt.tight_layout(pad=0.2)
fig.savefig(svg_path, format="svg", bbox_inches="tight", facecolor="white")
fig.savefig(png_path, format="png", dpi=300, bbox_inches="tight", facecolor="white")
print(f"Wrote {svg_path}")
print(f"Wrote {png_path}")
