# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9", "pillow>=11"]
# ///
"""Exact-Type3-ink, OCR-decision, and native-text PDF coverage visualizer.

No synthetic artwork: modifies actual optimized PDF Type3 painting colors in a
diagnostic copy, renders with Poppler, overlays geometry, and reports metrics.
"""
from __future__ import annotations

import argparse
import csv
import collections
import colorsys
import hashlib
import io
import json
import math
import pathlib
import subprocess
import tempfile
from typing import Any

import pikepdf
from PIL import Image, ImageDraw, ImageFont
from pikepdf import ContentStreamInstruction as Op, Operator


CATEGORIES = (
    "accepted", "accepted_ungrouped", "low_confidence",
    "native_text_overlap", "duplicate", "no_ocr",
)
RGB = {
    "accepted": (0.05, 0.79, 0.27),
    "accepted_ungrouped": (0.05, 0.39, 0.94),
    "low_confidence": (0.96, 0.72, 0.05),
    "native_text_overlap": (0.92, 0.08, 0.18),
    "duplicate": (0.70, 0.27, 0.91),
    "no_ocr": (0.61, 0.62, 0.65),
}
OCR_RGB = {c: tuple(round(v * 255) for v in RGB[c]) for c in RGB}
OCR_RGB["native_word"] = (0, 195, 230)


def point_center(quad):
    return (
        sum(p[0] for p in quad) / 4,
        sum(p[1] for p in quad) / 4,
    )


def inside(quad, point):
    # Match the Rust OCR gate, including its explicit box margins.
    x, y = point[0] - quad[0][0], point[1] - quad[0][1]
    ux = (quad[1][0] - quad[0][0], quad[1][1] - quad[0][1])
    uy = (quad[3][0] - quad[0][0], quad[3][1] - quad[0][1])
    det = ux[0] * uy[1] - ux[1] * uy[0]
    if abs(det) < 1e-8:
        return False
    along = (x * uy[1] - y * uy[0]) / det
    across = (ux[0] * y - ux[1] * x) / det
    padding = 0.5 / math.hypot(*uy) + 0.25
    return -0.01 <= along <= 1.01 and -padding <= across <= 1 + padding


def native_intersection(a, b):
    # Equivalent to native Rust SAT test, including 0.2 pt separation margin.
    for axis in (0, 1):
        overlap = min(max(p[axis] for p in a), max(p[axis] for p in b)) - \
            max(min(p[axis] for p in a), min(p[axis] for p in b))
        if overlap <= 0.2:
            return False
    for poly in (a, b):
        for i in range(4):
            dx = poly[(i + 1) % 4][0] - poly[i][0]
            dy = poly[(i + 1) % 4][1] - poly[i][1]
            length = math.hypot(dx, dy)
            if length <= 1e-12:
                return False
            normal = (-dy / length, dx / length)
            aa = [p[0] * normal[0] + p[1] * normal[1] for p in a]
            bb = [p[0] * normal[0] + p[1] * normal[1] for p in b]
            if min(max(aa), max(bb)) - max(min(aa), min(bb)) <= 0.2:
                return False
    return True


def duplicate_overlap(a, b):
    a0 = (min(p[0] for p in a), min(p[1] for p in a))
    a1 = (max(p[0] for p in a), max(p[1] for p in a))
    b0 = (min(p[0] for p in b), min(p[1] for p in b))
    b1 = (max(p[0] for p in b), max(p[1] for p in b))
    inter = max(0, min(a1[0], b1[0]) - max(a0[0], b0[0])) * \
        max(0, min(a1[1], b1[1]) - max(a0[1], b0[1]))
    aa = (a1[0] - a0[0]) * (a1[1] - a0[1])
    bb = (b1[0] - b0[0]) * (b1[1] - b0[1])
    return inter / min(aa, bb) if aa > 0 and bb > 0 else 0


def classify(page, instances, threshold):
    # Native Rust OCR v3 selection: confidence, native-text SAT, duplicate.
    # Grouped Type3 centers describe the accepted detections for *diagnostics*
    # but never determine OCR eligibility.
    candidate = sorted(page["lines"], key=lambda x: -x["confidence"])
    native = page.get("native_text_regions", [])
    centers = [x["center"] for x in instances]
    accepted = []
    classified = []
    for line in candidate:
        text = line["text"]
        quad = line["quad"]
        score = line["confidence"]
        if not text.strip():
            reason = "blank"
        elif not line.get("reviewed", False) and score < threshold:
            reason = "low_confidence"
        elif any(native_intersection(quad, q) for q in native):
            reason = "native_text_overlap"
        elif any(duplicate_overlap(quad, a["quad"]) > 0.60 for a in accepted):
            reason = "duplicate"
        else:
            matched = sum(inside(quad, p) for p in centers)
            reason = "accepted" if matched >= 2 else "accepted_ungrouped"
            accepted.append(line)
        classified.append((line, reason))
    # Assign each actual Type3 occurrence a coverage status. If missed by
    # accepted OCR, nearest rejected OCR category is retained for diagnosis.
    for item in instances:
        center = item["center"]
        status = "no_ocr"
        if any(inside(line["quad"], center) for line in accepted):
            status = "accepted"
        else:
            matching = [(line, reason) for line, reason in classified
                        if reason not in ("accepted", "accepted_ungrouped", "blank") and inside(line["quad"], center)]
            if matching:
                status = max(matching, key=lambda r: r[0]["confidence"])[1]
        item["status"] = status
    return classified, accepted


def stable_rgb(glyph_id):
    hue = int(hashlib.sha256(glyph_id.encode()).hexdigest()[:8], 16) / 2**32
    r, g, b = colorsys.hsv_to_rgb(hue, 0.73, 0.85)
    return r, g, b


def proc_page_catalog(catalog, pages):
    result = {p: [] for p in pages}
    for glyph in catalog["glyphs"]:
        for occ in glyph["occurrences"]:
            if occ["page"] in result:
                result[occ["page"]].append({
                    "id": glyph["id"],
                    "font": glyph["font_resource"],
                    "code": glyph["code"],
                    "index": occ["index_on_page"],
                    "quad": occ["page_quad"],
                    "center": point_center(occ["page_quad"]),
                })
    return result


def operator_name(ins):
    return str(ins.operator) if hasattr(ins, "operator") else "INLINE_IMAGE"


def parse_name(value):
    return str(value).removeprefix("/")


def parse_code(operand):
    # pikepdf String parses hex <01> to one-byte raw string.
    if isinstance(operand, pikepdf.String):
        binary = bytes(operand)
        if len(binary) == 1:
            return binary[0]
    return None


def glyph_site(instr, i):
    # The Type3 generator always wraps each placed glyph in an empty
    # /Span /ActualText marker followed by BT, Tf, Tj, ET, EMC.
    ins = instr[i]
    if operator_name(ins) != "BDC" or len(ins.operands) != 2 or \
       str(ins.operands[0]) != "/Span":
        return None
    props = ins.operands[1]
    if not isinstance(props, pikepdf.Dictionary):
        return None
    if "/ActualText" not in props or str(props["/ActualText"]) != "":
        return None
    font = code = None
    for item in instr[i+1:i+20]:
        name = operator_name(item)
        if name == "EMC":
            break
        if name == "Tf" and item.operands:
            font = parse_name(item.operands[0])
        if name == "Tj" and item.operands:
            code = parse_code(item.operands[0])
    return (font, code) if font is not None and code is not None else None


def recolor_pages(pdf_path, pages, grouped, out_path, family):
    """Change real Type3 glyph ink colors (not their bounding-box overlay)."""
    expected = collections.Counter()
    occurrences = {}
    for page in pages:
        for x in grouped[page]:
            k = (page, x["font"], x["code"])
            occurrences.setdefault(k, []).append(x)
            expected[page] += 1
    colored = collections.Counter()
    with pikepdf.open(pdf_path) as pdf:
        for page_no in pages:
            pg = pdf.pages[page_no-1]
            instr = pikepdf.parse_content_stream(pg)
            out = []
            seen = collections.Counter()
            for i, item in enumerate(instr):
                out.append(item)
                found = glyph_site(instr, i)
                if found is None:
                    continue
                font, code = found
                k = (page_no, font, code)
                idx = seen[k]
                seen[k] += 1
                if k not in occurrences or idx >= len(occurrences[k]):
                    raise ValueError(f"unmapped Type3 site: {k} #{idx}")
                occ = occurrences[k][idx]
                if family:
                    rgb = stable_rgb(occ["id"])
                else:
                    rgb = RGB.get(occ["status"], RGB["no_ocr"])
                # The source's outer graphics-state q/Q already surrounds
                # the BDC. Set a nonstroking fill color *before BT*.
                out.append(Op([float(z) for z in rgb], Operator("rg")))
                colored[page_no] += 1
            pg.Contents = pikepdf.Stream(pdf, pikepdf.unparse_content_stream(out))
        pdf.save(out_path, compress_streams=True)
    assert all(colored[p] == expected[p] for p in pages), \
        (colored, expected)
    return colored



def recolor_residual(pdf_path, pages, out_path):
    """Diagnostic PDF copy: magenta for residual page-level painted paths.

    Type3 CharProc fills are in *font streams*, not the page content stream.
    Thus the magenta paths are precisely those still drawn as ordinary page
    path paint operators. One fill can contain many contours; this is not a
    count of characters.
    """
    stats={}
    with pikepdf.open(pdf_path) as pdf:
        for number in pages:
            page=pdf.pages[number-1]
            out=[]
            count=0
            for instruction in pikepdf.parse_content_stream(page):
                if operator_name(instruction) in ("f","F","f*","B","B*","b","b*"):
                    out.extend((
                        Op([],Operator("q")),
                        Op([1.0,0.0,0.63],Operator("rg")),
                        Op([1.0,0.0,0.63],Operator("RG")),
                        instruction,
                        Op([],Operator("Q")),
                    ))
                    count+=1
                else:
                    out.append(instruction)
            page.Contents=pikepdf.Stream(pdf,pikepdf.unparse_content_stream(out))
            stats[number]=count
        pdf.save(out_path,compress_streams=True)
    return stats


def make_triptych(name, page, row, images, output):
    """Three genuine PDF-derived renders in one zoomable contact sheet."""
    font_path="/usr/share/fonts/liberation/LiberationSans-Regular.ttf"
    if not pathlib.Path(font_path).exists():
        font_path="/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf"
    font=ImageFont.truetype(font_path,27)
    small=ImageFont.truetype(font_path,21)
    tiny=ImageFont.truetype(font_path,17)
    with Image.open(images[0]) as im:
        width,height=im.size
    target_h=1130
    target_w=round(width*target_h/height)
    gap=22;top=153;bottom=75
    canvas=Image.new("RGB",(target_w*3+gap*4,target_h+top+bottom),(238,241,246))
    draw=ImageDraw.Draw(canvas)
    draw.text((24,13),f"{name} - page {page}",font=font,fill=(15,25,38))
    ratio=row["grouped_coverage_percent"]
    draw.text((24,51),f"{row['covered_grouped_occurrences']:,} / {row['grouped_occurrences']:,} reusable components in accepted OCR ({ratio:.1f}%) - {row['accepted_ocr_lines']} OCR lines",font=small,fill=(40,57,75))
    draw.text((24,83),f"{row['ocr_accepted_ungrouped']} new ungrouped OCR lines, {row['ocr_reject_low_confidence']} low-confidence, {row['ocr_reject_native']} existing native text",font=tiny,fill=(70,80,90))
    titles=[
        ("Glyph families","Stable template hue identifies repeated glyph shapes"),
        ("Coverage + residual","Green = OCR-covered; gray = missed; magenta = other fills"),
        ("OCR detection boxes","Green reused; blue ungrouped; yellow low-confidence; red native"),
    ]
    for i,image in enumerate(images):
        x=gap+(target_w+gap)*i
        with Image.open(image) as im:
            canvas.paste(im.convert("RGB").resize((target_w,target_h),Image.Resampling.LANCZOS),(x,top))
        draw.text((x,124),titles[i][0],font=small,fill=(20,35,52))
        draw.text((x,top+target_h+15),titles[i][1],font=tiny,fill=(48,66,80))
    canvas.save(output,quality=88,optimize=True,subsampling=0)

def inherited(page, key, default):
    obj = page.obj
    while obj is not None:
        if key in obj:
            return obj[key]
        obj = obj.get("/Parent")
    return default


def page_mapper(pdf, page_no, width, height):
    p = pdf.pages[page_no-1]
    crop = [float(v) for v in p.cropbox]
    x0, y0, x1, y1 = crop
    rotate = int(inherited(p, "/Rotate", 0)) % 360
    if rotate == 0:
        return lambda pt: ((pt[0]-x0)/(x1-x0)*width, (y1-pt[1])/(y1-y0)*height)
    if rotate == 90:
        return lambda pt: ((pt[1]-y0)/(y1-y0)*width, (pt[0]-x0)/(x1-x0)*height)
    if rotate == 180:
        return lambda pt: ((x1-pt[0])/(x1-x0)*width, (pt[1]-y0)/(y1-y0)*height)
    if rotate == 270:
        return lambda pt: ((y1-pt[1])/(y1-y0)*width, (x1-pt[0])/(x1-x0)*height)
    raise ValueError("unsupported page rotation")


def render_page(pdf_path, page_no, output, max_side):
    with tempfile.TemporaryDirectory(dir=output.parent) as tmp:
        stem = pathlib.Path(tmp) / "p"
        subprocess.run([
            "pdftoppm", "-f", str(page_no), "-l", str(page_no),
            "-scale-to", str(max_side), "-singlefile", "-png",
            str(pdf_path), str(stem)
        ], check=True, capture_output=True)
        with Image.open(stem.with_suffix(".png")) as rendered:
            rendered.convert("RGB").save(output)


def draw_diagnostics(image, mapper, classified, natives, title, filename):
    base = Image.open(image).convert("RGBA")
    overlay = Image.new("RGBA", base.size)
    draw = ImageDraw.Draw(overlay, "RGBA")
    for q in natives:
        pts = [mapper(p) for p in q]
        draw.line(pts+[pts[0]], fill=(*OCR_RGB["native_word"], 120), width=1)
    for line, reason in classified:
        if reason == "blank":
            continue
        pts = [mapper(p) for p in line["quad"]]
        color = OCR_RGB[reason]
        draw.line(pts+[pts[0]], fill=(*color, 245), width=4 if reason in ("accepted","accepted_ungrouped") else 3)
    final = Image.alpha_composite(base, overlay)
    box = ImageDraw.Draw(final, "RGBA")
    box.rounded_rectangle((10, 10, 610, 64), radius=8, fill=(18, 23, 32, 205))
    box.text((21, 20), title, fill="white")
    box.text((21, 40), "Green reused | Blue ungrouped | Yellow low confidence | Red native | Cyan native word", fill="white")
    final.convert("RGB").save(filename)


def make_summary(name, pages, grouped, decisions, accepted, output_root):
    rows = []
    template_status = collections.defaultdict(lambda: collections.Counter())
    for page in pages:
        arr = grouped[page]
        cats = collections.Counter(x["status"] for x in arr)
        reasons = collections.Counter(reason for _, reason in decisions[page])
        for x in arr:
            template_status[x["id"]][x["status"]] += 1
        rows.append({
            "document": name, "page": page,
            "grouped_occurrences": len(arr),
            "covered_grouped_occurrences": cats["accepted"],
            "grouped_coverage_percent": round(100*cats["accepted"]/len(arr), 2) if arr else 0,
            "groups_present": len({x["id"] for x in arr}),
            "ocr_lines": len(decisions[page]),
            "accepted_ocr_lines": reasons["accepted"]+reasons["accepted_ungrouped"],
            "ocr_accepted_ungrouped": reasons["accepted_ungrouped"],
            "ocr_accepted_grouped": reasons["accepted"],
            "accepted_ocr_chars": sum(len(x["text"]) for x in accepted[page]),
            "ocr_reject_low_confidence": reasons["low_confidence"],
            "ocr_reject_native": reasons["native_text_overlap"],
            "ocr_reject_duplicate": reasons["duplicate"],
            "glyph_rejected_low_conf": cats["low_confidence"],
            "glyph_rejected_native": cats["native_text_overlap"],
            "glyph_rejected_duplicate": cats["duplicate"],
            "glyph_no_detected_line": cats["no_ocr"],
        })
    top_missed = []
    for id, counts in template_status.items():
        if counts["accepted"] > 0 and not any(c for c in counts if c!="accepted"):
            continue
        item = {
            "id":id, "accepted_occurrences":counts["accepted"],
            "missed_occurrences":sum(v for k,v in counts.items() if k!="accepted"),
            "categories":dict(counts),
        }
        top_missed.append(item)
    top_missed.sort(key=lambda x:-x["missed_occurrences"])
    out = {
        "document":name, "pages":rows,
        "total_grouped_occurrences":sum(r["grouped_occurrences"] for r in rows),
        "total_accepted_grouped":sum(r["covered_grouped_occurrences"] for r in rows),
        "unique_templates_present":len(template_status),
        "unique_templates_with_accepted_occurrences":sum(c["accepted"]>0 for c in template_status.values()),
        "most_missed_templates":top_missed[:50],
    }
    total=out["total_grouped_occurrences"]
    out["grouped_coverage_percent"] = round(100*out["total_accepted_grouped"]/total,2) if total else 0
    (output_root / f"{name}-coverage.json").write_text(json.dumps(out,indent=2))
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pdf",type=pathlib.Path,required=True)
    parser.add_argument("--catalog",type=pathlib.Path,required=True)
    parser.add_argument("--ocr",type=pathlib.Path,required=True)
    parser.add_argument("--output",type=pathlib.Path,required=True)
    parser.add_argument("--max-side",type=int,default=1800)
    parser.add_argument("--min-confidence",type=float,default=0.95)
    args=parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    catalog=json.loads(args.catalog.read_text())
    draft=json.loads(args.ocr.read_text())
    assert hashlib.sha256(args.pdf.read_bytes()).hexdigest()==catalog["optimized_pdf_sha256"]
    assert draft["pdf_sha256"]==catalog["optimized_pdf_sha256"]
    assert draft["schema_version"]==3
    name=args.pdf.stem
    pages=[p["page"] for p in draft["pages"]]
    if len(set(pages))!=len(pages):
        raise ValueError("duplicate OCR pages")
    grouped=proc_page_catalog(catalog,pages)
    decisions={}
    accepted={}
    for page in draft["pages"]:
        p=page["page"]
        decisions[p],accepted[p]=classify(page,grouped[p],args.min_confidence)
    out=make_summary(name,pages,grouped,decisions,accepted,args.output)
    colored=(args.output / f"{name}-status.pdf")
    families=(args.output / f"{name}-families.pdf")
    recolor_pages(args.pdf,pages,grouped,colored,False)
    recolor_pages(args.pdf,pages,grouped,families,True)
    residual=args.output/f"{name}-residual.pdf"
    residual_counts=recolor_residual(colored,pages,residual)
    out["residual_painted_path_operations"]=residual_counts
    (args.output/f"{name}-coverage.json").write_text(json.dumps(out,indent=2))
    with (args.output/f"{name}-pages.csv").open("w",newline="") as stream:
        writer=csv.DictWriter(stream,fieldnames=list(out["pages"][0]))
        writer.writeheader()
        writer.writerows(out["pages"])
    with pikepdf.open(args.pdf) as pdf:
        for record in draft["pages"]:
            p=record["page"]
            family_image=args.output / f"{name}-p{p:03}-families.png"
            status_image=args.output / f"{name}-p{p:03}-status.png"
            decisions_image=args.output / f"{name}-p{p:03}-ocr.png"
            render_page(families,p,family_image,args.max_side)
            render_page(colored,p,status_image,args.max_side)
            with Image.open(status_image) as im:
                mapper=page_mapper(pdf,p,im.width,im.height)
            draw_diagnostics(status_image,mapper,decisions[p],record["native_text_regions"],
                             f"{name} page {p}: {len(accepted[p])} accepted OCR lines",
                             decisions_image)
            residual_image=args.output/f"{name}-p{p:03}-residual.png"
            combined_image=args.output/f"{name}-p{p:03}-combined.png"
            render_page(residual,p,residual_image,args.max_side)
            draw_diagnostics(residual_image,mapper,decisions[p],record["native_text_regions"],
                             f"{name} page {p}: remaining vector fills in magenta",
                             combined_image)
            row=next(r for r in out["pages"] if r["page"]==p)
            make_triptych(name,p,row,[family_image,residual_image,combined_image],
                args.output/f"{name}-p{p:03}-triptych.jpg")
            print("PAGE",json.dumps(row,ensure_ascii=False),flush=True)
    print("TOTAL",json.dumps({
        k:v for k,v in out.items() if k not in ("pages","most_missed_templates")
    }),flush=True)


if __name__ == "__main__":
    main()
