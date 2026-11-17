# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""Recover text-line OCR regions from PDF Type3 geometry, without a detector.

Uses every vector appearance, not only glyphs with recognized Unicode.
Regions are PDF-space and preserve writing direction; always validate source
PDF SHA256 and catalog occurrence indices. This is a research-stage region
proposal generator, not an independently trustworthy OCR replacement.
"""

import argparse
from collections import defaultdict, Counter
import hashlib
import json
import math
from pathlib import Path
import statistics

import pikepdf


def dot(a, b):
    return a[0] * b[0] + a[1] * b[1]


def frame(quad):
    ex = [quad[1][i] - quad[0][i] for i in range(2)]
    heightvec = [quad[3][i] - quad[0][i] for i in range(2)]
    width = math.hypot(*ex)
    height = math.hypot(*heightvec)
    if width < 1e-6 or height < 1e-6:
        return None
    ex = [x / width for x in ex]
    ey = [-ex[1], ex[0]]
    if dot(heightvec, ey) < 0:
        ey = [-x for x in ey]
    return ex, ey, width, height


def glyphs_by_page(catalog, selected, labels=None):
    output = defaultdict(list)
    valid = set()
    known = labels.get("labels", {}) if labels else {}
    overrides = {}
    if labels:
        for x in labels.get("overrides", []):
            key = (x["glyph_id"], x["page"], x["index_on_page"])
            if key in overrides:
                raise ValueError("duplicate glyph Unicode override")
            overrides[key] = x["text"]
    for template in catalog["glyphs"]:
        gid = template["id"]
        for occurrence in template["occurrences"]:
            page = occurrence["page"]
            key = (gid, page, occurrence["index_on_page"])
            if key in valid:
                raise ValueError("duplicate vector occurrence")
            valid.add(key)
            if page not in selected:
                continue
            quad = occurrence["page_quad"]
            if len(quad) != 4 or any(
                len(p) != 2 or not all(math.isfinite(v) for v in p) for p in quad
            ):
                raise ValueError("invalid source glyph quad")
            axes = frame(quad)
            if not axes:
                continue
            ex, ey, width, height = axes
            # Ink glyph bounds, not line-level contours. Discard enormous
            # decorative vector paths and degenerate subpixel separators.
            if not (0.4 <= height <= 26 and 0.3 <= width <= 32):
                continue
            if width > 3.4 * height and width > 8:
                continue
            angle = math.atan2(ex[1], ex[0])
            orientation = round(angle / (math.pi / 36))  # nearest 5 degrees
            theta = orientation * math.pi / 36
            vx = (math.cos(theta), math.sin(theta))
            vy = (-vx[1], vx[0])
            if dot(ey, vy) < 0:
                vy = (-vy[0], -vy[1])
            u = [dot(p, vx) for p in quad]
            v = [dot(p, vy) for p in quad]
            item = {
                "id": gid,
                "index_on_page": occurrence["index_on_page"],
                "u0": min(u),
                "u1": max(u),
                "v0": min(v),
                "v1": max(v),
                "uc": (min(u) + max(u)) / 2,
                "vc": (min(v) + max(v)) / 2,
                "width": width,
                "height": height,
                "orientation": orientation,
                "ex": vx,
                "ey": vy,
                "label": overrides.get(key, known.get(gid)),
            }
            output[page].append(item)
    if set(overrides) - valid:
        raise ValueError("Unicode override absent from catalog")
    return output


def lines_from_glyphs(
    items,
    min_components=3,
    gap_height=1.1,
    cap_gap=10.0,
    y_tolerance=0.38,
    min_width=10.0,
):
    if min_components < 1 or gap_height <= 0 or cap_gap <= 0 or not 0 < y_tolerance < 1:
        raise ValueError("invalid vector-line parameters")
    orientations = defaultdict(list)
    for item in items:
        if item["height"] >= 1.5:
            orientations[item["orientation"]].append(item)
    regions = []
    for angle, glyphs in orientations.items():
        # Seed rows with full-height letters, not tiny punctuation/strokes.
        # A small glyph as the fixed anchor can split the same printed line
        # into several overlapping rows with contradictory OCR crops.
        glyphs.sort(key=lambda x: (-x["height"], -x["vc"], x["uc"]))
        rows = []
        # Use a fixed row anchor to avoid transitive y-drift joining two lines.
        for item in glyphs:
            possible = []
            for row in rows:
                ref = row["anchor"]
                if abs(item["vc"] - ref["vc"]) > y_tolerance * max(
                    item["height"], ref["height"]
                ):
                    continue
                if max(item["height"], ref["height"]) > 2.5 * min(
                    item["height"], ref["height"]
                ):
                    continue
                vertical = min(item["v1"], ref["v1"]) - max(item["v0"], ref["v0"])
                if vertical < 0.30 * min(item["height"], ref["height"]):
                    continue
                possible.append((abs(item["vc"] - ref["vc"]), row))
            if possible:
                row = min(possible, key=lambda x: x[0])[1]
                row["items"].append(item)
            else:
                rows.append({"anchor": item, "items": [item]})
        # Punctuation and thin strokes are likely part of an existing line,
        # but can never create new lines by themselves.
        tiny = [i for i in items if i["orientation"] == angle and i["height"] < 1.5]
        for item in tiny:
            possible = []
            for row in rows:
                ref = row["anchor"]
                if (
                    not ref["v0"] - 0.4 * ref["height"]
                    <= item["vc"]
                    <= ref["v1"] + 0.4 * ref["height"]
                ):
                    continue
                ext = min(abs(item["uc"] - x["uc"]) for x in row["items"])
                if ext > 3 * ref["height"]:
                    continue
                possible.append((ext, row))
            if possible:
                min(possible, key=lambda x: x[0])[1]["items"].append(item)
        for row in rows:
            line = sorted(row["items"], key=lambda x: x["uc"])
            if not line:
                continue
            typical = statistics.median(x["height"] for x in line if x["height"] >= 1.5)
            max_gap = min(cap_gap, gap_height * typical)
            group = []
            end = -math.inf

            def flush():
                if len(group) < min_components:
                    return
                u0 = min(x["u0"] for x in group)
                u1 = max(x["u1"] for x in group)
                v0 = min(x["v0"] for x in group)
                v1 = max(x["v1"] for x in group)
                if u1 - u0 < min_width:
                    return
                ex, ey = group[0]["ex"], group[0]["ey"]

                # Reconstruct BL/BR/TR/TL PDF-space region.
                def point(u, v):
                    return [ex[0] * u + ey[0] * v, ex[1] * u + ey[1] * v]

                quad = [point(u0, v0), point(u1, v0), point(u1, v1), point(u0, v1)]
                regions.append(
                    {
                        "quad": quad,
                        "components": len(group),
                        "known_components": sum(bool(x["label"]) for x in group),
                        "text_if_fully_known": (
                            "".join(x["label"] for x in group)
                            if all(x["label"] for x in group)
                            else None
                        ),
                        "orientation_degrees": angle * 5,
                    }
                )

            for item in line:
                if group and item["u0"] - end > max_gap:
                    flush()
                    group = []
                group.append(item)
                end = max(end, item["u1"])
            flush()
    return regions


def build(
    pdf,
    catalog,
    labels,
    pages,
    min_components,
    gap_height,
    cap_gap,
    y_tolerance,
    min_width,
):
    with pdf.open("rb") as fp:
        digest = hashlib.file_digest(fp, "sha256").hexdigest()
    with pikepdf.Pdf.open(pdf) as document:
        if (
            not pages
            or len(pages) != len(set(pages))
            or any(n < 1 or n > len(document.pages) for n in pages)
        ):
            raise ValueError("invalid page selection for source PDF")
    if catalog["schema_version"] != 2 or catalog["optimized_pdf_sha256"] != digest:
        raise ValueError("Type3 catalog source PDF mismatch")
    if labels is not None and labels["schema_version"] != 1:
        raise ValueError("unknown glyph label schema")
    glyphs = glyphs_by_page(catalog, set(pages), labels)
    output = {
        "schema_version": 1,
        "pdf_sha256": digest,
        "method": "detector_free_type3_glyph_geometry",
        "parameters": {
            "min_components": min_components,
            "gap_height": gap_height,
            "cap_gap": cap_gap,
            "y_tolerance": y_tolerance,
            "min_width": min_width,
        },
        "pages": [],
    }
    total = Counter()
    for page in pages:
        regions = lines_from_glyphs(
            glyphs[page], min_components, gap_height, cap_gap, y_tolerance, min_width
        )
        stats = {
            "input_vector_appearance_candidates": len(glyphs[page]),
            "regions": len(regions),
            "components_in_regions": sum(x["components"] for x in regions),
            "fully_labeled_regions": sum(
                x["text_if_fully_known"] is not None for x in regions
            ),
        }
        total.update(stats)
        output["pages"].append({"page": page, "stats": stats, "regions": regions})
    output["summary"] = dict(total)
    return output


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=Path, required=True)
    p.add_argument("--catalog", type=Path, required=True)
    p.add_argument("--labels", type=Path)
    p.add_argument("--pages", required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--min-components", type=int, default=3)
    p.add_argument("--gap-height", type=float, default=1.1)
    p.add_argument("--cap-gap", type=float, default=10.0)
    p.add_argument("--y-tolerance", type=float, default=0.38)
    p.add_argument("--min-width", type=float, default=10.0)
    a = p.parse_args()
    selected = [int(x) for x in a.pages.split(",")]
    if len(selected) != len(set(selected)) or any(x <= 0 for x in selected):
        p.error("invalid selected pages")
    catalog = json.loads(a.catalog.read_text())
    labels = json.loads(a.labels.read_text()) if a.labels else None
    result = build(
        a.pdf,
        catalog,
        labels,
        selected,
        a.min_components,
        a.gap_height,
        a.cap_gap,
        a.y_tolerance,
        a.min_width,
    )
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(
        json.dumps(result, ensure_ascii=False, separators=(",", ":")) + "\n"
    )
    print(json.dumps(result["summary"], sort_keys=True))
    for page in result["pages"]:
        print("PAGE", page["page"], json.dumps(page["stats"], sort_keys=True))


if __name__ == "__main__":
    main()
