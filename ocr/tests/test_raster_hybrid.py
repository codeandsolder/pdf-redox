# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10","pillow>=11","rapidocr>=3,<4","onnxruntime>=1.21"]
# ///
"""Hybrid embedded raster OCR and document-page fallback logic tests."""

import hashlib
import sys
from pathlib import Path
from tempfile import TemporaryDirectory

import pikepdf

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from raster_hybrid import overlap_fraction, clean_image_only_pages, combine


def rect(x, y, w, h):
    return [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]


def bad(fn):
    try:
        fn()
    except ValueError:
        return
    raise AssertionError("expected ValueError")


def test_geometry():
    q = rect(20, 20, 60, 15)
    assert abs(overlap_fraction(q, q) - 1) < 1e-9
    assert overlap_fraction(q, rect(150, 20, 60, 15)) == 0
    assert abs(overlap_fraction(q, rect(50, 20, 60, 15)) - 0.5) < 1e-9
    rotated = [[0, 0], [0, 10], [-30, 10], [-30, 0]]
    assert overlap_fraction(rotated, rect(-20, 2, 10, 6)) > 0.95


def test_provenance_and_fallback():
    with TemporaryDirectory(prefix="hybrid-raster-tests-") as root:
        source = Path(root) / "source.pdf"
        with pikepdf.Pdf.new() as p:
            p.add_blank_page(page_size=(300, 400))
            p.add_blank_page(page_size=(300, 400))
            p.save(source)
        sha = hashlib.sha256(source.read_bytes()).hexdigest()
        raster = {
            "schema_version": 3,
            "pdf_sha256": sha,
            "model": "RapidOCR embedded",
            "pages": [
                {
                    "page": 1,
                    "native_text_regions": [],
                    "lines": [
                        {
                            "text": "CAPACITOR 47uF",
                            "quad": rect(20, 20, 120, 18),
                            "confidence": 0.99,
                            "reviewed": False,
                        }
                    ],
                },
                {
                    "page": 2,
                    "native_text_regions": [],
                    "lines": [
                        {
                            "text": "New image text",
                            "quad": rect(20, 20, 100, 18),
                            "confidence": 0.99,
                            "reviewed": False,
                        },
                        {
                            "text": "Possible duplicate",
                            "quad": rect(40, 80, 90, 12),
                            "confidence": 0.999,
                            "reviewed": False,
                        },
                    ],
                },
            ],
        }
        image_report = {
            "pages": [
                {"page": 1, "image_only_after_native": True},
                {"page": 2, "image_only_after_native": False},
            ],
            "sources": [{"source_id": "8:0", "status": "recognized"}],
            "placements": [
                {
                    "page": 1,
                    "source_id": "8:0",
                    "status": "recognized_at_native_source_resolution",
                    "page_quad": rect(10, 10, 200, 60),
                },
                {
                    "page": 2,
                    "source_id": "8:0",
                    "status": "recognized_at_native_source_resolution",
                    "page_quad": rect(10, 10, 200, 100),
                },
            ],
        }
        chosen, why = clean_image_only_pages(image_report, raster)
        assert chosen == {1}, (chosen, why)
        image_report["placements"].append(dict(image_report["placements"][0]))
        chosen, _ = clean_image_only_pages(image_report, raster)
        assert 1 not in chosen
        image_report["placements"].pop()
        baseline = {
            "schema_version": 3,
            "pdf_sha256": sha,
            "model": "RapidOCR page",
            "pages": [
                {
                    "page": 2,
                    "native_text_regions": [],
                    "lines": [
                        {
                            "text": "Existing text",
                            "quad": rect(40, 80, 90, 12),
                            "confidence": 0.98,
                            "reviewed": False,
                        }
                    ],
                }
            ],
        }
        # New mixed-page safety rule: native-image proposals are eligible
        # only after the original-vs-image-masked page render proves that
        # their source text was actually visible through later PDF paints.
        ignored, stats_without_proof, _ = combine(
            source,
            [1, 2],
            raster,
            image_report,
            baseline,
            {1},
        )
        assert stats_without_proof.get("nonoverlapping_image_lines_added", 0) == 0
        assert len(ignored["pages"][1]["lines"]) == 1
        baseline["_source_mask_verified"] = True
        baseline["_source_mask_verified_pages"] = [2]
        baseline["_source_mask_visibility"] = [
            {"page": 2, "text": line["text"], "quad": line["quad"], "visible": True}
            for line in raster["pages"][1]["lines"]
        ]
        merged, stats, audit = combine(
            source, [1, 2], raster, image_report, baseline, {1}
        )
        assert stats["image_source_only_pages"] == 1
        assert stats["page_fallback_pages"] == 1
        assert stats["nonoverlapping_image_lines_added"] == 1
        assert stats["raster_candidates_kept_in_audit_only"] == 1
        assert [x["text"] for x in merged["pages"][1]["lines"]] == [
            "Existing text",
            "New image text",
        ]
        baseline["pdf_sha256"] = "bad"
        bad(lambda: combine(source, [1, 2], raster, image_report, baseline, {1}))


if __name__ == "__main__":
    test_geometry()
    test_provenance_and_fallback()
    print("RASTER_HYBRID_TESTS_PASS")
