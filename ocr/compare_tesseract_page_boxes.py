# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""Map Tesseract real-page character boxes to canonical PDF vector glyph IDs.

Tesseract 5 makebox has no calibrated confidence, so this tool only produces
independent agreement statistics; it never directly writes PDF Unicode.
"""

import argparse
from collections import Counter, defaultdict
import hashlib
import json
from pathlib import Path

import pikepdf
from glyph_vote_evidence import index_catalog, match_char
from pdf_geometry import display_mapper


def pdf_quad(box, width, height, transform):
    if len(box) != 4 or not 1 <= width <= 100000 or not 1 <= height <= 100000:
        raise ValueError("invalid raster bbox/dimensions")
    x0, y0, x1, y1 = box
    if not all(isinstance(x, int) for x in box) or not (
        0 <= x0 < x1 <= width and 0 <= y0 < y1 <= height
    ):
        raise ValueError("Tesseract bbox outside raster")
    return [
        transform(x0 / width, 1 - y0 / height),
        transform(x1 / width, 1 - y0 / height),
        transform(x1 / width, 1 - y1 / height),
        transform(x0 / width, 1 - y1 / height),
    ]


def compare(catalog, votes, evidence, pdf):
    if catalog["schema_version"] != 2 or evidence["schema_version"] != 1:
        raise ValueError("unsupported source schema")
    digest = catalog["optimized_pdf_sha256"]
    if digest != votes["pdf_sha256"] or digest != evidence["pdf_sha256"]:
        raise ValueError("PDF, catalog and recognizers are bound to different content")
    if not evidence["engine"].startswith("Tesseract LSTM eng"):
        raise ValueError("unexpected independent recognizer")
    initial = {t["glyph_id"]: t for t in votes["templates"]}
    tested = {
        t["glyph_id"]: t
        for t in votes["holdout_validation"]["assessments"]
        if t["status"] == "consistent"
    }
    catalog_index, counts = index_catalog(catalog)
    with pdf.open("rb") as src:
        if hashlib.file_digest(src, "sha256").hexdigest() != digest:
            raise ValueError("actual PDF bytes do not match evidence digest")
    observations = defaultdict(lambda: defaultdict(list))
    summary = Counter()
    per_page = {}
    with pikepdf.Pdf.open(pdf) as doc:
        selected = set()
        for page in evidence["pages"]:
            n = page["page"]
            if not 1 <= n <= len(doc.pages) or n in selected:
                raise ValueError("invalid or duplicate PDF page")
            selected.add(n)
            transform, _ = display_mapper(doc.pages[n - 1])
            page_stats = Counter()
            for char in page["characters"]:
                summary["character_boxes_seen"] += 1
                page_stats["character_boxes_seen"] += 1
                label = char["text"]
                if len(label) != 1 or label.isspace():
                    summary["invalid_unicode"] += 1
                    continue
                quad = pdf_quad(char["box"], page["width"], page["height"], transform)
                match, reason = match_char(quad, catalog_index.get(n, {}))
                if reason:
                    summary[reason] += 1
                    continue
                gid, ordinal = match
                observations[gid][(n, ordinal)].append(label)
                summary["associated_boxes"] += 1
                page_stats["associated_boxes"] += 1
            per_page[str(n)] = dict(page_stats)
    details = []
    for gid, instances in observations.items():
        expected = initial.get(gid, {}).get("suggested_unicode")
        if not expected:
            continue
        labels = Counter()
        uncertain = 0
        for raw in instances.values():
            local = set(raw)
            if len(local) > 1:
                uncertain += 1
                continue
            labels[raw[0]] += 1
        agrees = labels[expected]
        total = sum(labels.values())
        eligible = gid in tested
        details.append(
            {
                "glyph_id": gid,
                "rapidocr_unicode": expected,
                "full_document_occurrences": counts[gid],
                "cross_page_consistent": eligible,
                "tesseract_observations": total,
                "agreeing": agrees,
                "disagreeing": total - agrees,
                "duplicate_box_conflicts": uncertain,
                "votes": [
                    {"text": label, "count": n} for label, n in labels.most_common()
                ],
                "pages_seen": len({page for page, _ in instances}),
            }
        )
    details.sort(key=lambda x: (-x["tesseract_observations"], x["glyph_id"]))
    relevant = [row for row in details if row["cross_page_consistent"]]
    summary["observed_templates"] = len(details)
    summary["holdout_consistent_templates_observed"] = len(relevant)
    summary["holdout_consistent_observations"] = sum(
        x["tesseract_observations"] for x in relevant
    )
    summary["holdout_consistent_agreements"] = sum(x["agreeing"] for x in relevant)
    summary["holdout_consistent_conflicts"] = sum(x["disagreeing"] for x in relevant)
    summary["holdout_consistent_unanimous_templates"] = sum(
        x["tesseract_observations"] >= 3 and x["disagreeing"] == 0 for x in relevant
    )
    return {
        "schema_version": 1,
        "pdf_sha256": digest,
        "model": evidence["engine"],
        "summary": dict(summary),
        "pages": per_page,
        "glyphs": details,
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=Path, required=True)
    p.add_argument("--glyph-catalog", type=Path, required=True)
    p.add_argument("--votes", type=Path, required=True)
    p.add_argument("--tesseract-pages", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    catalog = json.loads(args.glyph_catalog.read_text())
    votes = json.loads(args.votes.read_text())
    evidence = json.loads(args.tesseract_pages.read_text())
    result = compare(catalog, votes, evidence, args.pdf)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(result["summary"], sort_keys=True))
    for t in result["glyphs"]:
        if t["cross_page_consistent"] and t["tesseract_observations"] >= 3:
            print(
                t["rapidocr_unicode"],
                "tess",
                t["votes"],
                "across",
                t["pages_seen"],
                "pages",
            )


if __name__ == "__main__":
    main()
