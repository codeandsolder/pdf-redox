# /// script
# requires-python = ">=3.12"
# dependencies = ["rapidocr>=3,<4","onnxruntime>=1.21","pikepdf>=9","pillow>=11","opencv-python-headless>=4.11"]
# ///
"""Experimental detector-free OCR from Type3 vector-line proposals.

Recognize only original PDF geometry proposals, never neural text-detection
boxes; use native searchable words as exclusions and in the OCR output.
Input PDF and vector proposal JSON must be bound by exact SHA256.
"""

import argparse
import hashlib
import io
import importlib.metadata
import json
import math
from pathlib import Path
import subprocess
import tempfile
import time

import cv2
import numpy as np
import pikepdf
from PIL import Image
from rapidocr import RapidOCR, ModelType, OCRVersion
from mask_native_text import mask_pdf
from native_pdf_words import native_words_for_pages
from pdf_geometry import display_mapper, inverse_display_mapper
from vlm_refine_pdf import crop, convex_intersects


def from_geometry(
    pdf,
    proposals,
    max_side=1600,
    threads=6,
    batch_size=64,
    min_score=0.5,
):
    if (
        proposals.get("schema_version") != 1
        or proposals.get("method") != "detector_free_type3_glyph_geometry"
    ):
        raise ValueError("unknown vector proposal schema")
    with pdf.open("rb") as inp:
        digest = hashlib.file_digest(inp, "sha256").hexdigest()
    if digest != proposals["pdf_sha256"]:
        raise ValueError("vector regions refer to different PDF bytes")
    if not (
        500 <= max_side <= 4096
        and 1 <= threads <= 64
        and 1 <= batch_size <= 256
        and 0 <= min_score <= 1
    ):
        raise ValueError("invalid geometry OCR settings")
    started = time.monotonic()
    engine = RapidOCR(
        params={
            "Global.use_det": False,
            "Global.use_cls": True,
            "Global.use_rec": True,
            "Rec.ocr_version": OCRVersion.PPOCRV6,
            "Rec.model_type": ModelType.SMALL,
            "EngineConfig.onnxruntime.intra_op_num_threads": threads,
            "EngineConfig.onnxruntime.inter_op_num_threads": 1,
        }
    )
    pages = proposals["pages"]
    if not pages or len(set(p["page"] for p in pages)) != len(pages):
        raise ValueError("missing/duplicate region page")
    output = {
        "schema_version": 3,
        "pdf_sha256": digest,
        "model": f"RapidOCR {importlib.metadata.version('rapidocr')} PP-OCRv6 rec-small detector-free Type3 geometry hypotheses",
        "pages": [],
    }
    metrics = []
    with pikepdf.Pdf.open(pdf) as doc:
        nums = [p["page"] for p in pages]
        if any(n <= 0 or n > len(doc.pages) for n in nums):
            raise ValueError("invalid vector region page")
        mappers = {n: display_mapper(doc.pages[n - 1])[0] for n in nums}
        native = native_words_for_pages(pdf, nums, mappers)
        with tempfile.TemporaryDirectory(prefix="pdf-vector-rec-") as scratch:
            original = pdf
            to_mask = [n for n in nums if native[n]]
            if to_mask:
                original = Path(scratch) / "native-masked.pdf"
                mask_pdf(pdf, original, pages=to_mask)
            for data in pages:
                t = time.monotonic()
                n = data["page"]
                inverse, _ = inverse_display_mapper(doc.pages[n - 1])
                ppm = subprocess.check_output(
                    [
                        "pdftoppm",
                        "-f",
                        str(n),
                        "-l",
                        str(n),
                        "-scale-to",
                        str(max_side),
                        "-singlefile",
                        str(original),
                    ],
                    stderr=subprocess.DEVNULL,
                )
                with Image.open(io.BytesIO(ppm)) as im:
                    img = np.asarray(im.convert("RGB"))
                eligible = [
                    region
                    for region in data["regions"]
                    if not any(
                        convex_intersects(region["quad"], word) for word in native[n]
                    )
                ]
                # Text orientation is fixed in the source PDF geometry.
                crops = [
                    cv2.cvtColor(crop(img, reg["quad"], inverse, 1), cv2.COLOR_RGB2BGR)
                    for reg in eligible
                ]
                lines = []
                scores = []
                for offset in range(0, len(crops), batch_size):
                    chunk = crops[offset : offset + batch_size]
                    if not chunk:
                        break
                    rotated, _ = engine.cls_and_rotate(chunk)
                    res = engine.recognize_txt(rotated)
                    if len(res.txts or ()) != len(chunk):
                        raise ValueError("recognizer failed to produce all predictions")
                    for k, (text, confidence) in enumerate(
                        zip(res.txts, res.scores, strict=True)
                    ):
                        if not isinstance(
                            confidence, (int, float)
                        ) or not math.isfinite(confidence):
                            raise ValueError("invalid ONNX recognition confidence")
                        scores.append(float(confidence))
                        if not text.strip() or confidence < min_score:
                            continue
                        region = eligible[offset + k]
                        lines.append(
                            {
                                "text": text,
                                "confidence": 0.0,
                                "reviewed": False,
                                "quad": region["quad"],
                            }
                        )
                output["pages"].append(
                    {"page": n, "lines": lines, "native_text_regions": native[n]}
                )
                metrics.append(
                    {
                        "page": n,
                        "regions": len(data["regions"]),
                        "after_native_exclusion": len(eligible),
                        "recognized_regions": len(lines),
                        "seconds": round(time.monotonic() - t, 3),
                        "mean_model_score": round(sum(scores) / max(len(scores), 1), 4),
                    }
                )
                print(
                    "VECTOR_OCR_PAGE",
                    json.dumps(metrics[-1], sort_keys=True),
                    flush=True,
                )
    return output, {
        "schema_version": 1,
        "pdf_sha256": digest,
        "pages": metrics,
        "wall_seconds": round(time.monotonic() - started, 3),
        "model_initialized_once": True,
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=Path, required=True)
    p.add_argument("--regions", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--report", type=Path, required=True)
    p.add_argument("--max-side", type=int, default=1600)
    p.add_argument("--ocr-threads", type=int, default=6)
    p.add_argument("--batch-size", type=int, default=64)
    p.add_argument("--min-model-score", type=float, default=0.5)
    a = p.parse_args()
    prediction, report = from_geometry(
        a.pdf,
        json.loads(a.regions.read_text()),
        a.max_side,
        a.ocr_threads,
        a.batch_size,
        a.min_model_score,
    )
    for path, item in ((a.output, prediction), (a.report, report)):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(item, ensure_ascii=False, separators=(",", ":")) + "\n"
        )
    print("VECTOR_OCR_COMPLETE", json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
