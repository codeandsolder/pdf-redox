# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""Provable OCR bypass for PDF pages containing only native searchable text.

This is a *render-closure* check, not a native-word-density heuristic:
walk the complete page instruction stream and recursively invoked Form
XObjects. Any image, outlined path painting, shading, pattern XObject,
unknown paint operation, text clipping, unsafe font or annotation causes a
normal OCR fallback. The fast path does not inspect raster pixels and never
claims to recognize a graphic as text.
"""

from dataclasses import dataclass
import pikepdf
from mask_native_text import font_types, inherited_page_resources


TEXT_SHOW = {"Tj", "TJ", "'", '"'}
TEXT_STATE = {
    "BT",
    "ET",
    "Tf",
    "Tm",
    "Td",
    "TD",
    "T*",
    "Tc",
    "Tw",
    "Tz",
    "TL",
    "Ts",
    "Tr",
}
GRAPHICS_STATE = {
    "q",
    "Q",
    "cm",
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
}
MARKED = {"BMC", "BDC", "EMC", "MP", "DP"}
UNPAINTED_PATH = {"m", "l", "c", "v", "y", "h", "re", "n"}
# Even one painted glyph outline could be text invisible to a native CMap.
UNSAFE_PAINT = {
    "S",
    "s",
    "f",
    "F",
    "f*",
    "B",
    "B*",
    "b",
    "b*",
    "sh",
    "W",
    "W*",
    "BI",
    "ID",
    "EI",
    "gs",
    "d0",
    "d1",
}


@dataclass
class Result:
    eligible: bool = True
    reason: str = "all_visible_text_native_searchable"
    native_shows: int = 0
    invisible_shows: int = 0
    forms_checked: int = 0
    operators_checked: int = 0

    def reject(self, reason):
        self.eligible = False
        self.reason = reason
        return self


def inspect_stream(stream, resources, result, stack, depth=0):
    if depth > 32:
        return result.reject("form_recursion_depth_exceeded")
    # A form may occur more than once with different inherited resources.
    # Only detect cycles on the active stack; no unsafe global cache.
    oid = getattr(stream, "objgen", None)
    if oid is not None and oid in stack and oid != (0, 0):
        return result.reject("cyclic_form_reference")
    newstack = (*stack, oid)
    fonts = font_types(resources)
    graphics = []
    font = None
    mode = 0
    in_text = False
    markers = 0
    for ins in pikepdf.parse_content_stream(stream):
        op = str(ins.operator)
        vals = ins.operands
        result.operators_checked += 1
        if op in UNSAFE_PAINT:
            return result.reject("non_native_paint_" + op)
        if op == "q":
            graphics.append((font, mode))
        elif op == "Q":
            if not graphics:
                return result.reject("unbalanced_graphics_state")
            font, mode = graphics.pop()
        elif op == "BT":
            if in_text:
                return result.reject("nested_text_object")
            in_text = True
        elif op == "ET":
            if not in_text:
                return result.reject("unbalanced_text_object")
            in_text = False
        elif op == "Tf":
            if len(vals) < 2:
                return result.reject("malformed_font_selection")
            font = str(vals[0])
        elif op == "Tr":
            if len(vals) != 1:
                return result.reject("malformed_render_mode")
            try:
                mode = int(vals[0])
            except (ValueError, TypeError):
                return result.reject("invalid_render_mode")
            if not 0 <= mode <= 7:
                return result.reject("unknown_render_mode")
        elif op in TEXT_SHOW:
            if not in_text:
                return result.reject("text_show_outside_text_object")
            if mode == 3:
                result.invisible_shows += 1
            elif mode >= 4:
                return result.reject("text_clipping_mode")
            elif font not in fonts or not fonts[font][1]:
                return result.reject("non_searchable_visible_font")
            else:
                result.native_shows += 1
        elif op == "BMC" or op == "BDC":
            markers += 1
        elif op == "EMC":
            markers -= 1
            if markers < 0:
                return result.reject("unbalanced_marked_content")
        elif op == "Do":
            if len(vals) != 1:
                return result.reject("ambiguous_xobject_invocation")
            name = vals[0]
            xobjects = resources.get("/XObject", pikepdf.Dictionary())
            target = xobjects.get(name)
            if target is None:
                return result.reject("missing_xobject")
            if str(target.get("/Subtype", "")) != "/Form":
                return result.reject("image_or_unknown_xobject")
            result.forms_checked += 1
            child_resources = target.get("/Resources") or resources
            inspect_stream(target, child_resources, result, newstack, depth + 1)
            if not result.eligible:
                return result
        elif (
            op in GRAPHICS_STATE
            or op in TEXT_STATE
            or op in MARKED
            or op in UNPAINTED_PATH
        ):
            pass
        else:
            return result.reject("unknown_pdf_paint_operator_" + op)
    if graphics or in_text or markers:
        return result.reject("unbalanced_pdf_content_state")
    return result


def page_is_native_only(page):
    """Check one pikepdf page; reject on uncertainty, never infer from word count."""
    result = Result()
    try:
        annotations = page.obj.get("/Annots")
        if annotations is not None and len(annotations) > 0:
            return result.reject("visible_or_unknown_annotation_appearance")
        resources = inherited_page_resources(page)
        return inspect_stream(page, resources, result, ())
    except (pikepdf.PdfError, ValueError, TypeError, KeyError, AttributeError) as error:
        return result.reject(
            "malformed_or_unsupported_pdf_content_" + type(error).__name__
        )
