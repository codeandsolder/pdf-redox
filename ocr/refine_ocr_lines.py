# /// script
# requires-python = ">=3.12"
# dependencies = ["rapidocr>=3,<4", "onnxruntime>=1.21", "pikepdf>=9", "pillow>=11"]
# ///
"""Second-pass PP-OCRv6 recognition on selected text crops, without re-detection.

The source OCR draft supplies PDF-coordinate text quads, provenance and native
word exclusions. A stronger recognizer receives *only* uncertain cropped lines.
Outputs both variant predictions and an explicitly unreviewed/zero-confidence
diagnostic OCR draft; never silently changes the source text or PDF semantics.
"""

import argparse
import hashlib
import io
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
from rapidocr import ModelType, OCRVersion, RapidOCR
from rapidocr.utils.process_img import get_rotate_crop_image

from mask_native_text import mask_pdf
from pdf_geometry import inverse_display_mapper
from ocr_text_equivalence import meaningful_change


def raster_quad(quad, inverse, width, height):
    """PDF BL/BR/TR/TL into crop-image TL/TR/BR/BL pixel coordinates."""
    if len(quad) != 4 or any(len(q) != 2 for q in quad):
        raise ValueError("invalid OCR quad")
    points = []
    for point in (quad[3], quad[2], quad[1], quad[0]):
        if not all(isinstance(v, (int, float)) and math.isfinite(v) for v in point):
            raise ValueError("nonfinite OCR quad")
        u, v = inverse(*point)
        if not (-0.025 <= u <= 1.025 and -0.025 <= v <= 1.025):
            raise ValueError("OCR box lies well outside the source PDF CropBox")
        points.append([u * width, v * height])
    array = np.asarray(points, dtype=np.float32)
    if any(np.linalg.norm(array[i] - array[(i + 1) % 4]) < 1.0 for i in range(4)):
        raise ValueError("degenerate OCR crop quad")
    return array


def refine(
    pdf_path,
    draft,
    model_path,
    pages=None,
    max_primary_score=0.97,
    min_chars=2,
    max_side=1600,
    ocr_threads=6,
    run_cls=True,
):
    if draft["schema_version"] != 3 or not draft.get("model", "").startswith(
        "RapidOCR"
    ):
        raise ValueError("input is not a RapidOCR v3 line draft")
    with pdf_path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    if digest != draft["pdf_sha256"]:
        raise ValueError("OCR predictions are bound to different PDF bytes")
    if not model_path.is_file():
        raise ValueError("local secondary recognition model is missing")
    if (
        not (0 <= max_primary_score <= 1)
        or min_chars < 1
        or not 500 <= max_side <= 4096
        or ocr_threads < 1
    ):
        raise ValueError("invalid secondary recognition thresholds")
    started = time.monotonic()
    selected = {}
    seen = set()
    for entry in draft["pages"]:
        n = entry["page"]
        if n in seen or n < 1:
            raise ValueError("duplicate or invalid page in source OCR")
        seen.add(n)
        if pages is not None and n not in pages:
            continue
        idx = []
        for i, line in enumerate(entry["lines"]):
            confidence = line["confidence"]
            if not isinstance(confidence, (int, float)) or not math.isfinite(
                confidence
            ):
                raise ValueError("nonfinite OCR score")
            if (
                line["text"].strip()
                and len(line["text"].strip()) >= min_chars
                and confidence <= max_primary_score
            ):
                idx.append(i)
        if idx:
            selected[n] = idx
    results = []
    if selected:
        params = {
            "Global.use_det": False,
            "Global.use_cls": run_cls,
            "Global.use_rec": True,
            "Rec.model_type": ModelType.MEDIUM,
            "Rec.ocr_version": OCRVersion.PPOCRV6,
            "Rec.model_path": str(model_path.resolve()),
            "EngineConfig.onnxruntime.intra_op_num_threads": ocr_threads,
            "EngineConfig.onnxruntime.inter_op_num_threads": 1,
        }
        engine = RapidOCR(params=params)
        mapping = {p["page"]: p for p in draft["pages"]}
        with pikepdf.Pdf.open(pdf_path) as doc:
            if any(n > len(doc.pages) for n in selected):
                raise ValueError("unknown page in source OCR")
            with tempfile.TemporaryDirectory(prefix="pdf-redox-ocr-refine-") as temp:
                render_pdf = pdf_path
                # Match the primary adapter's native-searchable-text masking.
                masked = [n for n in selected if mapping[n]["native_text_regions"]]
                if masked:
                    render_pdf = Path(temp) / "masked-native.pdf"
                    mask_pdf(pdf_path, render_pdf, pages=masked)
                for n, indices in selected.items():
                    t0 = time.monotonic()
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
                            str(render_pdf),
                        ],
                        stderr=subprocess.DEVNULL,
                    )
                    with Image.open(io.BytesIO(ppm)) as im:
                        image = np.asarray(im.convert("RGB"))
                    h, w = image.shape[:2]
                    bgr = cv2.cvtColor(image, cv2.COLOR_RGB2BGR)
                    crops = []
                    for i in indices:
                        points = raster_quad(
                            mapping[n]["lines"][i]["quad"], inverse, w, h
                        )
                        crops.append(get_rotate_crop_image(bgr, points))
                    if run_cls:
                        cls_imgs, _ = engine.cls_and_rotate(crops)
                    else:
                        cls_imgs = crops
                    recognized = engine.recognize_txt(cls_imgs)
                    if len(recognized.txts or ()) != len(indices):
                        raise ValueError(
                            "secondary recognizer returned wrong batch size"
                        )
                    for k, idx in enumerate(indices):
                        before = mapping[n]["lines"][idx]
                        after = recognized.txts[k]
                        score = float(recognized.scores[k])
                        if not math.isfinite(score) or not 0 <= score <= 1:
                            raise ValueError("invalid secondary recognizer score")
                        results.append(
                            {
                                "page": n,
                                "line_index": idx,
                                "quad": before["quad"],
                                "primary_text": before["text"],
                                "primary_score": before["confidence"],
                                "secondary_text": after,
                                "secondary_score": score,
                                "different": meaningful_change(before["text"], after),
                                "format_equivalent": not meaningful_change(
                                    before["text"], after
                                ),
                            }
                        )
                    print(
                        "REFINE_PAGE",
                        n,
                        "crops",
                        len(indices),
                        "seconds",
                        round(time.monotonic() - t0, 2),
                        flush=True,
                    )
    report = {
        "schema_version": 1,
        "pdf_sha256": digest,
        "source_model": draft["model"],
        "secondary_model": "RapidOCR PP-OCRv6 rec-medium ONNX, recognition-only on original detected quads",
        "max_primary_score": max_primary_score,
        "min_chars": min_chars,
        "max_side": max_side,
        "summary": {
            "source_lines": sum(len(page["lines"]) for page in draft["pages"]),
            "selected_lines": len(results),
            "revised_predictions": sum(row["different"] for row in results),
            "seconds": round(time.monotonic() - started, 3),
            "source_pages": len(draft["pages"]),
            "pages_refined": len(selected),
        },
        "proposals": results,
    }
    draft2 = json.loads(json.dumps(draft))
    draft2["model"] = (
        draft["model"] + " + experimental PP-OCRv6 medium recognition-only hypotheses"
    )
    for page in draft2["pages"]:
        for row in page["lines"]:
            row["reviewed"] = False
            row["confidence"] = 0.0
    by_page = {row["page"]: row for row in draft2["pages"]}
    for row in results:
        if row["secondary_text"].strip() and row["different"]:
            by_page[row["page"]]["lines"][row["line_index"]]["text"] = row[
                "secondary_text"
            ]
    return report, draft2


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=Path, required=True)
    p.add_argument("--primary-ocr", type=Path, required=True)
    p.add_argument("--secondary-model", type=Path, required=True)
    p.add_argument("--report", type=Path, required=True)
    p.add_argument("--experimental-draft", type=Path, required=True)
    p.add_argument("--pages", help="optional page IDs; must occur in the source OCR")
    p.add_argument("--max-primary-score", type=float, default=0.97)
    p.add_argument("--min-chars", type=int, default=2)
    p.add_argument("--max-side", type=int, default=1600)
    p.add_argument("--ocr-threads", type=int, default=6)
    p.add_argument("--no-cls", action="store_true")
    args = p.parse_args()
    target = {int(x) for x in args.pages.split(",")} if args.pages else None
    ocr = json.loads(args.primary_ocr.read_text())
    report, draft = refine(
        args.pdf,
        ocr,
        args.secondary_model,
        target,
        args.max_primary_score,
        args.min_chars,
        args.max_side,
        args.ocr_threads,
        not args.no_cls,
    )
    for path, data in ((args.report, report), (args.experimental_draft, draft)):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
    print("REFINEMENT_SUMMARY", json.dumps(report["summary"], sort_keys=True))


if __name__ == "__main__":
    main()
