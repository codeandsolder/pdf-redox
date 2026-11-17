# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10", "pillow>=11", "numpy>=2"]
# ///
"""Exact PDF paint aliasing: page, reused resource, nested repeated Forms."""

import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory
import zlib

import numpy as np
import pikepdf
from PIL import Image, ImageDraw, ImageFont

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from mask_paint_invocations import rewrite_masked_paints
from pdf_image_placements import enumerate_image_paints


def _image_pdf(path, depth=0):
    doc = pikepdf.Pdf.new()
    page = doc.add_blank_page(page_size=(760, 620))
    other = doc.add_blank_page(page_size=(760, 620))
    bitmap = Image.new("RGB", (560, 160), "white")
    ImageDraw.Draw(bitmap).text(
        (24, 45),
        "CAPACITOR 47uF 16V",
        font=ImageFont.load_default(size=35),
        fill="black",
    )
    image = pikepdf.Stream(
        doc,
        zlib.compress(bitmap.tobytes(), 6),
        filter=pikepdf.Name("/FlateDecode"),
    )
    image["/Type"] = pikepdf.Name("/XObject")
    image["/Subtype"] = pikepdf.Name("/Image")
    image["/Width"] = 560
    image["/Height"] = 160
    image["/BitsPerComponent"] = 8
    image["/ColorSpace"] = pikepdf.Name("/DeviceRGB")
    image_id = f"{image.objgen[0]}:{image.objgen[1]}"

    resource = image
    for index in range(depth):
        form = pikepdf.Stream(doc, f"/N{index} Do".encode())
        form["/Type"] = pikepdf.Name("/XObject")
        form["/Subtype"] = pikepdf.Name("/Form")
        form["/BBox"] = pikepdf.Array([0, 0, 1, 1])
        inner_xobjects = pikepdf.Dictionary()
        inner_xobjects[pikepdf.Name(f"/N{index}")] = resource
        form["/Resources"] = pikepdf.Dictionary(XObject=inner_xobjects)
        resource = form
    name = pikepdf.Name("/Paint")
    xobjects = pikepdf.Dictionary()
    xobjects[name] = resource
    for entry in (page, other):
        entry.obj["/Resources"] = pikepdf.Dictionary(XObject=xobjects)
    # Exact same occurrence painted twice; the upper paint hides the lower.
    page.Contents = pikepdf.Stream(
        doc,
        b"q 340 0 0 140 80 360 cm /Paint Do Q\n"
        b"q 340 0 0 140 80 360 cm /Paint Do Q\n"
        b"q 0.2 g 20 30 100 20 re f Q\n",
    )
    other.Contents = pikepdf.Stream(
        doc,
        b"q 350 0 0 130 140 280 cm /Paint Do Q\n",
    )
    doc.save(path)
    return image_id


def _mask_source(control, masked, image_id):
    number, generation = map(int, image_id.split(":"))
    with pikepdf.Pdf.open(control) as doc:
        image = doc.get_object(number, generation)
        decoded = image.read_bytes()
        image.write(
            zlib.compress(bytes([255]) * len(decoded), 6),
            filter=pikepdf.Name("/FlateDecode"),
        )
        doc.save(masked)


def _raster(pdf, page=1):
    response = subprocess.run(
        [
            "pdftoppm",
            "-f",
            str(page),
            "-l",
            str(page),
            "-scale-to",
            "1200",
            "-singlefile",
            str(pdf),
        ],
        check=True,
        capture_output=True,
    )
    from io import BytesIO

    return np.array(Image.open(BytesIO(response.stdout)).convert("RGB"))


def _exercise(depth):
    with TemporaryDirectory(prefix=f"mask-paint-depth-{depth}-") as td:
        root = Path(td)
        control = root / "control.pdf"
        masked = root / "masked-all.pdf"
        _image_pdf(control, depth)
        with pikepdf.Pdf.open(control) as doc:
            placements, _, summary = enumerate_image_paints(doc, [1, 2])
        first = [p for p in placements if p["page"] == 1]
        src = first[0]["source_id"]
        _mask_source(control, masked, src)
        assert [p["paint_index"] for p in first] == [0, 1], first
        assert all(p["source_id"] == src for p in first), first
        assert [p["paint_index"] for p in placements if p["page"] == 2] == [0]
        report = {
            "placements": [
                {
                    "page": p["page"],
                    "source_id": p["source_id"],
                    "paint_index": p["paint_index"],
                    "status": "recognized_at_native_source_resolution",
                }
                for p in placements
            ]
        }
        lower = root / "masked-lower.pdf"
        upper = root / "masked-upper.pdf"
        both = root / "masked-both.pdf"
        for indices, out in [([0], lower), ([1], upper), ([0, 1], both)]:
            result = rewrite_masked_paints(control, masked, report, 1, indices, out)
            assert result["paint_indices"] == indices, result
            with pikepdf.Pdf.open(out) as updated:
                after, _, s = enumerate_image_paints(updated, [1, 2])
                assert len(after) == len(placements), (after, placements)
                assert s[0]["image_paints"] == 2
        original = _raster(control)
        # Cloning a Form may shift a handful of Poppler antialias pixels
        # at the edges even when the lower opaque paint is completely
        # covered. Such artifacts must stay far below any OCR text proof.
        lower_delta = np.max(
            np.abs(_raster(lower).astype(np.int16) - original.astype(np.int16)),
            axis=2,
        )
        assert np.count_nonzero(lower_delta > 8) < 8, (
            "masked lower paint produced too many visible differences",
            depth,
            int(np.count_nonzero(lower_delta > 8)),
        )
        assert not np.array_equal(_raster(upper), original), (
            "changing upper image paint must alter final pixels",
            depth,
        )
        assert not np.array_equal(_raster(both), original)
        assert np.array_equal(_raster(control, 2), _raster(upper, 2)), (
            "another page changed due to mutated shared resources",
            depth,
        )
        print(
            "PER_PAINT_REWRITING_PASS",
            depth,
            "image_placements",
            len(placements),
            "upper_pixels_changed",
            int(
                np.count_nonzero(
                    np.max(
                        np.abs(original.astype("i2") - _raster(upper).astype("i2")),
                        axis=2,
                    )
                    > 8,
                )
            ),
            flush=True,
        )


def test_simple_repeated_image():
    _exercise(0)


def test_form_reused_twice():
    _exercise(1)


def test_two_nested_forms_reused_twice():
    _exercise(2)


if __name__ == "__main__":
    test_simple_repeated_image()
    test_form_reused_twice()
    test_two_nested_forms_reused_twice()
    print("PER_PAINT_RESOURCE_CLONE_TESTS_PASS")
