#!/usr/bin/env python3
"""Proof-style checks for PDF-bound multi-model glyph candidate promotion."""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
from promote_glyph_labels import promote, promote_contextual


def fixture():
    dig = "a" * 64
    catalog = {
        "schema_version": 2,
        "optimized_pdf_sha256": dig,
        "glyphs": [
            {"id": "glyph-A", "occurrences": [{"page": 1}, {"page": 2}, {"page": 3}]},
            {"id": "glyph-B", "occurrences": [{"page": 1}, {"page": 2}]},
        ],
    }
    votes = {
        "schema_version": 1,
        "pdf_sha256": dig,
        "templates": [
            {
                "glyph_id": "glyph-A",
                "evidence_candidate": True,
                "suggested_unicode": "A",
                "observations": 5,
            },
            {
                "glyph_id": "glyph-B",
                "evidence_candidate": True,
                "suggested_unicode": "B",
                "observations": 5,
            },
        ],
        "holdout_validation": {
            "assessments": [
                {
                    "glyph_id": "glyph-A",
                    "trained_unicode": "A",
                    "status": "consistent",
                    "holdout_observations": 3,
                    "agreeing": 3,
                    "disagreeing": 0,
                },
                {
                    "glyph_id": "glyph-B",
                    "trained_unicode": "B",
                    "status": "conflicting",
                    "holdout_observations": 3,
                    "agreeing": 2,
                    "disagreeing": 1,
                },
            ]
        },
    }
    independent = {
        "pdf_sha256": dig,
        "source": "Tesseract 5 LSTM eng",
        "glyphs": [
            {
                "glyph_id": "glyph-A",
                "candidate_unicode": "A",
                "full_document_occurrences": 3,
                "predictions": {
                    "10": {"text": "A", "confidence": 90},
                    "13": {"text": "A", "confidence": 85},
                },
            },
            {
                "glyph_id": "glyph-B",
                "candidate_unicode": "B",
                "full_document_occurrences": 2,
                "predictions": {
                    "10": {"text": "B", "confidence": 99},
                    "13": {"text": "B", "confidence": 98},
                },
            },
        ],
    }
    return catalog, votes, independent


def check():
    c, v, i = fixture()
    labels, report = promote(c, v, i)
    assert labels == {"schema_version": 1, "labels": {"glyph-A": "A"}, "overrides": []}
    assert report["summary"]["corroborated_appearances"] == 3
    i["glyphs"][0]["predictions"]["13"]["confidence"] = 30
    assert promote(c, v, i)[0]["labels"] == {}
    i["glyphs"][0]["predictions"]["13"]["confidence"] = 90
    i["pdf_sha256"] = "bad"
    try:
        promote(c, v, i)
    except ValueError:
        pass
    else:
        raise AssertionError("hash mismatch allowed")
    i["pdf_sha256"] = c["optimized_pdf_sha256"]
    i["glyphs"][0]["candidate_unicode"] = "X"
    try:
        promote(c, v, i)
    except ValueError:
        pass
    else:
        raise AssertionError("conflicting character allowed")
    i["glyphs"][0]["candidate_unicode"] = "A"
    i["glyphs"][0]["predictions"].pop("13")
    try:
        promote(c, v, i)
    except ValueError:
        pass
    else:
        raise AssertionError("missing second OCR prediction allowed")


def check_contextual():
    c, v, _ = fixture()
    evidence = {
        "schema_version": 1,
        "pdf_sha256": c["optimized_pdf_sha256"],
        "model": "Tesseract LSTM eng psm11 makebox",
        "glyphs": [
            {
                "glyph_id": "glyph-A",
                "rapidocr_unicode": "A",
                "full_document_occurrences": 3,
                "cross_page_consistent": True,
                "tesseract_observations": 100,
                "agreeing": 99,
                "disagreeing": 1,
                "pages_seen": 3,
                "votes": [{"text": "A", "count": 99}, {"text": "H", "count": 1}],
            },
            {
                "glyph_id": "glyph-B",
                "rapidocr_unicode": "B",
                "full_document_occurrences": 2,
                "cross_page_consistent": False,
                "tesseract_observations": 100,
                "agreeing": 100,
                "disagreeing": 0,
                "pages_seen": 3,
                "votes": [{"text": "B", "count": 100}],
            },
        ],
    }
    labels, report = promote_contextual(c, v, evidence)
    assert labels["labels"] == {"glyph-A": "A"}, labels
    assert report["summary"]["selected_independent_disagreements"] == 1
    assert report["summary"]["corroborated_appearances"] == 3
    evidence["glyphs"][0]["disagreeing"] = 3
    evidence["glyphs"][0]["agreeing"] = 97
    evidence["glyphs"][0]["votes"][0]["count"] = 97
    evidence["glyphs"][0]["votes"][1]["count"] = 3
    assert promote_contextual(c, v, evidence)[0]["labels"] == {}
    evidence["glyphs"][0]["agreeing"] = 99
    evidence["glyphs"][0]["disagreeing"] = 1
    evidence["glyphs"][0]["votes"][0]["count"] = 98
    evidence["glyphs"][0]["votes"][1]["count"] = 2
    try:
        promote_contextual(c, v, evidence)
    except ValueError:
        pass
    else:
        raise AssertionError("inconsistent vote detail allowed")


if __name__ == "__main__":
    check()
    check_contextual()
    print("GLYPH_PROMOTION_TESTS_PASS")
