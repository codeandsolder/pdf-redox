# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10", "pillow>=11", "rapidocr>=3,<4", "onnxruntime>=1.21"]
# ///
"""Recognize *embedded PDF raster XObjects* at native pixel resolution.

The existing whole-page OCR remains independent. This adapter enumerates
actual image paints, follows nested forms and transforms recognized image
text back to exact PDF user-space quadrilaterals. A repeated XObject is OCR'd
only once, regardless of its number of placements. Unsupported paint,
clipping and tiny sprite fragments are explicitly reported, never silently
treated as text-free proof. The output is an unreviewed schema-3 OCR draft.
"""

import argparse
from collections import defaultdict
import hashlib
import importlib.metadata
import json
import math
import pathlib
import time

from PIL import Image
import pikepdf
from rapidocr import RapidOCR, ModelType, OCRVersion
from native_pdf_words import native_words_for_pages
from pdf_geometry import display_mapper
from pdf_image_placements import enumerate_image_paints, image_quad, word_visible


def decoded_image(image_stream):
    """Return rendered-on-white RGB pixels including PDF /SMask or /Mask."""
    image = pikepdf.PdfImage(image_stream)
    with image.as_pil_image(apply_decode_array=True, apply_mask=True) as raw:
        if raw.mode in ("RGBA", "LA") or "transparency" in raw.info:
            rgba = raw.convert("RGBA")
            white = Image.new("RGBA", rgba.size, (255, 255, 255, 255))
            white.alpha_composite(rgba)
            result = white.convert("RGB")
        else:
            result = raw.convert("RGB")
    return result


def merge_source_line_fragments(detections):
    """Assemble recognized words in the *source raster frame*, before PDF CTM.

    Detectors sometimes return one word per box even for one printed line.
    Joining after placement loses information when the same image is rotated
    or scaled differently on different pages. Keep separate columns and
    vertical writing as distinct candidates.
    """
    groups = []
    horizontal = []
    nonhorizontal = []
    for item in detections:
        text, quad, confidence = item
        dx = quad[1][0] - quad[0][0]
        dy = quad[1][1] - quad[0][1]
        width = math.hypot(dx, dy)
        height = math.hypot(quad[3][0] - quad[0][0], quad[3][1] - quad[0][1])
        if width < 2 or height < 2 or abs(dy) > 0.15 * width:
            nonhorizontal.append(item)
            continue
        left = min(p[0] for p in quad)
        right = max(p[0] for p in quad)
        top = min(p[1] for p in quad)
        bottom = max(p[1] for p in quad)
        horizontal.append((left, right, top, bottom, item))
    for segment in sorted(horizontal, key=lambda x: (x[2] + x[3], x[0])):
        x0, x1, y0, y1, item = segment
        candidates = []
        for row in groups:
            _, _, anchor_top, anchor_bottom, _ = row[0]
            h = y1 - y0
            anchor_height = anchor_bottom - anchor_top
            if max(h, anchor_height) > 2.0 * min(h, anchor_height):
                continue
            overlap = max(0, min(y1, anchor_bottom) - max(y0, anchor_top))
            if overlap < 0.55 * min(h, anchor_height):
                continue
            candidates.append((abs((y0 + y1) - (anchor_top + anchor_bottom)), row))
        if candidates:
            min(candidates, key=lambda x: x[0])[1].append(segment)
        else:
            groups.append([segment])
    merged = []
    for row in groups:
        row.sort(key=lambda x: x[0])
        pieces = []
        end = -math.inf

        def emit():
            nonlocal pieces, end
            if not pieces:
                return
            text = pieces[0][4][0]
            score = pieces[0][4][2]
            previous = pieces[0]
            for x in pieces[1:]:
                nxt = x[4][0]
                # In a source-raster detector each Latin word already has a
                # separate recognition output, even when polygons overlap.
                # CJK fragments without a visible space remain adjacent.
                a = text.rstrip()
                if (
                    a
                    and nxt
                    and a[-1].isascii()
                    and a[-1].isalnum()
                    and nxt[0].isascii()
                    and nxt[0].isalnum()
                ):
                    text += " "
                elif (
                    a
                    and nxt
                    and x[0] - previous[1]
                    > 0.30 * min(x[3] - x[2], previous[3] - previous[2])
                ):
                    text += " "
                text += nxt
                score = min(score, x[4][2])
                previous = x
            x0 = min(s[0] for s in pieces)
            x1 = max(s[1] for s in pieces)
            y0 = min(s[2] for s in pieces)
            y1 = max(s[3] for s in pieces)
            merged.append((text, [[x0, y0], [x1, y0], [x1, y1], [x0, y1]], score))
            pieces = []
            end = -math.inf

        for entry in row:
            h = entry[3] - entry[2]
            gap = entry[0] - end
            if pieces and (gap > 1.2 * h or gap < -0.50 * (entry[1] - entry[0])):
                emit()
            pieces.append(entry)
            end = max(end, entry[1])
        emit()
    return [*merged, *nonhorizontal]


def recognize(
    pdf_path,
    selected,
    max_pixels=30_000_000,
    min_source_width=48,
    min_source_height=18,
    max_ocr_images=1000,
    min_score=0.6,
    ocr_threads=6,
    raster_mode="all",
    selected_source_ids=None,
):
    if raster_mode not in ("auto", "all", "off"):
        raise ValueError("invalid raster image recognition mode")
    if not (
        1 <= max_pixels <= 200_000_000
        and 1 <= max_ocr_images <= 100000
        and 1 <= min_source_width <= 10000
        and 1 <= min_source_height <= 10000
        and 0 <= min_score <= 1
        and 1 <= ocr_threads <= 64
    ):
        raise ValueError("invalid raster OCR policy")
    start = time.monotonic()
    with pdf_path.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    emitted = defaultdict(list)
    sightings = []
    source_stats = []
    known_recognitions = {}
    model = None
    with pikepdf.Pdf.open(pdf_path) as doc:
        if not selected or len(set(selected)) != len(selected):
            raise ValueError("empty/duplicate page selection")
        placements, images, coverage = enumerate_image_paints(doc, selected)
        page_mode = {x["page"]: x for x in coverage}
        eligible_pages = {
            n
            for n in selected
            if raster_mode == "all"
            or (raster_mode == "auto" and page_mode[n]["image_only_after_native"])
        }
        selected_source_ids = set(selected_source_ids or ())
        if raster_mode == "off":
            selected_source_ids.clear()
        refs = defaultdict(list)
        for p in placements:
            refs[p["source_id"]].append(p)
        for source_id, occurrences in refs.items():
            target = images[source_id]
            w = occurrences[0]["width"]
            h = occurrences[0]["height"]
            stat = {
                "source_id": source_id,
                "width": w,
                "height": h,
                "placements": len(occurrences),
                "safe_placements": sum(not p["unsafe_reasons"] for p in occurrences),
                "status": "unprocessed",
                "native_recognition_lines": 0,
                "recognition_seconds": 0.0,
            }
            candidates = [
                p
                for p in occurrences
                if not p["unsafe_reasons"]
                and (p["page"] in eligible_pages or source_id in selected_source_ids)
            ]
            if not candidates:
                stat["status"] = (
                    "not_selected_for_raster_mode"
                    if any(not x["unsafe_reasons"] for x in occurrences)
                    else "unsupported_placements"
                )
            elif w < min_source_width or h < min_source_height:
                stat["status"] = "tiny_raster_sprite_or_icon"
            elif w * h > max_pixels:
                stat["status"] = "source_image_too_large"
            elif len(known_recognitions) >= max_ocr_images:
                stat["status"] = "ocr_source_budget"
            else:
                t = time.monotonic()
                try:
                    rgb = decoded_image(target)
                    if rgb.size != (w, h):
                        raise ValueError(
                            "decoded dimensions differ from source image metadata"
                        )
                except (
                    ValueError,
                    TypeError,
                    RuntimeError,
                    NotImplementedError,
                    pikepdf.PdfError,
                    OSError,
                ) as e:
                    stat["status"] = "decode_failed_" + type(e).__name__
                    source_stats.append(stat)
                    continue
                if model is None:
                    model = RapidOCR(
                        params={
                            "EngineConfig.onnxruntime.intra_op_num_threads": ocr_threads,
                            "EngineConfig.onnxruntime.inter_op_num_threads": 1,
                            "Rec.model_type": ModelType.SMALL,
                            "Rec.ocr_version": OCRVersion.PPOCRV6,
                        }
                    )
                    stat["model_initialized_here"] = True
                ocr = model(rgb)
                score_boxes = ocr.boxes if ocr.boxes is not None else []
                prediction = []
                for text, quad, score in zip(
                    ocr.txts or [], score_boxes, ocr.scores or [], strict=True
                ):
                    if (
                        not text.strip()
                        or not math.isfinite(float(score))
                        or score < min_score
                    ):
                        continue
                    corners = [[float(x), float(y)] for x, y in quad]
                    if len(corners) != 4 or any(
                        not all(math.isfinite(v) for v in xy) for xy in corners
                    ):
                        continue
                    prediction.append((str(text), corners, float(score)))
                prediction = merge_source_line_fragments(prediction)
                stat["source_text_boxes"] = [
                    {"text": text, "pixel_quad": quad, "confidence": float(score)}
                    for text, quad, score in prediction
                ]
                known_recognitions[source_id] = prediction
                stat["native_recognition_lines"] = len(prediction)
                stat["recognition_seconds"] = round(time.monotonic() - t, 3)
                stat["status"] = "recognized"
            source_stats.append(stat)
            for p in occurrences:
                if (
                    p["page"] not in eligible_pages
                    and source_id not in selected_source_ids
                ):
                    sightings.append(
                        {
                            "page": p["page"],
                            "source_id": source_id,
                            "status": "not_selected_for_raster_mode",
                        }
                    )
                    continue
                if p["unsafe_reasons"]:
                    sightings.append(
                        {
                            "page": p["page"],
                            "source_id": source_id,
                            "status": "fallback_unsafe_visual_context",
                            "reasons": p["unsafe_reasons"],
                        }
                    )
                    continue
                if source_id not in known_recognitions:
                    sightings.append(
                        {
                            "page": p["page"],
                            "source_id": source_id,
                            "status": "fallback_" + stat["status"],
                        }
                    )
                    continue
                usable = 0
                clipped = 0
                for text, corners, score in known_recognitions[source_id]:
                    quad = image_quad(p, corners)
                    if not word_visible(quad, p):
                        clipped += 1
                        continue
                    # The PDF retains its original raster painting. This
                    # draft describes the image's original text, not a copy.
                    emitted[p["page"]].append(
                        {
                            "text": text,
                            "quad": quad,
                            "confidence": float(score),
                            "reviewed": False,
                            "source_id": source_id,
                            "paint_index": p["paint_index"],
                        }
                    )
                    usable += 1
                raster_footprint = image_quad(p, [[0, 0], [w, 0], [w, h], [0, h]])
                sightings.append(
                    {
                        "page": p["page"],
                        "source_id": source_id,
                        "paint_index": p["paint_index"],
                        "page_quad": raster_footprint,
                        "status": "recognized_at_native_source_resolution",
                        "lines": usable,
                        "excluded_by_pdf_clip": clipped,
                    }
                )
        transforms = {n: display_mapper(doc.pages[n - 1])[0] for n in selected}
        native = native_words_for_pages(pdf_path, selected, transforms)
        doc_pages = [
            {"page": n, "native_text_regions": native[n], "lines": emitted[n]}
            for n in selected
        ]
    output = {
        "schema_version": 3,
        "pdf_sha256": digest,
        "model": f"RapidOCR {importlib.metadata.version('rapidocr')} on actual embedded PDF Image XObjects; native pixel coordinates + original content CTM",
        "pages": doc_pages,
    }
    report = {
        "schema_version": 1,
        "pdf_sha256": digest,
        "method": "native_source_image_xobject_ocr_not_page_rasterization",
        "raster_mode": raster_mode,
        "policy": {
            "max_pixels": max_pixels,
            "min_width": min_source_width,
            "min_height": min_source_height,
            "min_score": min_score,
            "max_ocr_images": max_ocr_images,
        },
        "summary": {
            "source_objects_used": len(refs),
            "source_objects_recognized": len(known_recognitions),
            "image_placements": len(placements),
            "ocr_inference_calls": len(known_recognitions),
            "raster_ocr_lines": sum(len(x["lines"]) for x in doc_pages),
            "native_page_words": sum(len(x["native_text_regions"]) for x in doc_pages),
            "wall_seconds": round(time.monotonic() - start, 3),
        },
        "pages": coverage,
        "sources": source_stats,
        "placements": sightings,
    }
    return output, report, model


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pdf", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument(
        "--pages", help="one-based comma-separated PDF pages; default all"
    )
    parser.add_argument("--raster-mode", choices=("all", "auto", "off"), default="all")
    parser.add_argument("--min-width", type=int, default=48)
    parser.add_argument("--min-height", type=int, default=18)
    parser.add_argument("--max-pixels", type=int, default=30_000_000)
    parser.add_argument("--max-ocr-images", type=int, default=1000)
    parser.add_argument("--min-score", type=float, default=0.6)
    parser.add_argument("--ocr-threads", type=int, default=6)
    args = parser.parse_args()
    with pikepdf.Pdf.open(args.pdf) as doc:
        pages = (
            [int(x) for x in args.pages.split(",")]
            if args.pages
            else list(range(1, len(doc.pages) + 1))
        )
    output, report, _ = recognize(
        args.pdf,
        pages,
        max_pixels=args.max_pixels,
        min_source_width=args.min_width,
        min_source_height=args.min_height,
        max_ocr_images=args.max_ocr_images,
        min_score=args.min_score,
        ocr_threads=args.ocr_threads,
        raster_mode=args.raster_mode,
    )
    for path, obj in ((args.output, output), (args.report, report)):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(obj, ensure_ascii=False, separators=(",", ":")) + "\n"
        )
    print("RASTER_IMAGE_OCR", json.dumps(report["summary"], sort_keys=True), flush=True)
    for page in report["pages"]:
        lines = next(x["lines"] for x in output["pages"] if x["page"] == page["page"])
        print(
            "PAGE",
            page["page"],
            "image_paints",
            page["image_paints"],
            "safe_placements",
            page["image_paints"] - page["unsafe_image_paints"],
            "lines",
            len(lines),
            "image_only",
            page["image_only_after_native"],
            flush=True,
        )


if __name__ == "__main__":
    main()
