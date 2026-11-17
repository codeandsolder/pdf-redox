# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""PDF-bound *per-appearance* Unicode votes from two independent recognizers.

Same-vector-shape glyphs are not necessarily semantically identical (l/I/i,
0/O, component collisions). This opt-in experimental adapter only proposes a
per-occurrence override when RapidOCR and contextual Tesseract recognize the
exact *same* source PDF glyph placement as the same Unicode character.
Uncertain, conflicting, duplicated or unmatched geometry is not promoted.
"""

import argparse
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path

import pikepdf
from glyph_vote_evidence import index_catalog, match_char
from pdf_geometry import display_mapper
from compare_tesseract_page_boxes import pdf_quad


def observations(catalog, rapid_files, tess, pdf_path, min_confidence=0.97):
    if catalog["schema_version"] != 2 or tess["schema_version"] != 1:
        raise ValueError("unsupported catalog or Tesseract schema")
    if not 0 <= min_confidence <= 1:
        raise ValueError("invalid confidence threshold")
    digest = catalog["optimized_pdf_sha256"]
    if digest != tess["pdf_sha256"] or not tess["engine"].startswith(
        "Tesseract LSTM eng"
    ):
        raise ValueError("independent evidence source or PDF digest mismatch")
    with pdf_path.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != digest:
            raise ValueError("actual PDF bytes mismatch")
    bins, _ = index_catalog(catalog)
    sources = {}
    for evidence in rapid_files:
        if evidence["schema_version"] != 1 or evidence["pdf_sha256"] != digest:
            raise ValueError("RapidOCR evidence is not bound to this PDF")
        if not evidence["model"].startswith("RapidOCR"):
            raise ValueError("unexpected RapidOCR source")
        for page in evidence["pages"]:
            n = page["page"]
            if n in sources or n <= 0:
                raise ValueError("duplicate or invalid RapidOCR page")
            sources[n] = page["characters"]
    rapid = defaultdict(set)
    tess_votes = defaultdict(set)
    summary = Counter()
    for number, characters in sources.items():
        for char in characters:
            summary["rapid_boxes_seen"] += 1
            word = char["text"]
            conf = char["confidence"]
            if (
                len(word) != 1
                or not word.isprintable()
                or not math.isfinite(conf)
                or conf < min_confidence
            ):
                summary["rapid_rejected"] += 1
                continue
            match, why = match_char(char["quad"], bins.get(number, {}))
            if why is not None:
                summary[f"rapid_{why}"] += 1
                continue
            gid, ordinal = match
            rapid[(gid, number, ordinal)].add(word)
    with pikepdf.Pdf.open(pdf_path) as pdf:
        seen_pages = set()
        for page in tess["pages"]:
            n = page["page"]
            if n not in sources:
                continue
            if n in seen_pages or not 1 <= n <= len(pdf.pages):
                raise ValueError("duplicate or invalid Tesseract page")
            seen_pages.add(n)
            transform, _ = display_mapper(pdf.pages[n - 1])
            for char in page["characters"]:
                summary["tess_boxes_seen"] += 1
                word = char["text"]
                if len(word) != 1 or not word.isprintable():
                    summary["tess_rejected"] += 1
                    continue
                quad = pdf_quad(char["box"], page["width"], page["height"], transform)
                match, why = match_char(quad, bins.get(n, {}))
                if why is not None:
                    summary[f"tess_{why}"] += 1
                    continue
                gid, ordinal = match
                tess_votes[(gid, n, ordinal)].add(word)
    summary["rapid_matched_instances"] = len(rapid)
    summary["tess_matched_instances"] = len(tess_votes)
    agreed = []
    conflicts = []
    for key in sorted(rapid.keys() & tess_votes.keys()):
        r, t = rapid[key], tess_votes[key]
        summary["paired_glyph_instances"] += 1
        if len(r) != 1 or len(t) != 1:
            summary["ambiguous_duplicate_boxes"] += 1
            conflicts.append(
                {
                    "glyph_id": key[0],
                    "page": key[1],
                    "index_on_page": key[2],
                    "rapid": sorted(r),
                    "tesseract": sorted(t),
                    "reason": "ambiguous_duplicate",
                }
            )
            continue
        a, b = next(iter(r)), next(iter(t))
        if a != b:
            summary["model_disagreements"] += 1
            conflicts.append(
                {
                    "glyph_id": key[0],
                    "page": key[1],
                    "index_on_page": key[2],
                    "rapid": [a],
                    "tesseract": [b],
                    "reason": "different_unicode",
                }
            )
            continue
        agreed.append(
            {"glyph_id": key[0], "page": key[1], "index_on_page": key[2], "text": a}
        )
    summary["model_agreements"] = len(agreed)
    return agreed, conflicts, dict(summary)


def propose(
    catalog, labels, rapid_files, tess, pdf, min_confidence=0.97, override_global=False
):
    if labels["schema_version"] != 1:
        raise ValueError("unexpected GlyphLabels schema")
    known_ids = {template["id"] for template in catalog["glyphs"]}
    if set(labels["labels"]) - known_ids:
        raise ValueError("global label refers to an unknown template")
    allowed = {
        (t["id"], o["page"], o["index_on_page"])
        for t in catalog["glyphs"]
        for o in t["occurrences"]
    }
    existing = {}
    for item in labels.get("overrides", []):
        key = (item["glyph_id"], item["page"], item["index_on_page"])
        if key not in allowed or key in existing:
            raise ValueError("duplicate/invalid existing Unicode override")
        existing[key] = item["text"]
    agreed, conflicts, summary = observations(
        catalog, rapid_files, tess, pdf, min_confidence
    )
    additions = []
    skipped = Counter()
    for item in agreed:
        key = (item["glyph_id"], item["page"], item["index_on_page"])
        if key not in allowed:
            raise ValueError("matched source occurrence not in catalog")
        if key in existing:
            if existing[key] != item["text"]:
                skipped["existing_override_disagreement"] += 1
            else:
                skipped["already_overridden"] += 1
            continue
        global_text = labels["labels"].get(item["glyph_id"])
        if global_text == item["text"]:
            skipped["already_global"] += 1
            continue
        if global_text is not None and not override_global:
            skipped["different_from_global_review"] += 1
            continue
        additions.append(item)
    output = {
        "schema_version": 1,
        "labels": dict(sorted(labels["labels"].items())),
        "overrides": [*labels.get("overrides", []), *additions],
    }
    summary.update({f"skipped_{k}": v for k, v in skipped.items()})
    summary["new_occurrence_overrides"] = len(additions)
    summary["new_override_characters"] = sum(len(item["text"]) for item in additions)
    summary["source_pages"] = len({p["page"] for e in rapid_files for p in e["pages"]})
    report = {
        "schema_version": 1,
        "pdf_sha256": catalog["optimized_pdf_sha256"],
        "status": "research_only_cross_model_per_occurrence",
        "summary": summary,
        "added": additions,
        "conflicts": conflicts,
    }
    return output, report


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=Path, required=True)
    p.add_argument("--glyph-catalog", type=Path, required=True)
    p.add_argument("--labels", type=Path, required=True)
    p.add_argument("--rapid-chars", type=Path, nargs="+", required=True)
    p.add_argument("--tesseract-pages", type=Path, required=True)
    p.add_argument("--output-labels", type=Path, required=True)
    p.add_argument("--report", type=Path, required=True)
    p.add_argument("--min-rapid-confidence", type=float, default=0.97)
    p.add_argument(
        "--override-global",
        action="store_true",
        help="research-only: permit competing per-occ labels to override a global label",
    )
    args = p.parse_args()
    catalog = json.loads(args.glyph_catalog.read_text())
    labels = json.loads(args.labels.read_text())
    rapid = [json.loads(path.read_text()) for path in args.rapid_chars]
    tesseract = json.loads(args.tesseract_pages.read_text())
    proposed, report = propose(
        catalog,
        labels,
        rapid,
        tesseract,
        args.pdf,
        args.min_rapid_confidence,
        args.override_global,
    )
    for path, obj in ((args.output_labels, proposed), (args.report, report)):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(obj, ensure_ascii=False, separators=(",", ":")) + "\n"
        )
    print(json.dumps(report["summary"], sort_keys=True))


if __name__ == "__main__":
    main()
