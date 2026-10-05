//! Generic OpenType subsetting backends used by PDF-specific font optimization.
//!
//! PDF graph/addressing policy stays in `font.rs`; this module delegates the
//! actual OpenType glyph/table rewrite to maintained font-subsetting crates.

use skera::{DEFAULT_DROP_TABLES, Plan, SubsetFlags, subset_font};
use std::collections::{BTreeMap, BTreeSet};
use write_fonts::{
    FontBuilder,
    read::{FileRef, FontRef, TableProvider, collections::IntSet},
    types::{GlyphId, NameId, Tag},
};

const PDF_RENDERING_UNUSED_TABLES: [Tag; 16] = [
    Tag::new(b"BASE"),
    Tag::new(b"GDEF"),
    Tag::new(b"GPOS"),
    Tag::new(b"GSUB"),
    Tag::new(b"JSTF"),
    Tag::new(b"MATH"),
    Tag::new(b"kern"),
    Tag::new(b"vhea"),
    Tag::new(b"vmtx"),
    Tag::new(b"DSIG"),
    Tag::new(b"name"),
    Tag::new(b"OS/2"),
    Tag::new(b"PCLT"),
    Tag::new(b"hdmx"),
    Tag::new(b"LTSH"),
    Tag::new(b"VDMX"),
];
const CID_TYPE2_UNUSED_TABLES: [Tag; 2] = [Tag::new(b"cmap"), Tag::new(b"post")];

fn rebuild_font(
    font: &FontRef<'_>,
    mut retain: impl FnMut(Tag) -> bool,
) -> Option<(Vec<u8>, usize)> {
    let mut builder = FontBuilder::new();
    let mut removed = 0_usize;
    let mut retained = 0_usize;
    for record in font.table_directory().table_records() {
        let tag = record.tag();
        let data = font.data_for_tag(tag)?;
        if retain(tag) {
            builder.add_raw_with_checksum(tag, data, record.checksum());
            retained = retained.saturating_add(1);
        } else {
            removed = removed.saturating_add(data.len());
        }
    }
    (retained > 0).then(|| (builder.build(), removed))
}

/// Extract the sole face of a TrueType/OpenType collection into a standalone
/// SFNT. Multi-face collections are intentionally left untouched because a PDF
/// `FontFile2` stream does not identify which face is semantically selected.
pub fn unwrap_single_face_collection(bytes: &[u8]) -> Option<Vec<u8>> {
    let FileRef::Collection(collection) = FileRef::new(bytes).ok()? else {
        return None;
    };
    if collection.len() != 1 {
        return None;
    }
    let font = collection.get(0).ok()?;
    rebuild_font(&font, |_| true).map(|(bytes, _)| bytes)
}

/// Rebuild a standalone OpenType font while omitting tables that an already-
/// positioned PDF text stream does not use for rendering.
pub fn strip_pdf_unused_tables(bytes: &[u8], cid_type2_only: bool) -> Option<(Vec<u8>, usize)> {
    let font = FontRef::new(bytes).ok()?;
    let result = rebuild_font(&font, |tag| {
        !(PDF_RENDERING_UNUSED_TABLES.contains(&tag)
            || cid_type2_only && CID_TYPE2_UNUSED_TABLES.contains(&tag))
    })?;
    (result.1 > 0).then_some(result)
}

/// Rebuild an SFNT using replacement `glyf`/`loca` tables while copying every
/// other table from the canonical source font.
pub fn replace_glyf_and_loca(bytes: &[u8], glyf: Vec<u8>, loca: Vec<u8>) -> Option<Vec<u8>> {
    let font = FontRef::new(bytes).ok()?;
    let mut builder = FontBuilder::new();
    builder.add_raw(Tag::new(b"glyf"), glyf);
    builder.add_raw(Tag::new(b"loca"), loca);
    builder.copy_missing_tables(font);
    Some(builder.build())
}

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

fn requested_glyphs(requested_gids: &BTreeSet<u16>) -> IntSet<GlyphId> {
    let mut gids = IntSet::<GlyphId>::empty();
    for &gid in requested_gids {
        gids.insert(GlyphId::new(u32::from(gid)));
    }
    gids
}

fn default_drop_tables() -> IntSet<Tag> {
    let mut tables = IntSet::<Tag>::empty();
    tables.extend(DEFAULT_DROP_TABLES.iter().copied());
    tables
}

fn plan_for_pdf_gids(
    font: &FontRef<'_>,
    requested_gids: &BTreeSet<u16>,
    retain_gids: bool,
) -> Plan {
    let mut flags = SubsetFlags::SUBSET_FLAGS_NOTDEF_OUTLINE
        | SubsetFlags::SUBSET_FLAGS_NO_PRUNE_UNICODE_RANGES
        | SubsetFlags::SUBSET_FLAGS_NO_LAYOUT_CLOSURE;
    if retain_gids {
        flags |= SubsetFlags::SUBSET_FLAGS_RETAIN_GIDS;
    }
    Plan::new(
        &requested_glyphs(requested_gids),
        &IntSet::<u32>::empty(),
        font,
        flags,
        &default_drop_tables(),
        &IntSet::<Tag>::all(),
        &IntSet::<Tag>::empty(),
        &IntSet::<NameId>::all(),
        &IntSet::<u16>::all(),
    )
}

/// Retain only glyph programs reachable from `requested_gids` while preserving
/// every retained glyph's original GID. This is the safe mode when PDF-side
/// glyph addressing cannot be rewritten.
pub fn retain_glyph_ids(
    bytes: &[u8],
    requested_gids: &BTreeSet<u16>,
) -> Option<RetainedGlyphSubset> {
    let font = FontRef::new(bytes).ok()?;
    let plan = plan_for_pdf_gids(&font, requested_gids, true);
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

/// Densely remap glyph IDs and return the exact old→new assignment. This is
/// only usable when every PDF-side user of the program can be rewritten
/// atomically (for example `CIDFontType2` with explicit `CIDToGIDMap` streams).
pub fn compact_glyph_ids(bytes: &[u8], requested_gids: &BTreeSet<u16>) -> Option<DenseGlyphSubset> {
    let font = FontRef::new(bytes).ok()?;
    let original_glyph_count = usize::from(font.maxp().ok()?.num_glyphs());
    let plan = plan_for_pdf_gids(&font, requested_gids, false);
    let old_to_new = plan
        .old_to_new_glyph_mapping()
        .map(|(old_gid, new_gid)| {
            Some((
                u16::try_from(old_gid.to_u32()).ok()?,
                u16::try_from(new_gid.to_u32()).ok()?,
            ))
        })
        .collect::<Option<BTreeMap<_, _>>>()?;
    let subset = subset_font(&font, &plan).ok()?;
    Some(DenseGlyphSubset {
        removed_decoded_bytes: bytes.len().saturating_sub(subset.len()),
        bytes: subset,
        old_to_new,
        original_glyph_count,
    })
}
