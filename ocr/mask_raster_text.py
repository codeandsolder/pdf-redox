# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10", "pillow>=11", "numpy>=2"]
# ///
"""Suppress source-image OCR text only in a temporary PDF used for page inference.

Modify the decoded native pixels of *eligible* Image XObjects; do not draw
opaque overlays on PDF pages, which would hide vector content painted after
the image. The real source PDF is never written or changed.

The raster-source report is SHA-bound to the original document and supplies
source-pixel word/line boxes; this module modifies only 8-bit Gray/RGB/CMYK
XObjects with simple colorspaces, without alpha, Decode, masks, ImageMask,
or any unsupported paint state. Other sources are left for regular page OCR.
"""

import collections
import hashlib
import math
import zlib
from pathlib import Path

import pikepdf
import numpy as np
from PIL import ImageDraw, ImageFilter, ImageStat
from mask_native_text import mask_document


COLORSPACES = {
    "/DeviceGray": ("L", 1),
    "/DeviceRGB": ("RGB", 3),
    "/DeviceCMYK": ("CMYK", 4),
}


def source_text_masks(report, selected, min_confidence=0.97):
    if not 0 <= min_confidence <= 1:
        raise ValueError("invalid source-mask confidence threshold")
    selected = set(selected)
    byid = collections.defaultdict(list)
    for source in report.get("sources", []):
        source_id = source["source_id"]
        if source["status"] != "recognized":
            continue
        for line in source.get("source_text_boxes", []):
            confidence = float(line["confidence"])
            if confidence < min_confidence or len(str(line["text"]).strip()) < 2:
                continue
            boxes = line["pixel_quad"]
            if len(boxes) != 4 or any(
                len(p) != 2 or not all(math.isfinite(float(v)) for v in p)
                for p in boxes
            ):
                raise ValueError("invalid PDF-bound image text source rectangle")
            byid[source_id].append(line)
    # A source may be reused on pages with unsupported/unknown render
    # conditions. If so, do not conceal it in any selected page.
    appearances = collections.defaultdict(list)
    for appearance in report.get("placements", []):
        if appearance["page"] in selected:
            appearances[appearance["source_id"]].append(appearance)
    return {
        key: lines
        for key, lines in byid.items()
        if key in appearances
        and all(
            p["status"] == "recognized_at_native_source_resolution"
            for p in appearances[key]
        )
    }


def _fill_color_from_border(bitmap, poly):
    # A robust local background estimate, in the ORIGINAL source color
    # components. No CMYK/RGB conversion or change outside the polygon.
    from PIL import Image, ImageChops

    w, h = bitmap.size
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).polygon(poly, fill=255)
    inner = mask.filter(ImageFilter.MaxFilter(7))
    ring = ImageChops.subtract(inner, mask)
    x0, y0, x1, y1 = inner.getbbox() or (0, 0, 0, 0)
    if x1 <= x0:
        return None
    cropped = bitmap.crop((x0, y0, x1, y1))
    region = ring.crop((x0, y0, x1, y1))
    background = ImageStat.Stat(cropped, region).median
    return tuple(int(round(v)) for v in background)


def prepare_render_image_pdf(
    original: Path,
    temporary: Path,
    report,
    selected,
    min_confidence=0.97,
    padding_pixels=3,
    control_pdf: Path | None = None,
    native_mask_pages=None,
    source_ids=None,
):
    if padding_pixels < 0 or padding_pixels > 24:
        raise ValueError("invalid source-image masking padding")
    if control_pdf is not None and control_pdf == temporary:
        raise ValueError("control and image-masked PDFs must be distinct")
    with original.open("rb") as stream:
        original_hash = hashlib.file_digest(stream, "sha256").hexdigest()
    if original_hash != report["pdf_sha256"]:
        raise ValueError("source-image OCR evidence PDF digest mismatch")
    selections = source_text_masks(report, selected, min_confidence)
    if source_ids is not None:
        source_ids = set(source_ids)
        selections = {
            source_id: lines
            for source_id, lines in selections.items()
            if source_id in source_ids
        }
    stats = collections.Counter()
    expectations = []
    with pikepdf.Pdf.open(original) as document:
        native_stats = (
            mask_document(document, native_mask_pages) if native_mask_pages else {}
        )
        if control_pdf is not None:
            control_pdf.parent.mkdir(parents=True, exist_ok=True)
            document.save(control_pdf, compress_streams=True)
        for source_id, lines in selections.items():
            try:
                object_num, generation = (int(v) for v in source_id.split(":"))
                image_stream = document.get_object(object_num, generation)
                if str(image_stream.get("/Subtype", "")) != "/Image":
                    stats["not_image_xobject"] += 1
                    continue
                colorspace = str(image_stream.get("/ColorSpace", ""))
                if (
                    colorspace not in COLORSPACES
                    or int(image_stream.get("/BitsPerComponent", 0)) != 8
                    or image_stream.get("/SMask") is not None
                    or image_stream.get("/Mask") is not None
                    or bool(image_stream.get("/ImageMask", False))
                    or image_stream.get("/Decode") is not None
                ):
                    stats["unsupported_image_encoding"] += 1
                    continue
                expected_mode, channels = COLORSPACES[colorspace]
                with pikepdf.PdfImage(image_stream).as_pil_image() as decoded:
                    if decoded.mode != expected_mode:
                        stats["decoder_color_mode_mismatch"] += 1
                        continue
                    bitmap = decoded.copy()
                width, height = bitmap.size
                if width * height > 30_000_000:
                    stats["oversized_source_image"] += 1
                    continue
                before = np.asarray(bitmap).copy()
                # Apply every line only after validating its source bounds.
                mask_records = []
                for line in lines:
                    poly = [
                        (round(float(x)), round(float(y)))
                        for x, y in line["pixel_quad"]
                    ]
                    if any(x < 0 or x > width or y < 0 or y > height for x, y in poly):
                        raise ValueError("OCR source text box outside original image")
                    mask_records.append(poly)
                for poly in mask_records:
                    from PIL import Image

                    mask = Image.new("L", (width, height), 0)
                    drawer = ImageDraw.Draw(mask)
                    drawer.polygon(poly, fill=255)
                    mask = mask.filter(ImageFilter.MaxFilter(2 * padding_pixels + 1))
                    color = _fill_color_from_border(bitmap, poly)
                    if color is None:
                        continue
                    ImageDraw.Draw(bitmap).bitmap((0, 0), mask, fill=color)
                    stats["source_text_lines_masked"] += 1
                after = np.asarray(bitmap)
                delta = np.max(
                    np.abs(before.astype(np.int16) - after.astype(np.int16)), axis=2
                )
                for line, poly in zip(lines, mask_records, strict=True):
                    x0 = max(0, math.floor(min(p[0] for p in poly)))
                    x1 = min(width, math.ceil(max(p[0] for p in poly)))
                    y0 = max(0, math.floor(min(p[1] for p in poly)))
                    y1 = min(height, math.ceil(max(p[1] for p in poly)))
                    source_polygon = Image.new("L", (x1 - x0, y1 - y0), 0)
                    ImageDraw.Draw(source_polygon).polygon(
                        [(x - x0, y - y0) for x, y in poly], fill=255
                    )
                    interior = np.asarray(source_polygon) > 0
                    count = int(np.count_nonzero((delta[y0:y1, x0:x1] > 8) & interior))
                    expectations.append(
                        {
                            "source_id": source_id,
                            "text": line["text"],
                            "pixel_quad": line["pixel_quad"],
                            "confidence": float(line["confidence"]),
                            "changed_source_pixels": count,
                            "width": width,
                            "height": height,
                        }
                    )
                if "/DecodeParms" in image_stream:
                    del image_stream["/DecodeParms"]
                # Convert to a lossless Flate raw stream in *the same* PDF
                # colorspace. The temporary copy may grow; never export it.
                image_stream.write(
                    zlib.compress(bitmap.tobytes(), level=6),
                    filter=pikepdf.Name("/FlateDecode"),
                )
                image_stream["/BitsPerComponent"] = 8
                image_stream["/ColorSpace"] = pikepdf.Name(colorspace)
                stats["source_images_masked"] += 1
            except (ValueError, TypeError, pikepdf.PdfError, KeyError) as ex:
                stats["source_rewrite_failed"] += 1
                stats["failure_type_" + type(ex).__name__] += 1
                continue
        temporary.parent.mkdir(parents=True, exist_ok=True)
        document.save(temporary, compress_streams=True)
    result = dict(stats)
    result["native_text_mask_statistics"] = native_stats
    result["visibility_expectations"] = expectations
    return result


def _pdf_quad_from_source(full_image_quad, source_pixel_quad, width, height):
    """Affine image-pixel -> original PDF user space (source frame is top-left)."""
    bl, br, tr, tl = full_image_quad
    transformed = [
        [
            tl[0]
            + float(x) / width * (tr[0] - tl[0])
            + float(y) / height * (bl[0] - tl[0]),
            tl[1]
            + float(x) / width * (tr[1] - tl[1])
            + float(y) / height * (bl[1] - tl[1]),
        ]
        for x, y in source_pixel_quad
    ]
    # OCR source frame TL/TR/BR/BL -> PDF annotation frame BL/BR/TR/TL.
    return [transformed[i] for i in (3, 2, 1, 0)]


def _polygon_area(points):
    return (
        abs(
            sum(
                points[i][0] * points[(i + 1) % len(points)][1]
                - points[(i + 1) % len(points)][0] * points[i][1]
                for i in range(len(points))
            )
        )
        / 2
    )


def _plan_visibility_crops(boxes, width, height, max_regions=4):
    """Plan cheap comparison crops around projected source-text polygons.

    Rendering one full treatment raster is necessary for remaining-page
    OCR. Only its control comparison can be cropped. At most four regional
    Poppler calls are used; the cost model includes a per-call penalty, so
    sparse labels benefit without repeatedly rendering the same PDF page
    hundreds of times.
    """
    if width <= 0 or height <= 0 or not 1 <= max_regions <= 4:
        raise ValueError("invalid visibility crop dimensions/budget")
    if not boxes or len(boxes) > 256:
        return []
    page_area = width * height
    regions = []
    for x0, y0, x1, y1 in boxes:
        bounds = (
            max(0, math.floor(x0) - 4),
            max(0, math.floor(y0) - 4),
            min(width, math.ceil(x1) + 4),
            min(height, math.ceil(y1) + 4),
        )
        if bounds[2] > bounds[0] and bounds[3] > bounds[1]:
            regions.append(bounds)
    if not regions:
        return []

    def union(rectangles):
        return (
            min(r[0] for r in rectangles),
            min(r[1] for r in rectangles),
            max(r[2] for r in rectangles),
            max(r[3] for r in rectangles),
        )

    def area(r):
        return (r[2] - r[0]) * (r[3] - r[1])

    # Alternative 1: a single crop containing all source label boxes.
    merged = [union(regions)]
    candidates = [merged]
    # Alternative 2: spatial quadrants (bounded 4 calls). Each label is
    # assigned whole to one region by its center, never cut in half.
    groups = collections.defaultdict(list)
    for rect in regions:
        cx = (rect[0] + rect[2]) / 2
        cy = (rect[1] + rect[3]) / 2
        groups[(cx >= width / 2, cy >= height / 2)].append(rect)
    quadrants = [union(group) for group in groups.values()]
    if len(quadrants) > 1 and len(quadrants) <= max_regions:
        candidates.append(quadrants)
    # Pixel-area reduction, with the equivalent of 8% of a page per extra
    # Poppler invocation. Only adopt crops with >=20% estimated saving
    # over one full control render, otherwise use the old full-page proof.
    call_cost = 0.08 * page_area
    best = min(candidates, key=lambda c: sum(area(r) for r in c) + call_cost * len(c))
    if sum(area(r) for r in best) + call_cost * len(best) >= 0.8 * (
        page_area + call_cost
    ):
        return []
    return sorted(best, key=lambda r: (r[1], r[0]))


def _page_pixels(pdf_path, page_number, max_side, crop=None):
    import io
    import subprocess
    from PIL import Image

    command = [
        "pdftoppm",
        "-f",
        str(page_number),
        "-l",
        str(page_number),
        "-scale-to",
        str(max_side),
        "-singlefile",
    ]
    if crop is not None:
        x0, y0, x1, y1 = map(int, crop)
        if x1 <= x0 or y1 <= y0:
            raise ValueError("invalid Poppler crop pixel rectangle")
        command.extend(
            ["-x", str(x0), "-y", str(y0), "-W", str(x1 - x0), "-H", str(y1 - y0)]
        )
    command.append(str(pdf_path))
    proc = subprocess.run(command, capture_output=True, check=True)
    with Image.open(io.BytesIO(proc.stdout)) as bitmap:
        return np.asarray(bitmap.convert("RGB")).copy()


def verify_full_page_reference(
    original,
    masked_pdf,
    source_report,
    mask_stats,
    pages,
    max_side=1600,
    minimum_visible_fraction=0.45,
    return_page_rasters=False,
):
    """Historical full-page differential reference, ONLY for benchmark tests.

    Do not use for semantic promotion: independently masked source images
    can overlap and make lower-layer text appear visible incorrectly.

    Check source text is genuinely visible after the PDF paint stack.

    Render original and temporary masked PDF using the same Poppler scale.
    Compare *source-original text box pixels*, excluding unrelated parts of
    the page. A later opaque object hiding an image's label will suppress
    its delta and therefore disqualify that source from automatic promotion.

    The expectation is scaled from the ORIGINAL pixel changes to the PDF
    image footprint, so typography reduced by page sampling gets a
    principled, scale-aware visibility threshold.
    """
    from PIL import Image, ImageDraw
    from pdf_geometry import inverse_display_mapper

    if max_side < 200 or not 0 < minimum_visible_fraction <= 1:
        raise ValueError("invalid visibility verification policy")
    expectations = collections.defaultdict(list)
    for entry in mask_stats.get("visibility_expectations", ()):
        expectations[entry["source_id"]].append(entry)
    appearances = collections.defaultdict(list)
    for placement in source_report.get("placements", ()):
        if (
            placement.get("status") == "recognized_at_native_source_resolution"
            and placement["page"] in pages
            and placement["source_id"] in expectations
        ):
            appearances[placement["page"]].append(placement)
    decisions = []
    rasters = {}
    with pikepdf.Pdf.open(original) as document:
        for page in pages:
            if not appearances.get(page):
                continue
            a = _page_pixels(original, page, max_side)
            b = _page_pixels(masked_pdf, page, max_side)
            if return_page_rasters:
                rasters[page] = (a, b)
            if a.shape != b.shape:
                raise ValueError("inconsistent image mask raster dimensions")
            delta = np.max(np.abs(a.astype(np.int16) - b.astype(np.int16)), axis=2)
            h, w = delta.shape
            inverse, _ = inverse_display_mapper(
                document.pages[page - 1], page_box="mediabox"
            )
            for appearance in appearances[page]:
                image_full_quad = appearance["page_quad"]
                source_id = appearance["source_id"]
                source_width = expectations[source_id][0]["width"]
                source_height = expectations[source_id][0]["height"]
                frame = [
                    (inverse(*pt)[0] * w, inverse(*pt)[1] * h) for pt in image_full_quad
                ]
                pixels_per_source = _polygon_area(frame) / (
                    source_width * source_height
                )
                for line in expectations[source_id]:
                    quad = _pdf_quad_from_source(
                        image_full_quad,
                        line["pixel_quad"],
                        source_width,
                        source_height,
                    )
                    screen = [
                        (inverse(*point)[0] * w, inverse(*point)[1] * h)
                        for point in quad
                    ]
                    x0 = max(0, math.floor(min(p[0] for p in screen)) - 1)
                    x1 = min(w, math.ceil(max(p[0] for p in screen)) + 1)
                    y0 = max(0, math.floor(min(p[1] for p in screen)) - 1)
                    y1 = min(h, math.ceil(max(p[1] for p in screen)) + 1)
                    observed = 0
                    if x0 < x1 and y0 < y1:
                        mask = Image.new("L", (x1 - x0, y1 - y0), 0)
                        ImageDraw.Draw(mask).polygon(
                            [(x - x0, y - y0) for x, y in screen], fill=255
                        )
                        roi = np.asarray(mask) > 0
                        observed = int(
                            np.count_nonzero((delta[y0:y1, x0:x1] > 8) & roi)
                        )
                    expected = float(line["changed_source_pixels"]) * pixels_per_source
                    visible = expected >= 10 and observed >= max(
                        8, minimum_visible_fraction * expected
                    )
                    decisions.append(
                        {
                            "page": page,
                            "source_id": source_id,
                            "text": line["text"],
                            "quad": quad,
                            "expected_changed_output_pixels": round(expected, 1),
                            "actual_changed_output_pixels": observed,
                            "visible": visible,
                        }
                    )
    return (decisions, rasters) if return_page_rasters else decisions


def _convex_quads_intersect(first, second, epsilon=1e-5):
    """Separating-axis test in final-page pixels (not AABB area overlap).

    Two independently masked Image XObject text polygons that overlap
    cannot prove visibility from one simultaneously masked page render:
    the observed delta might belong entirely to the top paint.
    """
    for polygon in (first, second):
        for i in range(len(polygon)):
            p = polygon[i]
            q = polygon[(i + 1) % len(polygon)]
            normal = (p[1] - q[1], q[0] - p[0])
            length = math.hypot(*normal)
            if length <= epsilon:
                continue
            values_a = [normal[0] * x + normal[1] * y for x, y in first]
            values_b = [normal[0] * x + normal[1] * y for x, y in second]
            if (
                min(max(values_a), max(values_b)) - max(min(values_a), min(values_b))
                <= epsilon * length
            ):
                return False
    return True


def _ambiguous_source_text_masks(candidates):
    """Detect source-text polygons whose different paint contributions mix.

    A broadphase sorted by source x-bounds avoids examining distant
    placements. Return both all inter-source polygon collisions and the
    same-XObject subset; the latter require Do-invocation-level proofs
    rather than a single source-object rewrite.
    """
    indices = sorted(range(len(candidates)), key=lambda i: candidates[i]["bounds"][0])
    ambiguous = set()
    same_source_ambiguous = set()
    for at, i in enumerate(indices):
        a = candidates[i]
        ax0, ay0, ax1, ay1 = a["bounds"]
        # No O(N^2) sliced-list copies for many spatially distinct paints.
        for pos in range(at + 1, len(indices)):
            j = indices[pos]
            b = candidates[j]
            bx0, by0, bx1, by1 = b["bounds"]
            if bx0 >= ax1:
                break
            if by0 >= ay1 or by1 <= ay0:
                continue
            if _convex_quads_intersect(a["screen"], b["screen"]):
                ambiguous.update((i, j))
                if a["source_id"] == b["source_id"]:
                    same_source_ambiguous.update((i, j))
    return ambiguous, same_source_ambiguous


def _changed_pixels_inside_source_polygon(original_roi, altered_roi, screen, x0, y0):
    """Actual changed pixels inside the original source text quadrilateral."""
    from PIL import Image, ImageDraw

    if original_roi.shape != altered_roi.shape:
        raise ValueError("source-isolated proof raster shape mismatch")
    h, w = original_roi.shape[:2]
    if h < 1 or w < 1:
        return 0
    delta = np.max(
        np.abs(original_roi.astype(np.int16) - altered_roi.astype(np.int16)),
        axis=2,
    )
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).polygon([(x - x0, y - y0) for x, y in screen], fill=255)
    return int(np.count_nonzero((delta > 8) & (np.asarray(mask) > 0)))


def _counterfactual_pixel_counts(
    counterfactual_pdf,
    candidates,
    indices,
    control_full,
    controls,
    page,
    width,
    height,
    max_side,
):
    """Read exact changed pixels for one temporary source/paint counterfactual.

    Every candidate has one invariant source PDF quad and one transformed
    screen quad. The original control comparison pixels can be shared
    across multiple counterfactuals, each crop planned independently.
    """
    if not indices:
        return {}
    regions = _plan_visibility_crops(
        [candidates[i]["bounds"] for i in indices],
        width,
        height,
    )
    if regions:
        changed = [
            (region, _page_pixels(counterfactual_pdf, page, max_side, crop=region))
            for region in regions
        ]
    else:
        changed = [
            ((0, 0, width, height), _page_pixels(counterfactual_pdf, page, max_side))
        ]
    # All pixels are decoded: there is no reason to retain a full-document
    # counterfactual on disk after the scoped Poppler renders.
    counterfactual_pdf.unlink(missing_ok=True)
    result = {}
    for index in indices:
        item = candidates[index]
        x0, y0, x1, y1 = item["bounds"]
        if x0 >= x1 or y0 >= y1:
            result[index] = 0
            continue
        original_roi = None
        if control_full is not None:
            original_roi = control_full[y0:y1, x0:x1]
        else:
            for region, pixels in controls:
                if (
                    region[0] <= x0
                    and region[1] <= y0
                    and region[2] >= x1
                    and region[3] >= y1
                ):
                    original_roi = pixels[
                        y0 - region[1] : y1 - region[1],
                        x0 - region[0] : x1 - region[0],
                    ]
                    break
        if original_roi is None:
            raise ValueError("counterfactual source text lacks control pixels")
        edited_roi = None
        for region, pixels in changed:
            if (
                region[0] <= x0
                and region[1] <= y0
                and region[2] >= x1
                and region[3] >= y1
            ):
                edited_roi = pixels[
                    y0 - region[1] : y1 - region[1],
                    x0 - region[0] : x1 - region[0],
                ]
                break
        if edited_roi is None:
            raise ValueError("counterfactual crop omitted source text polygon")
        result[index] = _changed_pixels_inside_source_polygon(
            original_roi,
            edited_roi,
            item["screen"],
            x0,
            y0,
        )
    return result


def _isolate_source_visibility(
    candidates,
    ambiguous,
    same_source_ambiguous,
    decisions,
    control_full,
    controls,
    page,
    width,
    height,
    max_side,
    source_pdf,
    source_report,
    min_confidence,
    native_mask_pages,
    isolation_root,
    minimum_visible_fraction,
):
    """Determine the *causal* paint contribution of each overlapping source.

    Modify only a single source image XObject in each temporary proof PDF,
    leaving every other source untouched. A covered lower image then yields
    zero changed output pixels even if an overlapping visible upper image
    contains identical text. Repeated paints of the same source are
    handled afterward by individual content-stream Do aliasing.
    """
    source_indices = collections.defaultdict(list)
    for i in ambiguous:
        source_indices[candidates[i]["source_id"]].append(i)
    for src, indices in source_indices.items():
        eligible_indices = [i for i in indices if i not in same_source_ambiguous]
        if not eligible_indices:
            continue
        isolated_pdf = isolation_root / (
            f"proof-page-{page}-source-{src.replace(':', '-')}.pdf"
        )
        isolated_stats = prepare_render_image_pdf(
            source_pdf,
            isolated_pdf,
            source_report,
            [page],
            min_confidence=min_confidence,
            native_mask_pages=native_mask_pages,
            source_ids={src},
        )
        if isolated_stats.get("source_images_masked", 0) != 1:
            continue
        observed_by_index = _counterfactual_pixel_counts(
            isolated_pdf,
            candidates,
            eligible_indices,
            control_full,
            controls,
            page,
            width,
            height,
            max_side,
        )
        for i, observed in observed_by_index.items():
            expected = candidates[i]["expected"]
            decisions[i]["actual_changed_output_pixels"] = observed
            decisions[i]["visible"] = expected >= 10 and observed >= max(
                8, minimum_visible_fraction * expected
            )
            decisions[i]["isolated_source_proof"] = True


def _isolate_paint_visibility(
    candidates,
    same_source_ambiguous,
    decisions,
    control_full,
    controls,
    page,
    width,
    height,
    max_side,
    control_pdf,
    masked_pdf,
    source_report,
    isolation_root,
    minimum_visible_fraction,
):
    """Resolve reused Image XObject occurrences by changing a single Do paint.

    Clone only the page/Form invocation path leading to the exact expanded
    paint index; all sibling uses and source images stay intact. Unlike
    source-wide masks, this identifies the visible upper occurrence when
    the same XObject was painted twice at identical PDF coordinates.
    """
    from mask_paint_invocations import rewrite_masked_paints

    paint_indices = collections.defaultdict(list)
    for i in same_source_ambiguous:
        paint_indices[candidates[i]["paint_index"]].append(i)
    for paint_index, indices in paint_indices.items():
        counterfactual = isolation_root / (f"proof-page-{page}-paint-{paint_index}.pdf")
        rewrite_masked_paints(
            control_pdf,
            masked_pdf,
            source_report,
            page,
            [paint_index],
            counterfactual,
        )
        observed_by_index = _counterfactual_pixel_counts(
            counterfactual,
            candidates,
            indices,
            control_full,
            controls,
            page,
            width,
            height,
            max_side,
        )
        for i, observed in observed_by_index.items():
            expected = candidates[i]["expected"]
            decisions[i]["actual_changed_output_pixels"] = observed
            decisions[i]["visible"] = expected >= 10 and observed >= max(
                8, minimum_visible_fraction * expected
            )
            decisions[i]["isolated_paint_proof"] = True


def verify_rendered_image_text(
    original,
    masked_pdf,
    source_report,
    mask_stats,
    pages,
    max_side=1600,
    minimum_visible_fraction=0.45,
    return_page_rasters=False,
    isolation_original=None,
    isolation_root=None,
    native_mask_pages=None,
    source_min_confidence=0.97,
):
    """Prove image-text visibility using one full treatment + cropped controls.

    The full *treatment* page is already needed for subsequent page OCR.
    Restrict control rendering to candidate source-text footprints, using
    exact Poppler pixel-coordinate crops at the identical page scale.
    On a failed proof, render the original full control only if the caller
    needs its raster for ordinary page OCR.

    Differential proof is based on final page painting, not on the
    presence of text in an embedded Image XObject. Distinct image sources
    use source-isolated counterfactuals; repeated uses of the same source
    use paint-indexed counterfactuals with cloned shared Form paths.
    """
    from PIL import Image, ImageDraw
    from pdf_geometry import inverse_display_mapper

    if max_side < 200 or not 0 < minimum_visible_fraction <= 1:
        raise ValueError("invalid image visibility verification thresholds")
    expectations = collections.defaultdict(list)
    for item in mask_stats.get("visibility_expectations", ()):
        expectations[item["source_id"]].append(item)
    by_page = collections.defaultdict(list)
    selected = set(pages)
    for placement in source_report.get("placements", ()):
        if (
            placement["page"] in selected
            and placement.get("status") == "recognized_at_native_source_resolution"
            and placement["source_id"] in expectations
        ):
            by_page[placement["page"]].append(placement)

    decisions = []
    rasters = {}
    with pikepdf.Pdf.open(original) as doc:
        for page in pages:
            if not by_page[page]:
                continue
            treatment = _page_pixels(masked_pdf, page, max_side)
            height, width = treatment.shape[:2]
            # Poppler defaults to /MediaBox, not /CropBox. Coordinate
            # mapping must match the rendered frame even on PDFs whose
            # boxes differ or whose pages are rotated.
            inverse, _ = inverse_display_mapper(
                doc.pages[page - 1], page_box="mediabox"
            )
            candidates = []
            for placement in by_page[page]:
                image_quad = placement["page_quad"]
                source_id = placement["source_id"]
                paint_index = placement["paint_index"]
                source_width = expectations[source_id][0]["width"]
                source_height = expectations[source_id][0]["height"]
                frame = [
                    (inverse(*point)[0] * width, inverse(*point)[1] * height)
                    for point in image_quad
                ]
                pixels_per_source = _polygon_area(frame) / (
                    source_width * source_height
                )
                for line in expectations[source_id]:
                    pdf_quad = _pdf_quad_from_source(
                        image_quad,
                        line["pixel_quad"],
                        source_width,
                        source_height,
                    )
                    screen = [
                        (inverse(*point)[0] * width, inverse(*point)[1] * height)
                        for point in pdf_quad
                    ]
                    x0 = max(0, math.floor(min(point[0] for point in screen)) - 1)
                    x1 = min(width, math.ceil(max(point[0] for point in screen)) + 1)
                    y0 = max(0, math.floor(min(point[1] for point in screen)) - 1)
                    y1 = min(height, math.ceil(max(point[1] for point in screen)) + 1)
                    candidates.append(
                        {
                            "page": page,
                            "source_id": source_id,
                            "paint_index": paint_index,
                            "line": line,
                            "quad": pdf_quad,
                            "screen": screen,
                            "bounds": (x0, y0, x1, y1),
                            "expected": float(line["changed_source_pixels"])
                            * pixels_per_source,
                        }
                    )
            ambiguous, same_source_ambiguous = _ambiguous_source_text_masks(candidates)
            regions = _plan_visibility_crops(
                [item["bounds"] for item in candidates],
                width,
                height,
            )
            control_full = None
            controls = []
            if regions:
                for region in regions:
                    cropped = _page_pixels(original, page, max_side, crop=region)
                    h = region[3] - region[1]
                    w = region[2] - region[0]
                    if cropped.shape != (h, w, 3):
                        raise ValueError("Poppler comparison crop dimensions disagree")
                    controls.append((region, cropped))
                mode = "cropped"
            else:
                control_full = _page_pixels(original, page, max_side)
                if control_full.shape != treatment.shape:
                    raise ValueError("inconsistent PDF render dimensions")
                mode = "full"

            page_decisions = []
            for index, item in enumerate(candidates):
                x0, y0, x1, y1 = item["bounds"]
                observed = 0
                if x0 < x1 and y0 < y1:
                    if control_full is not None:
                        original_roi = control_full[y0:y1, x0:x1]
                    else:
                        original_roi = None
                        for region, cropped in controls:
                            if (
                                region[0] <= x0
                                and region[1] <= y0
                                and region[2] >= x1
                                and region[3] >= y1
                            ):
                                original_roi = cropped[
                                    y0 - region[1] : y1 - region[1],
                                    x0 - region[0] : x1 - region[0],
                                ]
                                break
                        if original_roi is None:
                            raise ValueError("source text omitted from planned crop")
                    current_roi = treatment[y0:y1, x0:x1]
                    diff = np.max(
                        np.abs(
                            original_roi.astype(np.int16) - current_roi.astype(np.int16)
                        ),
                        axis=2,
                    )
                    mask = Image.new("L", (x1 - x0, y1 - y0), 0)
                    ImageDraw.Draw(mask).polygon(
                        [(x - x0, y - y0) for x, y in item["screen"]],
                        fill=255,
                    )
                    observed = int(
                        np.count_nonzero((diff > 8) & (np.asarray(mask) > 0))
                    )
                expected = item["expected"]
                visible = (
                    index not in ambiguous
                    and expected >= 10
                    and observed >= max(8, minimum_visible_fraction * expected)
                )
                page_decisions.append(
                    {
                        "page": page,
                        "source_id": item["source_id"],
                        "paint_index": item["paint_index"],
                        "text": item["line"]["text"],
                        "quad": item["quad"],
                        "expected_changed_output_pixels": round(expected, 1),
                        "actual_changed_output_pixels": observed,
                        "visible": visible,
                        "comparison_mode": mode,
                        "ambiguous_source_mask_overlap": index in ambiguous,
                        "control_regions": len(controls) if controls else 1,
                        "control_pixel_fraction": round(
                            sum((r[2] - r[0]) * (r[3] - r[1]) for r in regions)
                            / (width * height)
                            if regions
                            else 1.0,
                            4,
                        ),
                    }
                )
            if ambiguous and isolation_original is not None:
                if isolation_root is None:
                    raise ValueError("isolated source proofs require a work directory")
                _isolate_source_visibility(
                    candidates,
                    ambiguous,
                    same_source_ambiguous,
                    page_decisions,
                    control_full,
                    controls,
                    page,
                    width,
                    height,
                    max_side,
                    isolation_original,
                    source_report,
                    source_min_confidence,
                    native_mask_pages,
                    isolation_root,
                    minimum_visible_fraction,
                )
            if same_source_ambiguous:
                if isolation_root is None:
                    raise ValueError("paint-isolated proof needs a work directory")
                _isolate_paint_visibility(
                    candidates,
                    same_source_ambiguous,
                    page_decisions,
                    control_full,
                    controls,
                    page,
                    width,
                    height,
                    max_side,
                    original,
                    masked_pdf,
                    source_report,
                    isolation_root,
                    minimum_visible_fraction,
                )

            # Source text can be suppressed per individual paint: every
            # recognized word in an appearance must have passed its own
            # visibility test, not merely some other occurrence of the
            # same XObject or another image at matching PDF coordinates.
            paint_outcomes = collections.defaultdict(list)
            for decision in page_decisions:
                paint_outcomes[decision["paint_index"]].append(decision["visible"])
            accepted_paints = {
                index
                for index, outcomes in paint_outcomes.items()
                if outcomes and all(outcomes)
            }
            for decision in page_decisions:
                decision["visible"] = (
                    decision["visible"] and decision["paint_index"] in accepted_paints
                )
            all_paints = set(paint_outcomes)
            selected_pdf = original
            if accepted_paints == all_paints:
                chosen = treatment
                selected_pdf = masked_pdf
            elif accepted_paints and isolation_root is not None:
                from mask_paint_invocations import rewrite_masked_paints

                selected_pdf = isolation_root / f"proof-page-{page}-selected.pdf"
                rewrite_masked_paints(
                    original,
                    masked_pdf,
                    source_report,
                    page,
                    accepted_paints,
                    selected_pdf,
                )
                chosen = _page_pixels(selected_pdf, page, max_side)
                if chosen.shape != treatment.shape:
                    raise ValueError("selected paint raster geometry mismatch")
            else:
                accepted_paints.clear()
                # No selective rewrite can be constructed without a
                # work directory. Fall back to unmasked page OCR.
                for decision in page_decisions:
                    decision["visible"] = False
                if control_full is None and return_page_rasters:
                    control_full = _page_pixels(original, page, max_side)
                chosen = control_full
            for decision in page_decisions:
                decision["applied_mask_paint_indices"] = sorted(accepted_paints)
                decision["applied_mask_source_ids"] = sorted(
                    {
                        c["source_id"]
                        for c in candidates
                        if c["paint_index"] in accepted_paints
                    }
                )
                decision["selected_mask_pdf"] = str(selected_pdf)
            decisions.extend(page_decisions)
            if return_page_rasters:
                rasters[page] = (control_full, chosen)
    return (decisions, rasters) if return_page_rasters else decisions
