#!/usr/bin/env python3
"""Aggregate RapidOCR character votes over recurring pdf-redox Type3 outlines.

This produces a PDF-bound *diagnostic* report, never automatic Unicode labels.
Reused paths can represent parts of characters or non-text diagram strokes;
confidence from one OCR model is not independent evidence of correctness.
"""

import argparse
from collections import Counter, defaultdict
import json
import math
from pathlib import Path
import statistics

from structural_evidence import local_coordinates


def aabb(quad):
    return (
        min(p[0] for p in quad),
        min(p[1] for p in quad),
        max(p[0] for p in quad),
        max(p[1] for p in quad),
    )


def index_catalog(catalog, cell_size=16):
    """Spatial bins for glyph occurrences. Store center in each bin."""
    index = defaultdict(lambda: defaultdict(list))
    occurrence_count = Counter()
    for template in catalog["glyphs"]:
        gid = template["id"]
        for occurrence in template["occurrences"]:
            quad = occurrence["page_quad"]
            if len(quad) != 4 or any(not math.isfinite(v) for p in quad for v in p):
                continue
            page = occurrence["page"]
            x0, y0, x1, y1 = aabb(quad)
            width, height = x1 - x0, y1 - y0
            if width < 0.01 or height < 0.01:
                continue
            index[page][
                (
                    math.floor((x0 + x1) / 2 / cell_size),
                    math.floor((y0 + y1) / 2 / cell_size),
                )
            ].append((gid, occurrence["index_on_page"], quad, width * height))
            occurrence_count[gid] += 1
    return index, occurrence_count


def match_char(char_quad, bins, cell_size=16):
    """Return exactly one plausible full component, otherwise the ambiguity.

    Character and vector boxes come from independent processes. Only isolated
    candidates with similar bounds can supply evidence; components sharing a
    character box are not arbitrarily assigned a Unicode value.
    """
    if len(char_quad) != 4 or any(not math.isfinite(v) for p in char_quad for v in p):
        return None, "invalid_char_box"
    cx0, cy0, cx1, cy1 = aabb(char_quad)
    area = (cx1 - cx0) * (cy1 - cy0)
    if area <= 0.01:
        return None, "invalid_char_box"
    candidates = []
    # Padded broadphase for glyphs whose control-point bbox is slightly loose.
    for gx in range(
        math.floor((cx0 - 2) / cell_size), math.floor((cx1 + 2) / cell_size) + 1
    ):
        for gy in range(
            math.floor((cy0 - 2) / cell_size), math.floor((cy1 + 2) / cell_size) + 1
        ):
            for gid, ordinal, glyph_quad, glyph_area in bins.get((gx, gy), ()):
                if not 0.12 <= glyph_area / area <= 3.0:
                    continue
                center = [
                    sum(p[0] for p in glyph_quad) / 4,
                    sum(p[1] for p in glyph_quad) / 4,
                ]
                uv = local_coordinates(center, char_quad)
                if uv is None or not (0.02 <= uv[0] <= 0.98 and -0.08 <= uv[1] <= 1.08):
                    continue
                # Reject a glyph that extends far outside the character's
                # actual geometry (including outlined multi-character words).
                projected = [local_coordinates(p, char_quad) for p in glyph_quad]
                if any(v is None for v in projected):
                    continue
                if (
                    min(p[0] for p in projected) < -0.35
                    or max(p[0] for p in projected) > 1.35
                    or min(p[1] for p in projected) < -0.35
                    or max(p[1] for p in projected) > 1.35
                ):
                    continue
                candidates.append((gid, ordinal))
    if not candidates:
        return None, "no_full_component"
    if len(candidates) != 1:
        return None, "multiple_components"
    return candidates[0], None


def audit(catalog, evidence, min_confidence=0.97, min_votes=3):
    if catalog["schema_version"] != 2 or evidence["schema_version"] != 1:
        raise ValueError("unexpected glyph or character-evidence schema")
    if catalog["optimized_pdf_sha256"] != evidence["pdf_sha256"]:
        raise ValueError("catalog and OCR characters reference different PDFs")
    if not (0 <= min_confidence <= 1) or min_votes < 1:
        raise ValueError("invalid thresholds")
    bins, occurrences = index_catalog(catalog)
    votes = defaultdict(list)
    summary = Counter()
    matched_instances = defaultdict(set)
    for page in evidence["pages"]:
        number = page["page"]
        for char in page["characters"]:
            summary["characters_seen"] += 1
            text = char["text"]
            prob = char["confidence"]
            if len(text) != 1 or text.isspace() or not math.isfinite(prob):
                summary["invalid_character"] += 1
                continue
            if prob < min_confidence:
                summary["low_confidence"] += 1
                continue
            selected, reason = match_char(char["quad"], bins.get(number, {}))
            if reason:
                summary[reason] += 1
                continue
            gid, ordinal = selected
            key = (number, ordinal)
            if key in matched_instances[gid]:
                # Two OCR proposals for one vector occurrence: conflicting
                # detections are not independent votes.
                summary["duplicate_occurrence"] += 1
                continue
            matched_instances[gid].add(key)
            votes[gid].append((text, prob, number))
            summary["associated_characters"] += 1
    ranked = []
    for gid, observations in votes.items():
        labels = Counter(text for text, _, _ in observations)
        leader, winning = labels.most_common(1)[0]
        unanimous = len(labels) == 1
        pages = len(set(page for _, _, page in observations))
        confident = unanimous and winning >= min_votes
        ranked.append(
            {
                "glyph_id": gid,
                "catalog_occurrences": occurrences[gid],
                "observations": len(observations),
                "pages": pages,
                "unanimous": unanimous,
                "evidence_candidate": confident,
                "suggested_unicode": leader,
                "confidence_median": statistics.median(
                    prob for _, prob, _ in observations
                ),
                "votes": [
                    {"text": text, "count": count}
                    for text, count in labels.most_common()
                ],
            }
        )
    ranked.sort(
        key=lambda row: (
            -row["catalog_occurrences"] if row["evidence_candidate"] else 0,
            -row["observations"],
            row["glyph_id"],
        )
    )
    summary["observed_templates"] = len(ranked)
    summary["unanimous_templates"] = sum(r["unanimous"] for r in ranked)
    summary["conflicting_templates"] = sum(not r["unanimous"] for r in ranked)
    summary["evidence_candidates"] = sum(r["evidence_candidate"] for r in ranked)
    summary["candidate_occurrences_total"] = sum(
        r["catalog_occurrences"] for r in ranked if r["evidence_candidate"]
    )
    summary["candidate_observations"] = sum(
        r["observations"] for r in ranked if r["evidence_candidate"]
    )
    summary["catalog_templates_total"] = len(catalog["glyphs"])
    summary["catalog_occurrences_total"] = sum(
        len(g["occurrences"]) for g in catalog["glyphs"]
    )
    return {
        "schema_version": 1,
        "pdf_sha256": evidence["pdf_sha256"],
        "input_model": evidence["model"],
        "summary": dict(summary),
        "templates": ranked,
    }


def compare_holdout(training, holdout, min_holdout_votes=3):
    """Only score shapes learned before seeing separate holdout pages."""
    if training["pdf_sha256"] != holdout["pdf_sha256"]:
        raise ValueError("training and holdout refer to different PDF content")
    if min_holdout_votes < 1:
        raise ValueError("min_holdout_votes must be positive")
    trained = {
        row["glyph_id"]: row
        for row in training["templates"]
        if row["evidence_candidate"]
    }
    unseen = {row["glyph_id"]: row for row in holdout["templates"]}
    score = Counter()
    assessments = []
    for gid, source in trained.items():
        target = unseen.get(gid)
        if target is None or target["observations"] < min_holdout_votes:
            score["insufficient_holdout"] += 1
            continue
        expected = source["suggested_unicode"]
        agrees = sum(v["count"] for v in target["votes"] if v["text"] == expected)
        disagree = target["observations"] - agrees
        score["tested_templates"] += 1
        score["agreeing_characters"] += agrees
        score["disagreeing_characters"] += disagree
        status = "consistent" if disagree == 0 else "conflicting"
        score[status + "_templates"] += 1
        assessments.append(
            {
                "glyph_id": gid,
                "trained_unicode": expected,
                "holdout_observations": target["observations"],
                "agreeing": agrees,
                "disagreeing": disagree,
                "holdout_votes": target["votes"],
                "status": status,
            }
        )
    score["training_candidates"] = len(trained)
    return {"summary": dict(score), "assessments": assessments}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--glyph-catalog", type=Path, required=True)
    p.add_argument("--char-evidence", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--min-confidence", type=float, default=0.97)
    p.add_argument("--min-votes", type=int, default=3)
    p.add_argument(
        "--holdout-char-evidence",
        type=Path,
        help="distinct pages for cross-page prediction consistency audit",
    )
    p.add_argument("--min-holdout-votes", type=int, default=3)
    args = p.parse_args()
    catalog = json.loads(args.glyph_catalog.read_text())
    chars = json.loads(args.char_evidence.read_text())
    report = audit(catalog, chars, args.min_confidence, args.min_votes)
    if args.holdout_char_evidence:
        held = json.loads(args.holdout_char_evidence.read_text())
        training_pages = {page["page"] for page in chars["pages"]}
        holdout_pages = {page["page"] for page in held["pages"]}
        if not training_pages.isdisjoint(holdout_pages):
            raise ValueError("holdout character evidence must use disjoint PDF pages")
        holdout_report = audit(catalog, held, args.min_confidence, args.min_votes)
        report["holdout_validation"] = compare_holdout(
            report, holdout_report, args.min_holdout_votes
        )
        report["holdout_validation"]["pages"] = sorted(holdout_pages)
        print(
            "HOLDOUT_VALIDATION",
            json.dumps(report["holdout_validation"]["summary"], sort_keys=True),
        )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report["summary"], sort_keys=True))


if __name__ == "__main__":
    main()
