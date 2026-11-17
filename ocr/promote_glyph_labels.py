# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Create a *research-only* GlyphLabels v1 draft after two OCR systems agree.

Training and disjoint holdout must be unanimous RapidOCR predictions. Separate
Tesseract PSM 10 and 13 must both reproduce the same Unicode with per-mode
word confidence above the configured threshold. This is multi-model
corroboration, not manual ground truth; do not silently adopt for production.
"""
import argparse
from collections import Counter
import json
from pathlib import Path


def promote(catalog, votes, independent, min_tess_confidence=60):
    if catalog["schema_version"] != 2 or votes["schema_version"] != 1:
        raise ValueError("unsupported catalog or vote schema")
    digest=catalog["optimized_pdf_sha256"]
    if digest != votes["pdf_sha256"] or digest != independent["pdf_sha256"]:
        raise ValueError("candidate, OCR and PDF source digests disagree")
    if not (0 <= min_tess_confidence <= 100):
        raise ValueError("Tesseract confidence out of range")
    all_templates = {t["id"]:t for t in catalog["glyphs"]}
    if len(all_templates) != len(catalog["glyphs"]):
        raise ValueError("duplicate glyph templates")
    vote_rows = {row["glyph_id"]:row for row in votes["templates"]}
    if len(vote_rows) != len(votes["templates"]):
        raise ValueError("duplicate glyph vote records")
    assessments = votes.get("holdout_validation", {}).get("assessments", [])
    tested = {row["glyph_id"]:row for row in assessments}
    if len(tested) != len(assessments):
        raise ValueError("duplicate holdout assessments")
    if not independent["source"].startswith("Tesseract 5"):
        raise ValueError("unexpected independent OCR source")
    proposed={}
    skipped=Counter()
    scored=[]
    seen=set()
    for row in independent["glyphs"]:
        gid=row["glyph_id"]
        if gid in seen:
            raise ValueError("duplicate independent OCR glyph")
        seen.add(gid)
        source=all_templates.get(gid)
        model=vote_rows.get(gid)
        held=tested.get(gid)
        if source is None or model is None or held is None:
            raise ValueError("independent evidence refers to an unknown template")
        if (row["full_document_occurrences"] != len(source["occurrences"])
                or row["candidate_unicode"] != model["suggested_unicode"]
                or held["trained_unicode"] != row["candidate_unicode"]):
            raise ValueError("independent evidence does not match a canonical template")
        if not model["evidence_candidate"] or held["status"] != "consistent":
            skipped["not_cross_page_consistent"]+=1
            continue
        if len(row["candidate_unicode"])!=1 or row["candidate_unicode"].isspace():
            skipped["not_single_character"]+=1
            continue
        predictions=row["predictions"]
        if not {"10","13"}.issubset(predictions):
            raise ValueError("independent OCR report lacks both PSM modes")
        if any(predictions[mode]["text"] != row["candidate_unicode"]
               for mode in ("10","13")):
            skipped["independent_disagreement"]+=1
            continue
        if any(predictions[mode]["confidence"] is None
               or predictions[mode]["confidence"] < min_tess_confidence
               for mode in ("10","13")):
            skipped["low_independent_confidence"]+=1
            continue
        proposed[gid]=row["candidate_unicode"]
        scored.append({
            "glyph_id":gid,"unicode":row["candidate_unicode"],
            "occurrences":len(source["occurrences"]),
            "training_votes":model["observations"],
            "holdout_votes":held["holdout_observations"],
            "tesseract_psm10":predictions["10"],
            "tesseract_psm13":predictions["13"]
        })
    scored.sort(key=lambda x:(-x["occurrences"],x["glyph_id"]))
    report={
        "schema_version":1,"pdf_sha256":digest,
        "status":"research_draft_not_manually_verified",
        "summary":{
            "corroborated_templates":len(proposed),
            "corroborated_appearances":sum(x["occurrences"] for x in scored),
            "independent_templates_evaluated":len(seen),
            "exclusions":dict(skipped),
        },
        "accepted":scored,
    }
    return {"schema_version":1,"labels":dict(sorted(proposed.items())),
            "overrides":[]}, report


def promote_contextual(catalog, votes, contextual,
                       min_observations=10, min_pages=2, min_agreement=0.98,
                       min_holdout_agreement=1.0, min_holdout_votes=3):
    """Promote candidates corroborated by a second model in page context.

    Tesseract makebox gives no calibrated per-character probabilities. Repeated
    independent model agreement and geometric association are the evidence;
    the result remains an *unreviewed research hypothesis*.
    """
    if catalog["schema_version"] != 2 or votes["schema_version"] != 1:
        raise ValueError("unsupported catalog/vote schema")
    if contextual["schema_version"] != 1:
        raise ValueError("unsupported contextual evidence schema")
    digest = catalog["optimized_pdf_sha256"]
    if votes["pdf_sha256"] != digest or contextual["pdf_sha256"] != digest:
        raise ValueError("contextual evidence is bound to different PDF bytes")
    if not contextual["model"].startswith("Tesseract LSTM eng"):
        raise ValueError("unexpected contextual recognizer")
    if (min_observations < 1 or min_pages < 1 or not 0 < min_agreement <= 1
            or not 0 < min_holdout_agreement <= 1 or min_holdout_votes < 1):
        raise ValueError("invalid contextual selection thresholds")

    canonical={g["id"]:g for g in catalog["glyphs"]}
    training={g["glyph_id"]:g for g in votes["templates"]}
    holdout={g["glyph_id"]:g for g in votes["holdout_validation"]["assessments"]}
    if len(canonical)!=len(catalog["glyphs"]) or len(training)!=len(votes["templates"]):
        raise ValueError("duplicate catalog glyph or training record")
    seen=set()
    accepted=[]
    skipped=Counter()
    for row in contextual["glyphs"]:
        gid=row["glyph_id"]
        if gid in seen or gid not in canonical:
            raise ValueError("duplicate or unknown contextual glyph")
        seen.add(gid)
        original=canonical[gid]
        source=training.get(gid)
        proof=holdout.get(gid)
        if source is None or row["rapidocr_unicode"] != source["suggested_unicode"]:
            raise ValueError("contextual votes disagree with original training record")
        if row["full_document_occurrences"] != len(original["occurrences"]):
            raise ValueError("contextual occurrence count does not match PDF")
        if not source["evidence_candidate"] or proof is None:
            skipped["insufficient_rapidocr_holdout"]+=1
            continue
        if proof["trained_unicode"] != source["suggested_unicode"]:
            raise ValueError("holdout and training candidate disagree")
        observed=proof["holdout_observations"]
        if (observed < min_holdout_votes or
                proof["agreeing"]/observed < min_holdout_agreement):
            skipped["conflicting_rapidocr_holdout"]+=1
            continue
        if len(source["suggested_unicode"])!=1 or source["suggested_unicode"].isspace():
            skipped["not_single_unicode"]+=1
            continue
        n,agree,wrong = (row[k] for k in ("tesseract_observations","agreeing","disagreeing"))
        if not (isinstance(n,int) and isinstance(agree,int) and
                isinstance(wrong,int) and 0<=agree<=n and n-agree==wrong):
            raise ValueError("contextual independent vote tally malformed")
        if n < min_observations or row["pages_seen"] < min_pages:
            skipped["insufficient_independent_context"]+=1
            continue
        ratio=agree/n
        if ratio < min_agreement:
            skipped["independent_conflict"]+=1
            continue
        if sum(v["count"] for v in row["votes"] if v["text"] == source["suggested_unicode"]) != agree:
            raise ValueError("contextual vote details disagree with summary")
        accepted.append({
            "glyph_id":gid,"unicode":source["suggested_unicode"],
            "occurrences":len(original["occurrences"]),
            "training_votes":source["observations"],
            "holdout_votes":proof["holdout_observations"],
            "holdout_agreement":proof["agreeing"]/proof["holdout_observations"],
            "tesseract_observations":n,"tesseract_agreements":agree,
            "tesseract_disagreements":wrong,
            "independent_agreement":ratio,
            "pages_seen_by_independent_model":row["pages_seen"],
        })
    accepted.sort(key=lambda x:(-x["occurrences"],x["glyph_id"]))
    labels={row["glyph_id"]:row["unicode"] for row in accepted}
    report={"schema_version":1,"pdf_sha256":digest,
            "status":"research_draft_not_manually_verified",
            "strategy":"contextual_independent_character_box_agreement",
            "summary":{
                "corroborated_templates":len(accepted),
                "corroborated_appearances":sum(row["occurrences"] for row in accepted),
                "independent_templates_evaluated":len(seen),
                "selected_independent_observations":sum(row["tesseract_observations"] for row in accepted),
                "selected_independent_disagreements":sum(row["tesseract_disagreements"] for row in accepted),
                "exclusions":dict(skipped),
            },
            "accepted":accepted}
    return {"schema_version":1,"labels":dict(sorted(labels.items())),
            "overrides":[]},report


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("--glyph-catalog",type=Path,required=True)
    p.add_argument("--votes",type=Path,required=True)
    evidence=p.add_mutually_exclusive_group(required=True)
    evidence.add_argument("--independent",type=Path,
                          help="isolated Tesseract PSM10/13 comparison")
    evidence.add_argument("--contextual",type=Path,
                          help="independent full-page Tesseract glyph association")
    p.add_argument("--labels",type=Path,required=True)
    p.add_argument("--report",type=Path,required=True)
    p.add_argument("--min-tesseract-confidence",type=float,default=60)
    p.add_argument("--min-contextual-observations",type=int,default=10)
    p.add_argument("--min-contextual-pages",type=int,default=2)
    p.add_argument("--min-contextual-agreement",type=float,default=0.98)
    p.add_argument("--min-holdout-agreement",type=float,default=1.0)
    p.add_argument("--min-holdout-votes",type=int,default=3)
    args=p.parse_args()
    catalog=json.loads(args.glyph_catalog.read_text())
    votes=json.loads(args.votes.read_text())
    if args.independent is not None:
        independent=json.loads(args.independent.read_text())
        labels,report=promote(catalog,votes,independent,args.min_tesseract_confidence)
    else:
        contextual=json.loads(args.contextual.read_text())
        labels,report=promote_contextual(
            catalog,votes,contextual,args.min_contextual_observations,
            args.min_contextual_pages,args.min_contextual_agreement,
            args.min_holdout_agreement,args.min_holdout_votes)
    for path in (args.labels,args.report):
        path.parent.mkdir(parents=True,exist_ok=True)
    args.labels.write_text(json.dumps(labels,ensure_ascii=False,indent=2)+"\n")
    args.report.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n")
    print(json.dumps(report["summary"],sort_keys=True))

if __name__=="__main__":
    main()
