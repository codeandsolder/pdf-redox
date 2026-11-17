# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=10.10","pillow>=11","numpy>=2","rapidocr>=3,<4","onnxruntime>=1.21"]
# ///
"""Source-raster text suppression must not expose hidden PDF image lettering."""

from pathlib import Path
from tempfile import TemporaryDirectory
import sys
import json
import re
import subprocess
from unittest import mock

import pikepdf

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
sys.path.insert(0, str(Path(__file__).resolve().parent))
from mask_raster_text import (
    prepare_render_image_pdf,
    verify_full_page_reference,
    verify_rendered_image_text,
    _plan_visibility_crops,
    _page_pixels,
    _ambiguous_source_text_masks,
    _convex_quads_intersect,
)
from ocr_embedded_images import recognize
from raster_hybrid import run as hybrid_run
from test_ocr_embedded_images import fixture


def _assert_single_semantic_pdf_line(source, draft_data, root):
    """Exercise the Rust release importer and Poppler, not just the draft."""
    draft = Path(root) / "paint-isolated-semantic.json"
    annotated = Path(root) / "paint-isolated-semantic.pdf"
    draft.write_text(json.dumps(draft_data))
    subprocess.run(
        [
            str(
                Path(__file__).resolve().parents[2]
                / "target/release/pdf-redox-ocr-overlay"
            ),
            str(source),
            "--ocr",
            str(draft),
            "--apply-unreviewed",
            "--min-confidence",
            "0.97",
            "-o",
            str(annotated),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    with pikepdf.Pdf.open(annotated) as semantic:
        encoded = re.findall(
            rb"/ActualText\s*<([0-9A-Fa-f]+)>",
            semantic.pages[0].Contents.read_bytes(),
        )
        actual = [bytes.fromhex(c.decode()).decode("utf-16") for c in encoded]
        assert actual.count("CAPACITOR 47uF 16V") == 1, actual
    extracted = subprocess.run(
        [
            "pdftotext",
            "-f",
            "1",
            "-l",
            "1",
            "-raw",
            str(annotated),
            "-",
        ],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    assert extracted.count("CAPACITOR 47uF 16V") == 1, extracted

    def raster(path):
        return subprocess.run(
            [
                "pdftoppm",
                "-f",
                "1",
                "-l",
                "1",
                "-r",
                "150",
                "-singlefile",
                str(path),
            ],
            check=True,
            capture_output=True,
        ).stdout

    assert raster(source) == raster(annotated), "semantic overlay changed PDF ink"


def test_fully_occluded_source_text():
    with TemporaryDirectory(prefix="pdf-redox-occluded-image-") as root:
        base = Path(root) / "image-source.pdf"
        covered = Path(root) / "image-covered.pdf"
        masked = Path(root) / "image-covered-render-only.pdf"
        fixture(base)
        with pikepdf.Pdf.open(base) as doc:
            page = doc.pages[0]
            original = page.Contents.read_bytes()
            # The original 1200x300 image is painted as a 500x125 rectangle
            # at x=80,y=400. Cover the entire image *after* its /Do.
            page.Contents = pikepdf.Stream(
                doc, original + b"\nq 1 g 80 400 500 125 re f Q\n"
            )
            doc.save(covered)
        source_doc, report, _ = recognize(
            covered, [1], ocr_threads=2, raster_mode="all"
        )
        assert source_doc["pages"][0]["lines"], source_doc
        stats = prepare_render_image_pdf(covered, masked, report, [1])
        assert stats["source_images_masked"] == 1, stats
        evidence = verify_full_page_reference(
            covered, masked, report, stats, [1], max_side=1600
        )
        fast = verify_rendered_image_text(
            covered,
            masked,
            report,
            stats,
            [1],
            max_side=1600,
        )
        assert [
            (x["text"], x["visible"], x["actual_changed_output_pixels"]) for x in fast
        ] == [
            (x["text"], x["visible"], x["actual_changed_output_pixels"])
            for x in evidence
        ], (fast, evidence)
        assert len(evidence) > 0, evidence
        assert all(not x["visible"] for x in evidence), evidence
        result, combined = hybrid_run(
            covered, [1], Path(root), ocr_threads=2, raster_mode="all"
        )
        assert combined["summary"].get("nonoverlapping_image_lines_added", 0) == 0
        assert not combined["visibility_verified"]
        assert not result["pages"][0]["lines"], result
        assert all(
            not p["included_as_extra"] for p in combined["raster_page_merge_proposals"]
        )


def test_original_pdf_hash_bound():
    with TemporaryDirectory(prefix="pdf-redox-raster-source-sha-") as root:
        base = Path(root) / "source.pdf"
        fixture(base)
        _, report, _ = recognize(base, [1], ocr_threads=2, raster_mode="all")
        report["pdf_sha256"] = "0" * 64
        try:
            prepare_render_image_pdf(base, Path(root) / "bad.pdf", report, [1])
        except ValueError:
            pass
        else:
            raise AssertionError("mismatched original PDF source SHA was accepted")


def test_auto_mixed_native_image_resolution():
    with TemporaryDirectory(prefix="pdf-redox-auto-mixed-image-") as root:
        base = Path(root) / "image-source.pdf"
        source = Path(root) / "mixed-native.pdf"
        fixture(base)
        with pikepdf.Pdf.open(base) as doc:
            page = doc.pages[0]
            resources = page.obj["/Resources"]
            resources["/Font"] = pikepdf.Dictionary(
                F1=pikepdf.Dictionary(
                    Type=pikepdf.Name("/Font"),
                    Subtype=pikepdf.Name("/Type1"),
                    BaseFont=pikepdf.Name("/Helvetica"),
                )
            )
            # Native source is 1200x300, but the planned 1600px
            # Poppler render has only 480x120 pixels for it.
            page.Contents = pikepdf.Stream(
                doc,
                b"q 240 0 0 60 80 400 cm /Im0 Do Q\n"
                b"q 0 g 40 30 60 15 re f Q\n"
                b"BT /F1 14 Tf 40 300 Td (NATIVE NOTE) Tj ET\n",
            )
            doc.save(source)
        output, report = hybrid_run(
            source, [1], Path(root), ocr_threads=2, raster_mode="auto"
        )
        assert report["native_resolution_selection"]["selected_sources"] == 1, report
        assert report["reused_native_image_recognizer_for_page"], report
        assert report["visibility_verified"], report
        assert report["summary"]["nonoverlapping_image_lines_added"] >= 1, report
        lines = output["pages"][0]["lines"]
        assert any("CAPACITOR 47uF 16V" in x["text"] for x in lines), lines
        assert len(output["pages"][0]["native_text_regions"]) > 0, output

        # Exercise the compiled Rust PDF annotator, not merely the Python
        # merge draft. Old AABB duplicate suppression lost adjacent rotated
        # raster labels between successful recognition and actual extraction.
        cli = (
            Path(__file__).resolve().parents[2] / "target/release/pdf-redox-ocr-overlay"
        )
        draft = Path(root) / "native-image-ocr.json"
        annotated = Path(root) / "searchable.pdf"
        draft.write_text(json.dumps(output))
        subprocess.run(
            [
                str(cli),
                str(source),
                "--ocr",
                str(draft),
                "--apply-unreviewed",
                "--min-confidence",
                "0.97",
                "-o",
                str(annotated),
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        with pikepdf.Pdf.open(annotated) as document:
            content = document.pages[0].Contents.read_bytes()
            actual = [
                bytes.fromhex(encoded.decode()).decode("utf-16")
                for encoded in re.findall(rb"/ActualText\s*<([0-9A-Fa-f]+)>", content)
            ]
            assert "CAPACITOR 47uF 16V" in actual, actual
        extracted = subprocess.run(
            ["pdftotext", "-f", "1", "-l", "1", "-raw", str(annotated), "-"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        assert "CAPACITOR 47uF 16V" in extracted, extracted

        def render(path):
            return subprocess.run(
                [
                    "pdftoppm",
                    "-f",
                    "1",
                    "-l",
                    "1",
                    "-r",
                    "150",
                    "-singlefile",
                    str(path),
                ],
                check=True,
                capture_output=True,
            ).stdout

        assert render(source) == render(annotated), (
            "semantic PDF overlay changed visible pixels"
        )


def test_source_image_reuse_with_one_occluded_placement():
    # Two pages share the exact same image object, but the second
    # paints an opaque rectangle afterward. Text must be admitted
    # only where visible, not globally accepted/rejected per XObject.
    with TemporaryDirectory(prefix="pdf-redox-mixed-visible-hidden-") as td:
        original = Path(td) / "reuse.pdf"
        revised = Path(td) / "reuse-covered.pdf"
        fixture(original)
        with pikepdf.Pdf.open(original) as pdf:
            page_one = pdf.pages[0]
            page_one.Contents = pikepdf.Stream(
                pdf,
                page_one.Contents.read_bytes() + b"\nq 0.4 g 20 20 45 25 re f Q\n",
            )
            pg = pdf.pages[1]
            pg.Contents = pikepdf.Stream(
                pdf,
                pg.Contents.read_bytes() + b"\nq 1 g 415 45 125 500 re f Q\n",
            )
            pdf.save(revised)
        original_run = subprocess.run
        renders = []

        def count_renders(command, **kwargs):
            if isinstance(command, list) and command and command[0] == "pdftoppm":
                renders.append(tuple(command))
            return original_run(command, **kwargs)

        with mock.patch("subprocess.run", side_effect=count_renders):
            doc, report = hybrid_run(
                revised, [1, 2], Path(td), raster_mode="all", ocr_threads=2
            )
        assert len(renders) == 5, (
            "visible page requires one full treatment and one cropped control; "
            "occluded page additionally needs one full original for fallback OCR",
            renders,
        )
        assert sum("-x" in call for call in renders) == 2, renders
        assert sum(call[2] == "1" for call in renders) == 2, renders
        assert sum(call[2] == "2" for call in renders) == 3, renders
        assert report["verified_source_mask_pages"] == [1], report
        assert report["summary"]["nonoverlapping_image_lines_added"] == 1, report
        visible = {p["page"]: p for p in doc["pages"]}
        assert any(
            "CAPACITOR 47uF 16V" in line["text"] for line in visible[1]["lines"]
        ), visible[1]
        assert not visible[2]["lines"], visible[2]
        assert any(
            x["page"] == 2 and not x["included_as_extra"]
            for x in report["raster_page_merge_proposals"]
        ), report


def test_rotated_cropped_media_box_visibility():
    with TemporaryDirectory(prefix="pdf-redox-rotated-visibility-") as td:
        source = Path(td) / "source.pdf"
        rotated = Path(td) / "rotated-cropped.pdf"
        fixture(source)
        with pikepdf.Pdf.open(source) as doc:
            page = doc.pages[0]
            page.obj["/Rotate"] = 90
            page.cropbox = pikepdf.Array([10, 10, 790, 590])
            doc.save(rotated)
        draft, report, _ = recognize(rotated, [1], ocr_threads=2, raster_mode="all")
        assert draft["pages"][0]["lines"], report
        control = Path(td) / "control.pdf"
        masked = Path(td) / "masked.pdf"
        stats = prepare_render_image_pdf(
            rotated,
            masked,
            report,
            [1],
            control_pdf=control,
        )
        baseline = verify_full_page_reference(
            control, masked, report, stats, [1], max_side=1600
        )
        optimized = verify_rendered_image_text(
            control, masked, report, stats, [1], max_side=1600
        )

        def decisions(items):
            return [
                (
                    entry["text"],
                    entry["visible"],
                    entry["actual_changed_output_pixels"],
                    entry["expected_changed_output_pixels"],
                )
                for entry in items
            ]

        assert decisions(baseline) == decisions(optimized), (baseline, optimized)
        assert all(entry["visible"] for entry in optimized), optimized
        assert all(entry["comparison_mode"] == "cropped" for entry in optimized)
        full = _page_pixels(control, 1, 1600)
        h, w = full.shape[:2]
        x, y, ww, hh = int(w * 0.17), int(h * 0.21), int(w * 0.27), int(h * 0.28)
        regional = _page_pixels(control, 1, 1600, crop=(x, y, x + ww, y + hh))
        assert (regional == full[y : y + hh, x : x + ww]).all()


def test_many_reused_raster_placements():
    # Sixteen uses of the *same* high-resolution image object, in four
    # well-separated page quadrants. The verifier must use region groups,
    # not one Poppler render for every appearance, and must agree exactly
    # with the original full-page differential proof.
    with TemporaryDirectory(prefix="pdf-redox-16-image-places-") as td:
        base = Path(td) / "base.pdf"
        original = Path(td) / "sixteen.pdf"
        fixture(base)
        with pikepdf.Pdf.open(base) as pdf:
            page = pdf.pages[0]
            operations = [
                f"q 165 0 0 65 {15 + 195 * col} {15 + 142 * row} cm /Im0 Do Q"
                for row in range(4)
                for col in range(4)
            ]
            page.Contents = pikepdf.Stream(
                pdf,
                ("\n".join(operations)).encode(),
            )
            pdf.save(original)
        draft, report, _ = recognize(original, [1], ocr_threads=2, raster_mode="all")
        assert len(draft["pages"][0]["lines"]) == 16, draft
        control = Path(td) / "control.pdf"
        treatment = Path(td) / "treatment.pdf"
        stats = prepare_render_image_pdf(
            original,
            treatment,
            report,
            [1],
            control_pdf=control,
        )
        full = verify_full_page_reference(
            control,
            treatment,
            report,
            stats,
            [1],
            max_side=1600,
        )
        fast = verify_rendered_image_text(
            control,
            treatment,
            report,
            stats,
            [1],
            max_side=1600,
        )
        assert len(full) == len(fast) == 16, (len(full), len(fast))
        assert all(x["visible"] for x in fast), fast
        assert all(x["comparison_mode"] == "cropped" for x in fast), fast
        assert all(x["control_regions"] <= 4 for x in fast), fast
        assert sum(1 for x in fast if x["actual_changed_output_pixels"] > 0) == 16
        for a, b in zip(full, fast, strict=True):
            assert (
                a["page"],
                a["text"],
                a["visible"],
                a["actual_changed_output_pixels"],
            ) == (
                b["page"],
                b["text"],
                b["visible"],
                b["actual_changed_output_pixels"],
            ), (a, b)


def test_overlapping_image_source_masks_are_not_visibility_proof():
    # Original image A is entirely hidden under a separately stored image B
    # with matching text. Erasing both creates a page-render difference,
    # but that difference proves only that B was visible, never A.
    with TemporaryDirectory(prefix="pdf-redox-overlapping-image-masks-") as td:
        base = Path(td) / "base.pdf"
        source = Path(td) / "overlapped.pdf"
        fixture(base)
        with pikepdf.Pdf.open(base) as pdf:
            page = pdf.pages[0]
            resources = page.obj["/Resources"]["/XObject"]
            original = resources["/Im0"]
            copy = pikepdf.Stream(pdf, original.read_bytes())
            for k in (
                "/Type",
                "/Subtype",
                "/Width",
                "/Height",
                "/BitsPerComponent",
                "/ColorSpace",
            ):
                copy[k] = original[k]
            resources["/Im1"] = pdf.make_indirect(copy)
            page.Contents = pikepdf.Stream(
                pdf,
                b"q 500 0 0 125 80 400 cm /Im0 Do Q\n"
                b"q 500 0 0 125 80 400 cm /Im1 Do Q\n"
                b"q 0.3 g 20 20 80 10 re f Q\n",
            )
            pdf.save(source)
        document, report, _ = recognize(
            source,
            [1],
            ocr_threads=2,
            raster_mode="all",
        )
        assert document["pages"][0]["lines"], document
        assert (
            len(
                {
                    s["source_id"]
                    for s in report["sources"]
                    if s["status"] == "recognized"
                }
            )
            == 2
        ), report
        control = Path(td) / "control.pdf"
        treatment = Path(td) / "treatment.pdf"
        stats = prepare_render_image_pdf(
            source,
            treatment,
            report,
            [1],
            control_pdf=control,
        )
        baseline = verify_full_page_reference(
            control,
            treatment,
            report,
            stats,
            [1],
            max_side=1600,
        )
        fast = verify_rendered_image_text(
            control,
            treatment,
            report,
            stats,
            [1],
            max_side=1600,
        )
        # The old simultaneous-mask verifier accepted BOTH (false).
        assert len(baseline) == len(fast) == 2, (baseline, fast)
        assert all(d["visible"] for d in baseline), baseline
        assert all(
            not d["visible"] and d["ambiguous_source_mask_overlap"] for d in fast
        ), fast
        isolated = verify_rendered_image_text(
            control,
            treatment,
            report,
            stats,
            [1],
            max_side=1600,
            isolation_original=source,
            isolation_root=Path(td),
        )
        top_id = report["placements"][-1]["source_id"]
        bottom_id = report["placements"][0]["source_id"]
        assert [d["visible"] for d in isolated] == [False, True], isolated
        assert all(d["isolated_source_proof"] for d in isolated), isolated
        assert all(d["applied_mask_source_ids"] == [top_id] for d in isolated), isolated
        assert isolated[0]["source_id"] == bottom_id, isolated
        assert isolated[1]["source_id"] == top_id, isolated
        output, combined = hybrid_run(
            source,
            [1],
            Path(td),
            raster_mode="all",
            ocr_threads=2,
        )
        assert combined["visibility_verified"], combined
        assert combined["verified_source_mask_pages"] == [1], combined
        assert combined["summary"]["nonoverlapping_image_lines_added"] == 1, combined
        assert len(output["pages"][0]["lines"]) == 1, output
        proposals = combined["raster_page_merge_proposals"]
        assert len(proposals) == 2, proposals
        assert [p["included_as_extra"] for p in proposals] == [False, True], proposals
        assert [p["source_id"] for p in proposals] == [bottom_id, top_id], proposals
        assert output["pages"][0]["lines"][0]["text"] == "CAPACITOR 47uF 16V"
        assert "source_id" not in output["pages"][0]["lines"][0]
        _assert_single_semantic_pdf_line(source, output, td)


def test_partial_overlap_two_visible_source_images():
    # Two independently recognized source images overlap in their actual
    # text polygons, but both contribute visible text to the final PDF.
    # One is scaled smaller and shifted over the last few characters
    # of the first line. Isolation should accept both distinct sources.
    with TemporaryDirectory(prefix="pdf-redox-partial-source-overlap-") as td:
        base = Path(td) / "base.pdf"
        source = Path(td) / "partial.pdf"
        fixture(base)
        with pikepdf.Pdf.open(base) as pdf:
            page = pdf.pages[0]
            resources = page.obj["/Resources"]["/XObject"]
            original = resources["/Im0"]
            clone = pikepdf.Stream(pdf, original.read_bytes())
            for k in (
                "/Type",
                "/Subtype",
                "/Width",
                "/Height",
                "/BitsPerComponent",
                "/ColorSpace",
            ):
                clone[k] = original[k]
            resources["/Im1"] = pdf.make_indirect(clone)
            page.Contents = pikepdf.Stream(
                pdf,
                b"q 500 0 0 125 80 400 cm /Im0 Do Q\n"
                b"q 200 0 0 125 510 400 cm /Im1 Do Q\n"
                b"q 0.3 g 20 20 50 15 re f Q\n",
            )
            pdf.save(source)
        draft, report, _ = recognize(
            source,
            [1],
            ocr_threads=2,
            raster_mode="all",
        )
        assert len(draft["pages"][0]["lines"]) == 2, draft
        assert len({line["source_id"] for line in draft["pages"][0]["lines"]}) == 2
        control = Path(td) / "control.pdf"
        treatment = Path(td) / "treatment.pdf"
        stats = prepare_render_image_pdf(
            source,
            treatment,
            report,
            [1],
            control_pdf=control,
        )
        isolated = verify_rendered_image_text(
            control,
            treatment,
            report,
            stats,
            [1],
            max_side=1600,
            isolation_original=source,
            isolation_root=Path(td),
        )
        assert len(isolated) == 2, isolated
        assert all(i["ambiguous_source_mask_overlap"] for i in isolated)
        assert all(i["isolated_source_proof"] and i["visible"] for i in isolated), (
            isolated
        )
        assert len(isolated[0]["applied_mask_source_ids"]) == 2
        output, hybrid_report = hybrid_run(
            source,
            [1],
            Path(td),
            raster_mode="all",
            ocr_threads=2,
        )
        assert hybrid_report["summary"]["nonoverlapping_image_lines_added"] == 2, (
            hybrid_report
        )
        assert len(output["pages"][0]["lines"]) == 2, output
        assert all(
            x["text"] == "CAPACITOR 47uF 16V" for x in output["pages"][0]["lines"]
        )


def test_nested_form_reuse_source_paint_proof():
    # The *same* Form XObject is invoked twice. Nested Form references the
    # same image XObject, and the same page location is used both times.
    # The verifier must clone only the second Form-invocation path before
    # masking it; changing its shared /Resources would hide both paints.
    with TemporaryDirectory(prefix="pdf-redox-nested-form-paint-") as td:
        base = Path(td) / "base.pdf"
        source = Path(td) / "nested-forms.pdf"
        fixture(base)
        with pikepdf.Pdf.open(base) as pdf:
            page = pdf.pages[0]
            image = page.obj["/Resources"]["/XObject"]["/Im0"]
            resource = image
            for index in range(2):
                name = pikepdf.Name(f"/Child{index}")
                xobjects = pikepdf.Dictionary()
                xobjects[name] = resource
                form = pikepdf.Stream(pdf, f"{name} Do".encode())
                form["/Type"] = pikepdf.Name("/XObject")
                form["/Subtype"] = pikepdf.Name("/Form")
                form["/BBox"] = pikepdf.Array([0, 0, 1, 1])
                form["/Resources"] = pikepdf.Dictionary(XObject=xobjects)
                resource = form
            page.obj["/Resources"] = pikepdf.Dictionary(
                XObject=pikepdf.Dictionary(F0=resource)
            )
            page.Contents = pikepdf.Stream(
                pdf,
                b"q 500 0 0 125 80 400 cm /F0 Do Q\n"
                b"q 500 0 0 125 80 400 cm /F0 Do Q\n"
                b"q 0.2 g 20 20 40 20 re f Q\n",
            )
            pdf.save(source)
        native, report, _ = recognize(
            source,
            [1],
            ocr_threads=2,
            raster_mode="all",
        )
        assert len(native["pages"][0]["lines"]) == 2, native
        assert [p["paint_index"] for p in report["placements"]] == [0, 1]
        assert len({p["source_id"] for p in report["placements"]}) == 1
        final, audit = hybrid_run(
            source,
            [1],
            Path(td),
            raster_mode="all",
            ocr_threads=2,
        )
        assert audit["verified_source_mask_pages"] == [1], audit
        assert [
            p["included_as_extra"] for p in audit["raster_page_merge_proposals"]
        ] == [False, True], audit
        assert [p["paint_index"] for p in audit["raster_page_merge_proposals"]] == [
            0,
            1,
        ], audit
        assert len(final["pages"][0]["lines"]) == 1, final
        _assert_single_semantic_pdf_line(source, final, td)


def test_overlapping_repeated_same_source_isolated_by_paint():
    # Cloning only the selected Do invocation and its enclosing Form path
    # proves which paint of the same resource is actually visible.
    with TemporaryDirectory(prefix="pdf-redox-same-source-doublepaint-") as td:
        base = Path(td) / "base.pdf"
        source = Path(td) / "same-source.pdf"
        fixture(base)
        with pikepdf.Pdf.open(base) as pdf:
            page = pdf.pages[0]
            page.Contents = pikepdf.Stream(
                pdf,
                b"q 500 0 0 125 80 400 cm /Im0 Do Q\n"
                b"q 500 0 0 125 80 400 cm /Im0 Do Q\n"
                b"q 0.3 g 20 20 80 10 re f Q\n",
            )
            pdf.save(source)
        draft, report, _ = recognize(source, [1], ocr_threads=2, raster_mode="all")
        assert len(draft["pages"][0]["lines"]) == 2, draft
        assert len(report["placements"]) == 2, report
        assert len({line["source_id"] for line in draft["pages"][0]["lines"]}) == 1
        control = Path(td) / "control.pdf"
        treatment = Path(td) / "treatment.pdf"
        stats = prepare_render_image_pdf(
            source,
            treatment,
            report,
            [1],
            control_pdf=control,
        )
        isolated = verify_rendered_image_text(
            control,
            treatment,
            report,
            stats,
            [1],
            max_side=1600,
            isolation_original=source,
            isolation_root=Path(td),
        )
        assert len(isolated) == 2, isolated
        assert [x["visible"] for x in isolated] == [False, True], isolated
        assert all(x["isolated_paint_proof"] for x in isolated), isolated
        assert all(x["ambiguous_source_mask_overlap"] for x in isolated), isolated
        assert all(x["applied_mask_paint_indices"] == [1] for x in isolated), isolated
        combined, combined_report = hybrid_run(
            source,
            [1],
            Path(td),
            raster_mode="all",
            ocr_threads=2,
        )
        assert combined_report["visibility_verified"], combined_report
        assert combined_report["summary"]["nonoverlapping_image_lines_added"] == 1
        assert [
            p["included_as_extra"]
            for p in combined_report["raster_page_merge_proposals"]
        ] == [False, True]
        assert len(combined["pages"][0]["lines"]) == 1, combined
        assert combined["pages"][0]["lines"][0]["text"] == "CAPACITOR 47uF 16V"
        _assert_single_semantic_pdf_line(source, combined, td)


def test_source_polygon_overlap_geometry():
    def rect(x0, y0, x1, y1):
        return [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]

    a = rect(0, 0, 10, 10)
    b = rect(5, 5, 15, 15)
    c = rect(16, 0, 26, 10)
    assert _convex_quads_intersect(a, b)
    assert not _convex_quads_intersect(a, c)
    # Adjacent tilted labels can overlap in AABB but not polygon area.
    left = [(0, 0), (10, 10), (8, 12), (-2, 2)]
    right = [(4, -3), (14, 7), (12, 9), (2, -1)]
    assert not _convex_quads_intersect(left, right)
    entries = [
        {"bounds": (0, 0, 11, 11), "screen": a, "source_id": "A"},
        {"bounds": (5, 5, 16, 16), "screen": b, "source_id": "B"},
        {"bounds": (16, 0, 27, 11), "screen": c, "source_id": "C"},
    ]
    ambiguous, same_source = _ambiguous_source_text_masks(entries)
    assert ambiguous == {0, 1} and not same_source
    entries[1]["source_id"] = "A"
    ambiguous, same_source = _ambiguous_source_text_masks(entries)
    assert ambiguous == {0, 1} and same_source == {0, 1}


def test_crop_region_planner():
    # Up to four remote labels are handled by small, independent regions.
    sparse = [
        (15, 15, 35, 35),
        (945, 15, 965, 35),
        (15, 945, 35, 965),
        (945, 945, 965, 965),
    ]
    regions = _plan_visibility_crops(sparse, 1000, 1000)
    assert len(regions) == 4, regions
    assert sum((x1 - x0) * (y1 - y0) for x0, y0, x1, y1 in regions) < 10000
    assert _plan_visibility_crops([(15, 20, 985, 975)], 1000, 1000) == []


if __name__ == "__main__":
    test_nested_form_reuse_source_paint_proof()
    test_partial_overlap_two_visible_source_images()
    test_overlapping_repeated_same_source_isolated_by_paint()
    test_source_polygon_overlap_geometry()
    test_overlapping_image_source_masks_are_not_visibility_proof()
    test_many_reused_raster_placements()
    test_crop_region_planner()
    test_rotated_cropped_media_box_visibility()
    test_source_image_reuse_with_one_occluded_placement()
    test_auto_mixed_native_image_resolution()
    test_fully_occluded_source_text()
    test_original_pdf_hash_bound()
    print("MIXED_RASTER_VISIBLE_TEXT_PROOF_PASS")
