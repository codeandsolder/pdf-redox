"""Native searchable PDF word geometry from batched Poppler XHTML.

The same PDF-space geometry is used by raster OCR and structural OCR so the
final invisible-text overlay cannot duplicate already searchable native text.
"""

import re
import subprocess
import xml.etree.ElementTree as ET

_INVALID_XML_CONTROLS = re.compile(rb"[\x00-\x08\x0b\x0c\x0e-\x1f]")


def _parse_bbox_xhtml(data):
    return ET.fromstring(_INVALID_XML_CONTROLS.sub(b"", data))


def _extract_native_word_quads(element, transform):
    width, height = float(element.attrib["width"]), float(element.attrib["height"])
    if width <= 0 or height <= 0:
        raise ValueError("invalid native-text page dimensions")
    quads = []
    for word in element.findall("./{*}word"):
        if not (word.text or "").strip():
            continue
        x0, y0, x1, y1 = (
            float(word.attrib[k]) for k in ("xMin", "yMin", "xMax", "yMax")
        )
        if x1 - x0 < 0.25 or y1 - y0 < 0.25:
            continue
        quads.append(
            [
                transform(x0 / width, y1 / height),
                transform(x1 / width, y1 / height),
                transform(x1 / width, y0 / height),
                transform(x0 / width, y0 / height),
            ]
        )
    return quads


def native_words_for_pages(pdf_path, selected, transforms, chunk_size=24):
    """Extract native word boxes for arbitrary pages, batching contiguous runs."""
    if chunk_size < 1:
        raise ValueError("chunk_size must be positive")
    if not selected or len(set(selected)) != len(selected):
        raise ValueError("no pages or duplicate pages")
    ordered = sorted(selected)
    runs = []
    run = [ordered[0]]
    for num in ordered[1:]:
        if num == run[-1] + 1 and len(run) < chunk_size:
            run.append(num)
        else:
            runs.append(run)
            run = [num]
    runs.append(run)
    result = {}
    for run in runs:
        xml = subprocess.check_output(
            [
                "pdftotext",
                "-bbox",
                "-f",
                str(run[0]),
                "-l",
                str(run[-1]),
                str(pdf_path),
                "-",
            ],
            stderr=subprocess.DEVNULL,
        )
        elements = _parse_bbox_xhtml(xml).findall(".//{*}page")
        if len(elements) != len(run):
            raise ValueError(
                f"unexpected native text pages: requested {len(run)}, got {len(elements)}"
            )
        for number, element in zip(run, elements):
            result[number] = _extract_native_word_quads(element, transforms[number])
    return result
