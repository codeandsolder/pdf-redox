# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Export PDF-bound Tesseract LSTM character boxes from real page images.

Unlike isolated-template OCR, this preserves visual word context. The output
contains unscored character *observations*, never trusted Unicode mappings.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import struct
import subprocess
from tempfile import TemporaryDirectory
import time

_BOX=re.compile(r"^(.*?)\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s*$")

def png_dimensions(path):
    with path.open("rb") as file:
        raw=file.read(24)
    if raw[:8]!=b"\x89PNG\r\n\x1a\n" or raw[12:16]!=b"IHDR":
        raise ValueError("raster must be a PNG")
    return struct.unpack(">II",raw[16:24])

def parse_makebox(data,width,height):
    results=[]
    skipped=0
    for line in data.splitlines():
        m=_BOX.match(line)
        if not m:
            skipped+=1
            continue
        text=m[1]
        x0,y0,x1,y1,page=(int(m[i]) for i in range(2,7))
        if (len(text)!=1 or text.isspace() or page!=0 or
            not (0 <= x0 < x1 <= width and 0 <= y0 < y1 <= height)):
            skipped+=1
            continue
        results.append({"text":text,"box":[x0,y0,x1,y1]})
    return results,skipped

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf",type=Path,required=True)
    p.add_argument("--output",type=Path,required=True)
    p.add_argument("--pages",required=True,help="one-based comma-separated page IDs")
    p.add_argument("--max-side",type=int,default=1600)
    p.add_argument("--psm",type=int,choices=(3,6,11,12),default=11)
    args=p.parse_args()
    pages=[int(x) for x in args.pages.split(",")]
    if not pages or len(pages)!=len(set(pages)) or min(pages)<1:
        p.error("unique one-based page IDs required")
    if not 500 <= args.max_side <= 4000:
        p.error("max-side outside [500,4000]")
    with args.pdf.open("rb") as stream:
        digest=hashlib.file_digest(stream,"sha256").hexdigest()
    start=time.monotonic()
    output={"schema_version":1,"pdf_sha256":digest,
            "engine":f"Tesseract LSTM eng psm{args.psm} makebox",
            "max_side":args.max_side,"pages":[]}
    with TemporaryDirectory(prefix="pdf-redox-tess-raster-") as root:
        for page in pages:
            raster=Path(root)/f"p{page}.png"
            subprocess.run(["pdftoppm","-f",str(page),"-l",str(page),
                            "-scale-to",str(args.max_side),
                            "-singlefile","-png",str(args.pdf),str(raster.with_suffix(""))],
                           check=True,stderr=subprocess.PIPE,stdout=subprocess.DEVNULL)
            width,height=png_dimensions(raster)
            result=subprocess.run(["tesseract",str(raster),"stdout",
                                   "--oem","1","-l","eng","--psm",str(args.psm),"makebox"],
                                  check=True,text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            observations,skipped=parse_makebox(result.stdout,width,height)
            output["pages"].append({"page":page,"width":width,"height":height,
                                    "characters":observations,"skipped_invalid":skipped})
            print(f"PAGE {page} {width}x{height} observations={len(observations)} skipped={skipped}",
                  flush=True)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(output,ensure_ascii=False,separators=(",",":"))+"\n")
    print(json.dumps({"total_characters":sum(len(p["characters"]) for p in output["pages"]),
                      "seconds":round(time.monotonic()-start,2)},sort_keys=True))

if __name__=="__main__":
    main()
