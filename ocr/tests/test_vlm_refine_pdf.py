# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9", "pillow>=11", "numpy>=2", "opencv-python-headless>=4.11"]
# ///
"""Tests for structural escalation and PDF-bound, explicitly unreviewed VLM results."""

from pathlib import Path
import sys

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from vlm_refine_pdf import choose, crop, inside, convex_intersects, merge
from benchmark_recognition_models import edit_distance, normalize, score
from ocr_text_equivalence import meaningful_change


def failure(fn):
    try:
        fn()
    except ValueError:
        return
    raise AssertionError("expected ValueError")


def quadrilateral(x=10, y=10, w=20, h=10):
    return [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]


def test_select_and_geometry():
    q = quadrilateral()
    catalog = {
        "glyphs": [
            {
                "id": "known",
                "occurrences": [
                    {
                        "page": 1,
                        "index_on_page": 0,
                        "page_quad": quadrilateral(12, 12, 2, 2),
                    }
                ],
            },
            {
                "id": "unknown",
                "occurrences": [
                    {
                        "page": 1,
                        "index_on_page": 0,
                        "page_quad": quadrilateral(20, 12, 2, 2),
                    }
                ],
            },
        ]
    }
    labels = {"labels": {"known": "A"}, "overrides": []}
    draft = {
        "pages": [
            {
                "page": 1,
                "lines": [
                    {"quad": q, "text": "strange", "confidence": 0.91},
                    {
                        "quad": quadrilateral(70, 10),
                        "text": "exact",
                        "confidence": 0.999,
                    },
                ],
            }
        ]
    }
    selected = choose(draft, catalog, labels, {1}, 0.97, 1, 10, 4)
    assert len(selected) == 1, selected
    assert selected[0]["known_components"] == 1
    assert selected[0]["unresolved_components"] == 1
    assert inside((25, 15), q)
    assert convex_intersects(q, quadrilateral(11, 11, 5, 5))
    assert not convex_intersects(q, quadrilateral(75, 75, 5, 5))
    assert not inside((250, 15), q)
    assert choose(draft, catalog, labels, {2}, 0.97, 1, 10, 4) == []
    draft["pages"][0]["native_text_regions"] = [quadrilateral(11, 11, 5, 5)]
    assert choose(draft, catalog, labels, {1}, 0.97, 1, 10, 4) == []

    # Source rotation and arbitrary text-orientation dimensions are handled
    # by inverse display frames, not assumptions about page axes.
    def inverse(x, y):
        return x / 100, 1 - y / 100

    img = np.full((100, 100, 3), 255, dtype=np.uint8)
    img[65:90, 7:40] = 20
    bitmap = crop(img, q, inverse, 2)
    assert bitmap.shape[0] > 20 and bitmap.shape[1] > 40
    assert (bitmap[:, :, 0] < 128).sum() > 0
    failure(lambda: crop(img, [[float("nan"), 0]] * 4, inverse, 2))


def test_merge_immutable_provenance():
    q = quadrilateral()
    source = {
        "schema_version": 3,
        "pdf_sha256": "a" * 64,
        "model": "RapidOCR PP-OCRv6",
        "pages": [
            {
                "page": 1,
                "native_text_regions": [],
                "lines": [
                    {"text": "ON-OFF", "confidence": 0.95, "quad": q, "reviewed": True},
                    {
                        "text": "Other",
                        "confidence": 0.99,
                        "quad": quadrilateral(30, 20),
                        "reviewed": True,
                    },
                ],
            }
        ],
    }
    manifest = {
        "schema_version": 1,
        "pdf_sha256": "a" * 64,
        "regions": [
            {
                "page": 1,
                "line_index": 0,
                "quad": q,
                "primary_text": "ON-OFF",
                "crop_sha256": "b" * 64,
            }
        ],
    }
    result = {
        "schema_version": 1,
        "pdf_sha256": "a" * 64,
        "model": "PaddleOCR-VL-1.6-GGUF",
        "proposals": [
            {
                "page": 1,
                "line_index": 0,
                "crop_sha256": "b" * 64,
                "primary_text": "ON-OFF",
                "vlm_text": "ON-OFF",
            }
        ],
    }
    out, audit = merge(source, manifest, result)
    assert audit[0]["single_line"] and not audit[0]["changed"]
    assert out["pages"][0]["lines"][0]["confidence"] == 0
    assert not out["pages"][0]["lines"][0]["reviewed"]
    assert not out["pages"][0]["lines"][1]["reviewed"]
    assert source["pages"][0]["lines"][0]["confidence"] == 0.95
    result["proposals"][0]["vlm_text"] = "CN-OFF"
    changed, audit = merge(source, manifest, result)
    assert audit[0]["changed"]
    assert changed["pages"][0]["lines"][0]["text"] == "CN-OFF"
    result["proposals"][0]["vlm_text"] = "ON - OFF"
    equivalent, audit = merge(source, manifest, result)
    assert audit[0]["normalized_equivalent"]
    assert not audit[0]["changed"]
    assert equivalent["pages"][0]["lines"][0]["text"] == "ON-OFF"
    result["proposals"][0]["vlm_text"] = "ON\nOFF"
    multi, audit = merge(source, manifest, result)
    assert not audit[0]["single_line"]
    assert multi["pages"][0]["lines"][0]["text"] == "ON-OFF"
    result["proposals"][0]["vlm_text"] = "CN-OFF"
    result["proposals"][0]["finish_reason"] = "length"
    incomplete, audit = merge(source, manifest, result)
    assert not audit[0]["single_line"]
    assert incomplete["pages"][0]["lines"][0]["text"] == "ON-OFF"
    result["proposals"][0]["finish_reason"] = "stop"
    result["proposals"][0]["crop_sha256"] = "bad"
    failure(lambda: merge(source, manifest, result))
    result["proposals"][0]["crop_sha256"] = "b" * 64
    result["pdf_sha256"] = "bad"
    failure(lambda: merge(source, manifest, result))


def test_scoring():
    assert edit_distance("kitten", "sitting") == 3
    assert edit_distance("", "ABC") == 3
    assert normalize("A  ｂ C") == "A b C"
    assert not meaningful_change("5E4：", "5E4 :")
    assert not meaningful_change("μ", "µ")
    assert meaningful_change("0", "O")
    assert meaningful_change("0.8", "-0.8")
    assert meaningful_change("ON-OFF", "CN-OFF")
    test = {
        "schema_version": 1,
        "source_pdf_sha256": "z" * 64,
        "samples": [
            {"page": 1, "center": [20, 15], "text": "A 250V", "kind": "technical"}
        ],
    }
    ocr = {
        "schema_version": 3,
        "pdf_sha256": "z" * 64,
        "model": "RapidOCR",
        "pages": [
            {
                "page": 1,
                "lines": [
                    {"text": "A 250V", "quad": quadrilateral(), "confidence": 0.99}
                ],
            }
        ],
    }
    measured = score(test, ocr)
    assert measured["summary"]["all"]["cer"] == 0
    assert measured["summary"]["all"]["exact"] == 1
    ocr["pdf_sha256"] = "different"
    failure(lambda: score(test, ocr))


if __name__ == "__main__":
    test_select_and_geometry()
    test_merge_immutable_provenance()
    test_scoring()
    print("VLM_REFINEMENT_TESTS_PASS")
