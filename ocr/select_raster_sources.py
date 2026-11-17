# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10"]
# ///
"""Select embedded raster sources whose native pixels contain lost detail.

This is a *sampling* decision, not a guessed image-content classifier.
When a source Image XObject is downsampled by the page's planned OCR
resolution, page OCR has thrown away some of the original source detail.
Try native-image OCR only when the recoverable resolution gain justifies
another inference pass. Shared source images are considered just once.
"""

from collections import defaultdict
import math


def choose_mixed_raster_sources(
    placements,
    pages,
    max_side,
    max_inference_pixels=8_000_000,
    max_sources=12,
    minimum_source_pixels=20_000,
    min_gain=2.25,
):
    if (
        max_side < 100
        or max_inference_pixels < 1
        or max_sources < 1
        or minimum_source_pixels < 1
        or min_gain <= 1
    ):
        raise ValueError("invalid source-native OCR selection policy")
    page_scale = {}
    for page_number, page in pages.items():
        # Match pdftoppm -scale-to (which uses MediaBox by default).
        # CropBox can be much smaller and falsely suppress native OCR.
        x0, y0, x1, y1 = map(float, page.mediabox)
        if x1 <= x0 or y1 <= y0:
            raise ValueError("invalid source PDF MediaBox")
        page_scale[page_number] = max_side / max(x1 - x0, y1 - y0)
    sources = defaultdict(list)
    for placement in placements:
        if placement["unsafe_reasons"]:
            continue
        width = int(placement["width"])
        height = int(placement["height"])
        source_pixels = width * height
        if source_pixels < minimum_source_pixels or min(width, height) < 24:
            continue
        a, b, c, d, *_ = placement["ctm"]
        rendered_area = abs(a * d - b * c) * page_scale[placement["page"]] ** 2
        if not math.isfinite(rendered_area) or rendered_area <= 0:
            continue
        gain = source_pixels / rendered_area
        if gain < min_gain:
            continue
        sources[placement["source_id"]].append(
            {
                "page": placement["page"],
                "source_pixels": source_pixels,
                "rendered_pixels": rendered_area,
                "gain": gain,
            }
        )
    ranked = []
    for source_id, appears in sources.items():
        # Lower per-placement improvement also matters: no need to mask
        # the same XObject in a high-resolution placement where page OCR
        # already has all its pixels. Score by recoverable native pixels.
        best = max(appears, key=lambda p: p["gain"])
        inferred_cost = best["source_pixels"]
        saved = sum(max(0, p["source_pixels"] - p["rendered_pixels"]) for p in appears)
        ranked.append(
            (
                saved / inferred_cost,
                source_id,
                inferred_cost,
                best["gain"],
                len(appears),
            )
        )
    ranked.sort(key=lambda a: (-a[0], -a[3], -a[4], a[1]))
    selected = set()
    budget = 0
    diagnostics = []
    for score, source_id, cost, gain, uses in ranked:
        eligible = len(selected) < max_sources and budget + cost <= max_inference_pixels
        diagnostics.append(
            {
                "source_id": source_id,
                "native_source_pixels": cost,
                "native_to_page_render_area_gain": round(gain, 3),
                "eligible_placements": uses,
                "score": round(score, 3),
                "selected": eligible,
            }
        )
        if eligible:
            budget += cost
            selected.add(source_id)
    return selected, {
        "native_resolution_candidates": len(ranked),
        "selected_sources": len(selected),
        "pixel_budget_used": budget,
        "sources": diagnostics,
    }
