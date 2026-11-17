"""Comparison of OCR spellings without treating format-only rewrites as edits.

Widths, compatibility variants and whitespace are not sufficient reasons to
replace a source prediction. Case, punctuation meaning, units and digits
remain distinct. These checks intentionally prefer the original recognizer
when two outputs are semantically equivalent by this narrow normalization.
"""

import unicodedata


def canonical_for_equivalence(value):
    if not isinstance(value, str):
        raise TypeError("Unicode text must be str")
    return "".join(c for c in unicodedata.normalize("NFKC", value) if not c.isspace())


def meaningful_change(original, new):
    return canonical_for_equivalence(original) != canonical_for_equivalence(new)
