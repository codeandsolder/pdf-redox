# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10"]
# ///
"""Change one or more expanded PDF Image XObject paints without altering siblings.

The input is a *temporary* PDF whose native searchable text is already hidden.
A different temporary PDF provides masked source image bytes. We alias that
masked image only at requested per-page paint ordinals, cloning the Form XObject
chain as required when the same Form is invoked repeatedly. All other paints,
including other uses of exactly the same Image XObject, retain their source
image pixels. Original user documents are never modified.
"""

from pathlib import Path

import pikepdf
from pikepdf import ContentStreamInstruction, Operator

from mask_native_text import inherited_page_resources


def _source_object_id(stream):
    number, generation = stream.objgen
    if number <= 0:
        raise ValueError("cannot rewrite a nonindirect image XObject")
    return f"{number}:{generation}"


def _fresh_xobject_name(resources, ordinal):
    names = resources.get("/XObject", pikepdf.Dictionary())
    base = f"/OCRPaint{ordinal}"
    candidate = base
    suffix = 0
    while pikepdf.Name(candidate) in names:
        suffix += 1
        candidate = f"{base}_{suffix}"
    return pikepdf.Name(candidate)


def _resource_with_alias(resources, name, target):
    # Both dictionaries are shallow copies. In particular NEVER mutate an
    # inherited /Resources dictionary: it may serve many other pages/Forms.
    updated = pikepdf.Dictionary(resources)
    xobjects = pikepdf.Dictionary(resources.get("/XObject", pikepdf.Dictionary()))
    xobjects[name] = target
    updated["/XObject"] = xobjects
    return updated


def _clone_form(doc, original, stream_bytes, resources):
    # pikepdf Stream(...) encodes ordinary decoded bytes, then copies all
    # semantic Form dictionary entries. Do not copy stream encoding metadata
    # from the source (it describes an obsolete compressed byte sequence).
    clone = pikepdf.Stream(doc, stream_bytes)
    for name, value in original.items():
        if str(name) not in (
            "/Length",
            "/Filter",
            "/DecodeParms",
            "/F",
            "/FFilter",
            "/FDecodeParms",
            "/Resources",
        ):
            clone[name] = value
    clone["/Resources"] = resources
    return clone


def _clone_masked_image(doc, masked_stream):
    if str(masked_stream.get("/Subtype", "")) != "/Image":
        raise ValueError("masked source is not an Image XObject")
    if str(masked_stream.get("/ColorSpace", "")) not in (
        "/DeviceGray",
        "/DeviceRGB",
        "/DeviceCMYK",
    ):
        raise ValueError("unsupported masked image colorspace")
    decoded = masked_stream.read_bytes()
    clone = pikepdf.Stream(doc, decoded)
    for name in (
        "/Type",
        "/Subtype",
        "/Width",
        "/Height",
        "/BitsPerComponent",
        "/ColorSpace",
        "/Interpolate",
        "/Intent",
    ):
        if name in masked_stream:
            clone[name] = masked_stream[name]
    return clone


def _rewrite_node(
    doc, container, inherited, ordinals, wanted, masked_sources, ancestry, depth=0
):
    """Return a cloned Form node iff selected expanded Do paints lie below it.

    The ordinal counter visits all Image XObject paints in content-stream
    execution order and expands every Form invocation, exactly like
    pdf_image_placements.enumerate_image_paints.
    """
    if depth > 32:
        raise ValueError("Form nesting limit exceeded")
    resources = container.get("/Resources") or inherited
    instructions = list(pikepdf.parse_content_stream(container))
    changed = False
    altered = list(instructions)
    current_resources = resources
    for index, instruction in enumerate(instructions):
        if str(instruction.operator) != "Do" or len(instruction.operands) != 1:
            continue
        name = instruction.operands[0]
        target = resources.get("/XObject", pikepdf.Dictionary()).get(name)
        if target is None:
            raise ValueError("unresolved XObject name in content stream")
        subtype = str(target.get("/Subtype", ""))
        if subtype == "/Image":
            paint_index = ordinals[0]
            ordinals[0] += 1
            if paint_index not in wanted:
                continue
            source_id = _source_object_id(target)
            if source_id != wanted[paint_index]:
                raise ValueError(
                    "paint ordinal XObject mismatch: expected "
                    + wanted[paint_index]
                    + " got "
                    + source_id
                )
            alias = _fresh_xobject_name(current_resources, paint_index)
            current_resources = _resource_with_alias(
                current_resources,
                alias,
                masked_sources[source_id],
            )
            altered[index] = ContentStreamInstruction([alias], Operator("Do"))
            changed = True
        elif subtype == "/Form":
            if target.objgen in ancestry:
                raise ValueError("recursive Form invocation")
            cloned = _rewrite_node(
                doc,
                target,
                resources,
                ordinals,
                wanted,
                masked_sources,
                (*ancestry, target.objgen),
                depth + 1,
            )
            if cloned is None:
                continue
            alias = _fresh_xobject_name(current_resources, ordinals[0])
            current_resources = _resource_with_alias(
                current_resources,
                alias,
                cloned,
            )
            altered[index] = ContentStreamInstruction([alias], Operator("Do"))
            changed = True
    if not changed:
        return None
    updated_stream = pikepdf.unparse_content_stream(altered)
    if depth == 0:
        container.obj["/Resources"] = current_resources
        container.Contents = pikepdf.Stream(doc, updated_stream)
        return container
    return _clone_form(doc, container, updated_stream, current_resources)


def rewrite_masked_paints(
    control_pdf: Path,
    masked_pdf: Path,
    source_report,
    page: int,
    paint_indices,
    output_pdf: Path,
):
    """Rewrite exact paint indices using already prepared masked source bytes.

    The source report is originally SHA-bound and passed through the OCR
    pipeline. This operation verifies each ordinal's original XObject ID,
    and never updates the original input or shared Form/image resources.
    """
    paint_indices = set(paint_indices)
    if not paint_indices:
        raise ValueError("empty mask-paint selection")
    if page < 1:
        raise ValueError("invalid page")
    candidates = {
        p["paint_index"]: p["source_id"]
        for p in source_report["placements"]
        if p["page"] == page and p["status"] == "recognized_at_native_source_resolution"
    }
    if not paint_indices <= candidates.keys():
        raise ValueError("unrecognized or unproven original paint ordinal")
    wanted = {index: candidates[index] for index in paint_indices}
    with pikepdf.Pdf.open(control_pdf) as doc, pikepdf.Pdf.open(masked_pdf) as masks:
        if page > len(doc.pages) or len(doc.pages) != len(masks.pages):
            raise ValueError("mask/control page mismatch")
        masked_sources = {}
        for source_id in set(wanted.values()):
            obj, gen = map(int, source_id.split(":"))
            source = masks.get_object(obj, gen)
            if _source_object_id(doc.get_object(obj, gen)) != source_id:
                raise ValueError("source image object identity changed")
            masked_sources[source_id] = _clone_masked_image(doc, source)
        current = doc.pages[page - 1]
        ordinals = [0]
        modified = _rewrite_node(
            doc,
            current,
            inherited_page_resources(current),
            ordinals,
            wanted,
            masked_sources,
            (),
            0,
        )
        if modified is None:
            raise ValueError("no matching source paint found")
        if not paint_indices <= set(range(ordinals[0])):
            raise ValueError("selected image paint ordinal out of range")
        output_pdf.parent.mkdir(parents=True, exist_ok=True)
        doc.save(output_pdf, compress_streams=True)
    return {
        "page": page,
        "paint_indices": sorted(paint_indices),
        "masked_source_ids": sorted(set(wanted.values())),
        "paint_count": ordinals[0],
    }
