# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Render exact canonical PDF Type3 vector paths as portable OCR samples.

No page-image OCR is involved: the SVG comes directly from glyph CharProcs.
White-background PNGs are exported as a compact tar archive for evaluation
with a recognizer independent from RapidOCR.
"""
import argparse
import io
import json
import pathlib
import subprocess
import tarfile

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--glyph-catalog", type=pathlib.Path, required=True)
    parser.add_argument("--votes", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--include", choices=("consistent","conflicting","all"),default="consistent")
    parser.add_argument("--size",type=int,default=128)
    args=parser.parse_args()
    if not 32 <= args.size <= 512:
        parser.error("size must be between 32 and 512")
    catalog=json.loads(args.glyph_catalog.read_text())
    votes=json.loads(args.votes.read_text())
    if catalog["optimized_pdf_sha256"] != votes["pdf_sha256"]:
        raise ValueError("source glyph catalog and candidate report refer to different PDFs")
    outcomes={x["glyph_id"]:x["status"] for x in votes["holdout_validation"]["assessments"]}
    info={x["glyph_id"]:x for x in votes["templates"]}
    selected=[]
    for template in catalog["glyphs"]:
        status=outcomes.get(template["id"])
        if status is None:
            continue
        if args.include != "all" and args.include != status:
            continue
        original=info[template["id"]]
        selected.append({
            "glyph_id":template["id"],
            "candidate_unicode":original["suggested_unicode"],
            "status":status,
            "full_document_occurrences":len(template["occurrences"]),
            "confidence_median":original["confidence_median"],
            "train_observations":original["observations"],
            "svg_path":template["svg_path"],
            "fill_rule":template["fill_rule"],
        })
    selected.sort(key=lambda x:(-x["full_document_occurrences"],x["glyph_id"]))
    manifest={"schema_version":1,"source_pdf_sha256":votes["pdf_sha256"],
              "source":"pdf-redox GlyphCatalog v2 normalized vector paths",
              "rasterizer":"rsvg-convert", "size":args.size,
              "templates":[]}
    args.output.parent.mkdir(parents=True,exist_ok=True)
    with tarfile.open(args.output, "w:gz") as tar:
        for idx,row in enumerate(selected):
            filename=f"{idx:03d}_{row['glyph_id'][:16]}.png"
            svg=(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="-0.16 -0.16 1.32 1.32">'
                 f'<g transform="translate(0 1) scale(1 -1)"><path fill="black" '
                 f'fill-rule="{row["fill_rule"]}" d="{row["svg_path"]}"/></g></svg>')
            proc=subprocess.run(
                ["rsvg-convert","--width",str(args.size),"--height",str(args.size),
                 "--background-color","white"],
                input=svg.encode(),stdout=subprocess.PIPE,stderr=subprocess.PIPE,check=True)
            if not proc.stdout.startswith(b"\x89PNG\r\n\x1a\n"):
                raise ValueError("SVG renderer did not return PNG")
            meta={k:v for k,v in row.items() if k not in ("svg_path","fill_rule")}
            meta["file"]=filename
            manifest["templates"].append(meta)
            member=tarfile.TarInfo(filename)
            member.size=len(proc.stdout);member.mode=0o644
            tar.addfile(member,io.BytesIO(proc.stdout))
        content=json.dumps(manifest,ensure_ascii=False,indent=2).encode()
        member=tarfile.TarInfo("manifest.json")
        member.size=len(content);member.mode=0o644
        tar.addfile(member,io.BytesIO(content))
    print(json.dumps({"templates":len(selected),"tar_bytes":args.output.stat().st_size},sort_keys=True))

if __name__=="__main__":
    main()
