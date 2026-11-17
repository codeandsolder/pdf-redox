# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10", "pillow>=11"]
# ///
"""Verify that all Poppler-derived geometry uses the actual MediaBox frame."""

import io
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory

import pikepdf
from PIL import Image

import sys

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from pdf_geometry import display_mapper, inverse_display_mapper
from native_pdf_words import native_words_for_pages
from select_raster_sources import choose_mixed_raster_sources


def test_source_sampling_matches_poppler_mediabox():
    with pikepdf.Pdf.new() as pdf:
        page = pdf.add_blank_page(page_size=(800, 600))
        page.cropbox = pikepdf.Array([70, 390, 430, 540])
        placement = {
            "page": 1,
            "source_id": "15:0",
            "width": 1200,
            "height": 300,
            "ctm": [240, 0, 0, 60, 80, 400],
            "unsafe_reasons": [],
        }
        chosen, stats = choose_mixed_raster_sources(
            [placement], {1: page}, max_side=1600
        )
        # Native 1200x300 pixels against a 480x120 Poppler *MediaBox*
        # projection. Using the 360x150 CropBox would incorrectly reject it.
        assert chosen == {"15:0"}, stats
        assert stats["sources"][0]["native_to_page_render_area_gain"] == 6.25


def test_native_word_quads_match_poppler_nonzero_mediabox_origin():
    with TemporaryDirectory(prefix="pdf-redox-mediabox-native-") as td:
        path = Path(td) / "native.pdf"
        with pikepdf.Pdf.new() as doc:
            page = doc.add_blank_page(page_size=(800, 600))
            page.mediabox = pikepdf.Array([30, 40, 830, 640])
            page.cropbox = pikepdf.Array([160, 370, 470, 540])
            page.obj["/Resources"] = pikepdf.Dictionary(
                Font=pikepdf.Dictionary(
                    F1=pikepdf.Dictionary(
                        Type=pikepdf.Name("/Font"),
                        Subtype=pikepdf.Name("/Type1"),
                        BaseFont=pikepdf.Name("/Helvetica"),
                    )
                )
            )
            page.Contents = pikepdf.Stream(
                doc, b"BT /F1 20 Tf 200 440 Td (VISIBLE) Tj ET"
            )
            doc.save(path)
        with pikepdf.Pdf.open(path) as doc:
            media_mapper, _ = display_mapper(doc.pages[0])
            crop_mapper, _ = display_mapper(doc.pages[0], page_box="cropbox")
            word_quads = native_words_for_pages(path, [1], {1: media_mapper})[1]
            wrong_quads = native_words_for_pages(path, [1], {1: crop_mapper})[1]
        assert len(word_quads) == 1, word_quads
        left = min(point[0] for point in word_quads[0])
        wrong_left = min(point[0] for point in wrong_quads[0])
        assert abs(left - 200) < 1, (left, word_quads)
        assert abs(wrong_left - left) > 20, (wrong_left, left)


def test_rendered_pixel_alignment_all_quarter_turns():
    for rotation in (0, 90, 180, 270):
        with TemporaryDirectory(prefix="pdf-redox-mediabox-raster-") as td:
            path = Path(td) / f"rotation-{rotation}.pdf"
            with pikepdf.Pdf.new() as doc:
                page = doc.add_blank_page(page_size=(800, 600))
                page.mediabox = pikepdf.Array([30, 40, 830, 640])
                page.cropbox = pikepdf.Array([160, 370, 470, 540])
                page.obj["/Rotate"] = rotation
                page.Contents = pikepdf.Stream(doc, b"q 1 0 0 rg 200 400 60 30 re f Q")
                doc.save(path)
            with pikepdf.Pdf.open(path) as doc:
                inverse, angle = inverse_display_mapper(doc.pages[0])
                assert angle == rotation
                u, v = inverse(230, 415)
            ppm = subprocess.check_output(
                [
                    "pdftoppm",
                    "-f",
                    "1",
                    "-l",
                    "1",
                    "-scale-to",
                    "600",
                    "-singlefile",
                    str(path),
                ],
                stderr=subprocess.DEVNULL,
            )
            with Image.open(io.BytesIO(ppm)) as raster:
                width, height = raster.size
                point = raster.convert("RGB").getpixel(
                    (round(u * width), round(v * height))
                )
            assert point[0] > 240 and point[1] < 15 and point[2] < 15, (
                rotation,
                (u, v),
                point,
            )


if __name__ == "__main__":
    test_source_sampling_matches_poppler_mediabox()
    test_native_word_quads_match_poppler_nonzero_mediabox_origin()
    test_rendered_pixel_alignment_all_quarter_turns()
    print("PDF_MEDIABOX_GEOMETRY_TESTS_PASS")
