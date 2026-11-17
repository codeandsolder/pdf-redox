# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10","pillow>=11","numpy>=2","opencv-python-headless>=4.11","rapidocr>=3,<4","onnxruntime>=1.21"]
# ///
"""Experimental native-source-image mosaic for fragmented PDF rasters.

The input is the original image XObjects and their *actual paint order*.
Reconstruct only a user-specified PDF-space rectangle at the native
sampling pitch. Clipping and image alpha are applied before compositing.
The source PDF is never rewritten. Unsupported clipping/graphics effects
are explicitly reported so this mosaic is not a proof of visual identity.
"""

import argparse
from collections import Counter
import statistics
import hashlib
import json
import math
from pathlib import Path
import time

import cv2
import numpy as np
from PIL import Image
import pikepdf
from pdf_image_placements import enumerate_image_paints, image_quad


def inverse_canvas(point, region, pitch):
    left, bottom, right, top = region
    return ((point[0] - left) / pitch, (top - point[1]) / pitch)


def source_to_canvas(image_placement, region, pitch):
    image_width = image_placement["width"]
    image_height = image_placement["height"]
    bounds = image_quad(
        image_placement,
        [[0, 0], [image_width, 0], [image_width, image_height], [0, image_height]],
    )
    # image_quad returns PDF BL BR TR TL
    destination = np.float32(
        [
            inverse_canvas(bounds[3], region, pitch),
            inverse_canvas(bounds[2], region, pitch),
            inverse_canvas(bounds[0], region, pitch),
        ]
    )
    source = np.float32([(0, 0), (image_width, 0), (0, image_height)])
    return cv2.getAffineTransform(source, destination)


def composite(image_placements, images, region, pitch=0.48, max_pixels=12_000_000):
    left, bottom, right, top = map(float, region)
    if not (0 < pitch <= 10 and right > left and top > bottom):
        raise ValueError("invalid raster mosaic region/pitch")
    width = math.ceil((right - left) / pitch)
    height = math.ceil((top - bottom) / pitch)
    if width * height > max_pixels:
        raise ValueError("native reconstruction pixel budget exceeded")
    rgb = np.full((height, width, 3), 255, dtype=np.uint8)
    coverage = np.zeros((height, width), dtype=np.uint8)
    stats = Counter()
    parsed = {}
    # Do not sort by page resource name/ID; PDF paint order is essential.
    for placement in image_placements:
        source = placement["source_id"]
        stats["paint_instances"] += 1
        w = placement["width"]
        h = placement["height"]
        corners = image_quad(placement, [[0, 0], [w, 0], [w, h], [0, h]])
        minx = min(p[0] for p in corners)
        maxx = max(p[0] for p in corners)
        miny = min(p[1] for p in corners)
        maxy = max(p[1] for p in corners)
        if maxx < left or minx > right or maxy < bottom or miny > top:
            stats["outside_selected_region"] += 1
            continue
        if bool(images[source].get("/ImageMask", False)):
            stats["stencil_image_skip"] += 1
            continue
        if source not in parsed:
            try:
                raw = pikepdf.PdfImage(images[source]).as_pil_image(
                    apply_decode_array=True, apply_mask=True
                )
                rgba = np.asarray(raw.convert("RGBA"), dtype=np.uint8)
                parsed[source] = rgba.copy()
            except (
                TypeError,
                ValueError,
                RuntimeError,
                NotImplementedError,
                pikepdf.PdfError,
                OSError,
            ):
                stats["failed_image_decode"] += 1
                continue
        rgba = parsed[source]
        canvas_corners = np.asarray(
            [inverse_canvas(point, region, pitch) for point in corners],
            dtype=np.float64,
        )
        # Warp only the pixels covered by this specific image paint. Warping
        # a 12x300 strip to a 1200x300 canvas a hundred times is 100x
        # unnecessary work; translating the affine map to the local ROI is
        # mathematically equivalent.
        x0 = max(0, int(math.floor(float(np.min(canvas_corners[:, 0])))) - 2)
        y0 = max(0, int(math.floor(float(np.min(canvas_corners[:, 1])))) - 2)
        x1 = min(width, int(math.ceil(float(np.max(canvas_corners[:, 0])))) + 2)
        y1 = min(height, int(math.ceil(float(np.max(canvas_corners[:, 1])))) + 2)
        if x1 <= x0 or y1 <= y0:
            stats["outside_selected_region"] += 1
            continue
        size = (x1 - x0, y1 - y0)
        matrix = source_to_canvas(placement, region, pitch)
        matrix[0, 2] -= x0
        matrix[1, 2] -= y0
        warped = cv2.warpAffine(
            rgba,
            matrix,
            size,
            flags=cv2.INTER_LINEAR,
            borderMode=cv2.BORDER_CONSTANT,
            borderValue=(0, 0, 0, 0),
        )
        alpha = warped[:, :, 3].astype(np.float32) / 255
        for clip in placement["clip_polygons"]:
            pts = np.asarray(
                [inverse_canvas(point, region, pitch) for point in clip],
                dtype=np.float32,
            )
            pts -= np.array([x0, y0], dtype=np.float32)
            mask = np.zeros((size[1], size[0]), dtype=np.uint8)
            cv2.fillConvexPoly(
                mask, np.rint(pts).astype(np.int32), 255, lineType=cv2.LINE_8
            )
            alpha *= mask.astype(np.float32) / 255
        if any(placement["unsafe_reasons"]):
            stats["unsafe_placement_painted_for_diagnostic"] += 1
        nonempty = alpha > 0.005
        if not np.any(nonempty):
            stats["invisible_or_clipped_source"] += 1
            continue
        rgb_roi = rgb[y0:y1, x0:x1]
        coverage_roi = coverage[y0:y1, x0:x1]
        subset = rgb_roi[nonempty].astype(np.float32)
        rgb_roi[nonempty] = np.clip(
            warped[nonempty, :3].astype(np.float32) * alpha[nonempty, None]
            + subset * (1 - alpha[nonempty, None]),
            0,
            255,
        ).astype(np.uint8)
        coverage_roi[nonempty] = 255
        stats["painted_instances"] += 1
    report = {
        "page_region": [left, bottom, right, top],
        "native_pitch": pitch,
        "image_width": width,
        "image_height": height,
        "unique_decoded_images": len(parsed),
        "paint_statistics": dict(stats),
        "covered_pixels": int(np.count_nonzero(coverage)),
    }
    return Image.fromarray(rgb), Image.fromarray(coverage), report


def cluster_native_raster_paints(
    placements,
    page_rect,
    proximity_pixels=2.5,
    scale_tolerance=0.075,
    max_group_pixels=12_000_000,
):
    """Group neighboring actual image paints before OCR, not resource names.

    Two paints are eligible to share a mosaic if source-pixel pitches
    match and their on-page image footprints touch within a small number
    of native pixels. A single large image needs no reconstruction.
    Unsafe/clipped-image uncertainties are not dropped silently.
    """
    if proximity_pixels <= 0 or not 0 < scale_tolerance < 1:
        raise ValueError("invalid image fragment cluster thresholds")
    items = []
    for p in placements:
        if p["unsafe_reasons"]:
            continue
        w, h = p["width"], p["height"]
        if w <= 0 or h <= 0:
            continue
        c = p["ctm"]
        pitch_x = math.hypot(c[0], c[1]) / w
        pitch_y = math.hypot(c[2], c[3]) / h
        pitch = (pitch_x + pitch_y) / 2
        if not 0.04 < pitch <= 10 or abs(pitch_x - pitch_y) > scale_tolerance * pitch:
            continue
        q = image_quad(p, [[0, 0], [w, 0], [w, h], [0, h]])
        x0 = max(float(page_rect[0]), min(v[0] for v in q))
        y0 = max(float(page_rect[1]), min(v[1] for v in q))
        x1 = min(float(page_rect[2]), max(v[0] for v in q))
        y1 = min(float(page_rect[3]), max(v[1] for v in q))
        if x1 <= x0 or y1 <= y0:
            continue
        items.append((p, (x0, y0, x1, y1), pitch))
    parents = list(range(len(items)))

    def root(i):
        while parents[i] != i:
            parents[i] = parents[parents[i]]
            i = parents[i]
        return i

    def union(i, j):
        parents[root(j)] = root(i)

    for i, (p, a, pitch_a) in enumerate(items):
        for j in range(i + 1, len(items)):
            other, b, pitch_b = items[j]
            if abs(pitch_a - pitch_b) > scale_tolerance * max(pitch_a, pitch_b):
                continue
            xgap = max(0, a[0] - b[2], b[0] - a[2])
            ygap = max(0, a[1] - b[3], b[1] - a[3])
            if (
                xgap <= proximity_pixels * pitch_a
                and ygap <= proximity_pixels * pitch_a
            ):
                union(i, j)
    groups = {}
    for i, item in enumerate(items):
        groups.setdefault(root(i), []).append(item)
    result = []
    for group in groups.values():
        if len(group) < 2:
            continue
        pitch = statistics.median(z[2] for z in group)
        bound = (
            max(float(page_rect[0]), min(z[1][0] for z in group) - 5 * pitch),
            max(float(page_rect[1]), min(z[1][1] for z in group) - 5 * pitch),
            min(float(page_rect[2]), max(z[1][2] for z in group) + 5 * pitch),
            min(float(page_rect[3]), max(z[1][3] for z in group) + 5 * pitch),
        )
        pixels = math.ceil((bound[2] - bound[0]) / pitch) * math.ceil(
            (bound[3] - bound[1]) / pitch
        )
        if pixels > max_group_pixels:
            continue
        result.append(
            {
                "placements": [z[0] for z in group],
                "region": bound,
                "pitch": pitch,
                "estimated_pixels": pixels,
            }
        )
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=Path, required=True)
    p.add_argument("--page", type=int, required=True)
    p.add_argument(
        "--rect",
        nargs=4,
        type=float,
        required=True,
        metavar=("LEFT", "BOTTOM", "RIGHT", "TOP"),
    )
    p.add_argument("--pitch", type=float, default=0.48)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--mask", type=Path, required=True)
    p.add_argument("--report", type=Path, required=True)
    args = p.parse_args()
    start = time.monotonic()
    with pikepdf.Pdf.open(args.pdf) as pdf:
        placements, images, pages = enumerate_image_paints(pdf, [args.page])
        img, mask, report = composite(placements, images, args.rect, args.pitch)
        report["pdf_sha256"] = hashlib.sha256(args.pdf.read_bytes()).hexdigest()
        report["page"] = args.page
        report["page_paint"] = pages[0]
        report["wall_seconds"] = round(time.monotonic() - start, 3)
    for path in (args.output, args.mask, args.report):
        path.parent.mkdir(parents=True, exist_ok=True)
    img.save(args.output)
    mask.save(args.mask)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(
        "RECONSTRUCTED_RASTER",
        json.dumps(
            {
                "size": list(img.size),
                "covered": report["covered_pixels"],
                "stats": report["paint_statistics"],
                "wall_s": report["wall_seconds"],
            }
        ),
    )


if __name__ == "__main__":
    main()
