# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10","pillow>=11","rapidocr>=3,<4","onnxruntime>=1.21"]
# ///
"""Hybrid PDF OCR: native image XObjects first; page inference only if needed.

Raster-only pages can skip page render/detection entirely. A mixed raster+
native/vector page receives independent image-source OCR evidence, and normal
page OCR is retained for the remaining content. Source image candidates are
not duplicated when they overlap an existing page OCR line; unexpected
disagreements remain in the audit report. All results are source-PDF-bound.
"""

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import time

import pikepdf
from ocr_embedded_images import recognize as image_recognize
from pdf_image_placements import enumerate_image_paints
from select_raster_sources import choose_mixed_raster_sources
from reconstruct_raster_mosaic import cluster_native_raster_paints
from ocr_fragment_groups import pages_from_fragments
from native_only_pages import page_is_native_only
from native_pdf_words import native_words_for_pages
from pdf_geometry import display_mapper


def _area(polygon):
    return abs(
        sum(
            polygon[i][0] * polygon[(i + 1) % len(polygon)][1]
            - polygon[(i + 1) % len(polygon)][0] * polygon[i][1]
            for i in range(len(polygon))
        )
        / 2
    )


def _clip(subject, clip):
    """Sutherland–Hodgman intersection of arbitrary convex PDF quads."""
    sign = (
        1
        if sum(
            clip[i][0] * clip[(i + 1) % len(clip)][1]
            - clip[(i + 1) % len(clip)][0] * clip[i][1]
            for i in range(len(clip))
        )
        >= 0
        else -1
    )
    poly = [tuple(p) for p in subject]
    for i in range(len(clip)):
        a = clip[i]
        b = clip[(i + 1) % len(clip)]

        def cross(p):
            return sign * (
                (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
            )

        def crossing(p, q):
            x, y = cross(p), cross(q)
            d = x - y
            if abs(d) < 1e-12:
                return q
            t = x / d
            return (p[0] + (q[0] - p[0]) * t, p[1] + (q[1] - p[1]) * t)

        if not poly:
            break
        next_poly = []
        previous = poly[-1]
        for current in poly:
            p_in = cross(previous) >= -1e-7
            c_in = cross(current) >= -1e-7
            if c_in != p_in:
                next_poly.append(crossing(previous, current))
            if c_in:
                next_poly.append(current)
            previous = current
        poly = next_poly
    return poly


def overlap_fraction(a, b):
    aa = _area(a)
    bb = _area(b)
    if min(aa, bb) <= 1e-9:
        return 0.0
    return _area(_clip(a, b)) / min(aa, bb)


def clean_image_only_pages(image_report, image_draft):
    """Prove there is no nonimage paint or unsupported/overlapping image."""
    page_report = {p["page"]: p for p in image_report["pages"]}
    placements = image_report["placements"]
    bypage = {p["page"]: p for p in image_draft["pages"]}
    objects = {}
    for p in image_report["sources"]:
        objects[p["source_id"]] = p
    approved = set()
    reasons = {}
    for page, details in page_report.items():
        why = []
        if not details["image_only_after_native"]:
            why.append("mixed_pdf_visual_content_or_unsupported_paint")
        these = [item for item in placements if item["page"] == page]
        if not these:
            why.append("no_extracted_image_placements")
        if any(x["status"] != "recognized_at_native_source_resolution" for x in these):
            why.append("unrecognized_or_unsupported_raster_placement")
        if not bypage[page]["lines"]:
            why.append("no_positive_raster_text_evidence")
        # Even two independently OCR'd transparent images can visually
        # overpaint one another. Their isolated recognition is not the
        # appearance seen by the reader; use whole-page fallback instead.
        footprints = [
            item["page_quad"]
            for item in these
            if item["status"] == "recognized_at_native_source_resolution"
        ]
        if any(
            overlap_fraction(a, b) > 0.02
            for i, a in enumerate(footprints)
            for b in footprints[i + 1 :]
        ):
            why.append("overlapping_image_layers_require_page_compositing")
        # Require each raster source to be decoded and recognized once.
        if any(objects[item["source_id"]]["status"] != "recognized" for item in these):
            why.append("raster_decoder_or_model_missing")
        if why:
            reasons[page] = why
        else:
            approved.add(page)
    return approved, reasons


def _verified_raster_key(page, text, quad, source_id=None, paint_index=None):
    return (
        page,
        str(text),
        tuple(round(float(v), 4) for pt in quad for v in pt),
        source_id,
        paint_index,
    )


def combine(
    pdf,
    selected,
    raster_doc,
    raster_report,
    baseline,
    raster_only,
    additional_threshold=0.97,
):
    digest = raster_doc["pdf_sha256"]
    with pdf.open("rb") as fp:
        if hashlib.file_digest(fp, "sha256").hexdigest() != digest:
            raise ValueError("raster OCR source PDF changed")
    byras = {p["page"]: p for p in raster_doc["pages"]}
    bybase = {p["page"]: p for p in baseline["pages"]}
    if set(bybase) != (set(selected) - raster_only):
        raise ValueError("page fallback does not cover the required selected pages")
    if baseline["pdf_sha256"] != digest:
        raise ValueError("normal page OCR was run on a different PDF")
    verified_pages = set(baseline.get("_source_mask_verified_pages", ()))
    verified_source_lines = {
        _verified_raster_key(
            item["page"],
            item["text"],
            item["quad"],
            item.get("source_id"),
            item.get("paint_index"),
        )
        for item in baseline.get("_source_mask_visibility", ())
        if item["visible"] and item["page"] in verified_pages
    }
    pages = []
    stats = Counter()
    proposals = []
    for n in selected:
        raster = byras[n]
        if n in raster_only:
            pages.append(
                {
                    **raster,
                    "lines": [
                        {
                            k: v
                            for k, v in line.items()
                            if k not in ("source_id", "paint_index")
                        }
                        for line in raster["lines"]
                    ],
                }
            )
            stats["image_source_only_pages"] += 1
            stats["image_source_lines"] += len(raster["lines"])
            continue
        page = bybase[n]
        if page["native_text_regions"] != raster["native_text_regions"]:
            raise ValueError("inconsistent source native text coordinates")
        lines = list(page["lines"])
        stats["page_fallback_pages"] += 1
        stats["original_page_lines"] += len(lines)
        for image_line in raster["lines"]:
            overlap = max(
                (
                    overlap_fraction(image_line["quad"], x["quad"])
                    for x in page["lines"]
                ),
                default=0,
            )
            native_overlap = max(
                (
                    overlap_fraction(image_line["quad"], q)
                    for q in page["native_text_regions"]
                ),
                default=0,
            )
            genuinely_visible = (
                _verified_raster_key(
                    n,
                    image_line["text"],
                    image_line["quad"],
                    image_line.get("source_id"),
                    image_line.get("paint_index"),
                )
                in verified_source_lines
            )
            eligible = (
                genuinely_visible
                and overlap < 0.30
                and native_overlap < 0.20
                and image_line["confidence"] >= additional_threshold
                and len(image_line["text"].strip()) >= 2
            )
            proposals.append(
                {
                    "page": n,
                    "raster_text": image_line["text"],
                    "source_id": image_line.get("source_id"),
                    "paint_index": image_line.get("paint_index"),
                    "raster_confidence": image_line["confidence"],
                    "page_overlap": round(overlap, 4),
                    "native_overlap": round(native_overlap, 4),
                    "verified_visible_source": genuinely_visible,
                    "included_as_extra": eligible,
                }
            )
            if eligible:
                lines.append(
                    {
                        k: v
                        for k, v in image_line.items()
                        if k not in ("source_id", "paint_index")
                    }
                )
                stats["nonoverlapping_image_lines_added"] += 1
            else:
                stats["raster_candidates_kept_in_audit_only"] += 1
        pages.append(
            {
                "page": n,
                "lines": lines,
                "native_text_regions": page["native_text_regions"],
            }
        )
    result = {
        "schema_version": 3,
        "pdf_sha256": digest,
        "model": raster_doc["model"] + "; fallback " + baseline["model"],
        "pages": pages,
    }
    return result, dict(stats), proposals


def invoke_page_ocr(
    pdf,
    selected,
    temp,
    max_side,
    ocr_threads,
    keep_native,
    force_page=False,
    raster_source_report=None,
    existing_engine=None,
):
    """Reuse the already-running interpreter and ONNX runtime.

    A separate 'uv run' process was costing nearly a second per mixed PDF,
    in addition to re-importing Pillow, pikepdf and RapidOCR. The page
    adapter exposes an in-process, argument-driven entrypoint, preserving
    exactly the same output schema and native-text masking policy.
    """
    if not selected:
        raise ValueError("page OCR requires at least one selected page")
    from batch_rapidocr import main as primary_ocr_main

    filename = Path(temp) / "page-ocr.json"
    args = [
        "--pdf",
        str(pdf),
        "--output",
        str(filename),
        "--pages",
        ",".join(map(str, selected)),
        "--max-side",
        str(max_side),
        "--ocr-threads",
        str(ocr_threads),
        "--work-dir",
        str(temp),
    ]
    if keep_native:
        args.append("--keep-native-text")
    if raster_source_report is not None:
        source_report_file = Path(temp) / "raster-source-report.json"
        source_report_file.write_text(json.dumps(raster_source_report))
        args.extend(["--source-render-mask-report", str(source_report_file)])
    if force_page:
        args.append("--no-skip-native-only")
    result = primary_ocr_main(args, existing_engine=existing_engine)
    print(
        "PAGE_FALLBACK",
        selected,
        "DETECTIONS",
        sum(len(p["lines"]) for p in result["pages"]),
        flush=True,
    )
    return result


def _run_standard(
    pdf,
    pages,
    work_dir,
    max_side=1600,
    ocr_threads=6,
    min_raster_width=48,
    min_raster_height=18,
    max_ocr_images=1000,
    force_page=False,
    keep_native=False,
    raster_mode="auto",
):
    if not pages or len(set(pages)) != len(pages):
        raise ValueError("invalid page selection")
    start = time.monotonic()
    with pikepdf.Pdf.open(pdf) as source:
        if any(n < 1 or n > len(source.pages) for n in pages):
            raise ValueError("source PDF page selection is invalid")
        proofs = [page_is_native_only(source.pages[n - 1]) for n in pages]
        if not force_page and all(proof.eligible for proof in proofs):
            transforms = {n: display_mapper(source.pages[n - 1])[0] for n in pages}
            native = native_words_for_pages(pdf, pages, transforms)
            with pdf.open("rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            output = {
                "schema_version": 3,
                "pdf_sha256": digest,
                "model": "PDF native-searchable paint proof: no OCR required",
                "pages": [
                    {"page": n, "lines": [], "native_text_regions": native[n]}
                    for n in pages
                ],
            }
            return output, {
                "schema_version": 1,
                "pdf_sha256": digest,
                "method": "all_native_searchable_pdf_pages",
                "summary": {
                    "image_placements": 0,
                    "ocr_inference_calls": 0,
                    "raster_ocr_lines": 0,
                    "pure_native_pages": pages,
                    "pure_raster_pages": [],
                    "page_fallback_pages": [],
                    "wall_seconds": round(time.monotonic() - start, 3),
                },
            }
        paints, _, summaries = enumerate_image_paints(source, pages)
        if raster_mode == "auto":
            select_sources, selection_diagnostics = choose_mixed_raster_sources(
                paints,
                {n: source.pages[n - 1] for n in pages},
                max_side,
            )
        else:
            select_sources = set()
            selection_diagnostics = {
                "selection": "explicit_" + raster_mode,
                "selected_sources": 0,
            }
    eligible_native_rasters = bool(
        raster_mode == "all"
        or bool(select_sources)
        or (
            raster_mode == "auto"
            and any(s["image_only_after_native"] for s in summaries)
        )
    )
    if not paints or not eligible_native_rasters:
        # Do not waste a second native-word extraction, decode or ONNX load
        # on PDF content that doesn't benefit from native image OCR.
        with TemporaryDirectory(prefix="hybrid-page-", dir=work_dir) as temp:
            baseline = invoke_page_ocr(
                pdf, pages, temp, max_side, ocr_threads, keep_native, force_page
            )
        return baseline, {
            "schema_version": 1,
            "pdf_sha256": baseline["pdf_sha256"],
            "method": "pdf_structure_precheck_and_unchanged_page_ocr",
            "summary": {
                "image_placements": len(paints),
                "ocr_inference_calls": 0,
                "raster_ocr_lines": 0,
                "pure_raster_pages": [],
                "page_fallback_pages": pages,
                "wall_seconds": round(time.monotonic() - start, 3),
            },
            "raster_selection": "no relevant standalone source-image pages",
        }
    raster_doc, raster_report, image_engine = image_recognize(
        pdf,
        pages,
        min_source_width=min_raster_width,
        min_source_height=min_raster_height,
        max_ocr_images=max_ocr_images,
        ocr_threads=ocr_threads,
        raster_mode=raster_mode,
        selected_source_ids=select_sources,
    )
    raster_only, reasons = clean_image_only_pages(raster_report, raster_doc)
    if force_page:
        raster_only = set()
    fallback = [n for n in pages if n not in raster_only]
    with TemporaryDirectory(prefix="hybrid-page-", dir=work_dir) as temp:
        if fallback:
            baseline = invoke_page_ocr(
                pdf,
                fallback,
                temp,
                max_side,
                ocr_threads,
                keep_native,
                force_page,
                raster_source_report=(
                    raster_report if raster_mode in ("all", "auto") else None
                ),
                existing_engine=image_engine,
            )
        else:
            baseline = {
                "schema_version": 3,
                "pdf_sha256": raster_doc["pdf_sha256"],
                "model": "page OCR unnecessary (all selected pages scanned-raster-only)",
                "pages": [],
            }
        result, merge_stats, proposals = combine(
            pdf,
            pages,
            raster_doc,
            raster_report,
            baseline,
            raster_only,
        )
    report = {
        "schema_version": 1,
        "pdf_sha256": raster_doc["pdf_sha256"],
        "method": "native_embedded_image_ocr_with_pdf_page_fallback",
        "summary": {
            **raster_report["summary"],
            **merge_stats,
            "pure_raster_pages": sorted(raster_only),
            "page_fallback_pages": fallback,
            "wall_seconds": round(time.monotonic() - start, 3),
        },
        "fallback_reasons": reasons,
        "raster_source_report": raster_report,
        "native_resolution_selection": selection_diagnostics,
        "raster_page_merge_proposals": proposals,
        "visibility_verified": baseline.get("_source_mask_verified", False),
        "verified_source_mask_pages": baseline.get("_source_mask_verified_pages", []),
        "reused_native_image_recognizer_for_page": bool(
            image_engine is not None and fallback
        ),
    }
    return result, report


def run(
    pdf,
    pages,
    work_dir,
    max_side=1600,
    ocr_threads=6,
    min_raster_width=48,
    min_raster_height=18,
    max_ocr_images=1000,
    force_page=False,
    keep_native=False,
    raster_mode="auto",
):
    """Dispatch complete raster fragment groups before the conventional path.

    Fragment recognition is selected from PDF drawing instructions alone;
    the OCR output is never used as a page-region detector. Only complete,
    opaque and geometrically connected image groups with a native-resolution
    pixel saving over full-page rendering can bypass the page detector.
    """
    options = dict(
        max_side=max_side,
        ocr_threads=ocr_threads,
        min_raster_width=min_raster_width,
        min_raster_height=min_raster_height,
        max_ocr_images=max_ocr_images,
        force_page=force_page,
        keep_native=keep_native,
        raster_mode=raster_mode,
    )
    if raster_mode != "auto" or force_page:
        return _run_standard(pdf, pages, work_dir, **options)
    began = time.monotonic()
    selected = []
    with pikepdf.Pdf.open(pdf) as source:
        if (
            not pages
            or len(set(pages)) != len(pages)
            or any(n < 1 or n > len(source.pages) for n in pages)
        ):
            raise ValueError("invalid source PDF page selection")
        placements, _, records = enumerate_image_paints(source, pages)
        for record in records:
            n = record["page"]
            if not record["image_only_after_native"]:
                continue
            appearing = [x for x in placements if x["page"] == n]
            if len(appearing) < 2 or any(x["unsafe_reasons"] for x in appearing):
                continue
            small = sum(
                p["width"] < min_raster_width or p["height"] < min_raster_height
                for p in appearing
            )
            if small == 0:
                continue
            page_rect = tuple(float(x) for x in source.pages[n - 1].cropbox)
            groups = cluster_native_raster_paints(appearing, page_rect)
            covered = sum(len(g["placements"]) for g in groups)
            if covered != len(appearing):
                continue
            page_w = page_rect[2] - page_rect[0]
            page_h = page_rect[3] - page_rect[1]
            if page_w <= 0 or page_h <= 0:
                continue
            page_scale = max_side / max(page_w, page_h)
            page_pixels = page_w * page_h * page_scale**2
            group_pixels = sum(g["estimated_pixels"] for g in groups)
            if group_pixels >= 0.70 * page_pixels:
                continue
            selected.append(n)
    if not selected:
        return _run_standard(pdf, pages, work_dir, **options)
    fragments, frag_report = pages_from_fragments(
        pdf,
        selected,
        ocr_threads=ocr_threads,
        min_score=0.97,
    )
    accepted = set(frag_report["summary"]["eligible_pages"])
    if not accepted:
        return _run_standard(pdf, pages, work_dir, **options)
    unhandled = [n for n in pages if n not in accepted]
    if unhandled:
        baseline, base_report = _run_standard(pdf, unhandled, work_dir, **options)
        if baseline["pdf_sha256"] != fragments["pdf_sha256"]:
            raise ValueError("fragment and page OCR belong to different PDFs")
        normal_pages = {p["page"]: p for p in baseline["pages"]}
    else:
        baseline = None
        base_report = None
        normal_pages = {}
    merged = {p["page"]: p for p in fragments["pages"] if p["page"] in accepted}
    merged.update(normal_pages)
    if set(merged) != set(pages):
        raise ValueError("incomplete fragment/page fallback coverage")
    result = {
        "schema_version": 3,
        "pdf_sha256": fragments["pdf_sha256"],
        "model": "PDF native-fragment-group OCR + PP-OCRv6 regular fallback",
        "pages": [merged[n] for n in pages],
    }
    report = {
        "schema_version": 1,
        "pdf_sha256": fragments["pdf_sha256"],
        "method": "native_image_fragment_groups_or_regular_pdf_page_ocr",
        "summary": {
            "fragment_pages": sorted(accepted),
            "fragment_groups": frag_report["summary"]["total_groups"],
            "fragment_ocr_lines": frag_report["summary"]["ocr_lines"],
            "page_fallback_pages": unhandled,
            "wall_seconds": round(time.monotonic() - began, 3),
        },
        "fragment_report": frag_report,
        "fallback_report": base_report,
    }
    return result, report


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--report", type=Path)
    p.add_argument("--pages")
    p.add_argument("--max-side", type=int, default=1600)
    p.add_argument("--ocr-threads", type=int, default=6)
    p.add_argument("--work-dir", type=Path)
    p.add_argument("--min-raster-width", type=int, default=48)
    p.add_argument("--min-raster-height", type=int, default=18)
    p.add_argument("--max-ocr-images", type=int, default=1000)
    p.add_argument("--raster-mode", choices=("auto", "all", "off"), default="auto")
    p.add_argument("--force-page-ocr", action="store_true")
    p.add_argument("--keep-native-text", action="store_true")
    a = p.parse_args()
    with pikepdf.Pdf.open(a.pdf) as doc:
        selected = (
            [int(x) for x in a.pages.split(",")]
            if a.pages
            else list(range(1, len(doc.pages) + 1))
        )
    root = a.work_dir or a.output.parent
    root.mkdir(parents=True, exist_ok=True)
    output, report = run(
        a.pdf,
        selected,
        root,
        a.max_side,
        a.ocr_threads,
        a.min_raster_width,
        a.min_raster_height,
        a.max_ocr_images,
        a.force_page_ocr,
        a.keep_native_text,
        a.raster_mode,
    )
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(
        json.dumps(output, ensure_ascii=False, separators=(",", ":")) + "\n"
    )
    if a.report:
        a.report.parent.mkdir(parents=True, exist_ok=True)
        a.report.write_text(
            json.dumps(report, ensure_ascii=False, separators=(",", ":")) + "\n"
        )
    print("RASTER_HYBRID", json.dumps(report["summary"], sort_keys=True), flush=True)


if __name__ == "__main__":
    main()
