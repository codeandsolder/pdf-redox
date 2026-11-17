# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Benchmark OCR variants against a small independently screen-transcribed set.

Selected page snippets are manually checked against rendered PDF pages and are
not a substitute for representative corpus CER. Locations match by PDF-space
centroid rather than list index, avoiding false edits from extra detections.
"""

import argparse
import json
import math
from pathlib import Path
import re
import unicodedata


def normalize(text, strip_spaces=False):
    value = unicodedata.normalize("NFKC", text)
    value = re.sub(r"\s+", " ", value).strip()
    return "".join(value.split()) if strip_spaces else value


def edit_distance(a, b):
    """Exact Levenshtein edit distance using O(min(n,m)) memory."""
    if len(a) > len(b):
        a, b = b, a
    previous = list(range(len(a) + 1))
    for row, char in enumerate(b, 1):
        current = [row]
        for col, letter in enumerate(a, 1):
            current.append(
                min(
                    current[-1] + 1,
                    previous[col] + 1,
                    previous[col - 1] + (letter != char),
                )
            )
        previous = current
    return previous[-1]


def center(line):
    quad = line["quad"]
    if len(quad) != 4 or any(
        len(x) != 2 or any(not math.isfinite(v) for v in x) for x in quad
    ):
        raise ValueError("invalid OCR quad")
    return [sum(v[0] for v in quad) / 4, sum(v[1] for v in quad) / 4]


def score(ground_truth, ocr_document, max_distance=3.0):
    if (
        ground_truth["schema_version"] != 1
        or ocr_document["schema_version"] != 3
        or ground_truth["source_pdf_sha256"] != ocr_document["pdf_sha256"]
    ):
        raise ValueError("OCR output is not tied to reference PDF bytes")
    pages = {}
    for page in ocr_document["pages"]:
        if page["page"] in pages:
            raise ValueError("duplicate model page")
        pages[page["page"]] = page["lines"]
    already = set()
    samples = []
    for idx, ref in enumerate(ground_truth["samples"]):
        page = ref["page"]
        lines = pages.get(page, [])
        anchor = ref["center"]
        if len(anchor) != 2 or not all(math.isfinite(x) for x in anchor):
            raise ValueError("invalid reference anchor")
        nearest = []
        for i, line in enumerate(lines):
            x, y = center(line)
            distance = math.hypot(x - anchor[0], y - anchor[1])
            if distance <= max_distance:
                nearest.append((distance, i, line))
        nearest.sort(key=lambda x: (x[0], x[1]))
        if len(nearest) > 1 and nearest[1][0] - nearest[0][0] < 0.05:
            raise ValueError("ambiguous reference-to-OCR matching")
        selected = nearest[0] if nearest else None
        if selected and (page, selected[1]) in already:
            raise ValueError("two reference labels map to one OCR line")
        if selected:
            already.add((page, selected[1]))
        predicted = selected[2]["text"] if selected else ""
        original = ref["text"]
        normal_source = normalize(original)
        normal_predict = normalize(predicted)
        compact_source = normalize(original, True)
        compact_predict = normalize(predicted, True)
        samples.append(
            {
                "reference_index": idx,
                "page": page,
                "kind": ref["kind"],
                "reference": original,
                "predicted": predicted,
                "confidence": selected[2]["confidence"] if selected else None,
                "reference_chars": len(normal_source),
                "edit_distance": edit_distance(normal_source, normal_predict),
                "edit_no_spaces": edit_distance(compact_source, compact_predict),
                "ref_chars_no_spaces": len(compact_source),
                "exact": normal_source == normal_predict,
                "exact_no_spaces": compact_source == compact_predict,
            }
        )
    summary = {}

    def aggregate(rows):
        total = sum(row["reference_chars"] for row in rows)
        total_c = sum(row["ref_chars_no_spaces"] for row in rows)
        return {
            "samples": len(rows),
            "exact": sum(x["exact"] for x in rows),
            "exact_no_spaces": sum(x["exact_no_spaces"] for x in rows),
            "missing": sum(x["confidence"] is None for x in rows),
            "cer": sum(x["edit_distance"] for x in rows) / max(total, 1),
            "cer_no_spaces": sum(x["edit_no_spaces"] for x in rows) / max(total_c, 1),
            "edits": sum(x["edit_distance"] for x in rows),
            "reference_characters": total,
        }

    summary["all"] = aggregate(samples)
    for kind in sorted({x["kind"] for x in samples}):
        summary[kind] = aggregate([x for x in samples if x["kind"] == kind])
    return {"model": ocr_document["model"], "summary": summary, "samples": samples}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--reference", type=Path, required=True)
    p.add_argument("--model", type=Path, action="append", required=True)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    gt = json.loads(args.reference.read_text())
    results = {}
    for path in args.model:
        if path.stem in results:
            p.error("duplicate model filename")
        results[path.stem] = score(gt, json.loads(path.read_text()))
    report = {
        "reference_sha256": gt["source_pdf_sha256"],
        "reference_provenance": gt["provenance"],
        "results": results,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    for name, result in results.items():
        print(name, json.dumps(result["summary"]["all"], sort_keys=True))
        for kind, row in result["summary"].items():
            if kind != "all":
                print(
                    "  ",
                    kind,
                    row["samples"],
                    "exact",
                    row["exact"],
                    "CER",
                    round(row["cer"], 4),
                )
    if len(results) == 2:
        (a, first), (b, second) = list(results.items())
        print("MODEL_DIFFERENCES", a, "vs", b)
        for x, y in zip(first["samples"], second["samples"], strict=True):
            if x["edit_distance"] != y["edit_distance"]:
                winner = a if x["edit_distance"] < y["edit_distance"] else b
                print(
                    "PAGE",
                    x["page"],
                    "KIND",
                    x["kind"],
                    "WINNER",
                    winner,
                    "EDITS",
                    x["edit_distance"],
                    y["edit_distance"],
                    "TRUTH",
                    repr(x["reference"]),
                    "A",
                    repr(x["predicted"]),
                    "B",
                    repr(y["predicted"]),
                )


if __name__ == "__main__":
    main()
