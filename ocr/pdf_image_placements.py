# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10"]
# ///
"""Locate actual PDF raster paints with native-pixel -> PDF-page transforms.

This is a PDF content-stream interpreter for the relevant graphics state,
not a list of all image resources. It honors q/Q, cm, nested Form Matrix/BBox,
CropBox clipping and simple rectangle clipping. Unknown clipping/graphics
state makes that appearance non-OCRable (page fallback).
"""

from dataclasses import dataclass, field
import math

import pikepdf
from mask_native_text import font_types, inherited_page_resources

IDENTITY = (1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
PATH_PAINT = {"S", "s", "f", "F", "f*", "B", "B*", "b", "b*"}
PATH_OPS = {"m", "l", "c", "v", "y", "h", "re"}
SHOW = {"Tj", "TJ", "'", '"'}
# Supported no-paint/graphics operators. Anything else is a fallback.
SAFE = {
    "w",
    "J",
    "j",
    "M",
    "d",
    "ri",
    "i",
    "g",
    "G",
    "rg",
    "RG",
    "k",
    "K",
    "cs",
    "CS",
    "sc",
    "SC",
    "scn",
    "SCN",
    "Tc",
    "Tw",
    "Tz",
    "TL",
    "Ts",
    "Td",
    "TD",
    "Tm",
    "T*",
    "BT",
    "ET",
    "Tf",
    "Tr",
    "BMC",
    "BDC",
    "EMC",
    "MP",
    "DP",
}


def matrix(a):
    if len(a) != 6:
        raise ValueError("PDF transformation requires exactly six coefficients")
    result = tuple(float(x) for x in a)
    if not all(math.isfinite(x) for x in result):
        raise ValueError("nonfinite PDF transformation")
    return result


def concat(outer, inner):
    """Column-vector affine transform: CTM × new cm/Form Matrix."""
    a, b, c, d, e, f = outer
    A, B, C, D, E_, F = inner
    return (
        a * A + c * B,
        b * A + d * B,
        a * C + c * D,
        b * C + d * D,
        a * E_ + c * F + e,
        b * E_ + d * F + f,
    )


def transform(m, x, y):
    a, b, c, d, e, f = m
    return [a * x + c * y + e, b * x + d * y + f]


def rect_quad(m, rect):
    x0, y0, x1, y1 = map(float, rect)
    if x1 <= x0 or y1 <= y0:
        raise ValueError("invalid PDF rectangular clipping box")
    return [
        transform(m, x0, y0),
        transform(m, x1, y0),
        transform(m, x1, y1),
        transform(m, x0, y1),
    ]


def rectangle_path_quad(points):
    """Recognize an affine rectangle drawn by PDF m/l/h operators.

    Uses the path geometry after the CTM, so rotation and skewed page
    coordinates work without imposing page-axis-aligned rectangles.
    """
    cleaned = []
    for p in points:
        p = [float(v) for v in p]
        if not cleaned or math.dist(p, cleaned[-1]) > 1e-5:
            cleaned.append(p)
    if len(cleaned) == 5 and math.dist(cleaned[0], cleaned[-1]) < 1e-4:
        cleaned.pop()
    if len(cleaned) != 4:
        return None
    edges = [
        (
            cleaned[(i + 1) % 4][0] - cleaned[i][0],
            cleaned[(i + 1) % 4][1] - cleaned[i][1],
        )
        for i in range(4)
    ]
    lengths = [math.hypot(*x) for x in edges]
    if min(lengths) < 1e-4:
        return None
    for i in range(4):
        a = edges[i]
        b = edges[(i + 1) % 4]
        if abs(a[0] * b[0] + a[1] * b[1]) > 0.025 * lengths[i] * lengths[(i + 1) % 4]:
            return None
    if math.dist(
        cleaned[0],
        [
            (cleaned[1][0] + cleaned[3][0] - cleaned[2][0]),
            (cleaned[1][1] + cleaned[3][1] - cleaned[2][1]),
        ],
    ) > 0.02 * max(lengths):
        return None
    return cleaned


def inside_convex(point, polygon, eps=1e-5):
    signs = []
    for i in range(len(polygon)):
        a = polygon[i]
        b = polygon[(i + 1) % len(polygon)]
        cross = (b[0] - a[0]) * (point[1] - a[1]) - (b[1] - a[1]) * (point[0] - a[0])
        signs.append(cross)
    return min(signs) >= -eps or max(signs) <= eps


def image_quad(placement, pixels_quad):
    """Image top-left image-pixel OCR quad TL TR BR BL -> PDF BL BR TR TL."""
    m = placement["ctm"]
    width = placement["width"]
    height = placement["height"]
    if len(pixels_quad) != 4:
        raise ValueError("character OCR quad must have four corners")

    def point(pixel):
        x, y = map(float, pixel)
        if not all(math.isfinite(v) for v in (x, y)):
            raise ValueError("nonfinite image-space OCR point")
        return transform(m, x / width, 1.0 - y / height)

    return [point(pixels_quad[i]) for i in (3, 2, 1, 0)]


def word_visible(quad, placement):
    return all(
        inside_convex(point, clip, eps=1e-3)
        for clip in placement["clip_polygons"]
        for point in quad
    )


@dataclass
class State:
    ctm: tuple = IDENTITY
    clips: list = field(default_factory=list)
    clip_unknown: bool = False
    gs_unsafe: bool = False
    font: str | None = None
    render_mode: int = 0
    text_open: bool = False

    def duplicate(self):
        return State(
            self.ctm,
            self.clips.copy(),
            self.clip_unknown,
            self.gs_unsafe,
            self.font,
            self.render_mode,
            self.text_open,
        )


def check_extgs(ext):
    """Only known regular opaque compositing can be reconstructed standalone."""
    for field_name in ("/SMask", "/BM"):
        x = ext.get(field_name)
        if x is not None:
            if field_name == "/SMask" and str(x) == "/None":
                continue
            if field_name == "/BM" and str(x) == "/Normal":
                continue
            return False
    for name in ("/ca", "/CA"):
        if ext.get(name) is not None and abs(float(ext[name]) - 1) > 1e-6:
            return False
    # A common PDF graphics state explicitly declares disabled overprint
    # and alpha-is-shape=false. Those defaults do not alter isolated image
    # decoding. Enabled overprint, transfer or halftone changes do.
    for key in ("/OP", "/op", "/AIS"):
        if bool(ext.get(key, False)):
            return False
    for key in ("/TR", "/TR2"):
        if key in ext and str(ext[key]) not in ("/Identity", "/Default"):
            return False
    if "/HT" in ext and str(ext["/HT"]) not in ("/Default",):
        return False
    return True


def _normalize_resources(page_or_form, inherited):
    resources = page_or_form.get("/Resources")
    return resources if resources is not None else inherited


def _visit(container, resources, state, form_stack, stats, placements, images, depth=0):
    if depth > 32:
        stats["unsafe_form_recursion"] += 1
        return
    saved = []
    path_rect = []
    path_subpaths = []
    path_current = None
    path_other = False
    pending_clip = False
    # The q/Q stack is local to this Form or page.
    for instruction in pikepdf.parse_content_stream(container):
        op = str(instruction.operator)
        values = instruction.operands
        if op == "q":
            saved.append(state.duplicate())
        elif op == "Q":
            if not saved:
                stats["unbalanced_q"] += 1
                state.clip_unknown = True
            else:
                state = saved.pop()
        elif op == "cm":
            state.ctm = concat(state.ctm, matrix(values))
        elif op == "re":
            if len(values) != 4:
                path_other = True
            else:
                x, y, w, h = map(float, values)
                if abs(w) < 1e-8 or abs(h) < 1e-8:
                    path_other = True
                else:
                    path_rect.append(
                        rect_quad(
                            state.ctm,
                            (
                                min(x, x + w),
                                min(y, y + h),
                                max(x, x + w),
                                max(y, y + h),
                            ),
                        )
                    )
        elif op == "m":
            if len(values) != 2:
                path_other = True
            else:
                path_current = [
                    transform(state.ctm, float(values[0]), float(values[1]))
                ]
                path_subpaths.append(path_current)
        elif op == "l":
            if len(values) != 2 or path_current is None:
                path_other = True
            else:
                path_current.append(
                    transform(state.ctm, float(values[0]), float(values[1]))
                )
        elif op == "h":
            if path_current is None:
                path_other = True
            else:
                path_current.append(path_current[0])
        elif op in ("c", "v", "y"):
            path_other = True
        elif op in ("W", "W*"):
            pending_clip = True
        elif op == "n" or op in PATH_PAINT:
            if pending_clip:
                if not path_other and len(path_rect) == 1 and not path_subpaths:
                    state.clips.append(path_rect[0])
                elif not path_other and len(path_subpaths) == 1 and not path_rect:
                    clip = rectangle_path_quad(path_subpaths[0])
                    if clip is not None:
                        state.clips.append(clip)
                    else:
                        state.clip_unknown = True
                else:
                    state.clip_unknown = True
                pending_clip = False
            if op in PATH_PAINT:
                stats["non_image_visual_paint"] += 1
            path_rect = []
            path_subpaths = []
            path_current = None
            path_other = False
        elif op == "gs":
            xstate = (
                resources.get("/ExtGState", pikepdf.Dictionary()).get(values[0])
                if values
                else None
            )
            if xstate is None or not check_extgs(xstate):
                state.gs_unsafe = True
                stats["unsupported_extgs"] += 1
        elif op == "Tf":
            state.font = str(values[0]) if values else None
        elif op == "Tr":
            if not values:
                state.gs_unsafe = True
            else:
                state.render_mode = int(values[0])
        elif op in SHOW:
            if state.render_mode not in (3, 7):
                fonts = font_types(resources)
                is_native = state.font in fonts and fonts[state.font][1]
                if is_native:
                    stats["native_searchable_shows"] += 1
                else:
                    stats["non_image_visual_paint"] += 1
            if state.render_mode >= 4:
                stats["non_image_visual_paint"] += 1
        elif op == "sh":
            stats["non_image_visual_paint"] += 1
        elif op == "Do":
            if len(values) != 1:
                stats["unknown_paint"] += 1
                continue
            name = values[0]
            target = resources.get("/XObject", pikepdf.Dictionary()).get(name)
            if target is None:
                stats["unknown_paint"] += 1
                continue
            typ = str(target.get("/Subtype", ""))
            if typ == "/Form":
                id_ = target.objgen
                if id_ in form_stack or depth >= 32:
                    stats["unsafe_form_recursion"] += 1
                    continue
                child = state.duplicate()
                child.ctm = concat(child.ctm, matrix(target.get("/Matrix", IDENTITY)))
                try:
                    child.clips.append(rect_quad(child.ctm, target["/BBox"]))
                except (ValueError, KeyError, TypeError):
                    child.clip_unknown = True
                if target.get("/Group") is not None:
                    child.gs_unsafe = True
                child_resource = _normalize_resources(target, resources)
                stats["forms_visited"] += 1
                _visit(
                    target,
                    child_resource,
                    child,
                    (*form_stack, id_),
                    stats,
                    placements,
                    images,
                    depth + 1,
                )
            elif typ == "/Image":
                stats["image_paints"] += 1
                paint_index = stats["image_paints"] - 1
                width = int(target.get("/Width", 0))
                height = int(target.get("/Height", 0))
                determinant = state.ctm[0] * state.ctm[3] - state.ctm[1] * state.ctm[2]
                reasons = []
                if not (0 < width <= 100000 and 0 < height <= 100000):
                    reasons.append("invalid_source_dimensions")
                if not math.isfinite(determinant) or abs(determinant) < 1e-9:
                    reasons.append("degenerate_image_placement")
                if bool(target.get("/ImageMask", False)):
                    reasons.append("image_stencil_mask")
                if state.clip_unknown:
                    reasons.append("nonrectangular_or_untracked_clip")
                if state.gs_unsafe:
                    reasons.append("unsupported_transparency_or_graphics_state")
                source_id = list(target.objgen)
                if source_id == [0, 0]:
                    key = "direct:{}:{}".format(stats["page"], stats["image_paints"])
                else:
                    key = f"{source_id[0]}:{source_id[1]}"
                # A single XObject can be painted many times, in different
                # pages/forms/transformations. OCR its image bytes only once.
                images[key] = target
                placements.append(
                    {
                        "page": stats["page"],
                        "paint_index": paint_index,
                        "source_id": key,
                        "resource": str(name),
                        "ctm": list(state.ctm),
                        "width": width,
                        "height": height,
                        "clip_polygons": [*state.clips],
                        "unsafe_reasons": reasons,
                    }
                )
                if reasons:
                    stats["unsafe_image_paints"] += 1
            else:
                stats["unknown_paint"] += 1
        elif op in SAFE:
            pass
        elif op in ("BI", "ID", "EI"):
            stats["unsupported_inline_images"] += 1
        else:
            stats["unknown_paint"] += 1
    if saved or pending_clip:
        stats["unbalanced_stream"] += 1


def enumerate_image_paints(doc, selected_pages):
    """Return all image appearances and XObject dict, not all resource definitions."""
    placements = []
    images = {}
    summaries = []
    for page_number in selected_pages:
        if page_number < 1 or page_number > len(doc.pages):
            raise ValueError("PDF page number outside source")
        page = doc.pages[page_number - 1]
        x0, y0, x1, y1 = map(float, page.cropbox)
        if x1 <= x0 or y1 <= y0:
            raise ValueError("bad PDF CropBox")
        stats = {
            "page": page_number,
            "image_paints": 0,
            "unsafe_image_paints": 0,
            "forms_visited": 0,
            "native_searchable_shows": 0,
            "non_image_visual_paint": 0,
            "unknown_paint": 0,
            "unsupported_extgs": 0,
            "unsupported_inline_images": 0,
            "unsafe_form_recursion": 0,
            "unbalanced_q": 0,
            "unbalanced_stream": 0,
        }
        res = inherited_page_resources(page)
        state = State(clips=[rect_quad(IDENTITY, (x0, y0, x1, y1))])
        _visit(page, res, state, (), stats, placements, images)
        if page.obj.get("/Annots") is not None and len(page.obj["/Annots"]):
            stats["unknown_paint"] += 1
            stats["annotations"] = len(page.obj["/Annots"])
        stats["image_only_after_native"] = bool(
            stats["image_paints"]
            and not any(
                stats[x]
                for x in (
                    "unsafe_image_paints",
                    "non_image_visual_paint",
                    "unknown_paint",
                    "unsupported_inline_images",
                    "unsafe_form_recursion",
                    "unbalanced_q",
                    "unbalanced_stream",
                )
            )
        )
        summaries.append(stats)
    return placements, images, summaries
