# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Check that semantic-only pdf-redox annotation preserves actual page pixels."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time


def pages_in(pdf):
    out = subprocess.check_output(["pdfinfo", str(pdf)], text=True)
    match = re.search(r"^Pages:\s*(\d+)\s*$", out, re.MULTILINE)
    if not match:
        raise ValueError("pdfinfo did not return a valid page count")
    return int(match.group(1))


def render(pdf, page, dpi):
    # Using deterministic Poppler PPM output avoids PNG compression heuristics.
    return subprocess.check_output(
        [
            "pdftoppm",
            "-f",
            str(page),
            "-l",
            str(page),
            "-r",
            str(dpi),
            "-singlefile",
            str(pdf),
        ],
        stderr=subprocess.DEVNULL,
    )


def extract(pdf, page):
    return subprocess.check_output(
        ["pdftotext", "-f", str(page), "-l", str(page), "-layout", str(pdf), "-"],
        stderr=subprocess.DEVNULL,
    )


def verify(source, result, pages, dpis):
    start = time.monotonic()
    n0, n1 = pages_in(source), pages_in(result)
    if n0 != n1:
        raise ValueError("semantic insertion changed PDF page count")
    if any(page < 1 or page > n0 for page in pages):
        raise ValueError("page outside PDF range")
    records = []
    for page in pages:
        original = extract(source, page)
        annotated = extract(result, page)
        formats = []
        for dpi in dpis:
            a, b = render(source, page, dpi), render(result, page, dpi)
            formats.append(
                {
                    "dpi": dpi,
                    "byte_identical_ppm": a == b,
                    "original_sha256": hashlib.sha256(a).hexdigest(),
                    "annotated_sha256": hashlib.sha256(b).hexdigest(),
                    "render_bytes": len(a),
                }
            )
        records.append(
            {
                "page": page,
                "before_extracted_bytes": len(original),
                "after_extracted_bytes": len(annotated),
                "rendering": formats,
            }
        )
    equal = all(
        item["byte_identical_ppm"] for rec in records for item in rec["rendering"]
    )
    return {
        "original_bytes": source.stat().st_size,
        "annotated_bytes": result.stat().st_size,
        "pdf_pages": n0,
        "pages": records,
        "visual_pixel_identity": equal,
        "elapsed_seconds": round(time.monotonic() - start, 2),
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--before", type=Path, required=True)
    p.add_argument("--after", type=Path, required=True)
    p.add_argument("--pages", required=True)
    p.add_argument("--dpi", default="150,300")
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    selected = [int(x) for x in args.pages.split(",")]
    dpis = [int(x) for x in args.dpi.split(",")]
    if (
        not selected
        or len(selected) != len(set(selected))
        or not dpis
        or not all(24 <= x <= 600 for x in dpis)
    ):
        p.error("unique pages and DPI values in 24-600 required")
    report = verify(args.before, args.after, selected, dpis)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "pages"}, sort_keys=True))
    for row in report["pages"]:
        print(
            "PAGE",
            row["page"],
            "PDF_TEXT_BYTES",
            row["before_extracted_bytes"],
            "=>",
            row["after_extracted_bytes"],
            "PIXELS_IDENTICAL",
            all(d["byte_identical_ppm"] for d in row["rendering"]),
        )
    if not report["visual_pixel_identity"]:
        raise SystemExit("semantic annotation unexpectedly altered painting")


if __name__ == "__main__":
    main()
