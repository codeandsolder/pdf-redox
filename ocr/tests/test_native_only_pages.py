# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""PDF paint-closure tests for the native-only OCR fast path."""

import hashlib
import json
from pathlib import Path
import subprocess
import shutil
import sys
from tempfile import TemporaryDirectory

import pikepdf

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from native_only_pages import page_is_native_only


def make_text_page(
    doc, content=b"BT /F1 12 Tf 30 50 Td (HELLO WORLD) Tj ET", resources=None
):
    page = doc.add_blank_page(page_size=(300, 400))
    if resources is None:
        resources = pikepdf.Dictionary(
            Font=pikepdf.Dictionary(
                F1=pikepdf.Dictionary(
                    Type=pikepdf.Name("/Font"),
                    Subtype=pikepdf.Name("/Type1"),
                    BaseFont=pikepdf.Name("/Helvetica"),
                )
            )
        )
    page.obj["/Resources"] = resources
    page.Contents = pikepdf.Stream(doc, content)
    return page


def build_test_pdf(path):
    with pikepdf.Pdf.new() as doc:
        # Page 1 — the normal text-only fast path.
        clean = make_text_page(doc)
        assert page_is_native_only(clean).eligible
        # Page 2 — painted shapes cannot be assumed irrelevant text.
        path_page = make_text_page(
            doc, b"BT /F1 12 Tf 30 50 Td (HELLO WORLD) Tj ET 20 20 50 10 re f"
        )
        assert not page_is_native_only(path_page).eligible
        # Page 3 — invisible text isn't proof that a raster is searchable.
        image = pikepdf.Stream(doc, b"\xff")
        image["/Type"] = pikepdf.Name("/XObject")
        image["/Subtype"] = pikepdf.Name("/Image")
        image["/Width"] = 1
        image["/Height"] = 1
        image["/ColorSpace"] = pikepdf.Name("/DeviceGray")
        image["/BitsPerComponent"] = 8
        xobj = pikepdf.Dictionary(Im0=doc.make_indirect(image))
        resources = pikepdf.Dictionary(
            Font=clean.obj["/Resources"]["/Font"], XObject=xobj
        )
        scan = make_text_page(
            doc,
            b"BT /F1 12 Tf 30 50 Td (HIDDEN OCR) Tj ET q 50 0 0 50 10 10 cm /Im0 Do Q",
            resources,
        )
        assert page_is_native_only(scan).reason == "image_or_unknown_xobject"
        # Page 4 — recursively invoked text-only Form XObject.
        form = pikepdf.Stream(doc, b"BT /F1 12 Tf 20 20 Td (FORM NATIVE) Tj ET")
        form["/Type"] = pikepdf.Name("/XObject")
        form["/Subtype"] = pikepdf.Name("/Form")
        form["/BBox"] = pikepdf.Array([0, 0, 200, 100])
        resources = pikepdf.Dictionary(
            Font=clean.obj["/Resources"]["/Font"],
            XObject=pikepdf.Dictionary(Fm0=doc.make_indirect(form)),
        )
        nested = make_text_page(doc, b"/Fm0 Do", resources)
        proof = page_is_native_only(nested)
        assert proof.eligible and proof.forms_checked == 1, proof
        # Page 5 — a painted Form requires regular OCR.
        drawn = pikepdf.Stream(doc, b"30 30 40 40 re f")
        drawn["/Type"] = pikepdf.Name("/XObject")
        drawn["/Subtype"] = pikepdf.Name("/Form")
        drawn["/BBox"] = pikepdf.Array([0, 0, 200, 100])
        res = pikepdf.Dictionary(
            Font=clean.obj["/Resources"]["/Font"],
            XObject=pikepdf.Dictionary(Fm0=doc.make_indirect(drawn)),
        )
        assert not page_is_native_only(make_text_page(doc, b"/Fm0 Do", res)).eligible
        # Page 6 — text painted with unextractable font fails.
        badfont = pikepdf.Dictionary(
            Font=pikepdf.Dictionary(
                F1=pikepdf.Dictionary(
                    Type=pikepdf.Name("/Font"),
                    Subtype=pikepdf.Name("/Type3"),
                    FontBBox=pikepdf.Array([0, 0, 12, 12]),
                )
            )
        )
        unencoded = make_text_page(doc, resources=badfont)
        assert not page_is_native_only(unencoded).eligible
        # Page 7 — visible annotations can paint additional text.
        annotated = make_text_page(doc)
        annotated.obj["/Annots"] = pikepdf.Array(
            [
                pikepdf.Dictionary(
                    Type=pikepdf.Name("/Annot"),
                    Subtype=pikepdf.Name("/FreeText"),
                    Rect=pikepdf.Array([5, 5, 80, 60]),
                )
            ]
        )
        assert not page_is_native_only(annotated).eligible
        # Page 8 — empty pages can safely bypass OCR.
        empty = doc.add_blank_page(page_size=(300, 400))
        assert page_is_native_only(empty).eligible
        # Page 9 — text clipping and unknown font graphics semantics fail.
        clipped = make_text_page(doc, b"BT /F1 12 Tf 30 50 Td 7 Tr (DRAWN) Tj ET")
        assert not page_is_native_only(clipped).eligible
        doc.save(path)


def test_native_only_skip():
    with TemporaryDirectory(prefix="pdf-redox-native-only-") as tmp:
        root = Path(tmp)
        pdf = root / "native-only-fixtures.pdf"
        build_test_pdf(pdf)
        with pikepdf.Pdf.open(pdf) as doc:
            states = [page_is_native_only(p).eligible for p in doc.pages]
            assert states == [
                True,
                False,
                False,
                True,
                False,
                False,
                False,
                True,
                False,
            ], states
        # This real end-to-end call must not instantiate or run ONNX on
        # the selected text-only pages, and must preserve native word quads.
        for pages in ("1", "4", "1,4", "8"):
            target = root / f"draft-{pages.replace(',', '-')}.json"
            chars = root / f"chars-{pages.replace(',', '-')}.json"
            proc = subprocess.run(
                [
                    shutil.which("uv"),
                    "run",
                    "--python",
                    "3.12",
                    str(Path(__file__).resolve().parent.parent / "batch_rapidocr.py"),
                    "--pdf",
                    str(pdf),
                    "--output",
                    str(target),
                    "--char-evidence",
                    str(chars),
                    "--pages",
                    pages,
                    "--max-side",
                    "650",
                    "--ocr-threads",
                    "2",
                ],
                capture_output=True,
                text=True,
                check=True,
            )
            assert "MODEL_INITIALIZED" not in proc.stdout, proc.stdout
            assert proc.stdout.count("OCR_SKIPPED_NATIVE_ONLY") == len(pages.split(","))
            output = json.loads(target.read_text())
            assert len(output["pages"]) == len(pages.split(","))
            assert all(not x["lines"] for x in output["pages"])
            assert all(
                not x["characters"] for x in json.loads(chars.read_text())["pages"]
            )
            if pages != "8":
                assert any(x["native_text_regions"] for x in output["pages"])
            assert output["pdf_sha256"] == hashlib.sha256(pdf.read_bytes()).hexdigest()


if __name__ == "__main__":
    test_native_only_skip()
    print("NATIVE_ONLY_SKIP_TESTS_PASS")
