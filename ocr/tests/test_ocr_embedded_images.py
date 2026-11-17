# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10","pillow>=11","rapidocr>=3,<4","onnxruntime>=1.21"]
# ///
"""Real-PDF raster source recognition, image reuse, transforms and clipping."""

from pathlib import Path
from tempfile import TemporaryDirectory
import hashlib

from PIL import Image, ImageDraw, ImageFont
import pikepdf

import sys

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from pdf_image_placements import (
    IDENTITY,
    concat,
    transform,
    inside_convex,
    rect_quad,
    image_quad,
    word_visible,
    enumerate_image_paints,
)
from ocr_embedded_images import recognize, decoded_image


def fixture(path):
    with pikepdf.Pdf.new() as pdf:
        img = Image.new("RGB", (1200, 300), "white")
        painter = ImageDraw.Draw(img)
        font_path = "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf"
        font = ImageFont.truetype(font_path, 96)
        painter.text((65, 87), "CAPACITOR 47uF 16V", font=font, fill="black")
        raw = img.tobytes()
        stream = pikepdf.Stream(pdf, raw)
        stream["/Type"] = pikepdf.Name("/XObject")
        stream["/Subtype"] = pikepdf.Name("/Image")
        stream["/Width"] = img.width
        stream["/Height"] = img.height
        stream["/BitsPerComponent"] = 8
        stream["/ColorSpace"] = pikepdf.Name("/DeviceRGB")
        stream = pdf.make_indirect(stream)
        image_resource = pikepdf.Dictionary(Im0=stream)
        res = pikepdf.Dictionary(XObject=image_resource)
        p1 = pdf.add_blank_page(page_size=(800, 600))
        p1.obj["/Resources"] = res
        p1.Contents = pikepdf.Stream(pdf, b"q 500 0 0 125 80 400 cm /Im0 Do Q")
        p2 = pdf.add_blank_page(page_size=(800, 600))
        p2.obj["/Resources"] = res
        p2.Contents = pikepdf.Stream(pdf, b"q 0 500 -125 0 540 45 cm /Im0 Do Q")
        # Nested form with an inherited /Resources and an independent /Matrix.
        form = pikepdf.Stream(pdf, b"q 500 0 0 125 50 50 cm /Im0 Do Q")
        form["/Type"] = pikepdf.Name("/XObject")
        form["/Subtype"] = pikepdf.Name("/Form")
        form["/BBox"] = pikepdf.Array([0, 0, 650, 300])
        form["/Matrix"] = pikepdf.Array([1, 0, 0, 1, 20, 30])
        form = pdf.make_indirect(form)
        p3 = pdf.add_blank_page(page_size=(800, 600))
        p3.obj["/Resources"] = pikepdf.Dictionary(
            XObject=pikepdf.Dictionary(Im0=stream, Fm0=form)
        )
        p3.Contents = pikepdf.Stream(pdf, b"q 1 0 0 1 0 0 cm /Fm0 Do Q")
        p4 = pdf.add_blank_page(page_size=(800, 600))
        p4.obj["/Resources"] = res
        p4.Contents = pikepdf.Stream(
            pdf, b"q 80 400 150 125 re W n 500 0 0 125 80 400 cm /Im0 Do Q"
        )
        p5 = pdf.add_blank_page(page_size=(800, 600))
        p5.obj["/Resources"] = res
        p5.Contents = pikepdf.Stream(
            pdf, b"q 500 0 0 125 80 400 cm /Im0 Do Q 20 30 80 10 re f"
        )
        p6 = pdf.add_blank_page(page_size=(800, 600))
        p6.obj["/Resources"] = res
        p6.Contents = pikepdf.Stream(pdf, b"q 500 0 0 125 80 400 cm /Im0 Do Q")
        p6.obj["/Annots"] = pikepdf.Array(
            [
                pikepdf.Dictionary(
                    Type=pikepdf.Name("/Annot"),
                    Subtype=pikepdf.Name("/FreeText"),
                    Rect=pikepdf.Array([300, 100, 400, 120]),
                )
            ]
        )
        pdf.save(path, compress_streams=True)


def test_affine():
    left = (1, 2, 3, 4, 5, 6)
    right = (7, 8, 9, 10, 11, 12)
    point = (0.3, 0.7)
    expected = transform(left, *transform(right, *point))
    actual = transform(concat(left, right), *point)
    assert all(abs(x - y) < 1e-9 for x, y in zip(actual, expected))
    assert inside_convex((1, 1), rect_quad(IDENTITY, (0, 0, 2, 2)))
    assert not inside_convex((3, 1), rect_quad(IDENTITY, (0, 0, 2, 2)))


def test_image_placements_and_ocr():
    with TemporaryDirectory(prefix="pdf-redox-embedded-images-test-") as td:
        p = Path(td) / "scans.pdf"
        fixture(p)
        with pikepdf.Pdf.open(p) as doc:
            places, images, summaries = enumerate_image_paints(doc, [1, 2, 3, 4, 5, 6])
            assert len(places) == 6
            assert len(images) == 1, images
            assert all(x["source_id"] == places[0]["source_id"] for x in places)
            assert all(x["width"] == 1200 and x["height"] == 300 for x in places)
            assert not any(x["unsafe_reasons"] for x in places)
            assert [x["image_only_after_native"] for x in summaries] == [
                True,
                True,
                True,
                True,
                False,
                False,
            ], summaries
            q = image_quad(places[0], [(0, 0), (1200, 0), (1200, 300), (0, 300)])
            assert abs(q[0][0] - 80) < 1e-7 and abs(q[0][1] - 400) < 1e-7, q
            assert abs(q[1][0] - 580) < 1e-7 and abs(q[2][1] - 525) < 1e-7, q
            rotated = image_quad(places[1], [(0, 0), (1200, 0), (1200, 300), (0, 300)])
            assert abs(rotated[0][0] - 540) < 1e-7
            assert abs(rotated[1][1] - 545) < 1e-7
            form = image_quad(places[2], [(0, 0), (1200, 0), (1200, 300), (0, 300)])
            assert abs(form[0][0] - 70) < 1e-7 and abs(form[0][1] - 80) < 1e-7, form
            assert not word_visible(q, places[3])
            assert word_visible(q, places[0])
            img = decoded_image(images[places[0]["source_id"]])
            assert img.size == (1200, 300) and img.mode == "RGB"
        doc, report, engine = recognize(p, [1, 2, 3, 4, 5, 6], ocr_threads=4)
        assert engine is not None
        summary = report["summary"]
        assert summary["ocr_inference_calls"] == 1, summary
        assert summary["image_placements"] == 6
        assert summary["source_objects_used"] == 1
        pages = {page["page"]: page for page in doc["pages"]}
        for n in (1, 2, 3):
            assert pages[n]["lines"], f"native image OCR did not detect page {n}"
            txt = " ".join(x["text"] for x in pages[n]["lines"]).upper()
            assert "CAPACITOR" in txt and "47" in txt, txt
        assert not pages[4]["lines"], "clipped text must not leak outside PDF clip"
        assert all(x["page"] > 0 for x in doc["pages"])
        assert doc["pdf_sha256"] == hashlib.sha256(p.read_bytes()).hexdigest()
        assert all(not x["reviewed"] for page in doc["pages"] for x in page["lines"])


if __name__ == "__main__":
    test_affine()
    test_image_placements_and_ocr()
    print("EMBEDDED_RASTER_OCR_TESTS_PASS")
