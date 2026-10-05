//! Generic OpenType subsetting backends used by PDF-specific font optimization.
//!
//! PDF graph/addressing policy stays in `font.rs`; this module delegates the
//! actual OpenType glyph/table rewrite to maintained font-subsetting crates.

use oxifont_subset::{SubsetOptions, subset_with_gid_set_mapped, tables::read_table_directory};
use skera::{Plan, SubsetFlags, subset_font};
use std::collections::{BTreeMap, BTreeSet};
use write_fonts::{
    read::{FontRef, TableProvider, collections::IntSet},
    types::{GlyphId, NameId, Tag},
};

#[derive(Debug)]
pub struct RetainedGlyphSubset {
    pub bytes: Vec<u8>,
    pub removed_decoded_bytes: usize,
}

#[derive(Debug)]
pub struct DenseGlyphSubset {
    pub bytes: Vec<u8>,
    pub old_to_new: BTreeMap<u16, u16>,
    pub original_glyph_count: usize,
    pub removed_decoded_bytes: usize,
}

/// Retain only glyph programs reachable from `requested_gids` while preserving
/// every retained glyph's original GID. This is the safe mode when PDF-side
/// glyph addressing cannot be rewritten.
pub fn retain_glyph_ids(
    bytes: &[u8],
    requested_gids: &BTreeSet<u16>,
) -> Option<RetainedGlyphSubset> {
    let font = FontRef::new(bytes).ok()?;
    let mut gids = IntSet::<GlyphId>::empty();
    for &gid in requested_gids {
        gids.insert(GlyphId::new(u32::from(gid)));
    }

    let flags = SubsetFlags::SUBSET_FLAGS_RETAIN_GIDS
        | SubsetFlags::SUBSET_FLAGS_PASSTHROUGH_UNRECOGNIZED
        | SubsetFlags::SUBSET_FLAGS_NOTDEF_OUTLINE
        | SubsetFlags::SUBSET_FLAGS_GLYPH_NAMES
        | SubsetFlags::SUBSET_FLAGS_NO_PRUNE_UNICODE_RANGES
        | SubsetFlags::SUBSET_FLAGS_NO_LAYOUT_CLOSURE;
    let plan = Plan::new(
        &gids,
        &IntSet::<u32>::empty(),
        &font,
        flags,
        &IntSet::<Tag>::empty(),
        &IntSet::<Tag>::all(),
        &IntSet::<Tag>::empty(),
        &IntSet::<NameId>::all(),
        &IntSet::<u16>::all(),
    );
    let source_has_glyf = font.data_for_tag(Tag::new(b"glyf")).is_some();
    let subset = subset_font(&font, &plan).ok()?;
    let subset_font = FontRef::new(&subset).ok()?;
    if source_has_glyf
        && (subset_font.data_for_tag(Tag::new(b"glyf")).is_none()
            || subset_font.data_for_tag(Tag::new(b"head")).is_none()
            || subset_font.data_for_tag(Tag::new(b"loca")).is_none()
            || subset_font.glyf().is_err()
            || subset_font.head().is_err()
            || subset_font.loca(None).is_err())
    {
        return None;
    }
    (subset.len() < bytes.len()).then(|| RetainedGlyphSubset {
        removed_decoded_bytes: bytes.len() - subset.len(),
        bytes: subset,
    })
}

fn source_glyph_count(bytes: &[u8]) -> Option<usize> {
    let tables = read_table_directory(bytes).ok()?;
    let maxp = tables.get(b"maxp")?;
    let count = maxp.get(4..6)?;
    Some(usize::from(u16::from_be_bytes([count[0], count[1]])))
}

/// Densely remap glyph IDs and return the exact old→new assignment. This is
/// only usable when every PDF-side user of the program can be rewritten
/// atomically (for example `CIDFontType2` with explicit `CIDToGIDMap` streams).
pub fn compact_glyph_ids(bytes: &[u8], requested_gids: &BTreeSet<u16>) -> Option<DenseGlyphSubset> {
    let original_glyph_count = source_glyph_count(bytes)?;
    let options = SubsetOptions::default()
        .strip_hints(false)
        .retain_layout_tables(true)
        .retain_names(true);
    let (subset, stats, map) =
        subset_with_gid_set_mapped(bytes, requested_gids, &BTreeMap::new(), &options).ok()?;
    // OxiFont currently reports this fallback for CID-keyed CFF, where dense
    // remapping would make the copied charstrings invalid. Never accept it.
    if stats.cff_charstrings_verbatim {
        return None;
    }
    Some(DenseGlyphSubset {
        removed_decoded_bytes: bytes.len().saturating_sub(subset.len()),
        bytes: subset,
        old_to_new: map.iter().collect(),
        original_glyph_count,
    })
}
