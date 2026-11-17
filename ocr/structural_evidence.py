#!/usr/bin/env python3
"""Audit repeated PDF vector outlines against OCR line predictions.

This is an *evidence* tool, not a Unicode label generator.  A Type3 component
may be half a Chinese character, a dot, or part of a diagram, and OCR line
boxes can contain unindexed native/raster text.  Do not promote matches to
semantic PDF text without a coverage proof and independent recognition.
"""

import argparse
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
import statistics


def local_coordinates(point, quad):
    """Project a point onto the OCR quadrilateral's affine baseline frame.

    This works with PDF /Rotate, non-uniform CTMs and non-axis-aligned text.
    """
    origin, right, _, upper = quad
    ax, ay = right[0] - origin[0], right[1] - origin[1]
    bx, by = upper[0] - origin[0], upper[1] - origin[1]
    determinant = ax * by - ay * bx
    if not math.isfinite(determinant) or abs(determinant) < 1e-12:
        return None
    dx, dy = point[0] - origin[0], point[1] - origin[1]
    return ((dx * by - dy * bx) / determinant, (dy * ax - dx * ay) / determinant)


def intersection(a, b):
    """Convex-quadrilateral overlap via SAT; touching is not overlapping."""
    for poly in (a, b):
        for i in range(4):
            p, q = poly[i], poly[(i + 1) % 4]
            nx, ny = -(q[1] - p[1]), q[0] - p[0]
            if nx == 0 and ny == 0:
                return False
            aa = [nx * x + ny * y for x, y in a]
            bb = [nx * x + ny * y for x, y in b]
            if min(max(aa), max(bb)) <= max(min(aa), min(bb)):
                return False
    return True


def document_index(catalog):
    pages = defaultdict(list)
    for template in catalog["glyphs"]:
        for occurrence in template["occurrences"]:
            pages[occurrence["page"]].append((template["id"], occurrence["page_quad"]))
    return pages


def component_signature(line_quad, occurrences, quantization=0.1):
    """Relative geometry in glyph-height units, independent of page rotation.

    Reusing a signature is only a *candidate* for same-text recognition.
    It does not prove that OCR's full line ink was explained by the catalog.
    """
    selected = []
    for gid, quad in occurrences:
        corners = [local_coordinates(point, line_quad) for point in quad]
        if any(point is None for point in corners):
            continue
        xs, ys = [point[0] for point in corners], [point[1] for point in corners]
        center_x, center_y = sum(xs) / 4, sum(ys) / 4
        # Allow loose PDF control-point bounds and loose OCR detector boxes.
        if not (-0.04 <= center_x <= 1.04 and -0.12 <= center_y <= 1.12):
            continue
        if min(xs) < -0.12 or max(xs) > 1.12 or min(ys) < -0.3 or max(ys) > 1.3:
            continue
        selected.append((gid, center_x, center_y, max(xs) - min(xs), max(ys) - min(ys)))
    if len(selected) < 3:
        return None, len(selected)
    # Sorting in OCR reading direction also makes the result deterministic.
    selected.sort(key=lambda item: (item[1], -item[2], item[0]))
    scale = statistics.median(s[4] for s in selected if s[4] > 0)
    if not math.isfinite(scale) or scale <= 1e-6:
        return None, len(selected)
    x0 = selected[0][1]
    y0 = statistics.median(s[2] for s in selected)

    def q(value):
        return round(value / (scale * quantization))

    signature = tuple(
        (gid, q(x - x0), q(y - y0), q(w), q(h)) for gid, x, y, w, h in selected
    )
    return signature, len(selected)


def audit(catalog, ocr, min_components=3, min_confidence=0.9):
    if catalog["optimized_pdf_sha256"] != ocr["pdf_sha256"]:
        raise ValueError("glyph catalog and OCR draft reference different PDF bytes")
    if catalog["schema_version"] != 2 or ocr["schema_version"] != 3:
        raise ValueError("unexpected catalog or OCR schema")
    index = document_index(catalog)
    groups = defaultdict(list)
    stats = Counter()
    per_page = {}
    for page in ocr["pages"]:
        number = page["page"]
        occurrences = index.get(number, [])
        page_stats = Counter(
            ocr_lines=len(page["lines"]), glyph_appearances=len(occurrences)
        )
        for line in page["lines"]:
            stats["ocr_lines"] += 1
            if line["confidence"] < min_confidence or not line["text"].strip():
                stats["low_confidence"] += 1
                continue
            if any(
                intersection(line["quad"], native)
                for native in page["native_text_regions"]
            ):
                stats["native_intersections"] += 1
                continue
            signature, n = component_signature(line["quad"], occurrences)
            if signature is None or n < min_components:
                stats["insufficient_components"] += 1
                continue
            page_stats["structural_lines"] += 1
            stats["structural_lines"] += 1
            groups[signature].append(
                {
                    "page": number,
                    "text": line["text"],
                    "confidence": line["confidence"],
                    "component_count": n,
                    "quad": line["quad"],
                }
            )
        per_page[str(number)] = dict(page_stats)
    repeats = []
    for signature, rows in groups.items():
        if len(rows) <= 1:
            continue
        texts = Counter(row["text"].strip() for row in rows)
        ident = hashlib.sha256(repr(signature).encode()).hexdigest()[:20]
        repeats.append(
            {
                "shape_signature": ident,
                "components": len(signature),
                "observations": len(rows),
                "ocr_agrees": len(texts) == 1,
                "texts": [{"text": t, "count": c} for t, c in texts.most_common()],
                "examples": rows[:8],
            }
        )
    repeats.sort(key=lambda row: (-row["observations"], row["shape_signature"]))
    stats["repeated_groups"] = len(repeats)
    stats["repeated_observations"] = sum(row["observations"] for row in repeats)
    stats["ocr_conflicting_groups"] = sum(not row["ocr_agrees"] for row in repeats)
    stats["potential_recognitions_reusable"] = sum(
        row["observations"] - 1 for row in repeats
    )
    return {
        "schema_version": 1,
        "pdf_sha256": ocr["pdf_sha256"],
        "summary": dict(stats),
        "pages": per_page,
        "repeated_groups": repeats,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--glyph-catalog", type=Path, required=True)
    parser.add_argument("--ocr", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--min-components", type=int, default=3)
    args = parser.parse_args()
    if args.min_components < 3:
        parser.error("min-components must be >= 3")
    catalog = json.loads(args.glyph_catalog.read_text())
    ocr = json.loads(args.ocr.read_text())
    report = audit(catalog, ocr, min_components=args.min_components)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report["summary"], sort_keys=True))


if __name__ == "__main__":
    main()
