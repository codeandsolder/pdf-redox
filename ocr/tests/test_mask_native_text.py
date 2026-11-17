# /// script
# requires-python = ">=3.12"
# dependencies = ["pikepdf>=9"]
# ///
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import pikepdf
from pikepdf import ContentStreamInstruction as I, Operator as O
from mask_native_text import rewrite_content


def op(name, *args):
    return I(list(args), O(name))


operations = [
    op("BT"),
    op("Tf", pikepdf.Name("/Normal"), 12),
    op("Tj", pikepdf.String("NORMAL")),
    op("Tf", pikepdf.Name("/Glyphs"), 1),
    op("Tj", pikepdf.String("TYPE3")),
    op("Tf", pikepdf.Name("/Normal"), 12),
    op("Tr", 7),
    op("Tj", pikepdf.String("CLIPPING")),
    op("Tr", 0),
    op("q"),
    op("Tf", pikepdf.Name("/Glyphs"), 1),
    op("Tj", pikepdf.String("TYPE3")),
    op("Q"),
    op("Tj", pikepdf.String("NORMAL AGAIN")),
    op("Tf", pikepdf.Name("/Unknown"), 10),
    op("Tj", pikepdf.String("UNDECODABLE")),
    op("ET"),
]
out, stats = rewrite_content(
    operations,
    {
        "/Normal": ("/TrueType", True),
        "/Glyphs": ("/Type3", False),
        "/Unknown": ("/Type0", False),
    },
)
s = [str(x.operator) for x in out]
assert stats["hidden_native_text_show"] == 2, stats
assert stats["preserved_type3_text_show"] == 2, stats
assert stats["preserved_text_clipping"] == 1, stats
assert stats["unclassified_text_show"] == 1, stats
assert s.count("Tr") == 6, (s, stats)  # original two Tr + hidden before+after twice
assert len(out) == len(operations) + 4
print("MASK_INSTRUCTION_TEST_PASS", stats)
