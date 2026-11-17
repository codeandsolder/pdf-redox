# /// script
# requires-python = ">=3.12"
# dependencies = ["rapidocr>=3,<4", "onnxruntime>=1.21", "pikepdf>=9", "pillow>=11"]
# ///
"""Isolated ingestion and output pixel-equivalence checks for the OCR adapter."""

import io
import pathlib
import subprocess
import sys
import tempfile
from unittest import mock

import pikepdf
from PIL import Image, ImageChops

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
from native_pdf_words import (
    _extract_native_word_quads,
    _parse_bbox_xhtml,
    native_words_for_pages,
)


def test_malformed_unicode():
    data = (
        b'<doc><page width="100" height="200">'
        b'<word xMin="1" yMin="2" xMax="3" yMax="4">\x03</word>'
        b'<word xMin="10" yMin="20" xMax="50" yMax="30">CJK</word>'
        b"</page></doc>"
    )
    page = _parse_bbox_xhtml(data).find("page")
    assert len(_extract_native_word_quads(page, lambda x, y: [x, y])) == 1


def test_batched_runs():
    calls = []

    def fake_output(args, **kwargs):
        start = int(args[args.index("-f") + 1])
        end = int(args[args.index("-l") + 1])
        calls.append((start, end))
        page = b'<page width="100" height="100"><word xMin="10" yMin="20" xMax="30" yMax="40">ok</word></page>'
        return b"<doc>" + page * (end - start + 1) + b"</doc>"

    selected = [8, 1, 2, 9, 34, 3]
    transforms = {n: lambda x, y: [x, y] for n in selected}
    with mock.patch(
        "native_pdf_words.subprocess.check_output", side_effect=fake_output
    ):
        out = native_words_for_pages("irrelevant.pdf", selected, transforms)
    assert calls == [(1, 3), (8, 9), (34, 34)], calls
    assert sorted(out) == sorted(selected)
    assert all(len(v) == 1 for v in out.values())


def test_ppm_png_exact_pixels():
    with tempfile.TemporaryDirectory(prefix="ocr-ppm-test-") as tmp:
        path = pathlib.Path(tmp) / "single.pdf"
        with pikepdf.Pdf.new() as doc:
            page = doc.add_blank_page(page_size=(300, 400))
            page.Contents = pikepdf.Stream(
                doc, b"q 0.24 0.62 0.15 rg 30 30 150 150 re f Q"
            )
            doc.save(path)
        cmd = ["pdftoppm", "-f", "1", "-l", "1", "-scale-to", "650", "-singlefile"]
        ppm = subprocess.check_output([*cmd, str(path)], stderr=subprocess.DEVNULL)
        png = subprocess.check_output(
            [*cmd, "-png", str(path)], stderr=subprocess.DEVNULL
        )
        with Image.open(io.BytesIO(ppm)) as a, Image.open(io.BytesIO(png)) as b:
            assert a.size == b.size
            assert (
                ImageChops.difference(a.convert("RGB"), b.convert("RGB")).getbbox()
                is None
            )


if __name__ == "__main__":
    test_malformed_unicode()
    test_batched_runs()
    test_ppm_png_exact_pixels()
    print(
        "OCR_BATCH_TESTS_PASS: XHTML controls, batched page mapping, exact PPM/PNG raster equality"
    )
