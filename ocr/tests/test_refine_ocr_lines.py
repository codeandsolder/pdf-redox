# /// script
# requires-python = ">=3.12"
# dependencies = ["rapidocr>=3,<4", "onnxruntime>=1.21", "pikepdf>=9", "pillow>=11"]
# ///
"""Validate rotated PDF->raster crops and research-only escalation outputs."""

import hashlib
from pathlib import Path
import sys
from tempfile import TemporaryDirectory

import pikepdf

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from pdf_geometry import display_mapper, inverse_display_mapper
from refine_ocr_lines import raster_quad, refine


def bad(fn):
    try:
        fn()
    except ValueError:
        return
    raise AssertionError("expected invalid input")


def test_roundtrip():
    for angle in (0, 90, 180, 270):
        with pikepdf.Pdf.new() as pdf:
            page = pdf.add_blank_page(page_size=(300, 400))
            page.obj["/Rotate"] = angle
            f, rotation = display_mapper(page)
            inverse, other = inverse_display_mapper(page)
            assert rotation == other == angle
            q = [f(0.2, 0.8), f(0.35, 0.8), f(0.35, 0.65), f(0.2, 0.65)]
            raster = raster_quad(q, inverse, 1500, 2000)
            # Expected TL,TR,BR,BL in image: u .2-.35,v .65-.8.
            assert tuple(raster.shape) == (4, 2)
            for j, pt in enumerate(
                ((0.2, 0.65), (0.35, 0.65), (0.35, 0.8), (0.2, 0.8))
            ):
                assert abs(float(raster[j][0]) - pt[0] * 1500) < 0.02
                assert abs(float(raster[j][1]) - pt[1] * 2000) < 0.02
            bad(lambda: raster_quad([[0, 0]] * 4, inverse, 1500, 2000))
            bad(lambda: raster_quad([[float("nan"), 0]] * 4, inverse, 1500, 2000))


def test_binding_and_no_model_load():
    with TemporaryDirectory(prefix="pdf-redox-refine-test-") as root:
        path = Path(root) / "source.pdf"
        model = Path(root) / "fake.onnx"
        model.write_bytes(b"local model dummy, never loaded")
        with pikepdf.Pdf.new() as pdf:
            pdf.add_blank_page(page_size=(300, 400))
            pdf.save(path)
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        with pikepdf.Pdf.open(path) as pdf:
            f, _ = display_mapper(pdf.pages[0])
            quad = [f(0.2, 0.8), f(0.35, 0.8), f(0.35, 0.65), f(0.2, 0.65)]
        draft = {
            "schema_version": 3,
            "pdf_sha256": digest,
            "model": "RapidOCR PP-OCRv6 small",
            "pages": [
                {
                    "page": 1,
                    "native_text_regions": [],
                    "lines": [
                        {
                            "text": "AB",
                            "confidence": 0.99,
                            "quad": quad,
                            "reviewed": True,
                        }
                    ],
                }
            ],
        }
        report, out = refine(path, draft, model, max_primary_score=0.97)
        assert report["summary"]["selected_lines"] == 0
        assert report["summary"]["pages_refined"] == 0
        assert out["pages"][0]["lines"][0] == {
            "text": "AB",
            "confidence": 0.0,
            "quad": quad,
            "reviewed": False,
        }
        assert draft["pages"][0]["lines"][0]["confidence"] == 0.99
        draft["pdf_sha256"] = "invalid"
        bad(lambda: refine(path, draft, model))
        draft["pdf_sha256"] = digest
        draft["pages"].append(dict(draft["pages"][0]))
        bad(lambda: refine(path, draft, model))


if __name__ == "__main__":
    test_roundtrip()
    test_binding_and_no_model_load()
    print("REFINE_OCR_LINES_TESTS_PASS")
