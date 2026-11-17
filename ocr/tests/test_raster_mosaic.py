# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10","pillow>=11","numpy>=2","opencv-python-headless>=4.11","rapidocr>=3,<4","onnxruntime>=1.21"]
# ///
"""Source-pixel mosaic honors transformed m/l rectangle and negative re clips."""

from pathlib import Path
from tempfile import TemporaryDirectory
import sys

import numpy as np
from PIL import Image
import pikepdf

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from pdf_image_placements import enumerate_image_paints, rectangle_path_quad
from reconstruct_raster_mosaic import composite


def make_fixture(path):
    with pikepdf.Pdf.new() as doc:
        raw = Image.new("RGB", (120, 60), (0, 125, 215))
        img = pikepdf.Stream(doc, raw.tobytes())
        img["/Type"] = pikepdf.Name("/XObject")
        img["/Subtype"] = pikepdf.Name("/Image")
        img["/Width"] = 120
        img["/Height"] = 60
        img["/BitsPerComponent"] = 8
        img["/ColorSpace"] = pikepdf.Name("/DeviceRGB")
        img = doc.make_indirect(img)
        resources = pikepdf.Dictionary(XObject=pikepdf.Dictionary(Im0=img))
        lines = [
            b"q 80 20 m 320 20 l 320 140 l 80 140 l h W n 300 0 0 120 40 20 cm /Im0 Do Q",
            b"q 320 140 -240 -120 re W n 300 0 0 120 40 20 cm /Im0 Do Q",
        ]
        for content in lines:
            page = doc.add_blank_page(page_size=(400, 200))
            page.obj["/Resources"] = resources
            page.Contents = pikepdf.Stream(doc, content)
        doc.save(path)


def test_clips_and_mosaic():
    assert rectangle_path_quad([[0, 0], [10, 0], [10, 6], [0, 6]]) is not None
    assert rectangle_path_quad([[0, 0], [10, 0], [9, 6], [0, 6]]) is None
    with TemporaryDirectory(prefix="pdf-redox-native-mosaic-") as td:
        path = Path(td) / "fixture.pdf"
        make_fixture(path)
        with pikepdf.Pdf.open(path) as doc:
            layouts = []
            for n in (1, 2):
                placements, images, _ = enumerate_image_paints(doc, [n])
                assert len(placements) == 1
                assert not placements[0]["unsafe_reasons"], placements[0][
                    "unsafe_reasons"
                ]
                bitmap, mask, report = composite(
                    placements, images, [0, 0, 400, 200], pitch=1
                )
                samples = np.asarray(bitmap)
                assert all(samples[100, 50] == [255, 255, 255])
                assert all(samples[100, 100] == [0, 125, 215])
                assert all(samples[100, 335] == [255, 255, 255])
                assert report["paint_statistics"]["painted_instances"] == 1
                layouts.append(np.asarray(bitmap).copy())
            assert np.array_equal(layouts[0], layouts[1])


if __name__ == "__main__":
    test_clips_and_mosaic()
    print("PDF_NATIVE_RASTER_MOSAIC_CLIPPING_PASS")
