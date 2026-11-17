# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9", "pillow>=11", "numpy>=2", "opencv-python-headless>=4.11"]
# ///
"""Structure-guided PaddleOCR-VL escalation on specific PDF line crops.

prepare on Acer -> infer on Asus -> merge on Acer. All stages are bound to
the optimized PDF, and candidate text is always marked unreviewed with
confidence 0. This adapter does not modify the original PDF.
"""

import argparse
import base64
from collections import defaultdict
import hashlib
import io
import json
import math
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import time
from urllib.request import Request, urlopen

import cv2
import numpy as np
import pikepdf
from PIL import Image
from mask_native_text import mask_pdf
from pdf_geometry import inverse_display_mapper
from ocr_text_equivalence import meaningful_change


def sha(path):
    with path.open("rb") as fp:
        return hashlib.file_digest(fp, "sha256").hexdigest()


def inside(point, quad, pad=0.08):
    p = quad[0]
    u = [quad[1][i] - p[i] for i in range(2)]
    v = [quad[3][i] - p[i] for i in range(2)]
    d = u[0] * v[1] - u[1] * v[0]
    if abs(d) < 1e-10:
        return False
    dx, dy = point[0] - p[0], point[1] - p[1]
    a = (dx * v[1] - dy * v[0]) / d
    b = (u[0] * dy - u[1] * dx) / d
    return -pad <= a <= 1 + pad and -pad <= b <= 1 + pad


def convex_intersects(a, b, tolerance=0.2):
    """Convex PDF quads overlap enough to risk duplicating existing text."""
    for poly in (a, b):
        if len(poly) != 4:
            raise ValueError("invalid PDF word/line polygon")
        for i in range(4):
            p, q = poly[i], poly[(i + 1) % 4]
            ex, ey = q[0] - p[0], q[1] - p[1]
            length = math.hypot(ex, ey)
            if length < 1e-10:
                raise ValueError("degenerate PDF text polygon")
            nx, ny = -ey / length, ex / length
            av = [v[0] * nx + v[1] * ny for v in a]
            bv = [v[0] * nx + v[1] * ny for v in b]
            if min(max(av), max(bv)) - max(min(av), min(bv)) <= tolerance:
                return False
    return True


def glyph_index(catalog, labels):
    shapes = defaultdict(list)
    overrides = {}
    for row in labels.get("overrides", []):
        key = (row["glyph_id"], row["page"], row["index_on_page"])
        if key in overrides:
            raise ValueError("duplicate occurrence Unicode override")
        overrides[key] = row["text"]
    valid_occurrences = set()
    for template in catalog["glyphs"]:
        gid = template["id"]
        for occ in template["occurrences"]:
            key = (gid, occ["page"], occ["index_on_page"])
            if key in valid_occurrences:
                raise ValueError("duplicate glyph catalog occurrence")
            valid_occurrences.add(key)
            quad = occ["page_quad"]
            point = [sum(p[i] for p in quad) / 4 for i in range(2)]
            label = overrides.get(key, labels["labels"].get(gid))
            shapes[occ["page"]].append((point, bool(label)))
    if set(overrides) - valid_occurrences:
        raise ValueError("unknown glyph occurrence override")
    return shapes


def choose(
    draft,
    catalog,
    labels,
    pages=None,
    low_score=0.97,
    min_unresolved=5,
    limit=12,
    min_chars=4,
):
    if not 0 <= low_score <= 1 or min_unresolved < 0 or limit < 1 or min_chars < 1:
        raise ValueError("invalid OCR selection policy")
    glyphs = glyph_index(catalog, labels)
    candidates = []
    seen = set()
    for page in draft["pages"]:
        n = page["page"]
        if n < 1 or n in seen:
            raise ValueError("invalid/duplicate source OCR page")
        seen.add(n)
        if pages is not None and n not in pages:
            continue
        for i, line in enumerate(page["lines"]):
            conf = line["confidence"]
            if not isinstance(conf, (float, int)) or not math.isfinite(conf):
                raise ValueError("nonfinite OCR confidence")
            if len(line["text"].strip()) < min_chars:
                continue
            if any(
                convex_intersects(line["quad"], quad)
                for quad in page.get("native_text_regions", [])
            ):
                continue
            known = unknown = 0
            for point, labeled in glyphs.get(n, []):
                if inside(point, line["quad"]):
                    if labeled:
                        known += 1
                    else:
                        unknown += 1
            if conf > low_score and unknown < min_unresolved:
                continue
            score = round(2 * (1 - conf) + min(0.5, 0.015 * unknown), 6)
            candidates.append(
                {
                    "page": n,
                    "line_index": i,
                    "primary_text": line["text"],
                    "primary_score": conf,
                    "quad": line["quad"],
                    "known_components": known,
                    "unresolved_components": unknown,
                    "selection_score": score,
                }
            )
    candidates.sort(key=lambda x: (-x["selection_score"], x["page"], x["line_index"]))
    return candidates[:limit]


def crop(image, quad, inverse, upscale):
    h, w = image.shape[:2]
    corners = []
    for point in (quad[3], quad[2], quad[1], quad[0]):
        u, v = inverse(*point)
        if (
            not math.isfinite(u)
            or not math.isfinite(v)
            or not (-0.05 <= u <= 1.05 and -0.05 <= v <= 1.05)
        ):
            raise ValueError("text line outside PDF CropBox")
        corners.append(np.asarray((u * w, v * h), np.float32))
    tl, tr, br, bl = corners
    horizontal = tr - tl
    vertical = bl - tl
    width = float(np.linalg.norm(horizontal))
    height = float(np.linalg.norm(vertical))
    if not 2 <= width <= 4000 or not 2 <= height <= 1000:
        raise ValueError("invalid OCR text line dimensions")
    horizontal /= width
    vertical /= height
    px = max(3, height * 0.20)
    py = max(3, height * 0.20)
    src = np.asarray(
        [
            tl - px * horizontal - py * vertical,
            tr + px * horizontal - py * vertical,
            br + px * horizontal + py * vertical,
            bl - px * horizontal + py * vertical,
        ],
        np.float32,
    )
    dw = max(1, round(width + 2 * px))
    dh = max(1, round(height + 2 * py))
    dst = np.asarray([(0, 0), (dw, 0), (dw, dh), (0, dh)], np.float32)
    result = cv2.warpPerspective(
        image,
        cv2.getPerspectiveTransform(src, dst),
        (dw, dh),
        flags=cv2.INTER_CUBIC,
        borderMode=cv2.BORDER_CONSTANT,
        borderValue=(255, 255, 255),
    )
    if upscale > 1:
        result = cv2.resize(
            result, (dw * upscale, dh * upscale), interpolation=cv2.INTER_CUBIC
        )
    return result


def prepare(
    pdf,
    draft,
    catalog,
    labels,
    folder,
    pages,
    threshold,
    unknown,
    limit,
    max_side,
    upscale,
    min_chars=4,
):
    if draft["schema_version"] != 3 or not str(draft["model"]).startswith("RapidOCR"):
        raise ValueError("expected RapidOCR v3 OCR draft")
    if catalog["schema_version"] != 2 or labels["schema_version"] != 1:
        raise ValueError("unexpected catalog/label schema")
    digest = sha(pdf)
    if digest != draft["pdf_sha256"] or digest != catalog["optimized_pdf_sha256"]:
        raise ValueError("PDF/OCR/catalog checksum mismatch")
    if not 500 <= max_side <= 5000 or not 1 <= upscale <= 5:
        raise ValueError("bad source raster resolution")
    known = {x["id"] for x in catalog["glyphs"]}
    if set(labels["labels"]) - known:
        raise ValueError("unknown Unicode glyph labels")
    items = choose(draft, catalog, labels, pages, threshold, unknown, limit, min_chars)
    folder.mkdir(parents=True, exist_ok=True)
    page_data = {p["page"]: p for p in draft["pages"]}
    with pikepdf.Pdf.open(pdf) as doc:
        selected = sorted({x["page"] for x in items})
        if selected and selected[-1] > len(doc.pages):
            raise ValueError("unknown PDF page in OCR draft")
        with TemporaryDirectory(prefix="pdf-vlm-mask-", dir=folder) as scratch:
            render_pdf = pdf
            masked = [n for n in selected if page_data[n]["native_text_regions"]]
            if masked:
                render_pdf = Path(scratch) / "native-hidden.pdf"
                mask_pdf(pdf, render_pdf, pages=masked)
            for n in selected:
                inverse, _ = inverse_display_mapper(doc.pages[n - 1])
                ppm = subprocess.check_output(
                    [
                        "pdftoppm",
                        "-f",
                        str(n),
                        "-l",
                        str(n),
                        "-scale-to",
                        str(max_side),
                        "-singlefile",
                        str(render_pdf),
                    ],
                    stderr=subprocess.DEVNULL,
                )
                with Image.open(io.BytesIO(ppm)) as img:
                    rgb = np.asarray(img.convert("RGB"))
                for row in items:
                    if row["page"] != n:
                        continue
                    filename = f"p{n:04d}-l{row['line_index']:04d}.png"
                    output = folder / filename
                    Image.fromarray(crop(rgb, row["quad"], inverse, upscale)).save(
                        output
                    )
                    row["filename"] = filename
                    row["crop_sha256"] = sha(output)
    data = {
        "schema_version": 1,
        "pdf_sha256": digest,
        "source_model": draft["model"],
        "source_lines": sum(len(p["lines"]) for p in draft["pages"]),
        "policy": {
            "score_threshold": threshold,
            "unresolved_threshold": unknown,
            "min_chars": min_chars,
            "limit": limit,
            "max_side": max_side,
            "upscale": upscale,
        },
        "regions": items,
    }
    (folder / "manifest.json").write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n"
    )
    return data


def infer(manifest, folder, endpoint, output, max_tokens):
    if manifest.get("schema_version") != 1 or not 32 <= max_tokens <= 2048:
        raise ValueError("invalid manifest or token budget")
    result = {
        "schema_version": 1,
        "pdf_sha256": manifest["pdf_sha256"],
        "model": "PaddleOCR-VL-1.6-GGUF on llama.cpp",
        "proposals": [],
    }
    for region in manifest["regions"]:
        path = folder / region["filename"]
        if sha(path) != region["crop_sha256"]:
            raise ValueError("crop checksum mismatch")
        image_url = (
            "data:image/png;base64," + base64.b64encode(path.read_bytes()).decode()
        )
        payload = {
            "model": "PaddleOCR-VL-1.6",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "OCR:"},
                        {"type": "image_url", "image_url": {"url": image_url}},
                    ],
                }
            ],
            "temperature": 0,
            "max_tokens": max_tokens,
            "stream": False,
        }
        req = Request(
            endpoint,
            json.dumps(payload).encode(),
            {"Content-Type": "application/json"},
            method="POST",
        )
        start = time.perf_counter()
        with urlopen(req, timeout=240) as response:
            raw = json.load(response)
        if "error" in raw:
            raise RuntimeError(raw["error"])
        result["proposals"].append(
            {
                "page": region["page"],
                "line_index": region["line_index"],
                "crop_sha256": region["crop_sha256"],
                "primary_text": region["primary_text"],
                "vlm_text": raw["choices"][0]["message"].get("content"),
                "inference_seconds": round(time.perf_counter() - start, 3),
                "finish_reason": raw["choices"][0].get("finish_reason"),
                "usage": raw.get("usage"),
            }
        )
        row = result["proposals"][-1]
        print(
            "VLM",
            row["page"],
            row["line_index"],
            row["inference_seconds"],
            repr(row["primary_text"]),
            "=>",
            repr(row["vlm_text"]),
            flush=True,
        )
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    return result


def merge(draft, manifest, vlm):
    if (
        draft["schema_version"] != 3
        or manifest["schema_version"] != 1
        or vlm["schema_version"] != 1
    ):
        raise ValueError("unsupported source schema")
    if not str(vlm.get("model", "")).startswith("PaddleOCR-VL"):
        raise ValueError("unexpected VLM model provenance")
    if not (draft["pdf_sha256"] == manifest["pdf_sha256"] == vlm["pdf_sha256"]):
        raise ValueError("VLM proposal source digest mismatch")
    candidate = {(r["page"], r["line_index"]): r for r in manifest["regions"]}
    if len(candidate) != len(manifest["regions"]):
        raise ValueError("duplicate crop region")
    output = json.loads(json.dumps(draft))
    output["model"] = (
        str(draft["model"]) + " + research-only PaddleOCR-VL-1.6 hypotheses"
    )
    bypage = {p["page"]: p for p in output["pages"]}
    if len(bypage) != len(output["pages"]):
        raise ValueError("duplicate source OCR pages")
    for p in output["pages"]:
        for line in p["lines"]:
            line["reviewed"] = False
            line["confidence"] = 0.0
    audited = []
    seen = set()
    for row in vlm["proposals"]:
        key = (row["page"], row["line_index"])
        if key in seen or key not in candidate:
            raise ValueError("duplicate/unknown VLM result")
        seen.add(key)
        region = candidate[key]
        if (
            row["crop_sha256"] != region["crop_sha256"]
            or row["primary_text"] != region["primary_text"]
        ):
            raise ValueError("stale VLM crop evidence")
        page = bypage.get(key[0])
        if page is None or not 0 <= key[1] < len(page["lines"]):
            raise ValueError("unknown VLM line index")
        line = page["lines"][key[1]]
        if line["text"] != row["primary_text"] or line["quad"] != region["quad"]:
            raise ValueError("VLM line geometry or source text changed")
        alternate = row["vlm_text"]
        usable = (
            row.get("finish_reason") not in ("length", "content_filter", "error")
            and isinstance(alternate, str)
            and bool(alternate.strip())
            and len(alternate.splitlines()) == 1
            and all(ord(c) >= 32 for c in alternate)
        )
        normalized_equivalent = bool(
            usable and not meaningful_change(line["text"], alternate)
        )
        if usable and not normalized_equivalent:
            line["text"] = alternate.strip()
        audited.append(
            {
                "page": key[0],
                "line_index": key[1],
                "original": row["primary_text"],
                "vlm": alternate,
                "single_line": usable,
                "normalized_equivalent": normalized_equivalent,
                "changed": bool(usable and not normalized_equivalent),
            }
        )
    return output, audited


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)
    a = sub.add_parser("prepare")
    for name in ("pdf", "ocr", "catalog", "labels", "output-dir"):
        a.add_argument("--" + name, type=Path, required=True)
    a.add_argument("--pages")
    a.add_argument("--max-regions", type=int, default=12)
    a.add_argument("--min-chars", type=int, default=4)
    a.add_argument("--max-primary-confidence", type=float, default=0.97)
    a.add_argument("--min-unresolved-components", type=int, default=5)
    a.add_argument("--max-side", type=int, default=2000)
    a.add_argument("--upscale", type=int, default=2)
    b = sub.add_parser("infer")
    b.add_argument("--crop-dir", type=Path, required=True)
    b.add_argument("--output", type=Path, required=True)
    b.add_argument("--endpoint", default="http://127.0.0.1:18754/v1/chat/completions")
    b.add_argument("--max-tokens", type=int, default=300)
    c = sub.add_parser("merge")
    c.add_argument("--ocr", type=Path, required=True)
    c.add_argument("--manifest", type=Path, required=True)
    c.add_argument("--vlm", type=Path, required=True)
    c.add_argument("--output", type=Path, required=True)
    c.add_argument("--audit", type=Path, required=True)
    args = p.parse_args()
    if args.command == "prepare":
        data = prepare(
            args.pdf,
            json.loads(args.ocr.read_text()),
            json.loads(args.catalog.read_text()),
            json.loads(args.labels.read_text()),
            args.output_dir,
            {int(v) for v in args.pages.split(",")} if args.pages else None,
            args.max_primary_confidence,
            args.min_unresolved_components,
            args.max_regions,
            args.max_side,
            args.upscale,
            args.min_chars,
        )
        print("REGIONS", len(data["regions"]), "SOURCE_LINES", data["source_lines"])
        for row in data["regions"]:
            print(
                row["page"],
                row["line_index"],
                "score",
                row["primary_score"],
                "unresolved",
                row["unresolved_components"],
                repr(row["primary_text"]),
            )
    elif args.command == "infer":
        infer(
            json.loads((args.crop_dir / "manifest.json").read_text()),
            args.crop_dir,
            args.endpoint,
            args.output,
            args.max_tokens,
        )
    else:
        result, audit = merge(
            json.loads(args.ocr.read_text()),
            json.loads(args.manifest.read_text()),
            json.loads(args.vlm.read_text()),
        )
        for filename, data in ((args.output, result), (args.audit, audit)):
            filename.parent.mkdir(parents=True, exist_ok=True)
            filename.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
        print(
            "MERGED",
            len(audit),
            "USABLE",
            sum(a["single_line"] for a in audit),
            "CHANGED",
            sum(a["changed"] for a in audit),
        )


if __name__ == "__main__":
    main()
