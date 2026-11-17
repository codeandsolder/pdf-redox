# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Geometry-only detector-replacement feasibility against held-out OCR boxes.

Baseline model is only an *offline evaluation oracle*. The region generator
never reads the baseline and never depends on it at inference time.
"""

import argparse
import json


def bbox(quad):
    return (
        min(p[0] for p in quad),
        max(p[0] for p in quad),
        min(p[1] for p in quad),
        max(p[1] for p in quad),
    )


def y_aligned(a, b):
    x0, x1, y0, y1 = a
    u0, u1, v0, v1 = b
    h = y1 - y0
    g = v1 - v0
    if h <= 0 or g <= 0:
        return False
    y_overlap = max(0, min(y1, v1) - max(y0, v0))
    return y_overlap >= 0.45 * min(h, g) and max(h, g) < 3 * min(h, g)


def covered_width(line, regions):
    x0, x1, y0, y1 = line
    if x1 - x0 <= 0:
        return 0, 0
    intervals = []
    for region in regions:
        if not y_aligned(line, region):
            continue
        u0, u1, _, _ = region
        lower = max(x0, u0)
        upper = min(x1, u1)
        if upper > lower:
            intervals.append((lower, upper))
    if not intervals:
        return 0, 0
    intervals.sort()
    merged = []
    for low, high in intervals:
        if merged and low <= merged[-1][1]:
            merged[-1] = (merged[-1][0], max(high, merged[-1][1]))
        else:
            merged.append((low, high))
    return sum(high - low for low, high in merged) / (x1 - x0), len(intervals)


def evaluate(proposals, baseline):
    if proposals["pdf_sha256"] != baseline["pdf_sha256"]:
        raise ValueError("PDF mismatch")
    refs = {x["page"]: x["lines"] for x in baseline["pages"]}
    scores = []
    perpage = []
    for page in proposals["pages"]:
        p = page["page"]
        regions = [bbox(row["quad"]) for row in page["regions"]]
        data = []
        for line in refs.get(p, []):
            ratio, parts = covered_width(bbox(line["quad"]), regions)
            data.append(
                {
                    "text": line["text"],
                    "ratio": round(ratio, 3),
                    "parts": parts,
                    "native_overlap": None,
                }
            )
        good_regions = 0
        for region in regions:
            if any(
                covered_width(region, [bbox(x["quad"])])[0] >= 0.5
                for x in refs.get(p, [])
            ):
                good_regions += 1
        perpage.append(
            {
                "page": p,
                "baseline_lines": len(data),
                "vector_regions": len(regions),
                "vector_regions_overlapping_ocr": good_regions,
                "reference_coverage_50": sum(x["ratio"] >= 0.5 for x in data),
                "reference_coverage_80": sum(x["ratio"] >= 0.8 for x in data),
                "reference_coverage_95": sum(x["ratio"] >= 0.95 for x in data),
                "mean_fraction": round(
                    sum(x["ratio"] for x in data) / max(1, len(data)), 3
                ),
                "reference_lines": [x for x in data if x["ratio"] < 0.8],
            }
        )
        scores.extend(data)
    summary = {
        "reference_lines": len(scores),
        "vector_regions": sum(x["vector_regions"] for x in perpage),
        "coverage_50": sum(x["ratio"] >= 0.5 for x in scores),
        "coverage_80": sum(x["ratio"] >= 0.8 for x in scores),
        "coverage_95": sum(x["ratio"] >= 0.95 for x in scores),
        "mean_fraction": round(
            sum(x["ratio"] for x in scores) / max(len(scores), 1), 3
        ),
    }
    return {"summary": summary, "pages": perpage}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--proposals", required=True)
    p.add_argument("--baseline-ocr", required=True)
    p.add_argument("--report", required=True)
    args = p.parse_args()
    result = evaluate(
        json.load(open(args.proposals)), json.load(open(args.baseline_ocr))
    )
    with open(args.report, "w") as fp:
        json.dump(result, fp, ensure_ascii=False, indent=2)
    print("SUMMARY", result["summary"])
    for item in result["pages"]:
        print(
            "PAGE",
            item["page"],
            "baseline",
            item["baseline_lines"],
            "regions",
            item["vector_regions"],
            "covered80",
            item["reference_coverage_80"],
            "covered95",
            item["reference_coverage_95"],
            "mean",
            item["mean_fraction"],
            "matching_regions",
            item["vector_regions_overlapping_ocr"],
        )
        print(
            "MISSES",
            [(x["text"][:60], x["ratio"]) for x in item["reference_lines"][:13]],
        )


if __name__ == "__main__":
    main()
