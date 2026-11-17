# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""Reconstruct text runs from *source-order* reusable PDF glyph placements.

This is a structure-first, research-only semantic draft. Unlike raster OCR
detections, every character comes from a PDF content-stream Type3 show, an
exact catalog glyph occurrence, and a separately corroborated Unicode label.
Unlabeled components and source native-text shows are always run barriers.

Use --draft to emit a schema-3 OcrDocument for the existing Rust PDF overlay
writer, which remains review-first and requires --apply-unreviewed to import.
"""

import argparse
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
import time

import pikepdf
from pdf_geometry import display_mapper
from native_pdf_words import native_words_for_pages


def dot(a, b):
    return a[0] * b[0] + a[1] * b[1]


def sub(a, b):
    return a[0] - b[0], a[1] - b[1]


def norm(a):
    length = math.hypot(*a)
    if length <= 1e-9:
        return None
    return a[0] / length, a[1] / length


def bounds(quad, ex, ey):
    ux = [dot(p, ex) for p in quad]
    vy = [dot(p, ey) for p in quad]
    return min(ux), max(ux), min(vy), max(vy)


def axes(quad):
    return norm(sub(quad[1], quad[0])), norm(sub(quad[3], quad[0]))


def source_order(
    doc_page, page_number, catalog_index, expected_by_page, overrides=None
):
    """Walk real PDF instructions and verify every generated Type3 appearance.

    Source-order is safer than sorting geometric boxes: multicolumn, diagram,
    reordered and overlapping text are common in technical PDFs.
    """
    sequence = []
    seen = Counter()
    marked = []
    for ins in pikepdf.parse_content_stream(doc_page):
        op = str(ins.operator)
        vals = ins.operands
        if op == "BDC":
            generated = (
                len(vals) == 2
                and str(vals[0]) == "/Span"
                and isinstance(vals[1], pikepdf.Dictionary)
                and str(vals[1].get("/ActualText", "INVALID")) == ""
            )
            marked.append(
                {"font": None, "code": None, "count": 0} if generated else None
            )
            continue
        if op == "BMC":
            marked.append(None)
            continue
        if op == "EMC":
            if not marked:
                raise ValueError(f"unbalanced EMC on page {page_number}")
            glyph = marked.pop()
            if glyph is None:
                continue
            if glyph["count"] != 1 or glyph["font"] is None or glyph["code"] is None:
                raise ValueError(
                    f"unexpected generated Type3 span on page {page_number}"
                )
            font, code = glyph["font"], glyph["code"]
            item = catalog_index.get((font, code))
            if item is None:
                raise ValueError(
                    f"generated Type3 resource/code missing in catalog: {font}/{code}"
                )
            order = seen[item["id"]]
            examples = expected_by_page.get((item["id"], page_number), ())
            if order >= len(examples):
                raise ValueError(f"excess Type3 glyph on page {page_number}")
            source = examples[order]
            if source["index_on_page"] != order:
                raise ValueError("source occurrence ordinals are invalid")
            seen[item["id"]] += 1
            label = (overrides or {}).get(
                (item["id"], page_number, order), item["label"]
            )
            sequence.append(
                {
                    "glyph_id": item["id"],
                    "ordinal": order,
                    "label": label,
                    "quad": source["page_quad"],
                    "page": page_number,
                }
            )
            continue
        active = next((x for x in reversed(marked) if x is not None), None)
        if op == "Tf" and active is not None:
            if active["font"] is not None or not vals:
                raise ValueError("ambiguous generated Type3 font state")
            active["font"] = str(vals[0]).removeprefix("/")
        if op == "Tj" and active is not None:
            if not vals or active["code"] is not None:
                raise ValueError("ambiguous generated Type3 show")
            raw = bytes(vals[0])
            if len(raw) != 1:
                raise ValueError("generated Type3 code is not one byte")
            active["code"] = raw[0]
            active["count"] += 1
        elif op in ("Tj", "TJ", "'", '"') and active is None:
            sequence.append({"native_barrier": True})
    if marked:
        raise ValueError("unterminated marked content")
    for (gid, n), occ in expected_by_page.items():
        if n == page_number and seen[gid] != len(occ):
            raise ValueError(
                f"catalog vs PDF appearance mismatch for {gid}: {seen[gid]} vs {len(occ)}"
            )
    return sequence


def joinable(a, b, max_gap=0.20):
    if "quad" not in a or "quad" not in b:
        return False
    ex, ey = axes(a["quad"])
    bx, by = axes(b["quad"])
    if ex is None or ey is None or bx is None or by is None:
        return False
    if dot(ex, bx) < 0.995 or dot(ey, by) < 0.985:
        return False
    ax0, ax1, ay0, ay1 = bounds(a["quad"], ex, ey)
    bx0, bx1, by0, by1 = bounds(b["quad"], ex, ey)
    ah, bh = ay1 - ay0, by1 - by0
    if min(ah, bh) < 0.25 or max(ah, bh) > 2.3 * min(ah, bh):
        return False
    overlap = min(ay1, by1) - max(ay0, by0)
    if overlap < 0.50 * min(ah, bh):
        return False
    # True writing advance must be forward, with small kern overlap allowed.
    gap = bx0 - ax1
    return -0.22 * max(ah, bh) <= gap <= max_gap * max(ah, bh)


def enclosing_quad(glyphs):
    ex, ey = axes(glyphs[0]["quad"])
    if ex is None or ey is None:
        raise ValueError("degenerate glyph")
    # Use an orthogonal frame: each corner is a conservative union of the
    # actual visible Type3 bounds, not a guessed font's metrics.
    ey = (-ex[1], ex[0])
    all_points = [p for glyph in glyphs for p in glyph["quad"]]
    uv = [dot(p, ex) for p in all_points]
    vv = [dot(p, ey) for p in all_points]
    u0, u1 = min(uv), max(uv)
    v0, v1 = min(vv), max(vv)
    return [
        [ex[0] * u0 + ey[0] * v0, ex[1] * u0 + ey[1] * v0],
        [ex[0] * u1 + ey[0] * v0, ex[1] * u1 + ey[1] * v0],
        [ex[0] * u1 + ey[0] * v1, ex[1] * u1 + ey[1] * v1],
        [ex[0] * u0 + ey[0] * v1, ex[1] * u0 + ey[1] * v1],
    ]


def assemble(sequence, min_chars=2, max_gap=0.20):
    if min_chars < 1 or not 0 < max_gap < 2:
        raise ValueError("invalid run thresholds")
    runs = []
    active = []
    stats = Counter()

    def finish():
        nonlocal active
        text = "".join(item["label"] for item in active)
        if len(text) >= min_chars:
            runs.append(
                {
                    "text": text,
                    "quad": enclosing_quad(active),
                    "glyphs": [
                        {"glyph_id": g["glyph_id"], "index_on_page": g["ordinal"]}
                        for g in active
                    ],
                    "confidence": 0.0,
                    "reviewed": False,
                }
            )
            stats["runs"] += 1
            stats["run_characters"] += len(text)
            stats["run_components"] += len(active)
        elif active:
            stats["unassembled_labeled_characters"] += len(text)
        active = []

    for item in sequence:
        if item.get("native_barrier"):
            stats["native_barriers"] += 1
            finish()
            continue
        stats["catalog_glyphs"] += 1
        if not item.get("label"):
            stats["unlabeled_components"] += 1
            finish()
            continue
        stats["labeled_glyphs"] += 1
        if active and not joinable(active[-1], item, max_gap):
            stats["geometry_breaks"] += 1
            finish()
        active.append(item)
    finish()
    return runs, stats


def build(pdf_path, catalog, labels, selected_pages, min_chars, max_gap):
    if catalog["schema_version"] != 2 or labels["schema_version"] != 1:
        raise ValueError("unknown glyph catalog or labels schema")
    with pdf_path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    if catalog["optimized_pdf_sha256"] != digest:
        raise ValueError("glyph catalog belongs to a different PDF")
    catalog_index = {}
    by_page = defaultdict(list)
    allowed = set()
    for template in catalog["glyphs"]:
        resource = (template["font_resource"], template["code"])
        if resource in catalog_index:
            raise ValueError("ambiguous font/code mapping")
        catalog_index[resource] = {
            "id": template["id"],
            "label": labels["labels"].get(template["id"]),
        }
        for occ in template["occurrences"]:
            by_page[(template["id"], occ["page"])].append(occ)
            allowed.add((template["id"], occ["page"], occ["index_on_page"]))
    if any(
        len(items) != len(set(x["index_on_page"] for x in items))
        or [x["index_on_page"] for x in items] != list(range(len(items)))
        for items in by_page.values()
    ):
        raise ValueError("bad catalog per-template occurrence ordering")
    if set(labels["labels"]) - {x["id"] for x in catalog["glyphs"]}:
        raise ValueError("unknown Unicode label template")
    overrides = {}
    for item in labels.get("overrides", []):
        key = (item["glyph_id"], item["page"], item["index_on_page"])
        if key not in allowed or key in overrides:
            raise ValueError("unknown or duplicate per-occurrence Unicode override")
        overrides[key] = item["text"]
    if any(
        not isinstance(text, str)
        or any(ord(c) < 32 or 127 <= ord(c) < 160 for c in text)
        for text in list(labels["labels"].values()) + list(overrides.values())
    ):
        raise ValueError("Unicode labels may not contain control characters")
    record = {
        "schema_version": 1,
        "pdf_sha256": digest,
        "method": "verified_content_stream_source_order_plus_vector_geometry",
        "pages": [],
    }
    draft = {
        "schema_version": 3,
        "pdf_sha256": digest,
        "model": "pdf-redox structure-first corroborated Type3 glyph run hypotheses (unreviewed)",
        "pages": [],
    }
    overall = Counter()
    with pikepdf.Pdf.open(pdf_path) as doc:
        pages = selected_pages or list(range(1, len(doc.pages) + 1))
        if len(pages) != len(set(pages)) or any(
            not 1 <= n <= len(doc.pages) for n in pages
        ):
            raise ValueError("invalid or duplicate PDF page number")
        transforms = {n: display_mapper(doc.pages[n - 1])[0] for n in pages}
        native = native_words_for_pages(pdf_path, pages, transforms)
        for n in pages:
            ordered = source_order(
                doc.pages[n - 1], n, catalog_index, by_page, overrides
            )
            runs, stats = assemble(ordered, min_chars, max_gap)
            overall.update(stats)
            record["pages"].append({"page": n, "stats": dict(stats), "runs": runs})
            draft["pages"].append(
                {
                    "page": n,
                    "native_text_regions": native[n],
                    "lines": [
                        {k: run[k] for k in ("text", "quad", "confidence", "reviewed")}
                        for run in runs
                    ],
                }
            )
    record["summary"] = dict(overall)
    record["summary"]["pages"] = len(pages)
    record["summary"]["native_words"] = sum(
        len(x["native_text_regions"]) for x in draft["pages"]
    )
    return record, draft


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=Path, required=True)
    p.add_argument("--glyph-catalog", type=Path, required=True)
    p.add_argument("--labels", type=Path, required=True)
    p.add_argument("--report", type=Path, required=True)
    p.add_argument("--draft", type=Path, required=True)
    p.add_argument(
        "--pages", help="comma-separated selected PDF page numbers, default all"
    )
    p.add_argument("--min-chars", type=int, default=2)
    p.add_argument("--max-gap-height", type=float, default=0.20)
    args = p.parse_args()
    catalog = json.loads(args.glyph_catalog.read_text())
    labels = json.loads(args.labels.read_text())
    pages = [int(x) for x in args.pages.split(",")] if args.pages else None
    start = time.monotonic()
    record, draft = build(
        args.pdf, catalog, labels, pages, args.min_chars, args.max_gap_height
    )
    for path, obj in ((args.report, record), (args.draft, draft)):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(obj, ensure_ascii=False, separators=(",", ":")) + "\n"
        )
    print(
        json.dumps(
            {
                "summary": record["summary"],
                "wall_seconds": round(time.monotonic() - start, 2),
            },
            sort_keys=True,
        )
    )
    for page in record["pages"]:
        if page["runs"]:
            print(
                "PAGE",
                page["page"],
                "runs",
                len(page["runs"]),
                "longest",
                sorted((x["text"] for x in page["runs"]), key=len, reverse=True)[:8],
            )


if __name__ == "__main__":
    main()
