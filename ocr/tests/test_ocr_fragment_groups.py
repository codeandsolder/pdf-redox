# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10","pillow>=11","numpy>=2","opencv-python-headless>=4.11","rapidocr>=3,<4","onnxruntime>=1.21"]
# ///
"""Regress assembly and OCR of split PDF Image XObject raster text."""

from pathlib import Path
from tempfile import TemporaryDirectory
import hashlib
import sys

from PIL import Image, ImageDraw, ImageFont
import pikepdf

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from ocr_fragment_groups import pages_from_fragments
from raster_hybrid import run as run_hybrid
from pdf_image_placements import enumerate_image_paints
from reconstruct_raster_mosaic import cluster_native_raster_paints


def split_pdf(path, pages=1):
    source = Image.new("RGB", (1200, 300), "white")
    painter = ImageDraw.Draw(source)
    font = ImageFont.truetype(
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf", 96
    )
    painter.text((65, 87), "CAPACITOR 47uF 16V", font=font, fill="black")
    with pikepdf.Pdf.new() as pdf:
        resources = pikepdf.Dictionary()
        xobj = pikepdf.Dictionary()
        for i in range(100):
            segment = source.crop((12 * i, 0, 12 * (i + 1), 300))
            img = pikepdf.Stream(pdf, segment.tobytes())
            img["/Type"] = pikepdf.Name("/XObject")
            img["/Subtype"] = pikepdf.Name("/Image")
            img["/Width"] = 12
            img["/Height"] = 300
            img["/BitsPerComponent"] = 8
            img["/ColorSpace"] = pikepdf.Name("/DeviceRGB")
            name = f"/Stripe{i}"
            xobj[name] = pdf.make_indirect(img)
        resources["/XObject"] = xobj
        for n in range(pages):
            page = pdf.add_blank_page(page_size=(800, 500))
            page.obj["/Resources"] = resources
            code = [
                f"q 6 0 0 150 {100 + 6 * i + 10 * n} {150 + 12 * n} cm /Stripe{i} Do Q"
                for i in range(100)
            ]
            page.Contents = pikepdf.Stream(pdf, ("\n".join(code)).encode())
        pdf.save(path)


def test_native_striped_text():
    with TemporaryDirectory(prefix="pdf-redox-stripes-") as temp:
        path = Path(temp) / "striped.pdf"
        split_pdf(path)
        with pikepdf.Pdf.open(path) as pdf:
            placements, images, stats = enumerate_image_paints(pdf, [1])
            assert len(placements) == 100
            assert stats[0]["image_only_after_native"]
            groups = cluster_native_raster_paints(placements, [0, 0, 800, 500])
            assert len(groups) == 1, [
                (len(g["placements"]), g["region"]) for g in groups
            ]
            assert len(groups[0]["placements"]) == 100
            assert (
                1198
                <= round(
                    (groups[0]["region"][2] - groups[0]["region"][0])
                    / groups[0]["pitch"]
                )
                <= 1220
            )
        output, report = pages_from_fragments(path, [1], ocr_threads=4)
        assert report["summary"]["eligible_pages"] == [1], report
        assert report["summary"]["total_groups"] == 1
        lines = output["pages"][0]["lines"]
        assert lines, report
        text = " ".join(x["text"] for x in lines)
        assert "CAPACITOR" in text and "47uF" in text and "16V" in text, text
        assert all(not x["reviewed"] for x in lines)
        assert output["pdf_sha256"] == hashlib.sha256(path.read_bytes()).hexdigest()
        merged, dispatch = run_hybrid(path, [1], Path(temp), ocr_threads=4)
        assert dispatch["summary"]["fragment_pages"] == [1], dispatch
        assert [x["text"] for x in merged["pages"][0]["lines"]] == [
            x["text"] for x in lines
        ]
        assert merged["pdf_sha256"] == output["pdf_sha256"]


def test_repeated_fragment_group_cache():
    with TemporaryDirectory(prefix="pdf-redox-reused-fragments-") as temp:
        path = Path(temp) / "reused.pdf"
        split_pdf(path, pages=3)
        direct, report = pages_from_fragments(path, [1, 2, 3], ocr_threads=4)
        stats = report["summary"]
        assert stats["eligible_pages"] == [1, 2, 3], stats
        assert stats["total_groups"] == 3, stats
        assert stats["model_calls"] == 1, stats
        assert stats["recognized_group_cache_hits"] == 2, stats
        for pg in direct["pages"]:
            assert "CAPACITOR 47uF 16V" in " ".join(x["text"] for x in pg["lines"])
        merged, audit = run_hybrid(path, [1, 2, 3], Path(temp), ocr_threads=4)
        assert audit["summary"]["fragment_pages"] == [1, 2, 3], audit
        assert merged["pdf_sha256"] == direct["pdf_sha256"]


def test_mixed_fragment_page_fallback():
    with TemporaryDirectory(prefix="pdf-redox-fragment-mixed-") as temp:
        base = Path(temp) / "base.pdf"
        mixed = Path(temp) / "mixed.pdf"
        split_pdf(base, pages=2)
        with pikepdf.Pdf.open(base) as pdf:
            page = pdf.pages[1]
            old = page.Contents.read_bytes()
            page.Contents = pikepdf.Stream(pdf, b"q 0.7 g 25 25 60 40 re f Q\n" + old)
            pdf.save(mixed)
        output, report = run_hybrid(mixed, [1, 2], Path(temp), ocr_threads=4)
        summary = report["summary"]
        assert summary["fragment_pages"] == [1], summary
        assert summary["page_fallback_pages"] == [2], summary
        assert [p["page"] for p in output["pages"]] == [1, 2]
        assert all(
            "CAPACITOR" in " ".join(line["text"] for line in page["lines"])
            for page in output["pages"]
        )


def test_quarter_turn_fragment_group():
    with TemporaryDirectory(prefix="pdf-redox-rotated-fragments-") as temp:
        normal = Path(temp) / "normal.pdf"
        rotated = Path(temp) / "rotated.pdf"
        split_pdf(normal, pages=1)
        with pikepdf.Pdf.open(normal) as doc:
            pg = doc.pages[0]
            pg.mediabox = pikepdf.Array([0, 0, 800, 850])
            code = [
                f"q 0 6 -150 0 500 {70 + 6 * i} cm /Stripe{i} Do Q" for i in range(100)
            ]
            pg.Contents = pikepdf.Stream(doc, ("\n".join(code)).encode())
            doc.save(rotated)
        fragments, report = pages_from_fragments(
            rotated, [1], ocr_threads=4, min_score=0.97
        )
        assert report["summary"]["eligible_pages"] == [1], report
        line_text = " ".join(z["text"] for z in fragments["pages"][0]["lines"])
        assert "CAPACITOR" in line_text and "47uF" in line_text, line_text
        ocr, meta = run_hybrid(rotated, [1], Path(temp), ocr_threads=4)
        assert meta["summary"]["fragment_pages"] == [1], meta
        assert [z["text"] for z in ocr["pages"][0]["lines"]] == [
            z["text"] for z in fragments["pages"][0]["lines"]
        ]


if __name__ == "__main__":
    test_quarter_turn_fragment_group()
    test_mixed_fragment_page_fallback()
    test_repeated_fragment_group_cache()
    test_native_striped_text()
    print("FRAGMENTED_RASTER_SOURCE_OCR_PASS")
