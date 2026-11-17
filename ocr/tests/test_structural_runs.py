# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""Synthetic PDF structure, reading order, overrides and geometry regressions."""

from collections import defaultdict
import hashlib
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory

import pikepdf

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from structural_runs import assemble, build, source_order


def quad(x, y=10):
    return [[x, y], [x + 5, y], [x + 5, y + 8], [x, y + 8]]


def glyph(font, code, x, y=10):
    return (
        f"q /Span << /ActualText () >> BDC BT /{font} 1 Tf "
        f"5 0 0 8 {x} {y} Tm <{code:02X}> Tj ET EMC Q\n"
    )


def make_pdf(path, commands):
    with pikepdf.Pdf.new() as pdf:
        page = pdf.add_blank_page(page_size=(300, 300))
        page.Contents = pikepdf.Stream(pdf, commands.encode())
        pdf.save(path)


def expect_error(fn, message):
    try:
        fn()
    except ValueError:
        return
    raise AssertionError(message)


def test_source_and_barriers():
    # Deliberately include native text interleaved between otherwise adjacent
    # glyphs. Geometry is not permission to cross that semantic boundary.
    content = (
        glyph("FontA", 1, 10)
        + glyph("FontB", 2, 15.4)
        + "BT /Native 8 Tf (old) Tj ET\n"
        + glyph("FontA", 1, 20.8)
        + glyph("FontB", 2, 26.2)
        + glyph("FontA", 1, 31.6)
        + glyph("FontB", 2, 37)
        + glyph("FontB", 2, 60)
        + glyph("FontA", 1, 65.4)
    )
    with TemporaryDirectory(prefix="pdf-redox-runs-") as td:
        path = Path(td) / "synthetic.pdf"
        make_pdf(path, content)
        index = {
            ("FontA", 1): {"id": "A", "label": "A"},
            ("FontB", 2): {"id": "B", "label": "B"},
        }
        by_page = defaultdict(list)
        for i, x in enumerate((10, 20.8, 31.6, 65.4)):
            by_page[("A", 1)].append({"index_on_page": i, "page_quad": quad(x)})
        for i, x in enumerate((15.4, 26.2, 37, 60)):
            by_page[("B", 1)].append({"index_on_page": i, "page_quad": quad(x)})
        with pikepdf.Pdf.open(path) as doc:
            items = source_order(doc.pages[0], 1, index, by_page)
        assert sum("glyph_id" in x for x in items) == 8
        assert sum(x.get("native_barrier", False) for x in items) == 1
        runs, stats = assemble(items, min_chars=2)
        assert [r["text"] for r in runs] == ["AB", "ABAB", "BA"], runs
        assert stats["run_characters"] == 8
        # An appearance-specific suppressed label is always a run barrier,
        # independent of the global shape label.
        with pikepdf.Pdf.open(path) as doc:
            items = source_order(doc.pages[0], 1, index, by_page, {("B", 1, 1): ""})
        runs, _ = assemble(items)
        assert [x["text"] for x in runs] == ["AB", "AB", "BA"], runs
        # Strict agreement on the number of generated Type3 shows.
        by_page[("A", 1)].pop()
        with pikepdf.Pdf.open(path) as doc:
            expect_error(
                lambda: source_order(doc.pages[0], 1, index, by_page),
                "missing appearance should fail",
            )


def test_geometry_and_nonjoining():
    def item(label, q):
        return {"label": label, "quad": q, "glyph_id": label, "ordinal": 0}

    a = item("A", quad(0))
    b = item("B", quad(5.4))
    c = item("C", quad(11))
    assert [x["text"] for x in assemble([a, b, c])[0]] == ["ABC"]
    # A real word-space gap can be smaller than 0.32 of cap height;
    # do not silently join an adjacent unit/value or two words.
    word_gap = item("B", quad(7.25))
    assert assemble([a, word_gap])[0] == []
    assert [x["text"] for x in assemble([a, word_gap], max_gap=0.32)[0]] == ["AB"]
    multi = item("AB", quad(0))
    runs, stats = assemble([multi])
    assert runs[0]["text"] == "AB" and stats["run_components"] == 1
    gap = item("D", quad(25))
    assert [x["text"] for x in assemble([a, b, gap])[0]] == ["AB"]
    row = item("E", quad(11, 24))
    assert [x["text"] for x in assemble([a, b, row])[0]] == ["AB"]
    blocker = {"glyph_id": "?", "label": None, "quad": quad(11), "ordinal": 0}
    assert [x["text"] for x in assemble([a, b, blocker, c])[0]] == ["AB"]

    # Rotated page content is grouped in local writing direction.
    def rotate(q):
        return [[100 - y, 10 + x] for x, y in q]

    rotated = [item("A", rotate(quad(0))), item("B", rotate(quad(5.4)))]
    word = assemble(rotated)[0][0]
    assert word["text"] == "AB"
    assert word["quad"][0][1] < word["quad"][1][1]
    # No accidental grouping of a backwards/source-reversed show.
    assert assemble([b, a])[0] == []


def test_document_binding_and_overrides():
    with TemporaryDirectory(prefix="pdf-redox-runs-") as td:
        pdf = Path(td) / "sample.pdf"
        make_pdf(pdf, glyph("FontA", 1, 10) + glyph("FontA", 1, 15.4))
        digest = hashlib.sha256(pdf.read_bytes()).hexdigest()
        catalog = {
            "schema_version": 2,
            "optimized_pdf_sha256": digest,
            "glyphs": [
                {
                    "id": "A",
                    "font_resource": "FontA",
                    "code": 1,
                    "occurrences": [
                        {"page": 1, "index_on_page": i, "page_quad": quad(x)}
                        for i, x in enumerate([10, 15.4])
                    ],
                }
            ],
        }
        labels = {
            "schema_version": 1,
            "labels": {"A": "A"},
            "overrides": [
                {"glyph_id": "A", "page": 1, "index_on_page": 1, "text": "B"}
            ],
        }
        report, draft = build(pdf, catalog, labels, [1], 2, 0.32)
        assert [x["text"] for x in report["pages"][0]["runs"]] == ["AB"]
        assert draft["schema_version"] == 3
        assert draft["pages"][0]["lines"][0]["reviewed"] is False
        assert draft["pages"][0]["lines"][0]["confidence"] == 0.0
        assert report["summary"]["catalog_glyphs"] == 2
        bad = json.loads(json.dumps(labels))
        bad["overrides"].append(dict(bad["overrides"][0]))
        expect_error(
            lambda: build(pdf, catalog, bad, [1], 2, 0.32),
            "duplicate override should fail",
        )
        catalog["optimized_pdf_sha256"] = "0" * 64
        expect_error(
            lambda: build(pdf, catalog, labels, [1], 2, 0.32),
            "hash mismatch should fail",
        )


if __name__ == "__main__":
    test_source_and_barriers()
    test_geometry_and_nonjoining()
    test_document_binding_and_overrides()
    print("STRUCTURAL_RUN_TESTS_PASS")
