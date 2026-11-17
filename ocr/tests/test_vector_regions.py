# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9","rapidocr>=3,<4","onnxruntime>=1.21","pillow>=11","opencv-python-headless>=4.11"]
# ///
"""Detector-free PDF Type3 text-region proposal and inference guards."""

import hashlib
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import pikepdf

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from vector_regions import lines_from_glyphs, build
from evaluate_vector_regions import bbox, covered_width
from vector_recognize import from_geometry


def quad(x, y, w=4, h=8):
    return [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]


def glyph(x, y, ch=""):
    return {
        "quad": quad(x, y),
        "u0": x,
        "u1": x + 4,
        "v0": y,
        "v1": y + 8,
        "uc": x + 2,
        "vc": y + 4,
        "width": 4,
        "height": 8,
        "orientation": 0,
        "ex": (1.0, 0.0),
        "ey": (0.0, 1.0),
        "label": ch or None,
    }


def fail(fn):
    try:
        fn()
    except ValueError:
        return
    raise AssertionError("expected invalid data rejection")


def test_clusters():
    items = [glyph(x, 10, ch) for x, ch in ((10, "A"), (15, "B"), (20, "C"), (25, "D"))]
    items.extend(glyph(x, 30) for x in (10, 15, 20, 25))
    items.extend(glyph(x, 10) for x in (90, 95, 100))
    regions = lines_from_glyphs(items, 3, 1.1, 10, 0.38, 10)
    assert len(regions) == 3, regions
    assert any(x["text_if_fully_known"] == "ABCD" for x in regions)
    assert any(x["text_if_fully_known"] is None for x in regions)
    assert not any(x["components"] == 7 for x in regions)
    # 90-degree rotation must retain vertical writing orientation.
    rotated = [dict(glyph(10, y)) for y in (10, 15, 20)]
    for item in rotated:
        x, y = item["uc"], item["vc"]
        item.update(
            {
                "orientation": 18,
                "ex": (0.0, 1.0),
                "ey": (-1.0, 0.0),
                "uc": y,
                "vc": -x,
                "u0": item["v0"],
                "u1": item["v1"],
                "v0": -item["u1"],
                "v1": -item["u0"],
            }
        )
    regions = lines_from_glyphs(rotated, 3, 1.1, 10, 0.38, 10)
    assert len(regions) == 1, regions
    assert regions[0]["orientation_degrees"] == 90
    fail(lambda: lines_from_glyphs(items, min_components=0))
    assert (
        covered_width(bbox(quad(10, 10, 20, 8)), [bbox(quad(10, 10, 20, 8))])[0] == 1.0
    )


def test_pdf_binding():
    with TemporaryDirectory(prefix="vector-region-tests-") as tmp:
        pdfpath = Path(tmp) / "test.pdf"
        with pikepdf.Pdf.new() as doc:
            doc.add_blank_page(page_size=(300, 400))
            doc.save(pdfpath)
        digest = hashlib.sha256(pdfpath.read_bytes()).hexdigest()
        c = {
            "schema_version": 2,
            "optimized_pdf_sha256": digest,
            "glyphs": [
                {
                    "id": "A",
                    "occurrences": [
                        {"page": 1, "index_on_page": 0, "page_quad": quad(10, 10)},
                        {"page": 1, "index_on_page": 1, "page_quad": quad(15, 10)},
                        {"page": 1, "index_on_page": 2, "page_quad": quad(20, 10)},
                    ],
                }
            ],
        }
        out = build(
            pdfpath,
            c,
            {"schema_version": 1, "labels": {"A": "A"}},
            [1],
            3,
            1.1,
            10,
            0.38,
            10,
        )
        assert out["summary"]["regions"] == 1
        assert out["pages"][0]["regions"][0]["text_if_fully_known"] == "AAA"
        bad = json.loads(json.dumps(c))
        bad["optimized_pdf_sha256"] = "0" * 64
        fail(lambda: build(pdfpath, bad, None, [1], 3, 1.1, 10, 0.38, 10))
        fail(lambda: build(pdfpath, c, None, [2], 3, 1.1, 10, 0.38, 10))
        bad = json.loads(json.dumps(c))
        bad["glyphs"][0]["occurrences"][1]["index_on_page"] = 0
        fail(lambda: build(pdfpath, bad, None, [1], 3, 1.1, 10, 0.38, 10))
        fail(
            lambda: from_geometry(
                pdfpath,
                {
                    "schema_version": 1,
                    "method": "detector_free_type3_glyph_geometry",
                    "pdf_sha256": "0" * 64,
                    "pages": [{"page": 1, "regions": []}],
                },
            )
        )


if __name__ == "__main__":
    test_clusters()
    test_pdf_binding()
    print("VECTOR_REGION_TESTS_PASS")
