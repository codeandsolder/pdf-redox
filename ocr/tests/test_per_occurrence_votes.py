# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""Unit tests for two-model per-appearance Unicode override provenance."""

import hashlib
from pathlib import Path
import sys
from tempfile import TemporaryDirectory

import pikepdf

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from per_occurrence_votes import propose


def failure(fn):
    try:
        fn()
    except ValueError:
        return
    raise AssertionError("expected validation error")


def test():
    with TemporaryDirectory(prefix="pdf-redox-appearance-test-") as td:
        path = Path(td) / "sample.pdf"
        with pikepdf.Pdf.new() as pdf:
            pdf.add_blank_page(page_size=(100, 100))
            pdf.save(path)
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        quad = [[10, 10], [20, 10], [20, 20], [10, 20]]
        catalog = {
            "schema_version": 2,
            "optimized_pdf_sha256": digest,
            "glyphs": [
                {
                    "id": "multiuse",
                    "occurrences": [
                        {"page": 1, "index_on_page": 0, "page_quad": quad},
                    ],
                }
            ],
        }
        labels = {"schema_version": 1, "labels": {}, "overrides": []}
        char = {"text": "I", "confidence": 0.99, "quad": quad}
        rapid = [
            {
                "schema_version": 1,
                "pdf_sha256": digest,
                "model": "RapidOCR v3",
                "pages": [{"page": 1, "characters": [char]}],
            }
        ]
        tess = {
            "schema_version": 1,
            "pdf_sha256": digest,
            "engine": "Tesseract LSTM eng psm11 makebox",
            "pages": [
                {
                    "page": 1,
                    "width": 100,
                    "height": 100,
                    "characters": [{"text": "I", "box": [10, 10, 20, 20]}],
                }
            ],
        }
        output, report = propose(catalog, labels, rapid, tess, path)
        assert output["overrides"] == [
            {"glyph_id": "multiuse", "page": 1, "index_on_page": 0, "text": "I"}
        ]
        assert report["summary"]["model_agreements"] == 1
        assert report["summary"]["new_occurrence_overrides"] == 1
        assert report["summary"]["paired_glyph_instances"] == 1

        labels["labels"]["multiuse"] = "I"
        out, stats = propose(catalog, labels, rapid, tess, path)
        assert out["overrides"] == []
        assert stats["summary"]["skipped_already_global"] == 1
        labels["labels"]["multiuse"] = "l"
        out, stats = propose(catalog, labels, rapid, tess, path)
        assert out["overrides"] == []
        assert stats["summary"]["skipped_different_from_global_review"] == 1
        assert (
            propose(catalog, labels, rapid, tess, path, override_global=True)[0][
                "overrides"
            ]
            == output["overrides"]
        )
        labels["labels"].clear()

        tess["pages"][0]["characters"][0]["text"] = "l"
        out, stats = propose(catalog, labels, rapid, tess, path)
        assert out["overrides"] == []
        assert stats["summary"]["model_disagreements"] == 1

        tess["pages"][0]["characters"][0]["text"] = "I"
        rapid[0]["pages"][0]["characters"].append(
            {"text": "l", "confidence": 0.99, "quad": quad}
        )
        out, stats = propose(catalog, labels, rapid, tess, path)
        assert out["overrides"] == []
        assert stats["summary"]["ambiguous_duplicate_boxes"] == 1
        rapid[0]["pages"][0]["characters"].pop()
        rapid[0]["pages"][0]["characters"][0]["confidence"] = 0.6
        assert propose(catalog, labels, rapid, tess, path)[0]["overrides"] == []
        rapid[0]["pages"][0]["characters"][0]["confidence"] = 0.99

        labels["overrides"] = output["overrides"] * 2
        failure(lambda: propose(catalog, labels, rapid, tess, path))
        labels["overrides"] = []
        tess["pdf_sha256"] = "0" * 64
        failure(lambda: propose(catalog, labels, rapid, tess, path))
        tess["pdf_sha256"] = digest
        rapid[0]["pages"].append(dict(rapid[0]["pages"][0]))
        failure(lambda: propose(catalog, labels, rapid, tess, path))


if __name__ == "__main__":
    test()
    print("PER_OCCURRENCE_VOTES_TESTS_PASS")
