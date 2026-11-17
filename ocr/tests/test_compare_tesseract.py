# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
"""Check Tesseract raster-to-PDF coordinates and shape-evidence matching."""
import hashlib
from pathlib import Path
import sys
from tempfile import TemporaryDirectory

import pikepdf
sys.path.insert(0,str(Path(__file__).resolve().parent.parent))
from compare_tesseract_page_boxes import compare,pdf_quad
from pdf_geometry import display_mapper


def check_rotation(rotation):
    with TemporaryDirectory(prefix="test-tess-pdf-") as root:
        path=Path(root)/"test.pdf"
        with pikepdf.Pdf.new() as doc:
            page=doc.add_blank_page(page_size=(200,300))
            page.obj[pikepdf.Name("/Rotate")]=rotation
            doc.save(path)
        digest=hashlib.sha256(path.read_bytes()).hexdigest()
        with pikepdf.Pdf.open(path) as doc:
            mapper,rot=display_mapper(doc.pages[0])
            assert rot==rotation
            quad=pdf_quad([10,10,20,25],100,100,mapper)
        if rotation==0:
            assert quad[0]==[20,30],quad
        if rotation==90:
            assert quad[0]==[180,30],quad
        catalog={"schema_version":2,"optimized_pdf_sha256":digest,
                 "glyphs":[{"id":"a","occurrences":[{
                     "page":1,"index_on_page":0,"page_quad":quad}]}]}
        votes={"schema_version":1,"pdf_sha256":digest,
               "templates":[{"glyph_id":"a","suggested_unicode":"A"}],
               "holdout_validation":{"assessments":[
                   {"glyph_id":"a","status":"consistent"}]}}
        evidence={"schema_version":1,"pdf_sha256":digest,
                  "engine":"Tesseract LSTM eng psm11 makebox",
                  "pages":[{"page":1,"width":100,"height":100,
                            "characters":[{"text":"A","box":[10,10,20,25]}]}]}
        result=compare(catalog,votes,evidence,path)
        assert result["summary"]["associated_boxes"]==1,result
        assert result["summary"]["holdout_consistent_agreements"]==1,result
        evidence["pdf_sha256"]="0"*64
        try:
            compare(catalog,votes,evidence,path)
        except ValueError:
            pass
        else:
            raise AssertionError("mismatched digest accepted")


if __name__=="__main__":
    for rotate in (0,90,180,270):
        check_rotation(rotate)
    print("TESSERACT_PDF_BOX_TESTS_PASS")
