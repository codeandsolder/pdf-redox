# /// script
# requires-python = ">=3.12"
# dependencies = ["rapidocr>=3,<4", "onnxruntime>=1.21", "pikepdf>=9", "pillow>=11"]
# ///
"""Multi-page OCR draft generator for pdf-redox.

The OCR model is an external inference adapter. PDF annotation, native-text
suppression, and opt-in policy remain in Rust. Recognition does not require
outlined-glyph deduplication or a glyph catalog.
Usage:
  uv run batch_rapidocr.py --pdf optimized.pdf --output draft.json
    --pages 8,10,11 --max-side 1600

Native searchable text is hidden in a render-only copy before OCR inference
unless --keep-native-text is specified. The original PDF remains the semantic
output target and native-word geometry is retained for overlap exclusion.
"""

import argparse
import io
import os
import hashlib
import importlib.metadata
import json
import pathlib
import subprocess
import tempfile
import time

import pikepdf
from PIL import Image
from rapidocr import ModelType, OCRVersion, RapidOCR
from mask_native_text import mask_pdf
from mask_raster_text import prepare_render_image_pdf, verify_rendered_image_text
from pdf_geometry import display_mapper
from native_pdf_words import native_words_for_pages
from native_only_pages import page_is_native_only


def main(argv=None, existing_engine=None):
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pdf", type=pathlib.Path, required=True)
    p.add_argument("--output", type=pathlib.Path, required=True)
    p.add_argument(
        "--pages", help="comma-separated one-based page numbers; default all"
    )
    p.add_argument("--max-pages", type=int, default=None)
    p.add_argument("--max-side", type=int, default=1600)
    p.add_argument("--work-dir", type=pathlib.Path)
    p.add_argument(
        "--source-render-mask-report",
        type=pathlib.Path,
        help="source-image OCR report to suppress recognized raster text only in a temporary PDF for page inference",
    )
    p.add_argument("--source-render-mask-min-confidence", type=float, default=0.97)
    p.add_argument(
        "--char-evidence",
        type=pathlib.Path,
        help="optional PDF-bound per-character recognition sidecar (diagnostic, not labels)",
    )
    p.add_argument(
        "--rec-size",
        choices=("small", "medium", "tiny"),
        default="small",
        help="PP-OCRv6 recognition tier; default preserves existing small model",
    )
    p.add_argument(
        "--rec-model-path",
        type=pathlib.Path,
        help="local verified ONNX recognizer (avoid downloading into package cache)",
    )
    p.add_argument(
        "--ocr-threads",
        type=int,
        default=min(os.cpu_count() or 1, 6),
        help="ONNX Runtime CPU intra-op threads (default up to 6)",
    )
    p.add_argument(
        "--skip-native-only",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="bypass OCR when every painting instruction is proven native-searchable",
    )
    p.add_argument(
        "--keep-native-text",
        action="store_true",
        help="disable preprocessing native-text masking (diagnostic comparison)",
    )
    args = p.parse_args(argv)

    if args.max_side < 500:
        p.error("max-side must be >=500")
    if args.ocr_threads < 1:
        p.error("ocr-threads must be >=1")
    if existing_engine is not None and (
        args.rec_size != "small" or args.rec_model_path is not None
    ):
        p.error("native-image recognizer reuse requires default PP-OCRv6 small")
    with args.pdf.open("rb") as stream:
        pdf_hash = hashlib.file_digest(stream, "sha256").hexdigest()
    model = (
        f"RapidOCR {importlib.metadata.version('rapidocr')} "
        f"PP-OCRv6 det-small/rec-{args.rec_size} Chinese-English ONNX Runtime "
        f"(CPU threads={args.ocr_threads}, rec batch=6, angle classifier=on)"
    )
    start = time.monotonic()
    params = {
        "EngineConfig.onnxruntime.intra_op_num_threads": args.ocr_threads,
        "EngineConfig.onnxruntime.inter_op_num_threads": 1,
        "Rec.model_type": ModelType(args.rec_size),
        "Rec.ocr_version": OCRVersion.PPOCRV6,
    }
    if args.rec_model_path is not None:
        if not args.rec_model_path.is_file():
            p.error(f"local recognition model not found: {args.rec_model_path}")
        params["Rec.model_path"] = str(args.rec_model_path.resolve())
    engine = existing_engine  # lazy: native-only pages never initialize ONNX
    pages = []
    character_pages = []
    total_detected = 0

    with pikepdf.Pdf.open(args.pdf) as doc:
        selected = (
            [int(v) for v in args.pages.split(",")]
            if args.pages
            else list(range(1, len(doc.pages) + 1))
        )
        if args.max_pages is not None:
            selected = list(selected)[: args.max_pages]
        if len(set(selected)) != len(selected) or not selected:
            raise ValueError("empty or duplicate page selection")

        locations = {}
        for num in selected:
            if not 1 <= num <= len(doc.pages):
                raise ValueError(f"page outside PDF range: {num}")
            transform, rotation = display_mapper(doc.pages[num - 1])
            locations[num] = (transform, rotation)

        skipped = {}
        if args.skip_native_only:
            for num in selected:
                proof = page_is_native_only(doc.pages[num - 1])
                if proof.eligible:
                    skipped[num] = proof
            print(
                "PDF_STRUCTURE_NATIVE_ONLY",
                len(skipped),
                "of",
                len(selected),
                flush=True,
            )
        ocr_selected = [num for num in selected if num not in skipped]
        if engine is not None and ocr_selected:
            print("MODEL_REUSED_FROM_NATIVE_IMAGE", len(ocr_selected), flush=True)
        t_native = time.monotonic()
        native_regions = native_words_for_pages(
            args.pdf, selected, {num: locations[num][0] for num in selected}
        )
        print(
            "NATIVE_WORD_EXTRACTION",
            len(selected),
            "pages",
            "s",
            round(time.monotonic() - t_native, 2),
            flush=True,
        )

        root = args.work_dir if args.work_dir else args.output.parent
        root.mkdir(parents=True, exist_ok=True)

        render_mask_active = False
        mask_visibility = []
        source_proof_pages = set()
        verified_mask_pages = set()
        cached_page_rasters = {}
        visible_by_page = {}
        native_pages = (
            [num for num in ocr_selected if native_regions[num]]
            if not args.keep_native_text
            else []
        )
        with tempfile.TemporaryDirectory(prefix="ocr-masked-", dir=root) as mask_dir:
            visible = args.pdf
            native_mask_prepared = False
            if args.source_render_mask_report is not None and ocr_selected:
                report = json.loads(args.source_render_mask_report.read_text())
                t_mask = time.monotonic()
                image_mask_pdf = pathlib.Path(mask_dir) / "image-text-hidden.pdf"
                control_pdf = pathlib.Path(mask_dir) / "native-text-only-hidden.pdf"
                # Make two images from the SAME pikepdf document:
                #   control: native glyphs hidden, source-image text visible;
                #   treatment: native glyphs hidden, source-image text erased.
                # Source image object IDs remain stable because the comparison
                # documents are saved from one in-memory source PDF.
                image_stats = prepare_render_image_pdf(
                    args.pdf,
                    image_mask_pdf,
                    report,
                    ocr_selected,
                    min_confidence=args.source_render_mask_min_confidence,
                    control_pdf=control_pdf,
                    native_mask_pages=native_pages,
                )
                native_mask_prepared = bool(native_pages)
                visible = control_pdf
                source_proof_pages = set()
                if image_stats.get("source_images_masked", 0):
                    eligible_ids = {
                        item["source_id"]
                        for item in image_stats["visibility_expectations"]
                    }
                    source_proof_pages = {
                        item["page"]
                        for item in report["placements"]
                        if item["source_id"] in eligible_ids
                        and item["status"] == "recognized_at_native_source_resolution"
                    }
                print(
                    "RASTER_SOURCE_TEXT_PREPARED",
                    image_stats.get("source_images_masked", 0),
                    "verification_pages",
                    sorted(source_proof_pages),
                    "prep_s",
                    round(time.monotonic() - t_mask, 3),
                    flush=True,
                )
                if native_mask_prepared:
                    print(
                        "NATIVE_TEXT_HIDDEN",
                        image_stats["native_text_mask_statistics"],
                        "included_in_render_proof",
                        True,
                        flush=True,
                    )
            if native_pages and not native_mask_prepared:
                native_source = visible
                visible = pathlib.Path(mask_dir) / "render-only.pdf"
                t = time.monotonic()
                stats = mask_pdf(native_source, visible, pages=native_pages)
                print(
                    "NATIVE_TEXT_HIDDEN",
                    stats,
                    "prep_s",
                    round(time.monotonic() - t, 2),
                    flush=True,
                )
            elif not native_pages:
                print(
                    "NATIVE_TEXT_NO_MASK",
                    "by_option" if args.keep_native_text else "no_searchable_words",
                    flush=True,
                )

            for num in selected:
                if num in skipped:
                    proof = skipped[num]
                    pages.append(
                        {
                            "page": num,
                            "lines": [],
                            "native_text_regions": native_regions[num],
                        }
                    )
                    if args.char_evidence:
                        character_pages.append({"page": num, "characters": []})
                    print(
                        "OCR_SKIPPED_NATIVE_ONLY",
                        num,
                        "native_shows",
                        proof.native_shows,
                        "forms",
                        proof.forms_checked,
                        flush=True,
                    )
                    continue
                if engine is None:
                    t_model = time.monotonic()
                    engine = RapidOCR(params=params)
                    print(
                        "MODEL_INITIALIZED",
                        round(time.monotonic() - t_model, 2),
                        flush=True,
                    )
                transform, rotation = locations[num]
                native = native_regions[num]
                if num in source_proof_pages:
                    try:
                        current, comparisons = verify_rendered_image_text(
                            control_pdf,
                            image_mask_pdf,
                            report,
                            image_stats,
                            [num],
                            args.max_side,
                            return_page_rasters=True,
                            isolation_original=args.pdf,
                            isolation_root=pathlib.Path(mask_dir),
                            native_mask_pages=native_pages,
                            source_min_confidence=args.source_render_mask_min_confidence,
                        )
                        if current:
                            mask_visibility.extend(current)
                            verified = any(item["visible"] for item in current)
                            if verified:
                                verified_mask_pages.add(num)
                                render_mask_active = True
                            # At most one page's image is kept in memory.
                            # The verifier already returned the exact
                            # selectively source-masked raster; for a mixed
                            # occlusion page this may differ from both the
                            # all-masked treatment and unmodified control.
                            cached_page_rasters[num] = comparisons[num][1]
                            visible_by_page[num] = pathlib.Path(
                                current[0]["selected_mask_pdf"]
                            )
                            print(
                                "RASTER_SOURCE_TEXT_HIDDEN",
                                image_stats["source_images_masked"],
                                "page",
                                num,
                                "verified",
                                verified,
                                "visibility",
                                [
                                    (
                                        item["source_id"],
                                        item["text"],
                                        item["visible"],
                                        item.get("isolated_source_proof", False),
                                    )
                                    for item in current
                                ],
                                "masked_sources",
                                current[0]["applied_mask_source_ids"],
                                "control_mode",
                                current[0]["comparison_mode"],
                                "control_fraction",
                                current[0]["control_pixel_fraction"],
                                flush=True,
                            )
                    except (
                        ValueError,
                        OSError,
                        RuntimeError,
                        subprocess.CalledProcessError,
                    ) as error:
                        print(
                            "RASTER_SOURCE_MASK_VERIFICATION_FAILED",
                            num,
                            type(error).__name__,
                            str(error)[:240],
                            flush=True,
                        )
                if num in cached_page_rasters:
                    # The image is RGB uint8 exactly as Poppler returned
                    # during the image-text visibility proof. Pass a PIL RGB
                    # image to RapidOCR; NumPy arrays are interpreted as BGR.
                    image = Image.fromarray(cached_page_rasters.pop(num), "RGB")
                    render_s = 0.0
                    print("OCR_REUSED_VERIFIED_RENDER", num, flush=True)
                else:
                    t_render = time.monotonic()
                    raster = subprocess.run(
                        [
                            "pdftoppm",
                            "-f",
                            str(num),
                            "-l",
                            str(num),
                            "-scale-to",
                            str(args.max_side),
                            "-singlefile",
                            str(visible_by_page.get(num, visible)),
                        ],
                        check=True,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                    )
                    render_s = time.monotonic() - t_render
                    # With no output-root, pdftoppm streams raw PPM to stdout.
                    # This avoids PNG compression and temporary image files.
                    # RapidOCR's PIL-image loader performs RGB -> BGR itself.
                    with Image.open(io.BytesIO(raster.stdout)) as im:
                        image = im.convert("RGB")
                    del raster
                im_w, im_h = image.size
                # A selectively masked page PDF is needed only until its
                # RGB raster is decoded. Release its full-document bytes
                # before recognition and before advancing to the next page.
                proof_pdf = visible_by_page.pop(num, None)
                if (
                    proof_pdf is not None
                    and proof_pdf.name == f"proof-page-{num}-selected.pdf"
                ):
                    proof_pdf.unlink(missing_ok=True)
                t = time.monotonic()
                ocr = (
                    engine(image, return_word_box=True, return_single_char_box=True)
                    if args.char_evidence
                    else engine(image)
                )
                infer_s = time.monotonic() - t
                if args.char_evidence:
                    # RapidOCR word_results follows the line detector's order.
                    word_results = ocr.word_results or ()
                    if len(word_results) != len(ocr.txts or ()):
                        raise ValueError(
                            "per-character OCR result not aligned to detected lines"
                        )
                    characters = []
                    for words in word_results:
                        for char, confidence, corners in words:
                            if not corners or len(char) != 1 or not char.strip():
                                continue
                            quad = [
                                transform(
                                    float(corners[index][0]) / im_w,
                                    float(corners[index][1]) / im_h,
                                )
                                for index in (3, 2, 1, 0)
                            ]
                            characters.append(
                                {
                                    "text": char,
                                    "confidence": float(confidence),
                                    "quad": quad,
                                }
                            )
                    character_pages.append({"page": num, "characters": characters})
                lines = []
                for text, corners, confidence in zip(
                    ocr.txts or [],
                    ocr.boxes if ocr.boxes is not None else [],
                    ocr.scores or [],
                ):
                    if not str(text).strip():
                        continue
                    quad = [
                        transform(
                            float(corners[index][0]) / im_w,
                            float(corners[index][1]) / im_h,
                        )
                        for index in (3, 2, 1, 0)
                    ]
                    lines.append(
                        {
                            "text": str(text),
                            "confidence": float(confidence),
                            "quad": quad,
                            "reviewed": False,
                        }
                    )
                pages.append(
                    {
                        "page": num,
                        "lines": lines,
                        "native_text_regions": native,
                    }
                )
                total_detected += len(lines)
                print(
                    "OCR_PAGE",
                    num,
                    "native_words",
                    len(native),
                    "detections",
                    len(lines),
                    "render_s",
                    round(render_s, 2),
                    "infer_s",
                    round(infer_s, 2),
                    "rotation",
                    rotation,
                    flush=True,
                )

    result = {
        "schema_version": 3,
        "pdf_sha256": pdf_hash,
        "model": model,
        "pages": pages,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(result, ensure_ascii=False, separators=(",", ":"))
    )
    if args.char_evidence:
        evidence = {
            "schema_version": 1,
            "pdf_sha256": pdf_hash,
            "model": model + " (per-character boxes)",
            "pages": character_pages,
        }
        args.char_evidence.parent.mkdir(parents=True, exist_ok=True)
        args.char_evidence.write_text(
            json.dumps(evidence, ensure_ascii=False, separators=(",", ":"))
        )
        print(
            "CHAR_EVIDENCE_WRITTEN",
            args.char_evidence,
            "observations",
            sum(len(p["characters"]) for p in character_pages),
            flush=True,
        )
    print(
        "DRAFT_WRITTEN",
        args.output,
        "pages",
        len(pages),
        "detections",
        total_detected,
        "total_s",
        round(time.monotonic() - start, 2),
        flush=True,
    )
    result["_source_mask_verified"] = render_mask_active
    result["_source_mask_verified_pages"] = sorted(verified_mask_pages)
    result["_source_mask_visibility"] = mask_visibility
    return result


if __name__ == "__main__":
    main()
