#!/usr/bin/env python3
"""Shape-vote evidence tests: rotations, partial characters, conflicting votes."""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
from glyph_vote_evidence import audit, compare_holdout, match_char


def rect(x, y, w, h):
    return [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]


def rotated(quad):
    return [[-p[1], p[0]] for p in quad]


def cat(chars):
    return {
        "schema_version": 2,
        "optimized_pdf_sha256": "0" * 64,
        "glyphs": [
            {
                "id": ident,
                "occurrences": [
                    {"page": page, "index_on_page": i, "page_quad": quad}
                    for i, (page, quad) in enumerate(instances)
                ],
            }
            for ident, instances in chars.items()
        ],
    }


def ocr(pages):
    return {
        "schema_version": 1,
        "pdf_sha256": "0" * 64,
        "model": "test-model",
        "pages": [
            {
                "page": page,
                "characters": [
                    {"text": text, "confidence": prob, "quad": quad}
                    for text, prob, quad in chars
                ],
            }
            for page, chars in pages.items()
        ],
    }


def test_votes():
    c = cat(
        {
            "A": [
                (1, rect(1, 1, 4, 7)),
                (2, rect(11, 11, 4, 7)),
                (3, rotated(rect(1, 1, 4, 7))),
            ],
            "dot": [(1, rect(16, 17, 1, 1)), (1, rect(16, 12, 1, 3))],
        }
    )
    e = ocr(
        {
            1: [("A", 0.99, rect(1, 1, 5, 8)), ("i", 1.0, rect(15, 10, 4, 10))],
            2: [("A", 0.98, rect(11, 11, 5, 8))],
            3: [("A", 0.99, rotated(rect(1, 1, 5, 8)))],
        }
    )
    report = audit(c, e)
    s = report["summary"]
    assert s["associated_characters"] == 3, report
    assert s["evidence_candidates"] == 1, report
    assert s["candidate_occurrences_total"] == 3, report
    assert s.get("multiple_components", 0) == 1 or s.get("no_full_component", 0) == 1, (
        report
    )
    e["pages"][2]["characters"][0]["text"] = "H"
    s = audit(c, e)["summary"]
    assert s["conflicting_templates"] == 1 and s["evidence_candidates"] == 0, s
    e["pdf_sha256"] = "f" * 64
    try:
        audit(c, e)
    except ValueError:
        pass
    else:
        raise AssertionError("missing PDF digest enforcement")


def test_exactly_one():
    rect1 = rect(2, 3, 7, 10)
    char = rect(2, 3, 8, 11)
    assert match_char(char, {(0, 0): [("x", 0, rect1, 70)]})[0] == ("x", 0)
    assert (
        match_char(char, {(0, 0): [("x", 0, rect1, 70), ("y", 1, rect1, 70)]})[1]
        == "multiple_components"
    )


def test_holdout():
    training = {
        "pdf_sha256": "0" * 64,
        "templates": [
            {"glyph_id": "A", "evidence_candidate": True, "suggested_unicode": "A"},
            {"glyph_id": "B", "evidence_candidate": True, "suggested_unicode": "B"},
        ],
    }
    held = {
        "pdf_sha256": "0" * 64,
        "templates": [
            {
                "glyph_id": "A",
                "observations": 5,
                "votes": [{"text": "A", "count": 4}, {"text": "H", "count": 1}],
            },
            {"glyph_id": "B", "observations": 3, "votes": [{"text": "B", "count": 3}]},
        ],
    }
    comparison = compare_holdout(training, held)
    s = comparison["summary"]
    assert s["training_candidates"] == 2 and s["tested_templates"] == 2, s
    assert s["consistent_templates"] == 1 and s["conflicting_templates"] == 1, s
    assert s["agreeing_characters"] == 7 and s["disagreeing_characters"] == 1, s
    held["pdf_sha256"] = "f" * 64
    try:
        compare_holdout(training, held)
    except ValueError:
        pass
    else:
        raise AssertionError("holdout digest mismatch not rejected")


if __name__ == "__main__":
    test_votes()
    test_exactly_one()
    test_holdout()
    print("GLYPH_VOTE_TESTS_PASS")
