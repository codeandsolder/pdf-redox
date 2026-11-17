# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""Create a render-only PDF copy with native font text hidden.

Preserves Type3 glyphs, including outlined glyphs from pdf-redox. Existing
native Tj/TJ/'/" text-show operations advance exactly as before, but have
rendering mode 3 (invisible). The source PDF is never modified.

All page streams and recursively referenced Form XObjects are handled;
a form using inherited resources is processed in its page context.
"""

from __future__ import annotations

import argparse
import collections
import pathlib

import pikepdf
from pikepdf import ContentStreamInstruction as Instruction, Operator


SHOW = {"Tj", "TJ", "'", '"'}
TEXT_STATE = "Tr"


def inherited_page_resources(page):
    node = page.obj
    while node is not None:
        resources = node.get("/Resources")
        if resources is not None:
            return resources
        node = node.get("/Parent")
    return pikepdf.Dictionary()


def font_types(resources):
    """Classify fonts by render type and reliable Unicode extraction.

    A normal PDF font can also contain unusable glyph codes. Do *not* mask
    it unless its ToUnicode CMap or standard encoding supports extraction.
    Preserve all Type3 fonts: outlined-shape reuse and graphical Type3 fonts
    must remain visible for OCR. Native searchable Type3 text is still
    excluded by the final native-word overlap test.
    """
    fonts = resources.get("/Font", pikepdf.Dictionary())
    result = {}
    for name, data in fonts.items():
        try:
            subtype = str(data.get("/Subtype", ""))
            to_unicode = data.get("/ToUnicode") is not None
            encoding = data.get("/Encoding")
            known_encoding = (
                encoding
                in (
                    pikepdf.Name("/WinAnsiEncoding"),
                    pikepdf.Name("/MacRomanEncoding"),
                    pikepdf.Name("/StandardEncoding"),
                )
                if isinstance(encoding, pikepdf.Name)
                else False
            )
            base = str(data.get("/BaseFont", ""))
            base_name = base.removeprefix("/").split("+")[-1]
            base14 = base_name in {
                "Courier",
                "Courier-Bold",
                "Courier-Oblique",
                "Courier-BoldOblique",
                "Helvetica",
                "Helvetica-Bold",
                "Helvetica-Oblique",
                "Helvetica-BoldOblique",
                "Times-Roman",
                "Times-Bold",
                "Times-Italic",
                "Times-BoldItalic",
            }
            default_encoding = subtype == "/Type1" and base14 and encoding is None
            result[str(name)] = (
                subtype,
                subtype != "/Type3"
                and (to_unicode or known_encoding or default_encoding),
            )
        except (TypeError, ValueError, AttributeError):
            result[str(name)] = ("", False)
    return result


def rewrite_content(instructions, font_map):
    output = []
    font = None
    render_mode = 0
    state = []
    counts = collections.Counter()

    for ins in instructions:
        op = str(ins.operator)
        if op == "q":
            state.append((font, render_mode))
        elif op == "Q":
            if state:
                font, render_mode = state.pop()
        elif op == "Tf" and len(ins.operands) >= 1:
            font = str(ins.operands[0])
        elif op == TEXT_STATE and ins.operands:
            render_mode = int(ins.operands[0])

        if op in SHOW:
            subtype, searchable = font_map.get(font, ("", False))
            if searchable:
                counts["native_text_show"] += 1
                # Rendering modes 4..7 also establish text clipping paths.
                # Suppressing them would alter the graphics in OCR rasters.
                # Mode 3 is already invisible and requires no modification.
                if render_mode >= 4:
                    counts["preserved_text_clipping"] += 1
                elif render_mode != 3:
                    output.append(Instruction([3], Operator("Tr")))
                    output.append(ins)
                    output.append(Instruction([render_mode], Operator("Tr")))
                    counts["hidden_native_text_show"] += 1
                    continue
            elif subtype == "/Type3":
                counts["preserved_type3_text_show"] += 1
            else:
                counts["unclassified_text_show"] += 1
        output.append(ins)
    return output, counts


def rewrite_form(form, inherited_resources, seen, stats):
    if form.objgen in seen:
        return
    seen.add(form.objgen)
    resources = form.get("/Resources") or inherited_resources
    types = font_types(resources)
    instructions = pikepdf.parse_content_stream(form)
    rewrite, counts = rewrite_content(instructions, types)
    stats.update(counts)
    if counts["hidden_native_text_show"]:
        form.write(pikepdf.unparse_content_stream(rewrite))
    for obj in resources.get("/XObject", pikepdf.Dictionary()).values():
        if str(obj.get("/Subtype", "")) == "/Form":
            rewrite_form(obj, resources, seen, stats)


def mask_document(doc, pages=None):
    """Hide native PDF text in an already-open document, retaining object IDs.

    A caller can save the native-only control rendering, mask source-image
    lettering in that same document, and save a treatment rendering. Their
    difference then proves image-text visibility with *identical* native text
    masking, without an extra page render for the subsequent OCR.
    """
    totals = collections.Counter()
    seen_forms = set()
    selected = set(range(1, len(doc.pages) + 1)) if pages is None else set(pages)
    if not selected or any(n < 1 or n > len(doc.pages) for n in selected):
        raise ValueError("invalid page selection")
    for page_no in sorted(selected):
        page = doc.pages[page_no - 1]
        resources = inherited_page_resources(page)
        types = font_types(resources)
        content = pikepdf.parse_content_stream(page)
        rewrite, counts = rewrite_content(content, types)
        totals.update(counts)
        if counts["hidden_native_text_show"]:
            page.Contents = pikepdf.Stream(doc, pikepdf.unparse_content_stream(rewrite))
        for obj in resources.get("/XObject", pikepdf.Dictionary()).values():
            if str(obj.get("/Subtype", "")) == "/Form":
                rewrite_form(obj, resources, seen_forms, totals)
    return dict(totals)


def mask_pdf(input_path, output_path, pages=None):
    """Hide native text in a render-only PDF copy, leaving Type3 ink."""
    with pikepdf.Pdf.open(input_path) as doc:
        totals = mask_document(doc, pages)
        doc.save(output_path, compress_streams=True)
    return totals


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("pdf", type=pathlib.Path)
    p.add_argument("output", type=pathlib.Path)
    p.add_argument(
        "--pages", help="one-based page numbers, separated by commas; default all"
    )
    a = p.parse_args()
    pages = [int(x) for x in a.pages.split(",")] if a.pages else None
    print(mask_pdf(a.pdf, a.output, pages=pages))


if __name__ == "__main__":
    main()
