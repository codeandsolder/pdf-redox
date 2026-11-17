# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Independent Tesseract OCR audit of canonical vector glyph samples.

Inputs are exact PDF Type3 shape rasterizations and a manifest containing
RapidOCR's hypothesis. No character labels are taken from Tesseract's input.
"""
import argparse
import csv
import io
import json
import math
import os
from pathlib import Path
import subprocess
import tarfile
import time

def tesseract(png, mode):
    proc=subprocess.run(["tesseract", str(png), "stdout", "--psm", str(mode),
                         "--oem", "1", "-l", "eng", "tsv"],
                        env={**os.environ,"OMP_THREAD_LIMIT":"1"},
                        stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,
                        timeout=15)
    if proc.returncode:
        raise RuntimeError(proc.stderr)
    results=list(csv.DictReader(io.StringIO(proc.stdout),delimiter="\t"))
    words=[item for item in results if item.get("level")=="5" and item.get("text","").strip()]
    text=" ".join(item["text"].strip() for item in words)
    confidences=[]
    for item in words:
        try:
            value=float(item["conf"])
            if math.isfinite(value) and value >= 0:
                confidences.append(value)
        except (ValueError,KeyError):
            pass
    return {"text":text,"confidence":sum(confidences)/len(confidences) if confidences else None}

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("--archive",type=Path,required=True)
    p.add_argument("--output",type=Path,required=True)
    p.add_argument("--modes",default="10,13")
    args=p.parse_args()
    modes=[int(x) for x in args.modes.split(",")]
    if not modes or any(x not in (6,7,10,13) for x in modes):
        p.error("modes must be OCR page segmentation modes 6,7,10,13")
    start=time.monotonic()
    result={"source":"Tesseract 5 LSTM, eng", "modes":modes}
    with tarfile.open(args.archive,mode="r:gz") as tar:
        raw=tar.extractfile("manifest.json")
        if raw is None:
            raise ValueError("missing manifest")
        manifest=json.load(raw)
        result["pdf_sha256"]=manifest["source_pdf_sha256"]
        result["glyphs"]=[]
        for row in manifest["templates"]:
            item=tar.extractfile(row["file"])
            if item is None:
                raise ValueError("missing glyph raster")
            from tempfile import TemporaryDirectory
            with TemporaryDirectory(prefix="glyph-tess-") as scratch:
                path=Path(scratch)/"glyph.png"
                path.write_bytes(item.read())
                predictions={str(mode):tesseract(path,mode) for mode in modes}
            result["glyphs"].append({
                "glyph_id":row["glyph_id"],"candidate_unicode":row["candidate_unicode"],
                "full_document_occurrences":row["full_document_occurrences"],
                "model_consensus_status":row["status"],
                "predictions":predictions,
            })
    summary={"glyphs_total":len(result["glyphs"])}
    for mode in modes:
        correct=sum(x["predictions"][str(mode)]["text"]==x["candidate_unicode"]
                    for x in result["glyphs"])
        insensitive=sum(x["predictions"][str(mode)]["text"].casefold() ==
                        x["candidate_unicode"].casefold() for x in result["glyphs"])
        candidates=sum(x["full_document_occurrences"] for x in result["glyphs"]
                       if x["predictions"][str(mode)]["text"]==x["candidate_unicode"])
        summary[f"psm{mode}_exact"]=correct
        summary[f"psm{mode}_casefold"]=insensitive
        summary[f"psm{mode}_exact_weighted_occurrences"]=candidates
    result["summary"]=summary
    result["wall_seconds"]=round(time.monotonic()-start,2)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(result,ensure_ascii=False,indent=2)+"\n")
    print(json.dumps({"summary":summary,"wall_seconds":result["wall_seconds"]},sort_keys=True))
    for row in result["glyphs"]:
        print(row["candidate_unicode"],"\t",
              " ".join(f"{mode}:{row['predictions'][str(mode)]['text']!r}" for mode in modes))
if __name__=="__main__":
    main()
