#!/usr/bin/env python3
"""Exact-frame and schema tests for PDF-aware OCR structural evidence."""
import json
import pathlib
import sys
import tempfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
from structural_evidence import audit, component_signature, intersection, local_coordinates

def square(x, y, size=1):
    return [[x,y],[x+size,y],[x+size,y+size],[x,y+size]]

def rotate(quad):
    return [[-p[1],p[0]] for p in quad]

def match_test():
    bbox = [[0,0],[50,0],[50,10],[0,10]]
    occurrences = [("A",square(5,2,3)),("B",square(15,2,3)),
                   ("C",square(25,2,3))]
    sig,n=component_signature(bbox,occurrences)
    assert sig is not None and n==3
    rotated_bbox=rotate(bbox)
    rotated_occurrences=[(gid,rotate(quad)) for gid,quad in occurrences]
    rotated_sig,_=component_signature(rotated_bbox,rotated_occurrences)
    assert sig==rotated_sig,(sig,rotated_sig)
    assert local_coordinates([25,5],bbox)==(0.5,0.5)
    assert intersection(square(0,0,5),square(2,2,1))
    assert not intersection(square(0,0,5),square(5,2,1))

def audit_test():
    quad=[[0,0],[50,0],[50,10],[0,10]]
    glyphs=[
        {"id":gid,"occurrences":[{"page":page,"page_quad":square(x,2,3)}
          for page in (1,2)]}
        for gid,x in (("A",5),("B",15),("C",25))
    ]
    cat={"schema_version":2,"optimized_pdf_sha256":"0"*64,"glyphs":glyphs}
    ocr={"schema_version":3,"pdf_sha256":"0"*64,"pages":[
        {"page":p,"native_text_regions":[],"lines":[{
           "text":txt,"confidence":0.99,"quad":quad
        }]} for p,txt in ((1,"ABC"),(2,"ABG"))
    ]}
    report=audit(cat,ocr)
    assert report["summary"]["repeated_groups"]==1,report
    assert report["summary"]["ocr_conflicting_groups"]==1,report
    assert report["summary"]["potential_recognitions_reusable"]==1,report
    ocr["pages"][0]["native_text_regions"]=[square(14,3,2)]
    report=audit(cat,ocr)
    assert report["summary"]["native_intersections"]==1,report
    assert report["summary"].get("repeated_groups",0)==0,report
    cat["optimized_pdf_sha256"]="1"*64
    try:
        audit(cat,ocr)
    except ValueError:
        pass
    else:
        raise AssertionError("must reject wrong PDF content")

if __name__=="__main__":
    match_test()
    audit_test()
    print("STRUCTURAL_EVIDENCE_TESTS_PASS: rotation, overlap, OCR conflict, hash binding")
