# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10","pillow>=11","numpy>=2","opencv-python-headless>=4.11","rapidocr>=3,<4","onnxruntime>=1.21"]
# ///
"""Native-resolution OCR of PDF raster fragments assembled by paint geometry.

This is the image counterpart of the Type3 glyph assembly: small image
XObjects are placed into local source-resolution mosaics using original
PDF transforms and clip paths. A page is eligible for skipping page OCR
only if every visible non-native paint is accounted for by recognized
mosaics. Any other page follows the existing full-page pipeline.
"""

import argparse
import hashlib
import json
import math
from pathlib import Path
import time

import pikepdf
from PIL import Image
from rapidocr import RapidOCR
from pdf_geometry import display_mapper
from native_pdf_words import native_words_for_pages
from pdf_image_placements import enumerate_image_paints, image_quad, inside_convex
from reconstruct_raster_mosaic import composite, cluster_native_raster_paints
from ocr_embedded_images import merge_source_line_fragments


def from_raster_frame(region, pitch, box):
    if len(box) != 4:
        raise ValueError("OCR image quad needs four corners")
    left, bottom, right, top = region

    def point(index):
        x, y = map(float, box[index])
        if not (math.isfinite(x) and math.isfinite(y)):
            raise ValueError("nonfinite OCR source raster box")
        return [left + x * pitch, top - y * pitch]

    return [point(i) for i in (3, 2, 1, 0)]


def rectify_group_orientation(bitmap, placements):
    """Return source-text-upright bitmap and an inverse point mapping.

    PDF page rotations and CTM quarter turns are distinct. Rotate fragment
    mosaics by their source-raster x-axis, then invert this transform before
    returning OCR coordinates to the PDF.
    """
    quadrants = set()
    for placement in placements:
        a, b = placement["ctm"][:2]
        if math.hypot(a, b) < 1e-6:
            raise ValueError("degenerate fragment image x-axis")
        angle = math.degrees(math.atan2(-b, a))
        quarter = round(angle / 90)
        if abs(angle - quarter * 90) > 7:
            raise ValueError("fragment group needs arbitrary affine rectification")
        quadrants.add(quarter % 4)
    if len(quadrants) != 1:
        raise ValueError("group contains conflicting raster-text orientations")
    quarter = quadrants.pop()
    w, h = bitmap.size
    if quarter == 0:
        return bitmap, lambda point: point
    if quarter == 1:
        # Source text points down in page-raster space: rotate CCW.
        return bitmap.transpose(Image.Transpose.ROTATE_90), lambda point: (
            w - point[1],
            point[0],
        )
    if quarter == 2:
        return bitmap.transpose(Image.Transpose.ROTATE_180), lambda point: (
            w - point[0],
            h - point[1],
        )
    # Source text points up in page-raster space: rotate clockwise.
    return bitmap.transpose(Image.Transpose.ROTATE_270), lambda point: (
        point[1],
        h - point[0],
    )


def group_cache_key(group):
    """Canonicalize a source-image paint group under pure page translation.

    The source objects, their paint order, relative source transforms and
    relative PDF clip polygons must be identical. A different crop or
    scale is a different key even if the same images are referenced.
    """
    left, bottom, right, top = group["region"]

    def six(values):
        return tuple(round(float(v), 6) for v in values)

    components = []
    for placement in group["placements"]:
        c = placement["ctm"]
        corners = image_quad(
            placement,
            [
                [0, 0],
                [placement["width"], 0],
                [placement["width"], placement["height"]],
                [0, placement["height"]],
            ],
        )
        # Ignore page/form clips that do not intersect source pixels:
        # shifting the same group to another page location cannot change
        # its bitmap when every pixel remains inside those clips.
        clips = tuple(
            tuple(six((p[0] - left, p[1] - bottom)) for p in polygon)
            for polygon in placement["clip_polygons"]
            if not all(inside_convex(point, polygon, eps=1e-5) for point in corners)
        )
        components.append(
            (
                placement["source_id"],
                placement["width"],
                placement["height"],
                six((*c[:4], c[4] - left, c[5] - bottom)),
                clips,
            )
        )
    return (
        six((right - left, top - bottom, group["pitch"])),
        tuple(components),
    )


def pages_from_fragments(
    pdf_path, selected, ocr_threads=6, min_score=0.60, max_group_pixels=12_000_000
):
    with pdf_path.open("rb") as stream:
        source_hash = hashlib.file_digest(stream, "sha256").hexdigest()
    if len(set(selected)) != len(selected) or not selected:
        raise ValueError("page selection must be nonempty and unique")
    start = time.monotonic()
    engine = None
    cache = {}
    cache_hits = 0
    model_calls = 0
    by_page = []
    metadata = []
    with pikepdf.Pdf.open(pdf_path) as doc:
        paints, images, stats = enumerate_image_paints(doc, selected)
        by_page_placements = {
            n: [p for p in paints if p["page"] == n] for n in selected
        }
        transforms = {n: display_mapper(doc.pages[n - 1])[0] for n in selected}
        native = native_words_for_pages(pdf_path, selected, transforms)
        for record in stats:
            n = record["page"]
            sources = by_page_placements[n]
            info = {
                "page": n,
                "paint_count": len(sources),
                "source_image_groups": 0,
                "images_covered": 0,
                "ocr_lines": 0,
                "eligible": False,
                "reason": "not_pure_image_content",
            }
            if not record["image_only_after_native"]:
                metadata.append(info)
                continue
            if any(x["unsafe_reasons"] for x in sources):
                info["reason"] = "unknown_compositing_or_clipping"
                metadata.append(info)
                continue
            groups = cluster_native_raster_paints(
                sources,
                tuple(float(x) for x in doc.pages[n - 1].cropbox),
                max_group_pixels=max_group_pixels,
            )
            info["source_image_groups"] = len(groups)
            used = [item for group in groups for item in group["placements"]]
            info["images_covered"] = len(used)
            if len(used) != len(sources):
                info["reason"] = "unreconstructed_or_isolated_image"
                metadata.append(info)
                continue
            candidates = []
            for group in groups:
                key = group_cache_key(group)
                if key in cache:
                    lines_from_pixels = cache[key]
                    cache_hits += 1
                else:
                    raster, _, report = composite(
                        group["placements"],
                        images,
                        group["region"],
                        group["pitch"],
                        max_pixels=max_group_pixels,
                    )
                    if report["paint_statistics"]["painted_instances"] != len(
                        group["placements"]
                    ):
                        info["reason"] = "incomplete_source_image_composite"
                        break
                    try:
                        upright, inverse = rectify_group_orientation(
                            raster, group["placements"]
                        )
                    except ValueError:
                        info["reason"] = "unsupported_fragment_group_orientation"
                        break
                    if engine is None:
                        engine = RapidOCR(
                            params={
                                "EngineConfig.onnxruntime.intra_op_num_threads": ocr_threads,
                                "EngineConfig.onnxruntime.inter_op_num_threads": 1,
                            }
                        )
                    result = engine(upright)
                    model_calls += 1
                    detections = []
                    for text, box, confidence in zip(
                        result.txts or [],
                        result.boxes if result.boxes is not None else [],
                        result.scores or [],
                        strict=True,
                    ):
                        if (
                            text.strip()
                            and math.isfinite(float(confidence))
                            and confidence >= min_score
                        ):
                            detections.append(
                                (
                                    str(text),
                                    [[float(v) for v in p] for p in box],
                                    float(confidence),
                                )
                            )
                    # Join adjacent words in the OCR-upright coordinate
                    # system. Joining after unrotating would treat a
                    # vertical line as several unrelated word fragments.
                    upright_lines = merge_source_line_fragments(detections)
                    lines_from_pixels = [
                        (text, [list(inverse(tuple(p))) for p in quad], score)
                        for text, quad, score in upright_lines
                    ]
                    cache[key] = lines_from_pixels
                for text, quad, confidence in lines_from_pixels:
                    candidates.append(
                        {
                            "text": text,
                            "confidence": confidence,
                            "quad": from_raster_frame(
                                group["region"], group["pitch"], quad
                            ),
                            "reviewed": False,
                        }
                    )
            else:
                if candidates:
                    info["eligible"] = True
                    info["reason"] = "all_source_fragments_reconstructed_and_recognized"
                    info["ocr_lines"] = len(candidates)
                    by_page.append(
                        {
                            "page": n,
                            "lines": candidates,
                            "native_text_regions": native[n],
                        }
                    )
                else:
                    info["reason"] = "no_detected_raster_text"
            metadata.append(info)
    return {
        "schema_version": 3,
        "pdf_sha256": source_hash,
        "model": "RapidOCR PP-OCRv6 small native-mosaic of PDF Image XObject fragments",
        "pages": by_page,
    }, {
        "schema_version": 1,
        "pdf_sha256": source_hash,
        "method": "source_image_fragment_native_pixel_reconstruction",
        "pages": metadata,
        "summary": {
            "selected_pages": len(selected),
            "eligible_pages": [p["page"] for p in metadata if p["eligible"]],
            "total_groups": sum(p["source_image_groups"] for p in metadata),
            "groups_with_text": sum(p["eligible"] for p in metadata),
            "ocr_lines": sum(p["ocr_lines"] for p in metadata),
            "model_calls": model_calls,
            "recognized_group_cache_hits": cache_hits,
            "wall_seconds": round(time.monotonic() - start, 3),
        },
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=Path, required=True)
    p.add_argument("--pages", required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--report", type=Path, required=True)
    p.add_argument("--ocr-threads", type=int, default=6)
    p.add_argument("--max-group-pixels", type=int, default=12_000_000)
    args = p.parse_args()
    pages = [int(x) for x in args.pages.split(",")]
    doc, report = pages_from_fragments(
        args.pdf, pages, args.ocr_threads, max_group_pixels=args.max_group_pixels
    )
    for path, item in ((args.output, doc), (args.report, report)):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(item, ensure_ascii=False, indent=2) + "\n")
    print(
        "RASTER_FRAGMENT_OCR", json.dumps(report["summary"], sort_keys=True), flush=True
    )


if __name__ == "__main__":
    main()
