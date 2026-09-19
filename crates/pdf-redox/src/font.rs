use crate::{
    EditDocument, Error, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    StreamData, content::decoded_content_value, source::CurrentObject,
};
#[cfg(test)]
use flpdf::{DecodeLevel, ObjectRef, Pdf};
use flpdf::{
    ObjectHandle as FlObjectHandle, ObjectHandleParserCallbacks, ParseControl,
    filters::{decode_stream_data, encode_stream_data_with_flate_level},
};
use hayro_syntax::object::{
    Dict as HayroDict, MaybeRef as HayroMaybeRef, Name as HayroName, Object as HayroObject,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
#[cfg(test)]
use std::{
    collections::HashSet,
    io::{Read, Seek},
    rc::Rc,
};

const RENDERING_UNUSED_TABLES: [[u8; 4]; 13] = [
    *b"BASE", *b"GDEF", *b"GPOS", *b"GSUB", *b"JSTF", *b"MATH", *b"kern", *b"vhea", *b"vmtx",
    *b"DSIG", *b"name", *b"OS/2", *b"PCLT",
];

const CIDFONT_TYPE2_UNUSED_TABLES: [[u8; 4]; 2] = [*b"cmap", *b"post"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FontProgramUsage {
    simple_truetype: bool,
    cidfont_type2: bool,
}

impl FontProgramUsage {
    fn cidfont_type2_only(self) -> bool {
        self.cidfont_type2 && !self.simple_truetype
    }

    fn simple_truetype_only(self) -> bool {
        self.simple_truetype && !self.cidfont_type2
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FontOptimizationStats {
    pub programs_optimized: usize,
    pub programs_glyph_subset: usize,
    pub original_encoded_bytes: usize,
    pub optimized_encoded_bytes: usize,
    pub decoded_table_bytes_removed: usize,
    pub glyph_outline_bytes_removed: usize,
}

fn be16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
    ]))
}

fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
        *bytes.get(offset + 2)?,
        *bytes.get(offset + 3)?,
    ]))
}

fn checksum32(data: &[u8]) -> u32 {
    data.chunks(4).fold(0_u32, |sum, chunk| {
        let mut word = [0_u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

fn is_sfnt_magic(magic: &[u8]) -> bool {
    matches!(magic, [0, 1, 0, 0] | b"OTTO" | b"true" | b"typ1")
}

/// Rebuild an sfnt while removing tables that PDF consumers do not use to
/// render already-positioned text. Returns `(rebuilt, removed_decoded_bytes)`.
///
/// PDF's embedded-TrueType rules require the outline/metric/hinting core and,
/// for simple fonts, `cmap`. Advanced line-layout tables are not required for
/// display. PDF also defines vertical metrics through `CIDFont` `/DW2`/`/W2`,
/// making sfnt `vhea`/`vmtx` irrelevant to PDF rendering.
fn sfnt_for_pdf_rendering(bytes: &[u8], usage: FontProgramUsage) -> Option<(Vec<u8>, usize)> {
    if bytes.len() < 12 || !is_sfnt_magic(&bytes[..4]) {
        return None;
    }
    let table_count = usize::from(be16(bytes, 4)?);
    let directory_bytes = table_count.checked_mul(16)?.checked_add(12)?;
    if directory_bytes > bytes.len() || table_count == 0 {
        return None;
    }

    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(table_count);
    let mut removed_bytes = 0_usize;
    for index in 0..table_count {
        let record = 12 + index * 16;
        let tag: [u8; 4] = bytes[record..record + 4].try_into().ok()?;
        let offset = usize::try_from(be32(bytes, record + 8)?).ok()?;
        let length = usize::try_from(be32(bytes, record + 12)?).ok()?;
        let end = offset.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        if RENDERING_UNUSED_TABLES.contains(&tag)
            || (usage.cidfont_type2_only() && CIDFONT_TYPE2_UNUSED_TABLES.contains(&tag))
        {
            removed_bytes = removed_bytes.saturating_add(length);
            continue;
        }
        let mut data = bytes[offset..end].to_vec();
        if tag == *b"head" {
            if data.len() < 12 {
                return None;
            }
            data[8..12].fill(0);
        }
        tables.push((tag, data));
    }
    if removed_bytes == 0 || tables.is_empty() {
        return None;
    }

    tables.sort_unstable_by_key(|(tag, _)| *tag);
    let new_count = tables.len();
    let new_count_u16 = u16::try_from(new_count).ok()?;
    let max_power = 1_usize << (usize::BITS - 1 - new_count.leading_zeros());
    let search_range = u16::try_from(max_power.checked_mul(16)?).ok()?;
    let entry_selector = u16::try_from(max_power.trailing_zeros()).ok()?;
    let range_shift = u16::try_from(new_count.checked_mul(16)?)
        .ok()?
        .checked_sub(search_range)?;

    let mut output = Vec::with_capacity(bytes.len().saturating_sub(removed_bytes));
    output.extend_from_slice(&bytes[..4]);
    output.extend_from_slice(&new_count_u16.to_be_bytes());
    output.extend_from_slice(&search_range.to_be_bytes());
    output.extend_from_slice(&entry_selector.to_be_bytes());
    output.extend_from_slice(&range_shift.to_be_bytes());
    let directory_offset = output.len();
    output.resize(directory_offset + new_count * 16, 0);

    let mut head_offset = None;
    for (index, (tag, data)) in tables.iter().enumerate() {
        while output.len() % 4 != 0 {
            output.push(0);
        }
        let offset = output.len();
        output.extend_from_slice(data);
        while output.len() % 4 != 0 {
            output.push(0);
        }

        let record = directory_offset + index * 16;
        output[record..record + 4].copy_from_slice(tag);
        output[record + 4..record + 8].copy_from_slice(&checksum32(data).to_be_bytes());
        output[record + 8..record + 12].copy_from_slice(&u32::try_from(offset).ok()?.to_be_bytes());
        output[record + 12..record + 16]
            .copy_from_slice(&u32::try_from(data.len()).ok()?.to_be_bytes());
        if tag == b"head" {
            head_offset = Some(offset);
        }
    }

    if let Some(offset) = head_offset {
        let adjustment = 0xB1B0_AFBA_u32.wrapping_sub(checksum32(&output));
        output[offset + 8..offset + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    Some((output, removed_bytes))
}

fn sfnt_table_record(bytes: &[u8], wanted: &[u8; 4]) -> Option<(usize, usize)> {
    if bytes.len() < 12 || !is_sfnt_magic(&bytes[..4]) {
        return None;
    }
    let table_count = usize::from(be16(bytes, 4)?);
    let directory_end = 12usize.checked_add(table_count.checked_mul(16)?)?;
    if directory_end > bytes.len() {
        return None;
    }
    for index in 0..table_count {
        let record = 12 + index * 16;
        if bytes.get(record..record + 4)? != wanted {
            continue;
        }
        let offset = usize::try_from(be32(bytes, record + 8)?).ok()?;
        let length = usize::try_from(be32(bytes, record + 12)?).ok()?;
        let end = offset.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        return Some((offset, length));
    }
    None
}

fn sfnt_table<'a>(bytes: &'a [u8], wanted: &[u8; 4]) -> Option<&'a [u8]> {
    let (offset, length) = sfnt_table_record(bytes, wanted)?;
    bytes.get(offset..offset.checked_add(length)?)
}

fn cmap_format4_gid(table: &[u8], codepoint: u16) -> Option<u16> {
    if be16(table, 0)? != 4 {
        return None;
    }
    let length = usize::from(be16(table, 2)?);
    if length > table.len() || length < 16 {
        return None;
    }
    let table = &table[..length];
    let seg_count = usize::from(be16(table, 6)? / 2);
    if seg_count == 0 {
        return None;
    }
    let end_code = 14usize;
    let start_code = end_code
        .checked_add(seg_count.checked_mul(2)?)?
        .checked_add(2)?;
    let id_delta = start_code.checked_add(seg_count.checked_mul(2)?)?;
    let id_range_offset = id_delta.checked_add(seg_count.checked_mul(2)?)?;
    if id_range_offset.checked_add(seg_count.checked_mul(2)?)? > table.len() {
        return None;
    }
    for index in 0..seg_count {
        let end = be16(table, end_code + index * 2)?;
        let start = be16(table, start_code + index * 2)?;
        if codepoint < start || codepoint > end {
            continue;
        }
        let delta_offset = id_delta + index * 2;
        let delta = i16::from_be_bytes([table[delta_offset], table[delta_offset + 1]]) as i32;
        let range_word = id_range_offset + index * 2;
        let range = usize::from(be16(table, range_word)?);
        if range == 0 {
            return Some(((i32::from(codepoint) + delta) & 0xffff) as u16);
        }
        let glyph_offset = range_word
            .checked_add(range)?
            .checked_add(usize::from(codepoint - start).checked_mul(2)?)?;
        let glyph = be16(table, glyph_offset)?;
        if glyph == 0 {
            return Some(0);
        }
        return Some(((i32::from(glyph) + delta) & 0xffff) as u16);
    }
    None
}

fn cmap_format12_gid(table: &[u8], codepoint: u32) -> Option<u16> {
    if be16(table, 0)? != 12 || table.len() < 16 {
        return None;
    }
    let length = usize::try_from(be32(table, 4)?).ok()?;
    if length > table.len() || length < 16 {
        return None;
    }
    let groups = usize::try_from(be32(table, 12)?).ok()?;
    if 16usize.checked_add(groups.checked_mul(12)?)? > length {
        return None;
    }
    for index in 0..groups {
        let offset = 16 + index * 12;
        let start = be32(table, offset)?;
        let end = be32(table, offset + 4)?;
        if codepoint < start || codepoint > end {
            continue;
        }
        let first_gid = be32(table, offset + 8)?;
        let gid = first_gid.checked_add(codepoint - start)?;
        return u16::try_from(gid).ok();
    }
    None
}

fn sfnt_unicode_gid(bytes: &[u8], codepoint: u32) -> Option<u16> {
    let cmap = sfnt_table(bytes, b"cmap")?;
    if cmap.len() < 4 {
        return None;
    }
    let count = usize::from(be16(cmap, 2)?);
    if 4usize.checked_add(count.checked_mul(8)?)? > cmap.len() {
        return None;
    }
    let mut subtables = Vec::new();
    for index in 0..count {
        let record = 4 + index * 8;
        let platform = be16(cmap, record)?;
        let encoding = be16(cmap, record + 2)?;
        let offset = usize::try_from(be32(cmap, record + 4)?).ok()?;
        if offset >= cmap.len() {
            continue;
        }
        let priority = match (platform, encoding) {
            (3, 10) => 0,
            (3, 1) => 1,
            (0, _) => 2,
            _ => continue,
        };
        subtables.push((priority, offset));
    }
    subtables.sort_unstable();
    for (_, offset) in subtables {
        let table = &cmap[offset..];
        let gid = match be16(table, 0)? {
            4 if codepoint <= u32::from(u16::MAX) => cmap_format4_gid(table, codepoint as u16),
            12 => cmap_format12_gid(table, codepoint),
            _ => None,
        };
        if let Some(gid) = gid {
            return Some(gid);
        }
    }
    None
}

fn sfnt_winansi_ascii_glyph_ids(bytes: &[u8], codes: &BTreeSet<u8>) -> Option<BTreeSet<u16>> {
    let mut gids = BTreeSet::new();
    for code in codes {
        if !(0x20..=0x7e).contains(code) {
            return None;
        }
        let gid = sfnt_unicode_gid(bytes, u32::from(*code))?;
        if gid == 0 {
            return None;
        }
        gids.insert(gid);
    }
    Some(gids)
}

fn composite_components(glyph: &[u8]) -> Option<Vec<u16>> {
    if glyph.len() < 10 {
        return Some(Vec::new());
    }
    let contours = i16::from_be_bytes([glyph[0], glyph[1]]);
    if contours >= 0 {
        return Some(Vec::new());
    }

    const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
    const WE_HAVE_A_SCALE: u16 = 0x0008;
    const MORE_COMPONENTS: u16 = 0x0020;
    const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
    const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;
    const WE_HAVE_INSTRUCTIONS: u16 = 0x0100;

    let mut offset = 10usize;
    let mut components = Vec::new();
    loop {
        let flags = be16(glyph, offset)?;
        let component = be16(glyph, offset + 2)?;
        components.push(component);
        offset = offset.checked_add(4)?;
        offset = offset.checked_add(if flags & ARG_1_AND_2_ARE_WORDS != 0 {
            4
        } else {
            2
        })?;
        offset = offset.checked_add(if flags & WE_HAVE_A_SCALE != 0 {
            2
        } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
            4
        } else if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
            8
        } else {
            0
        })?;
        if offset > glyph.len() {
            return None;
        }
        if flags & MORE_COMPONENTS == 0 {
            if flags & WE_HAVE_INSTRUCTIONS != 0 {
                let instruction_len = usize::from(be16(glyph, offset)?);
                offset = offset.checked_add(2)?.checked_add(instruction_len)?;
                if offset > glyph.len() {
                    return None;
                }
            }
            break;
        }
    }
    Some(components)
}

/// Keep original glyph IDs stable while removing outlines for glyphs that the
/// PDF never addresses. This deliberately leaves `maxp`, metrics, widths, and
/// all PDF CID machinery untouched; only `glyf` and `loca` are rebuilt.
/// Composite-glyph dependencies are retained recursively.
fn sfnt_retain_glyph_ids(bytes: &[u8], requested_gids: &BTreeSet<u16>) -> Option<(Vec<u8>, usize)> {
    if bytes.len() < 12 || bytes.get(..4)? != [0, 1, 0, 0] {
        return None;
    }
    let maxp = sfnt_table(bytes, b"maxp")?;
    let head = sfnt_table(bytes, b"head")?;
    let glyf = sfnt_table(bytes, b"glyf")?;
    let loca = sfnt_table(bytes, b"loca")?;
    if maxp.len() < 6 || head.len() < 52 {
        return None;
    }
    let glyph_count = usize::from(be16(maxp, 4)?);
    if glyph_count == 0
        || requested_gids
            .iter()
            .any(|gid| usize::from(*gid) >= glyph_count)
    {
        return None;
    }
    let loca_format = i16::from_be_bytes([head[50], head[51]]);
    let mut offsets = Vec::with_capacity(glyph_count + 1);
    match loca_format {
        0 => {
            if loca.len() < (glyph_count + 1).checked_mul(2)? {
                return None;
            }
            for index in 0..=glyph_count {
                offsets.push(usize::from(be16(loca, index * 2)?).checked_mul(2)?);
            }
        }
        1 => {
            if loca.len() < (glyph_count + 1).checked_mul(4)? {
                return None;
            }
            for index in 0..=glyph_count {
                offsets.push(usize::try_from(be32(loca, index * 4)?).ok()?);
            }
        }
        _ => return None,
    }
    if offsets.windows(2).any(|pair| pair[0] > pair[1]) || offsets.last().copied()? > glyf.len() {
        return None;
    }

    let mut keep = vec![false; glyph_count];
    keep[0] = true;
    let mut pending = VecDeque::new();
    pending.push_back(0usize);
    for gid in requested_gids {
        let gid = usize::from(*gid);
        if !keep[gid] {
            keep[gid] = true;
            pending.push_back(gid);
        }
    }
    while let Some(gid) = pending.pop_front() {
        let glyph = glyf.get(offsets[gid]..offsets[gid + 1])?;
        for component in composite_components(glyph)? {
            let component = usize::from(component);
            if component >= glyph_count {
                return None;
            }
            if !keep[component] {
                keep[component] = true;
                pending.push_back(component);
            }
        }
    }

    let alignment = if loca_format == 0 { 2usize } else { 4usize };
    let mut rebuilt_glyf = Vec::new();
    let mut rebuilt_offsets = Vec::with_capacity(glyph_count + 1);
    let mut removed_outline_bytes = 0usize;
    for gid in 0..glyph_count {
        rebuilt_offsets.push(rebuilt_glyf.len());
        let glyph = &glyf[offsets[gid]..offsets[gid + 1]];
        if keep[gid] {
            rebuilt_glyf.extend_from_slice(glyph);
            while rebuilt_glyf.len() % alignment != 0 {
                rebuilt_glyf.push(0);
            }
        } else {
            removed_outline_bytes = removed_outline_bytes.saturating_add(glyph.len());
        }
    }
    rebuilt_offsets.push(rebuilt_glyf.len());
    if removed_outline_bytes == 0 {
        return None;
    }

    let mut rebuilt_loca = Vec::with_capacity(loca.len());
    match loca_format {
        0 => {
            for offset in &rebuilt_offsets {
                if offset % 2 != 0 || offset / 2 > usize::from(u16::MAX) {
                    return None;
                }
                rebuilt_loca.extend_from_slice(&u16::try_from(offset / 2).ok()?.to_be_bytes());
            }
        }
        1 => {
            for offset in &rebuilt_offsets {
                rebuilt_loca.extend_from_slice(&u32::try_from(*offset).ok()?.to_be_bytes());
            }
        }
        _ => unreachable!(),
    }

    let table_count = usize::from(be16(bytes, 4)?);
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(table_count);
    for index in 0..table_count {
        let record = 12 + index * 16;
        let tag: [u8; 4] = bytes.get(record..record + 4)?.try_into().ok()?;
        let data = if tag == *b"glyf" {
            rebuilt_glyf.clone()
        } else if tag == *b"loca" {
            rebuilt_loca.clone()
        } else {
            let (offset, length) = sfnt_table_record(bytes, &tag)?;
            let mut data = bytes.get(offset..offset.checked_add(length)?)?.to_vec();
            if tag == *b"head" {
                if data.len() < 12 {
                    return None;
                }
                data[8..12].fill(0);
            }
            data
        };
        tables.push((tag, data));
    }
    tables.sort_unstable_by_key(|(tag, _)| *tag);

    let new_count = tables.len();
    let max_power = 1_usize << (usize::BITS - 1 - new_count.leading_zeros());
    let search_range = u16::try_from(max_power.checked_mul(16)?).ok()?;
    let entry_selector = u16::try_from(max_power.trailing_zeros()).ok()?;
    let range_shift = u16::try_from(new_count.checked_mul(16)?)
        .ok()?
        .checked_sub(search_range)?;
    let mut output = Vec::with_capacity(bytes.len().saturating_sub(removed_outline_bytes));
    output.extend_from_slice(&bytes[..4]);
    output.extend_from_slice(&u16::try_from(new_count).ok()?.to_be_bytes());
    output.extend_from_slice(&search_range.to_be_bytes());
    output.extend_from_slice(&entry_selector.to_be_bytes());
    output.extend_from_slice(&range_shift.to_be_bytes());
    let directory_offset = output.len();
    output.resize(directory_offset + new_count * 16, 0);
    let mut head_offset = None;
    for (index, (tag, data)) in tables.iter().enumerate() {
        while output.len() % 4 != 0 {
            output.push(0);
        }
        let offset = output.len();
        output.extend_from_slice(data);
        while output.len() % 4 != 0 {
            output.push(0);
        }
        let record = directory_offset + index * 16;
        output[record..record + 4].copy_from_slice(tag);
        output[record + 4..record + 8].copy_from_slice(&checksum32(data).to_be_bytes());
        output[record + 8..record + 12].copy_from_slice(&u32::try_from(offset).ok()?.to_be_bytes());
        output[record + 12..record + 16]
            .copy_from_slice(&u32::try_from(data.len()).ok()?.to_be_bytes());
        if tag == b"head" {
            head_offset = Some(offset);
        }
    }
    if let Some(offset) = head_offset {
        let adjustment = 0xB1B0_AFBA_u32.wrapping_sub(checksum32(&output));
        output[offset + 8..offset + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    Some((output, removed_outline_bytes))
}

fn subset_base_font_name(name: &[u8]) -> Vec<u8> {
    if name.len() > 7 && name[6] == b'+' && name[..6].iter().all(u8::is_ascii_uppercase) {
        name[7..].to_vec()
    } else {
        name.to_vec()
    }
}

fn descriptor_base_font_name(
    document: &EditDocument,
    descriptor: CowObjectHandle,
) -> Result<Option<Vec<u8>>> {
    let Some(object) = document.current_owned_object(descriptor)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    Ok(
        owned_name_value(document, dictionary.get(b"FontName".as_slice()))?
            .map(|name| subset_base_font_name(&name)),
    )
}

fn sparse_cid_union_skeleton_hash(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.get(..4)? != [0, 1, 0, 0] {
        return None;
    }
    let table_count = usize::from(be16(bytes, 4)?);
    let mut tables = Vec::<([u8; 4], Vec<u8>)>::with_capacity(table_count);
    let mut required = [false; 6];
    for index in 0..table_count {
        let record = 12usize.checked_add(index.checked_mul(16)?)?;
        let tag: [u8; 4] = bytes.get(record..record + 4)?.try_into().ok()?;
        let (offset, length) = sfnt_table_record(bytes, &tag)?;
        let mut data = bytes.get(offset..offset.checked_add(length)?)?.to_vec();
        match &tag {
            b"glyf" => {
                required[0] = true;
                continue;
            }
            b"loca" => {
                required[1] = true;
                continue;
            }
            b"head" => {
                required[2] = true;
                if data.len() < 54 {
                    return None;
                }
                // checkSumAdjustment is expected to differ between otherwise
                // equivalent subset sfnts. Keep every other head field exact,
                // including the font-wide bbox: some renderers consult it.
                data[8..12].fill(0);
            }
            b"maxp" => required[3] = true,
            b"hhea" => required[4] = true,
            b"hmtx" => required[5] = true,
            _ => {}
        }
        tables.push((tag, data));
    }
    if required.iter().any(|present| !present) {
        return None;
    }
    tables.sort_unstable_by_key(|(tag, _)| *tag);
    let mut hasher = Sha256::new();
    hasher.update(bytes.get(..4)?);
    hasher.update((tables.len() as u64).to_le_bytes());
    for (tag, data) in tables {
        hasher.update(tag);
        hasher.update((data.len() as u64).to_le_bytes());
        hasher.update(data);
    }
    Some(hasher.finalize().into())
}

fn sfnt_glyph_offsets(bytes: &[u8]) -> Option<Vec<usize>> {
    let maxp = sfnt_table(bytes, b"maxp")?;
    let head = sfnt_table(bytes, b"head")?;
    let loca = sfnt_table(bytes, b"loca")?;
    if maxp.len() < 6 || head.len() < 52 {
        return None;
    }
    let glyph_count = usize::from(be16(maxp, 4)?);
    let loca_format = i16::from_be_bytes([head[50], head[51]]);
    let mut offsets = Vec::with_capacity(glyph_count + 1);
    match loca_format {
        0 => {
            if loca.len() < (glyph_count + 1).checked_mul(2)? {
                return None;
            }
            for index in 0..=glyph_count {
                offsets.push(usize::from(be16(loca, index * 2)?).checked_mul(2)?);
            }
        }
        1 => {
            if loca.len() < (glyph_count + 1).checked_mul(4)? {
                return None;
            }
            for index in 0..=glyph_count {
                offsets.push(usize::try_from(be32(loca, index * 4)?).ok()?);
            }
        }
        _ => return None,
    }
    let glyf = sfnt_table(bytes, b"glyf")?;
    if offsets.windows(2).any(|pair| pair[0] > pair[1]) || offsets.last().copied()? > glyf.len() {
        return None;
    }
    Some(offsets)
}

fn sfnt_union_sparse_glyphs(fonts: &[(&[u8], &BTreeSet<u16>)]) -> Option<Vec<u8>> {
    if fonts.len() < 2 {
        return None;
    }
    let base = fonts.first()?.0;
    let base_hash = sparse_cid_union_skeleton_hash(base)?;
    let maxp = sfnt_table(base, b"maxp")?;
    let head = sfnt_table(base, b"head")?;
    let glyph_count = usize::from(be16(maxp, 4)?);
    let loca_format = i16::from_be_bytes([head[50], head[51]]);
    let alignment = if loca_format == 0 {
        2usize
    } else if loca_format == 1 {
        4usize
    } else {
        return None;
    };

    let mut union_glyphs = vec![None::<Vec<u8>>; glyph_count];
    for (font, _) in fonts {
        if sparse_cid_union_skeleton_hash(font)? != base_hash {
            return None;
        }
        let offsets = sfnt_glyph_offsets(font)?;
        if offsets.len() != glyph_count + 1 {
            return None;
        }
        let glyf = sfnt_table(font, b"glyf")?;
        for gid in 0..glyph_count {
            let glyph = glyf.get(offsets[gid]..offsets[gid + 1])?;
            if glyph.is_empty() {
                continue;
            }
            match &union_glyphs[gid] {
                Some(existing) if existing.as_slice() != glyph => return None,
                Some(_) => {}
                None => union_glyphs[gid] = Some(glyph.to_vec()),
            }
        }
    }

    // Stronger than merely proving non-conflicting sparse programs: every GID
    // that this document actually addresses through each source program, plus
    // every recursively referenced composite component, must resolve to exactly
    // the same outline before and after unioning. GID 0 is included because a
    // renderer may use .notdef for an invalid/missing character without that
    // GID appearing literally in the content stream.
    for (font, used_gids) in fonts {
        let offsets = sfnt_glyph_offsets(font)?;
        let glyf = sfnt_table(font, b"glyf")?;
        let mut pending = VecDeque::new();
        pending.push_back(0usize);
        pending.extend(used_gids.iter().map(|gid| usize::from(*gid)));
        let mut checked = BTreeSet::new();
        while let Some(gid) = pending.pop_front() {
            if gid >= glyph_count || !checked.insert(gid) {
                if gid >= glyph_count {
                    return None;
                }
                continue;
            }
            let source_glyph = glyf.get(offsets[gid]..offsets[gid + 1])?;
            let union_glyph = union_glyphs[gid].as_deref().unwrap_or_default();
            if source_glyph != union_glyph {
                return None;
            }
            for component in composite_components(source_glyph)? {
                pending.push_back(usize::from(component));
            }
        }
    }

    let mut rebuilt_glyf = Vec::new();
    let mut rebuilt_offsets = Vec::with_capacity(glyph_count + 1);
    for glyph in &union_glyphs {
        rebuilt_offsets.push(rebuilt_glyf.len());
        let Some(glyph) = glyph else {
            continue;
        };
        rebuilt_glyf.extend_from_slice(glyph);
        while rebuilt_glyf.len() % alignment != 0 {
            rebuilt_glyf.push(0);
        }
    }
    rebuilt_offsets.push(rebuilt_glyf.len());

    let mut rebuilt_loca = Vec::new();
    match loca_format {
        0 => {
            for offset in &rebuilt_offsets {
                if offset % 2 != 0 || offset / 2 > usize::from(u16::MAX) {
                    return None;
                }
                rebuilt_loca.extend_from_slice(&u16::try_from(offset / 2).ok()?.to_be_bytes());
            }
        }
        1 => {
            for offset in &rebuilt_offsets {
                rebuilt_loca.extend_from_slice(&u32::try_from(*offset).ok()?.to_be_bytes());
            }
        }
        _ => return None,
    }

    let table_count = usize::from(be16(base, 4)?);
    let mut tables = Vec::<([u8; 4], Vec<u8>)>::with_capacity(table_count);
    for index in 0..table_count {
        let record = 12 + index * 16;
        let tag: [u8; 4] = base.get(record..record + 4)?.try_into().ok()?;
        let data = if tag == *b"glyf" {
            rebuilt_glyf.clone()
        } else if tag == *b"loca" {
            rebuilt_loca.clone()
        } else {
            let (offset, length) = sfnt_table_record(base, &tag)?;
            let mut data = base.get(offset..offset.checked_add(length)?)?.to_vec();
            if tag == *b"head" {
                if data.len() < 12 {
                    return None;
                }
                // Preserve every rendering-related head field from the
                // canonical source subset. Only checkSumAdjustment must be
                // cleared before rebuilding the sfnt checksum below.
                data[8..12].fill(0);
            }
            data
        };
        tables.push((tag, data));
    }
    tables.sort_unstable_by_key(|(tag, _)| *tag);

    let new_count = tables.len();
    let max_power = 1_usize << (usize::BITS - 1 - new_count.leading_zeros());
    let search_range = u16::try_from(max_power.checked_mul(16)?).ok()?;
    let entry_selector = u16::try_from(max_power.trailing_zeros()).ok()?;
    let range_shift = u16::try_from(new_count.checked_mul(16)?)
        .ok()?
        .checked_sub(search_range)?;
    let mut output = Vec::new();
    output.extend_from_slice(base.get(..4)?);
    output.extend_from_slice(&u16::try_from(new_count).ok()?.to_be_bytes());
    output.extend_from_slice(&search_range.to_be_bytes());
    output.extend_from_slice(&entry_selector.to_be_bytes());
    output.extend_from_slice(&range_shift.to_be_bytes());
    let directory_offset = output.len();
    output.resize(directory_offset + new_count * 16, 0);
    let mut head_offset = None;
    for (index, (tag, data)) in tables.iter().enumerate() {
        while output.len() % 4 != 0 {
            output.push(0);
        }
        let offset = output.len();
        output.extend_from_slice(data);
        while output.len() % 4 != 0 {
            output.push(0);
        }
        let record = directory_offset + index * 16;
        output[record..record + 4].copy_from_slice(tag);
        output[record + 4..record + 8].copy_from_slice(&checksum32(data).to_be_bytes());
        output[record + 8..record + 12].copy_from_slice(&u32::try_from(offset).ok()?.to_be_bytes());
        output[record + 12..record + 16]
            .copy_from_slice(&u32::try_from(data.len()).ok()?.to_be_bytes());
        if tag == b"head" {
            head_offset = Some(offset);
        }
    }
    if let Some(offset) = head_offset {
        let adjustment = 0xB1B0_AFBA_u32.wrapping_sub(checksum32(&output));
        output[offset + 8..offset + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    Some(output)
}

fn redirect_font_descriptor_program(
    document: &mut EditDocument,
    descriptor: CowObjectHandle,
    from: CowObjectHandle,
    to: CowObjectHandle,
) -> Result<bool> {
    let object = match descriptor {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    let Some(dictionary) = object.as_dictionary_mut() else {
        return Ok(false);
    };
    let Some(OwnedObject::Reference(current)) = dictionary.get(b"FontFile2".as_slice()) else {
        return Ok(false);
    };
    if *current != from {
        return Ok(false);
    }
    dictionary.insert(b"FontFile2".to_vec(), OwnedObject::Reference(to));
    Ok(true)
}

struct IncomingReferenceOwners {
    owners: HashMap<CowObjectHandle, BTreeSet<CowObjectHandle>>,
    roots: BTreeSet<CowObjectHandle>,
}

fn incoming_reference_owners(document: &EditDocument) -> Result<IncomingReferenceOwners> {
    let roots = document.output_roots().into_iter().collect::<BTreeSet<_>>();
    let mut incoming = HashMap::<CowObjectHandle, BTreeSet<CowObjectHandle>>::new();
    document.walk_output_objects(|owner, object| {
        let references = match object {
            CurrentObject::Source(_) => {
                let CowObjectHandle::Existing(id) = owner else {
                    return Err(Error::Invalid(
                        "source object unexpectedly has a new-object handle".to_owned(),
                    ));
                };
                document
                    .source()
                    .references(id)?
                    .into_iter()
                    .map(CowObjectHandle::Existing)
                    .collect::<Vec<_>>()
            }
            CurrentObject::Owned(object) => object.references(),
        };
        for target in references {
            incoming.entry(target).or_default().insert(owner);
        }
        Ok(())
    })?;
    Ok(IncomingReferenceOwners {
        owners: incoming,
        roots,
    })
}

struct SparseCidUnionCandidate {
    program: CowObjectHandle,
    descriptors: Vec<CowObjectHandle>,
    base_name: Vec<u8>,
    font: Vec<u8>,
    raw_len: usize,
}

fn union_sparse_cid_font_programs_hayro(
    document: &mut EditDocument,
    flate_level: i32,
    program_usage: &HashMap<CowObjectHandle, FontProgramUsage>,
    program_descriptors: &HashMap<CowObjectHandle, Vec<CowObjectHandle>>,
    union_unsafe_programs: &BTreeSet<CowObjectHandle>,
) -> Result<FontOptimizationStats> {
    let mut candidates = Vec::<SparseCidUnionCandidate>::new();
    let mut ordered_programs = program_usage
        .iter()
        .map(|(&program, &usage)| (program, usage))
        .collect::<Vec<_>>();
    ordered_programs.sort_unstable_by_key(|(program, _)| *program);
    for (program, usage) in ordered_programs {
        if !usage.cidfont_type2_only() || union_unsafe_programs.contains(&program) {
            continue;
        }
        let Some(descriptors) = program_descriptors.get(&program) else {
            continue;
        };
        if descriptors.is_empty() {
            continue;
        }
        let mut base_name = None::<Vec<u8>>;
        let mut consistent_name = true;
        for descriptor in descriptors {
            let Some(name) = descriptor_base_font_name(document, *descriptor)? else {
                consistent_name = false;
                break;
            };
            match &base_name {
                Some(existing) if existing != &name => {
                    consistent_name = false;
                    break;
                }
                Some(_) => {}
                None => base_name = Some(name),
            }
        }
        if !consistent_name {
            continue;
        }
        let Some(base_name) = base_name else {
            continue;
        };

        let Some(object) = document.current_owned_object(program)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, data } = object else {
            continue;
        };
        let Some(filter_dictionary) = flpdf_filter_dictionary(document, &dictionary)? else {
            continue;
        };
        let raw = data.bytes(document.source())?;
        let Ok(decoded) = decode_stream_data(&filter_dictionary, raw.as_ref()) else {
            continue;
        };
        if decoded.get(..4) != Some(&[0, 1, 0, 0]) {
            continue;
        }
        let font = sfnt_for_pdf_rendering(&decoded, usage)
            .map(|(trimmed, _)| trimmed)
            .unwrap_or(decoded);
        let Some(_) = sparse_cid_union_skeleton_hash(&font) else {
            continue;
        };
        candidates.push(SparseCidUnionCandidate {
            program,
            descriptors: descriptors.clone(),
            base_name,
            font,
            raw_len: raw.len(),
        });
    }

    let mut groups = BTreeMap::<(Vec<u8>, [u8; 32]), Vec<usize>>::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(skeleton) = sparse_cid_union_skeleton_hash(&candidate.font) else {
            continue;
        };
        groups
            .entry((candidate.base_name.clone(), skeleton))
            .or_default()
            .push(index);
    }

    if groups.values().all(|indices| indices.len() < 2) {
        return Ok(FontOptimizationStats::default());
    }

    // Only pay the page-content glyph-usage scan after proving that at least
    // one 2+ program rendering-skeleton group exists.
    let (identity_glyph_usage, _) = font_glyph_usage(document)?;
    let incoming_references = incoming_reference_owners(document)?;

    let mut stats = FontOptimizationStats::default();
    let mut eliminated_programs = BTreeSet::new();
    for indices in groups.into_values() {
        let indices = indices
            .into_iter()
            .filter(|index| identity_glyph_usage.contains_key(&candidates[*index].program))
            .collect::<Vec<_>>();
        if indices.len() < 2 {
            continue;
        }
        let fonts = indices
            .iter()
            .map(|index| {
                let candidate = &candidates[*index];
                (
                    candidate.font.as_slice(),
                    &identity_glyph_usage[&candidate.program],
                )
            })
            .collect::<Vec<_>>();
        let Some(mut union_font) = sfnt_union_sparse_glyphs(&fonts) else {
            continue;
        };
        let requested_gids = indices
            .iter()
            .flat_map(|index| {
                identity_glyph_usage[&candidates[*index].program]
                    .iter()
                    .copied()
            })
            .collect::<BTreeSet<_>>();
        if let Some((subset_union, _)) = sfnt_retain_glyph_ids(&union_font, &requested_gids) {
            union_font = subset_union;
        }

        let canonical_index = indices[0];
        let canonical = &candidates[canonical_index];

        // Prove that every program we are about to eliminate becomes
        // unreachable. The only permitted incoming owners are the exact
        // indirect FontDescriptor objects that this pass will rewrite.
        let mut closure_ok = true;
        for index in indices
            .iter()
            .copied()
            .filter(|index| *index != canonical_index)
        {
            let candidate = &candidates[index];
            if incoming_references.roots.contains(&candidate.program) {
                closure_ok = false;
                break;
            }
            let expected = candidate
                .descriptors
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            let actual = incoming_references
                .owners
                .get(&candidate.program)
                .cloned()
                .unwrap_or_default();
            if actual != expected {
                closure_ok = false;
                break;
            }
        }
        if !closure_ok {
            continue;
        }
        let Some(object) = document.current_owned_object(canonical.program)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, .. } = object else {
            continue;
        };
        let Some(filter_dictionary) = flpdf_filter_dictionary(document, &dictionary)? else {
            continue;
        };
        let Ok(encoded) =
            encode_stream_data_with_flate_level(&filter_dictionary, &union_font, flate_level)
        else {
            continue;
        };
        let original_encoded = indices
            .iter()
            .map(|index| candidates[*index].raw_len)
            .sum::<usize>();
        if encoded.len() >= original_encoded {
            continue;
        }

        replace_current_stream_data(document, canonical.program, encoded.clone())?;
        // Keep Length1 truthful for the newly synthesized program. Existing
        // single-program table stripping intentionally preserves candidate-32
        // behavior; this only applies to the union stream.
        let object = match canonical.program {
            CowObjectHandle::Existing(id) => document.edit_object(id)?,
            CowObjectHandle::New(id) => document
                .overlay_mut()
                .added_mut(id)
                .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
        };
        if let Some(dictionary) = object.as_dictionary_mut() {
            dictionary.insert(
                b"Length1".to_vec(),
                OwnedObject::Integer(i64::try_from(union_font.len()).map_err(|_| {
                    Error::Invalid("union font Length1 exceeds PDF integer range".to_owned())
                })?),
            );
        }

        let expected_redirects = indices
            .iter()
            .filter(|index| candidates[**index].program != canonical.program)
            .map(|index| candidates[*index].descriptors.len())
            .sum::<usize>();
        let mut redirected = 0usize;
        for index in &indices {
            let candidate = &candidates[*index];
            if candidate.program == canonical.program {
                continue;
            }
            for descriptor in &candidate.descriptors {
                redirected += usize::from(redirect_font_descriptor_program(
                    document,
                    *descriptor,
                    candidate.program,
                    canonical.program,
                )?);
            }
        }
        if redirected != expected_redirects {
            return Err(Error::Invalid(format!(
                "sparse CID font union redirected {redirected} of {expected_redirects} proven descriptor reference(s)"
            )));
        }
        for index in &indices {
            let program = candidates[*index].program;
            if program != canonical.program {
                eliminated_programs.insert(program);
            }
        }

        stats.programs_optimized += indices.len();
        stats.original_encoded_bytes += original_encoded;
        stats.optimized_encoded_bytes += encoded.len();
    }
    if !eliminated_programs.is_empty() {
        let reachable = document
            .reachable_output_objects()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let residual = eliminated_programs
            .intersection(&reachable)
            .copied()
            .collect::<Vec<_>>();
        if !residual.is_empty() {
            return Err(Error::Invalid(format!(
                "sparse CID font union left {} eliminated program(s) reachable",
                residual.len()
            )));
        }
    }
    Ok(stats)
}

#[cfg(test)]
fn is_lone_flate(stream_dict: &flpdf::ObjectHandle) -> Result<bool> {
    let filter = stream_dict.try_get_key(b"/Filter")?;
    Ok(filter.try_is_name_and_equals(b"FlateDecode")? && !stream_dict.try_has_key(b"/F")?)
}

#[cfg(test)]
pub(crate) fn strip_font_editing_tables<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    flate_level: i32,
) -> Result<FontOptimizationStats> {
    let objects = pdf.get_all_objects()?;

    // Record how each FontDescriptor is used. `cmap` and `post` are needed by
    // simple TrueType fonts, but CIDFontType2 selects glyphs through PDF's
    // CID-to-GID machinery and does not need those sfnt tables for rendering.
    let mut descriptor_usage = HashMap::<ObjectRef, FontProgramUsage>::new();
    for object in &objects {
        if !object.try_is_dictionary()? {
            continue;
        }
        let subtype = object
            .try_get_key(b"/Subtype")?
            .as_name()
            .unwrap_or_default();
        let descriptor = object.try_get_key(b"/FontDescriptor")?;
        let Some(descriptor_ref) = descriptor.object_ref() else {
            continue;
        };
        let usage = descriptor_usage.entry(descriptor_ref).or_default();
        if subtype == b"TrueType" {
            usage.simple_truetype = true;
        } else if subtype == b"CIDFontType2" {
            usage.cidfont_type2 = true;
        }
    }

    // A program can be shared by multiple FontDescriptors. Merge usage before
    // deciding which tables can be discarded so a simple-font reference keeps
    // `cmap`/`post` even if another descriptor uses the same program as CIDFontType2.
    let mut program_usage = HashMap::<ObjectRef, FontProgramUsage>::new();
    for object in &objects {
        let Some(descriptor_ref) = object.object_ref() else {
            continue;
        };
        let Some(usage) = descriptor_usage.get(&descriptor_ref).copied() else {
            continue;
        };
        if !object.try_is_dictionary()? {
            continue;
        }
        let keys = object.try_get_keys()?;
        for key in [b"/FontFile2".as_slice(), b"/FontFile3".as_slice()] {
            if !keys.contains(key) {
                continue;
            }
            let program = object.try_get_key(key)?;
            let Some(program_ref) = program.object_ref() else {
                continue;
            };
            let merged = program_usage.entry(program_ref).or_default();
            merged.simple_truetype |= usage.simple_truetype;
            merged.cidfont_type2 |= usage.cidfont_type2;
        }
    }

    let mut stats = FontOptimizationStats::default();
    let mut seen = HashSet::<ObjectRef>::new();
    for object in objects {
        if !object.try_is_dictionary()? {
            continue;
        }
        let keys = object.try_get_keys()?;
        for key in [b"/FontFile2".as_slice(), b"/FontFile3".as_slice()] {
            if !keys.contains(key) {
                continue;
            }
            let program = object.try_get_key(key)?;
            let Some(program_ref) = program.object_ref() else {
                continue;
            };
            if !seen.insert(program_ref) {
                continue;
            }
            let Some(stream_dict) = program.as_stream_dict() else {
                continue;
            };
            if !is_lone_flate(&stream_dict)? {
                continue;
            }
            let decoded = match program.get_stream_data(DecodeLevel::Generalized) {
                Ok(data) => data,
                Err(_) => continue,
            };
            let usage = program_usage.get(&program_ref).copied().unwrap_or_default();
            let Some((trimmed, removed_decoded_bytes)) =
                sfnt_for_pdf_rendering(decoded.as_ref(), usage)
            else {
                continue;
            };
            let encoded =
                match encode_stream_data_with_flate_level(&stream_dict, &trimmed, flate_level) {
                    Ok(data) => data,
                    Err(_) => continue,
                };
            let original = program.get_raw_stream_data()?;
            if encoded.len() >= original.len() {
                continue;
            }

            stats.programs_optimized += 1;
            stats.original_encoded_bytes += original.len();
            stats.optimized_encoded_bytes += encoded.len();
            stats.decoded_table_bytes_removed += removed_decoded_bytes;
            program.replace_stream_data(Rc::new(encoded), None, None);
            program.set_filter_on_write(false)?;
            pdf.mark_object_handle_dirty(&program)?;
        }
    }
    Ok(stats)
}

const HAYRO_FONT_FILE_KEYS: [&[u8]; 2] = [b"FontFile2", b"FontFile3"];

fn owned_name_value(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<Vec<u8>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

fn direct_owned_reference(value: Option<&OwnedObject>) -> Option<CowObjectHandle> {
    match value {
        Some(OwnedObject::Reference(handle)) => Some(*handle),
        _ => None,
    }
}

fn resolved_owned_dictionary(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<OwnedDictionary>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(document
        .resolve_owned_value(value)?
        .and_then(|value| value.as_dictionary().cloned()))
}

fn descriptor_program_ref(
    document: &EditDocument,
    descriptor: Option<&OwnedObject>,
) -> Result<Option<CowObjectHandle>> {
    let Some(descriptor) = resolved_owned_dictionary(document, descriptor)? else {
        return Ok(None);
    };
    Ok(direct_owned_reference(
        descriptor.get(b"FontFile2".as_slice()),
    ))
}

fn union_fontfile2_program(
    document: &EditDocument,
    descriptor: CowObjectHandle,
) -> Result<Option<CowObjectHandle>> {
    let Some(object) = document.current_owned_object(descriptor)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    let font_file2 = direct_owned_reference(dictionary.get(b"FontFile2".as_slice()));
    if font_file2.is_none()
        || direct_owned_reference(dictionary.get(b"FontFile".as_slice())).is_some()
        || direct_owned_reference(dictionary.get(b"FontFile3".as_slice())).is_some()
    {
        return Ok(None);
    }
    Ok(font_file2)
}

/// Return the embedded TrueType program and whether this font resource has the
/// narrow Identity CID semantics required for retain-GID subsetting.
fn page_font_program(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<(CowObjectHandle, bool)>> {
    let Some(font) = resolved_owned_dictionary(document, Some(value))? else {
        return Ok(None);
    };
    let subtype = owned_name_value(document, font.get(b"Subtype".as_slice()))?;
    if subtype.as_deref() == Some(b"TrueType") {
        return Ok(
            descriptor_program_ref(document, font.get(b"FontDescriptor".as_slice()))?
                .map(|program| (program, false)),
        );
    }
    if subtype.as_deref() != Some(b"Type0") {
        return Ok(None);
    }
    let encoding = owned_name_value(document, font.get(b"Encoding".as_slice()))?;
    let identity_encoding = matches!(encoding.as_deref(), Some(b"Identity-H" | b"Identity-V"));
    let Some(descendants) = font.get(b"DescendantFonts".as_slice()) else {
        return Ok(None);
    };
    let Some(OwnedObject::Array(descendants)) = document.resolve_owned_value(descendants)? else {
        return Ok(None);
    };
    if descendants.len() != 1 {
        return Ok(None);
    }
    let Some(cid_font) = resolved_owned_dictionary(document, descendants.first())? else {
        return Ok(None);
    };
    if owned_name_value(document, cid_font.get(b"Subtype".as_slice()))?.as_deref()
        != Some(b"CIDFontType2")
    {
        return Ok(None);
    }
    let cid_to_gid_identity = match cid_font.get(b"CIDToGIDMap".as_slice()) {
        None => true,
        Some(value) => owned_name_value(document, Some(value))?.as_deref() == Some(b"Identity"),
    };
    Ok(
        descriptor_program_ref(document, cid_font.get(b"FontDescriptor".as_slice()))?
            .map(|program| (program, identity_encoding && cid_to_gid_identity)),
    )
}

struct IdentityCidGlyphScanner<'a> {
    fonts: &'a HashMap<Vec<u8>, CowObjectHandle>,
    current_program: Option<CowObjectHandle>,
    operands: Vec<FlObjectHandle>,
    used: HashMap<CowObjectHandle, BTreeSet<u16>>,
    unsafe_programs: BTreeSet<CowObjectHandle>,
}

impl IdentityCidGlyphScanner<'_> {
    fn record_string(&mut self, bytes: &[u8]) {
        let Some(program) = self.current_program else {
            return;
        };
        let (pairs, remainder) = bytes.as_chunks::<2>();
        if !remainder.is_empty() {
            self.unsafe_programs.insert(program);
            return;
        }
        let used = self.used.entry(program).or_default();
        for pair in pairs {
            used.insert(u16::from_be_bytes(*pair));
        }
    }

    fn apply_operator(&mut self, operator: &[u8]) {
        match operator {
            b"Tf" => {
                self.current_program = self
                    .operands
                    .first()
                    .and_then(FlObjectHandle::as_name)
                    .and_then(|name| self.fonts.get(&name).copied());
            }
            b"Tj" | b"'" => {
                if let Some(bytes) = self.operands.first().and_then(FlObjectHandle::as_string) {
                    self.record_string(&bytes);
                }
            }
            b"\"" => {
                if let Some(bytes) = self.operands.get(2).and_then(FlObjectHandle::as_string) {
                    self.record_string(&bytes);
                }
            }
            b"TJ" => {
                if let Some(array) = self.operands.first().and_then(FlObjectHandle::as_array) {
                    for item in array {
                        if let Some(bytes) = item.as_string() {
                            self.record_string(&bytes);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl ObjectHandleParserCallbacks for IdentityCidGlyphScanner<'_> {
    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        _offset: usize,
        _length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.apply_operator(&operator);
            self.operands.clear();
        } else if object.as_inline_image().is_none() {
            self.operands.push(object);
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

fn page_simple_truetype_program(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<(CowObjectHandle, bool)>> {
    let Some(font) = resolved_owned_dictionary(document, Some(value))? else {
        return Ok(None);
    };
    if owned_name_value(document, font.get(b"Subtype".as_slice()))?.as_deref() != Some(b"TrueType")
    {
        return Ok(None);
    }
    let Some(program) = descriptor_program_ref(document, font.get(b"FontDescriptor".as_slice()))?
    else {
        return Ok(None);
    };
    let winansi = owned_name_value(document, font.get(b"Encoding".as_slice()))?.as_deref()
        == Some(b"WinAnsiEncoding");
    Ok(Some((program, winansi)))
}

struct WinAnsiCodeScanner<'a> {
    fonts: &'a HashMap<Vec<u8>, CowObjectHandle>,
    current_program: Option<CowObjectHandle>,
    operands: Vec<FlObjectHandle>,
    used: HashMap<CowObjectHandle, BTreeSet<u8>>,
    unsafe_programs: BTreeSet<CowObjectHandle>,
}

impl WinAnsiCodeScanner<'_> {
    fn record_string(&mut self, bytes: &[u8]) {
        let Some(program) = self.current_program else {
            return;
        };
        if bytes.iter().any(|byte| !(0x20..=0x7e).contains(byte)) {
            self.unsafe_programs.insert(program);
            return;
        }
        self.used
            .entry(program)
            .or_default()
            .extend(bytes.iter().copied());
    }

    fn apply_operator(&mut self, operator: &[u8]) {
        match operator {
            b"Tf" => {
                self.current_program = self
                    .operands
                    .first()
                    .and_then(FlObjectHandle::as_name)
                    .and_then(|name| self.fonts.get(&name).copied());
            }
            b"Tj" | b"'" => {
                if let Some(bytes) = self.operands.first().and_then(FlObjectHandle::as_string) {
                    self.record_string(&bytes);
                }
            }
            b"\"" => {
                if let Some(bytes) = self.operands.get(2).and_then(FlObjectHandle::as_string) {
                    self.record_string(&bytes);
                }
            }
            b"TJ" => {
                if let Some(array) = self.operands.first().and_then(FlObjectHandle::as_array) {
                    for item in array {
                        if let Some(bytes) = item.as_string() {
                            self.record_string(&bytes);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl ObjectHandleParserCallbacks for WinAnsiCodeScanner<'_> {
    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        _offset: usize,
        _length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.apply_operator(&operator);
            self.operands.clear();
        } else if object.as_inline_image().is_none() {
            self.operands.push(object);
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

fn eligible_winansi_fonts(
    document: &EditDocument,
    resources: &OwnedDictionary,
    unsafe_programs: &mut BTreeSet<CowObjectHandle>,
) -> Result<HashMap<Vec<u8>, CowObjectHandle>> {
    let Some(font_resources) =
        resolved_owned_dictionary(document, resources.get(b"Font".as_slice()))?
    else {
        return Ok(HashMap::new());
    };
    let mut eligible = HashMap::new();
    for (name, value) in &font_resources {
        let Some((program, winansi)) = page_simple_truetype_program(document, value)? else {
            continue;
        };
        if winansi {
            eligible.insert(name.clone(), program);
        } else {
            unsafe_programs.insert(program);
        }
    }
    Ok(eligible)
}

fn eligible_identity_fonts(
    document: &EditDocument,
    resources: &OwnedDictionary,
    unsafe_programs: &mut BTreeSet<CowObjectHandle>,
) -> Result<HashMap<Vec<u8>, CowObjectHandle>> {
    let Some(font_resources) =
        resolved_owned_dictionary(document, resources.get(b"Font".as_slice()))?
    else {
        return Ok(HashMap::new());
    };
    let mut eligible = HashMap::new();
    for (name, value) in &font_resources {
        let Some((program, identity)) = page_font_program(document, value)? else {
            continue;
        };
        if identity {
            eligible.insert(name.clone(), program);
        } else {
            // One embedded program can be referenced through multiple font
            // dictionaries. Any non-Identity use makes retain-GID subsetting
            // unsafe for the shared program.
            unsafe_programs.insert(program);
        }
    }
    Ok(eligible)
}

struct FontUsageScanner<'a> {
    identity: IdentityCidGlyphScanner<'a>,
    winansi: WinAnsiCodeScanner<'a>,
}

impl ObjectHandleParserCallbacks for FontUsageScanner<'_> {
    fn content_size(&mut self, size: usize) -> flpdf::Result<()> {
        self.identity.content_size(size)?;
        self.winansi.content_size(size)
    }

    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        let identity = self
            .identity
            .handle_object(object.clone(), offset, length)?;
        let winansi = self.winansi.handle_object(object, offset, length)?;
        Ok(
            if matches!(identity, ParseControl::Stop) || matches!(winansi, ParseControl::Stop) {
                ParseControl::Stop
            } else {
                ParseControl::Continue
            },
        )
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        self.identity.handle_eof()?;
        self.winansi.handle_eof()
    }
}

fn scan_font_usage_scope(
    content: &[u8],
    identity_eligible: &HashMap<Vec<u8>, CowObjectHandle>,
    winansi_eligible: &HashMap<Vec<u8>, CowObjectHandle>,
    identity_used: &mut HashMap<CowObjectHandle, BTreeSet<u16>>,
    identity_unsafe: &mut BTreeSet<CowObjectHandle>,
    winansi_used: &mut HashMap<CowObjectHandle, BTreeSet<u8>>,
    winansi_unsafe: &mut BTreeSet<CowObjectHandle>,
) {
    if identity_eligible.is_empty() && winansi_eligible.is_empty() {
        return;
    }
    let mut scanner = FontUsageScanner {
        identity: IdentityCidGlyphScanner {
            fonts: identity_eligible,
            current_program: None,
            operands: Vec::new(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
        },
        winansi: WinAnsiCodeScanner {
            fonts: winansi_eligible,
            current_program: None,
            operands: Vec::new(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
        },
    };
    if flpdf::parse_detached_content_stream(content, "font glyph usage", &mut scanner).is_err() {
        identity_unsafe.extend(identity_eligible.values().copied());
        winansi_unsafe.extend(winansi_eligible.values().copied());
        return;
    }
    identity_unsafe.extend(scanner.identity.unsafe_programs);
    for (program, gids) in scanner.identity.used {
        identity_used.entry(program).or_default().extend(gids);
    }
    winansi_unsafe.extend(scanner.winansi.unsafe_programs);
    for (program, codes) in scanner.winansi.used {
        winansi_used.entry(program).or_default().extend(codes);
    }
}

fn form_xobject_handles(document: &EditDocument) -> Result<Vec<CowObjectHandle>> {
    let mut forms = Vec::new();
    document.walk_output_objects(|handle, object| {
        let subtype = match object {
            CurrentObject::Source(HayroObject::Stream(stream)) => stream
                .dict()
                .get::<HayroName<'_>>(b"Subtype")
                .map(|name| name.as_ref().to_vec()),
            CurrentObject::Owned(OwnedObject::Stream { dictionary, .. }) => {
                owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?
            }
            _ => None,
        };
        if subtype.as_deref() == Some(b"Form") {
            forms.push(handle);
        }
        Ok(())
    })?;
    Ok(forms)
}

type IdentityGlyphUsage = HashMap<CowObjectHandle, BTreeSet<u16>>;
type WinAnsiCodeUsage = HashMap<CowObjectHandle, BTreeSet<u8>>;
type FontGlyphUsage = (IdentityGlyphUsage, WinAnsiCodeUsage);

fn font_glyph_usage(document: &EditDocument) -> Result<FontGlyphUsage> {
    let mut identity_used = HashMap::<CowObjectHandle, BTreeSet<u16>>::new();
    let mut identity_unsafe = BTreeSet::new();
    let mut winansi_used = HashMap::<CowObjectHandle, BTreeSet<u8>>::new();
    let mut winansi_unsafe = BTreeSet::new();

    for page in document.page_handles()? {
        let Some(resources_value) = document.inherited_page_value(page, b"Resources")? else {
            continue;
        };
        let Some(resources) = resolved_owned_dictionary(document, Some(&resources_value))? else {
            continue;
        };
        let identity_eligible =
            eligible_identity_fonts(document, &resources, &mut identity_unsafe)?;
        let winansi_eligible = eligible_winansi_fonts(document, &resources, &mut winansi_unsafe)?;
        if identity_eligible.is_empty() && winansi_eligible.is_empty() {
            continue;
        }
        let Some(page_object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(page_dictionary) = page_object.as_dictionary() else {
            continue;
        };
        let Some(contents) = page_dictionary.get(b"Contents".as_slice()) else {
            continue;
        };
        let mut content = Vec::new();
        decoded_content_value(document, contents, &mut content)?;
        scan_font_usage_scope(
            &content,
            &identity_eligible,
            &winansi_eligible,
            &mut identity_used,
            &mut identity_unsafe,
            &mut winansi_used,
            &mut winansi_unsafe,
        );
    }

    for form in form_xobject_handles(document)? {
        let Some(form_object) = document.current_owned_object(form)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, .. } = &form_object else {
            continue;
        };
        let Some(resources) =
            resolved_owned_dictionary(document, dictionary.get(b"Resources".as_slice()))?
        else {
            // A resource-less Form borrows its caller's scope. Until usage scanning
            // is call-graph-aware, it poisons both retain-GID proofs.
            return Ok((HashMap::new(), HashMap::new()));
        };
        let identity_eligible =
            eligible_identity_fonts(document, &resources, &mut identity_unsafe)?;
        let winansi_eligible = eligible_winansi_fonts(document, &resources, &mut winansi_unsafe)?;
        if identity_eligible.is_empty() && winansi_eligible.is_empty() {
            continue;
        }
        let Ok(content) = document.decoded_content_stream_data(form) else {
            identity_unsafe.extend(identity_eligible.values().copied());
            winansi_unsafe.extend(winansi_eligible.values().copied());
            continue;
        };
        scan_font_usage_scope(
            &content,
            &identity_eligible,
            &winansi_eligible,
            &mut identity_used,
            &mut identity_unsafe,
            &mut winansi_used,
            &mut winansi_unsafe,
        );
    }

    for program in &identity_unsafe {
        identity_used.remove(program);
    }
    for program in &winansi_unsafe {
        winansi_used.remove(program);
    }
    Ok((identity_used, winansi_used))
}

fn inspect_hayro_font_dictionary(
    holder: CowObjectHandle,
    dictionary: &HayroDict<'_>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
    descriptor_program_edges: &mut Vec<(CowObjectHandle, CowObjectHandle)>,
) {
    if let Some(descriptor) = dictionary.get_ref(b"FontDescriptor") {
        let subtype = dictionary.get::<HayroName<'_>>(b"Subtype");
        let usage = match subtype.as_ref().map(AsRef::<[u8]>::as_ref) {
            Some(b"TrueType") => FontProgramUsage {
                simple_truetype: true,
                cidfont_type2: false,
            },
            Some(b"CIDFontType2") => FontProgramUsage {
                simple_truetype: false,
                cidfont_type2: true,
            },
            _ => FontProgramUsage::default(),
        };
        if usage != FontProgramUsage::default() {
            descriptor_usage_edges.push((CowObjectHandle::Existing(descriptor.into()), usage));
        }
    }

    for key in HAYRO_FONT_FILE_KEYS {
        if let Some(program) = dictionary.get_ref(key) {
            descriptor_program_edges.push((holder, CowObjectHandle::Existing(program.into())));
        }
    }
}

fn inspect_owned_font_dictionary(
    document: &EditDocument,
    holder: CowObjectHandle,
    dictionary: &OwnedDictionary,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
    descriptor_program_edges: &mut Vec<(CowObjectHandle, CowObjectHandle)>,
) -> Result<()> {
    if let Some(descriptor) = direct_owned_reference(dictionary.get(b"FontDescriptor".as_slice())) {
        let usage =
            match owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?.as_deref() {
                Some(b"TrueType") => FontProgramUsage {
                    simple_truetype: true,
                    cidfont_type2: false,
                },
                Some(b"CIDFontType2") => FontProgramUsage {
                    simple_truetype: false,
                    cidfont_type2: true,
                },
                _ => FontProgramUsage::default(),
            };
        if usage != FontProgramUsage::default() {
            descriptor_usage_edges.push((descriptor, usage));
        }
    }

    for key in HAYRO_FONT_FILE_KEYS {
        if let Some(program) = direct_owned_reference(dictionary.get(key)) {
            descriptor_program_edges.push((holder, program));
        }
    }
    Ok(())
}

fn merge_program_usage(
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
    program: CowObjectHandle,
    usage: FontProgramUsage,
) {
    let merged = program_usage.entry(program).or_default();
    merged.simple_truetype |= usage.simple_truetype;
    merged.cidfont_type2 |= usage.cidfont_type2;
}

fn hayro_dictionary_font_usage(dictionary: &HayroDict<'_>) -> FontProgramUsage {
    match dictionary
        .get::<HayroName<'_>>(b"Subtype")
        .as_ref()
        .map(AsRef::<[u8]>::as_ref)
    {
        Some(b"TrueType") => FontProgramUsage {
            simple_truetype: true,
            cidfont_type2: false,
        },
        Some(b"CIDFontType2") => FontProgramUsage {
            simple_truetype: false,
            cidfont_type2: true,
        },
        _ => FontProgramUsage::default(),
    }
}

fn inspect_hayro_direct_font_dictionary(
    dictionary: &HayroDict<'_>,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
) {
    let usage = hayro_dictionary_font_usage(dictionary);
    if usage != FontProgramUsage::default() {
        for (name, value) in dictionary.entries() {
            if name.as_ref() != b"FontDescriptor" {
                continue;
            }
            let HayroMaybeRef::NotRef(HayroObject::Dict(descriptor)) = value else {
                break;
            };
            for key in HAYRO_FONT_FILE_KEYS {
                if let Some(program) = descriptor.get_ref(key) {
                    merge_program_usage(
                        program_usage,
                        CowObjectHandle::Existing(program.into()),
                        usage,
                    );
                }
            }
            break;
        }
    }

    for (_, value) in dictionary.entries() {
        let HayroMaybeRef::NotRef(value) = value else {
            continue;
        };
        inspect_hayro_direct_font_object(&value, program_usage);
    }
}

fn inspect_hayro_direct_font_object(
    object: &HayroObject<'_>,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
) {
    match object {
        HayroObject::Dict(dictionary) => {
            inspect_hayro_direct_font_dictionary(dictionary, program_usage);
        }
        HayroObject::Stream(stream) => {
            inspect_hayro_direct_font_dictionary(stream.dict(), program_usage);
        }
        HayroObject::Array(array) => {
            for value in array.raw_iter() {
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                inspect_hayro_direct_font_object(&value, program_usage);
            }
        }
        HayroObject::Null(_)
        | HayroObject::Boolean(_)
        | HayroObject::Number(_)
        | HayroObject::String(_)
        | HayroObject::Name(_) => {}
    }
}

fn owned_dictionary_font_usage(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<FontProgramUsage> {
    Ok(
        match owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?.as_deref() {
            Some(b"TrueType") => FontProgramUsage {
                simple_truetype: true,
                cidfont_type2: false,
            },
            Some(b"CIDFontType2") => FontProgramUsage {
                simple_truetype: false,
                cidfont_type2: true,
            },
            _ => FontProgramUsage::default(),
        },
    )
}

fn inspect_owned_direct_font_dictionary(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
) -> Result<()> {
    let usage = owned_dictionary_font_usage(document, dictionary)?;
    if usage != FontProgramUsage::default()
        && let Some(OwnedObject::Dictionary(descriptor)) =
            dictionary.get(b"FontDescriptor".as_slice())
    {
        for key in HAYRO_FONT_FILE_KEYS {
            if let Some(program) = direct_owned_reference(descriptor.get(key)) {
                merge_program_usage(program_usage, program, usage);
            }
        }
    }

    for value in dictionary.values() {
        if matches!(value, OwnedObject::Reference(_)) {
            continue;
        }
        inspect_owned_direct_font_object(document, value, program_usage)?;
    }
    Ok(())
}

fn inspect_owned_direct_font_object(
    document: &EditDocument,
    object: &OwnedObject,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
) -> Result<()> {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            inspect_owned_direct_font_dictionary(document, dictionary, program_usage)?;
        }
        OwnedObject::Array(values) => {
            for value in values {
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                inspect_owned_direct_font_object(document, value, program_usage)?;
            }
        }
        OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_)
        | OwnedObject::Reference(_) => {}
    }
    Ok(())
}

fn owned_to_flpdf_resolved(
    document: &EditDocument,
    value: &OwnedObject,
    depth: usize,
) -> Result<FlObjectHandle> {
    if depth > 64 {
        return Err(Error::Invalid(
            "font stream filter object nesting exceeds supported depth".to_owned(),
        ));
    }
    match value {
        OwnedObject::Reference(handle) => {
            let Some(value) = document.current_owned_object(*handle)? else {
                return Ok(FlObjectHandle::null());
            };
            owned_to_flpdf_resolved(document, &value, depth + 1)
        }
        OwnedObject::Null => Ok(FlObjectHandle::null()),
        OwnedObject::Boolean(value) => Ok(FlObjectHandle::boolean(*value)),
        OwnedObject::Integer(value) => Ok(FlObjectHandle::integer(*value)),
        OwnedObject::Real(value) => Ok(FlObjectHandle::real(*value)),
        OwnedObject::Name(value) => Ok(FlObjectHandle::name(value.clone())),
        OwnedObject::String(value) => Ok(FlObjectHandle::string(value.clone())),
        OwnedObject::Array(values) => Ok(FlObjectHandle::array(
            values
                .iter()
                .map(|value| owned_to_flpdf_resolved(document, value, depth + 1))
                .collect::<Result<Vec<_>>>()?,
        )),
        OwnedObject::Dictionary(dictionary) => Ok(FlObjectHandle::dictionary(
            dictionary
                .iter()
                .map(|(key, value)| {
                    Ok((
                        [b"/".as_slice(), key.as_slice()].concat(),
                        owned_to_flpdf_resolved(document, value, depth + 1)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
        )),
        OwnedObject::Stream { .. } => Err(Error::Invalid(
            "stream object cannot be used as font filter parameter".to_owned(),
        )),
    }
}

fn is_single_flate_filter_array(document: &EditDocument, value: &OwnedObject) -> Result<bool> {
    let Some(value) = document.resolve_owned_value(value)? else {
        return Ok(false);
    };
    let OwnedObject::Array(values) = value else {
        return Ok(false);
    };
    Ok(values.len() == 1
        && owned_name_value(document, values.first())?.as_deref() == Some(b"FlateDecode"))
}

fn is_lone_flate_filter(document: &EditDocument, value: &OwnedObject) -> Result<bool> {
    let Some(value) = document.resolve_owned_value(value)? else {
        return Ok(false);
    };
    match value {
        OwnedObject::Name(name) => Ok(name == b"FlateDecode"),
        OwnedObject::Array(values) if values.len() == 1 => {
            Ok(owned_name_value(document, values.first())?.as_deref() == Some(b"FlateDecode"))
        }
        _ => Ok(false),
    }
}

fn flpdf_filter_dictionary(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Option<FlObjectHandle>> {
    let Some(filter) = dictionary.get(b"Filter".as_slice()) else {
        return Ok(None);
    };
    if !is_lone_flate_filter(document, filter)? || dictionary.contains_key(b"F".as_slice()) {
        return Ok(None);
    }

    let mut entries = vec![(
        b"/Filter".to_vec(),
        owned_to_flpdf_resolved(document, filter, 0)?,
    )];
    if let Some(params) = dictionary.get(b"DecodeParms".as_slice()) {
        entries.push((
            b"/DecodeParms".to_vec(),
            owned_to_flpdf_resolved(document, params, 0)?,
        ));
    }
    Ok(Some(FlObjectHandle::dictionary(entries)))
}

fn replace_current_stream_data(
    document: &mut EditDocument,
    handle: CowObjectHandle,
    encoded: Vec<u8>,
) -> Result<()> {
    let object = match handle {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    match object {
        OwnedObject::Stream { data, .. } => {
            *data = StreamData::Owned(encoded);
            Ok(())
        }
        _ => Err(Error::Invalid(
            "font program reference does not resolve to a stream".to_owned(),
        )),
    }
}

/// Union compatible sparse `CIDFontType2` programs after exact font-program
/// deduplication has already canonicalized byte-identical stripped subsets.
pub(crate) fn union_sparse_cid_font_programs_after_dedup_hayro(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<FontOptimizationStats> {
    let mut descriptor_usage_edges = Vec::new();
    let mut descriptor_program_edges = Vec::new();
    let mut direct_program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    document.walk_output_objects(|handle, object| match object {
        CurrentObject::Source(object) => {
            match &object {
                HayroObject::Dict(dictionary) => inspect_hayro_font_dictionary(
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                HayroObject::Stream(stream) => inspect_hayro_font_dictionary(
                    handle,
                    stream.dict(),
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                _ => {}
            }
            inspect_hayro_direct_font_object(&object, &mut direct_program_usage);
            Ok(())
        }
        CurrentObject::Owned(object) => {
            if let Some(dictionary) = object.as_dictionary() {
                inspect_owned_font_dictionary(
                    document,
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                )?;
            }
            inspect_owned_direct_font_object(document, object, &mut direct_program_usage)?;
            Ok(())
        }
    })?;

    let mut descriptor_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, usage) in descriptor_usage_edges {
        let merged = descriptor_usage.entry(descriptor).or_default();
        merged.simple_truetype |= usage.simple_truetype;
        merged.cidfont_type2 |= usage.cidfont_type2;
    }

    let mut program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    let mut program_descriptors = HashMap::<CowObjectHandle, Vec<CowObjectHandle>>::new();
    let mut union_unsafe_programs = BTreeSet::<CowObjectHandle>::new();
    for (descriptor, program) in descriptor_program_edges {
        if union_fontfile2_program(document, descriptor)? == Some(program) {
            program_descriptors
                .entry(program)
                .or_default()
                .push(descriptor);
        } else {
            union_unsafe_programs.insert(program);
        }
        let Some(usage) = descriptor_usage.get(&descriptor).copied() else {
            continue;
        };
        merge_program_usage(&mut program_usage, program, usage);
    }

    union_unsafe_programs.extend(direct_program_usage.keys().copied());
    for (program, usage) in direct_program_usage {
        merge_program_usage(&mut program_usage, program, usage);
    }

    if program_usage.is_empty() {
        return Ok(FontOptimizationStats::default());
    }

    union_sparse_cid_font_programs_hayro(
        document,
        flate_level,
        &program_usage,
        &program_descriptors,
        &union_unsafe_programs,
    )
}

/// Hayro/COW port of [`strip_font_editing_tables`].
///
/// The graph walk and mutation are Hayro-native. flpdf is used only for the
/// already-tested stream filter codec semantics so `/DecodeParms` behavior
/// remains identical during the migration.
pub(crate) fn strip_font_editing_tables_hayro(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<FontOptimizationStats> {
    let mut descriptor_usage_edges = Vec::new();
    let mut descriptor_program_edges = Vec::new();
    let mut direct_program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    document.walk_output_objects(|handle, object| match object {
        CurrentObject::Source(object) => {
            match &object {
                HayroObject::Dict(dictionary) => inspect_hayro_font_dictionary(
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                HayroObject::Stream(stream) => inspect_hayro_font_dictionary(
                    handle,
                    stream.dict(),
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                _ => {}
            }
            inspect_hayro_direct_font_object(&object, &mut direct_program_usage);
            Ok(())
        }
        CurrentObject::Owned(object) => {
            if let Some(dictionary) = object.as_dictionary() {
                inspect_owned_font_dictionary(
                    document,
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                )?;
            }
            inspect_owned_direct_font_object(document, object, &mut direct_program_usage)?;
            Ok(())
        }
    })?;

    let mut descriptor_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, usage) in descriptor_usage_edges {
        let merged = descriptor_usage.entry(descriptor).or_default();
        merged.simple_truetype |= usage.simple_truetype;
        merged.cidfont_type2 |= usage.cidfont_type2;
    }

    let mut program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, program) in descriptor_program_edges {
        let Some(usage) = descriptor_usage.get(&descriptor).copied() else {
            continue;
        };
        merge_program_usage(&mut program_usage, program, usage);
    }
    let legacy_subset_programs = program_usage.keys().copied().collect::<BTreeSet<_>>();
    for (program, usage) in direct_program_usage {
        merge_program_usage(&mut program_usage, program, usage);
    }

    if program_usage.is_empty() {
        return Ok(FontOptimizationStats::default());
    }

    let mut outline_subset_programs = BTreeSet::new();
    for program in legacy_subset_programs {
        let Some(object) = document.current_owned_object(program)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, .. } = object else {
            continue;
        };
        let filter_is_new_single_array = dictionary
            .get(b"Filter".as_slice())
            .map(|filter| is_single_flate_filter_array(document, filter))
            .transpose()?
            .unwrap_or(false);
        if !filter_is_new_single_array {
            outline_subset_programs.insert(program);
        }
    }

    let (identity_glyph_usage, winansi_code_usage) = if outline_subset_programs.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        font_glyph_usage(document)?
    };
    let mut stats = FontOptimizationStats::default();
    for (&program, &usage) in &program_usage {
        let Some(object) = document.current_owned_object(program)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, data } = object else {
            continue;
        };
        let Some(filter_dictionary) = flpdf_filter_dictionary(document, &dictionary)? else {
            continue;
        };
        let raw = data.bytes(document.source())?;
        let Ok(decoded) = decode_stream_data(&filter_dictionary, raw.as_ref()) else {
            continue;
        };
        let allow_outline_subset = outline_subset_programs.contains(&program);

        let mut candidate = decoded.clone();
        let mut removed_decoded_bytes = 0usize;
        let mut removed_outline_bytes = 0usize;
        let mut changed = false;
        if allow_outline_subset
            && usage.cidfont_type2_only()
            && let Some(gids) = identity_glyph_usage.get(&program)
            && !gids.is_empty()
            && let Some((subset, removed)) = sfnt_retain_glyph_ids(&candidate, gids)
        {
            candidate = subset;
            removed_outline_bytes = removed;
            changed = true;
        } else if allow_outline_subset
            && usage.simple_truetype_only()
            && let Some(codes) = winansi_code_usage.get(&program)
            && !codes.is_empty()
            && let Some(gids) = sfnt_winansi_ascii_glyph_ids(&candidate, codes)
            && let Some((subset, removed)) = sfnt_retain_glyph_ids(&candidate, &gids)
        {
            candidate = subset;
            removed_outline_bytes = removed;
            changed = true;
        }
        if let Some((trimmed, removed)) = sfnt_for_pdf_rendering(&candidate, usage) {
            candidate = trimmed;
            removed_decoded_bytes = removed;
            changed = true;
        }
        if !changed {
            continue;
        }
        let Ok(encoded) =
            encode_stream_data_with_flate_level(&filter_dictionary, &candidate, flate_level)
        else {
            continue;
        };
        if encoded.len() >= raw.len() {
            continue;
        }

        stats.programs_optimized += 1;
        stats.programs_glyph_subset += usize::from(removed_outline_bytes > 0);
        stats.original_encoded_bytes += raw.len();
        stats.optimized_encoded_bytes += encoded.len();
        stats.decoded_table_bytes_removed += removed_decoded_bytes;
        stats.glyph_outline_bytes_removed += removed_outline_bytes;
        replace_current_stream_data(document, program, encoded)?;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourcePdf;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::{Cursor, Write};

    fn append_pdf_object(pdf: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]) {
        offsets.push(pdf.len());
        pdf.extend_from_slice(body);
    }

    fn hayro_font_fixture_with_filter_array(array_filter: bool) -> Result<Vec<u8>> {
        let head = [0_u8; 54];
        let gsub = vec![0x55_u8; 8192];
        let source_font = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyph-data"),
            (*b"hmtx", b"metric-data"),
            (*b"cmap", b"mapping-data"),
            (*b"post", b"post-data"),
            (*b"GSUB", gsub.as_slice()),
        ]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&source_font)?;
        let encoded = encoder.finish()?;

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"5 0 obj\n<< /Type /Font /Subtype /TrueType /BaseFont /TestFont /FontDescriptor 6 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /FontDescriptor /FontName /TestFont /FontFile2 7 0 R >>\nendobj\n",
        );
        let header = if array_filter {
            format!(
                "7 0 obj\n<< /Length {} /Filter [ /FlateDecode ] /DecodeParms [ << /Predictor 1 >> ] >>\nstream\n",
                encoded.len()
            )
        } else {
            format!(
                "7 0 obj\n<< /Length {} /Filter /FlateDecode /DecodeParms << /Predictor 1 >> >>\nstream\n",
                encoded.len()
            )
        };
        let mut stream_object = header.into_bytes();
        stream_object.extend_from_slice(&encoded);
        stream_object.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &stream_object);

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 8 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        Ok(pdf)
    }

    fn hayro_font_fixture() -> Result<Vec<u8>> {
        hayro_font_fixture_with_filter_array(false)
    }

    fn hayro_direct_cid_font_fixture() -> Result<Vec<u8>> {
        let head = [0_u8; 54];
        let gsub = vec![0x55_u8; 8192];
        let source_font = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyph-data"),
            (*b"hmtx", b"metric-data"),
            (*b"cmap", b"mapping-data"),
            (*b"post", b"post-data"),
            (*b"GSUB", gsub.as_slice()),
        ]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&source_font)?;
        let encoded = encoder.finish()?;

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Font << /F1 << /Type /Font /Subtype /Type0 /BaseFont /TestFont /Encoding /Identity-H /DescendantFonts [ << /Type /Font /Subtype /CIDFontType2 /BaseFont /TestFont /FontDescriptor << /Type /FontDescriptor /FontName /TestFont /FontFile2 5 0 R >> >> ] >> >> >> /Contents 4 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        let header = format!(
            "5 0 obj\n<< /Length {} /Filter /FlateDecode /DecodeParms << /Predictor 1 >> >>\nstream\n",
            encoded.len()
        );
        let mut stream_object = header.into_bytes();
        stream_object.extend_from_slice(&encoded);
        stream_object.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &stream_object);

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        Ok(pdf)
    }

    #[test]
    fn hayro_direct_cid_descriptor_font_is_optimized() -> Result<()> {
        let input = hayro_direct_cid_font_fixture()?;
        let mut document = EditDocument::from_bytes(input)?;
        let first = strip_font_editing_tables_hayro(&mut document, 9)?;
        assert_eq!(first.programs_optimized, 1);
        assert_eq!(first.programs_glyph_subset, 0);
        assert!(first.optimized_encoded_bytes < first.original_encoded_bytes);
        assert!(first.decoded_table_bytes_removed > 8192);

        let output = document.write_compact()?;
        let mut reparsed = EditDocument::from_bytes(output)?;
        let second = strip_font_editing_tables_hayro(&mut reparsed, 9)?;
        assert_eq!(second.programs_optimized, 0);
        Ok(())
    }

    #[test]
    fn lone_flate_filter_rejects_multi_filter_arrays() -> Result<()> {
        let document = EditDocument::from_bytes(hayro_font_fixture()?)?;
        assert!(is_lone_flate_filter(
            &document,
            &OwnedObject::Name(b"FlateDecode".to_vec())
        )?);
        assert!(is_lone_flate_filter(
            &document,
            &OwnedObject::Array(vec![OwnedObject::Name(b"FlateDecode".to_vec())])
        )?);
        assert!(!is_lone_flate_filter(
            &document,
            &OwnedObject::Array(vec![
                OwnedObject::Name(b"ASCII85Decode".to_vec()),
                OwnedObject::Name(b"FlateDecode".to_vec()),
            ])
        )?);
        assert!(!is_lone_flate_filter(
            &document,
            &OwnedObject::Array(vec![OwnedObject::Name(b"LZWDecode".to_vec())])
        )?);
        Ok(())
    }

    #[test]
    fn hayro_font_table_strip_accepts_single_flate_filter_array() -> Result<()> {
        let input = hayro_font_fixture_with_filter_array(true)?;
        let mut document = EditDocument::from_bytes(input)?;
        let first = strip_font_editing_tables_hayro(&mut document, 9)?;
        assert_eq!(first.programs_optimized, 1);
        assert!(first.optimized_encoded_bytes < first.original_encoded_bytes);
        assert_eq!(first.decoded_table_bytes_removed, 8192);

        let output = document.write_compact()?;
        let mut reparsed = EditDocument::from_bytes(output)?;
        let second = strip_font_editing_tables_hayro(&mut reparsed, 9)?;
        assert_eq!(second.programs_optimized, 0);
        Ok(())
    }

    #[test]
    fn hayro_font_table_strip_matches_flpdf_accounting() -> Result<()> {
        let input = hayro_font_fixture()?;
        let mut flpdf = Pdf::open(Cursor::new(input.clone()))?;
        let expected = strip_font_editing_tables(&mut flpdf, 9)?;

        let mut document = EditDocument::from_bytes(input)?;
        let actual = strip_font_editing_tables_hayro(&mut document, 9)?;
        assert_eq!(actual, expected);
        assert_eq!(actual.programs_optimized, 1);
        assert!(actual.optimized_encoded_bytes < actual.original_encoded_bytes);
        assert_eq!(actual.decoded_table_bytes_removed, 8192);

        let output = document.write_compact()?;
        let reparsed = SourcePdf::from_bytes(output)?;
        assert_eq!(reparsed.page_count(), 1);
        Ok(())
    }

    fn sfnt(tables: &[([u8; 4], &[u8])]) -> Vec<u8> {
        let mut owned: Vec<([u8; 4], Vec<u8>)> = tables
            .iter()
            .map(|(tag, data)| (*tag, data.to_vec()))
            .collect();
        owned.sort_unstable_by_key(|(tag, _)| *tag);
        let count = owned.len();
        let max_power = 1_usize << (usize::BITS - 1 - count.leading_zeros());
        let search_range = (max_power * 16) as u16;
        let mut out = Vec::new();
        out.extend_from_slice(&[0, 1, 0, 0]);
        out.extend_from_slice(&(count as u16).to_be_bytes());
        out.extend_from_slice(&search_range.to_be_bytes());
        out.extend_from_slice(&(max_power.trailing_zeros() as u16).to_be_bytes());
        out.extend_from_slice(&((count * 16) as u16 - search_range).to_be_bytes());
        let directory = out.len();
        out.resize(directory + count * 16, 0);
        for (index, (tag, data)) in owned.iter().enumerate() {
            while out.len() % 4 != 0 {
                out.push(0);
            }
            let offset = out.len();
            out.extend_from_slice(data);
            while out.len() % 4 != 0 {
                out.push(0);
            }
            let record = directory + index * 16;
            out[record..record + 4].copy_from_slice(tag);
            out[record + 4..record + 8].copy_from_slice(&checksum32(data).to_be_bytes());
            out[record + 8..record + 12].copy_from_slice(&(offset as u32).to_be_bytes());
            out[record + 12..record + 16].copy_from_slice(&(data.len() as u32).to_be_bytes());
        }
        out
    }

    fn sparse_union_test_font(glyphs: &[Option<Vec<u8>>], bbox: [i16; 4]) -> Vec<u8> {
        let mut head = [0_u8; 54];
        head[36..38].copy_from_slice(&bbox[0].to_be_bytes());
        head[38..40].copy_from_slice(&bbox[1].to_be_bytes());
        head[40..42].copy_from_slice(&bbox[2].to_be_bytes());
        head[42..44].copy_from_slice(&bbox[3].to_be_bytes());
        head[50..52].copy_from_slice(&1_i16.to_be_bytes()); // long loca
        let Ok(glyph_count_u8) = u8::try_from(glyphs.len()) else {
            panic!("sparse-union test fixture has too many glyphs");
        };
        let Ok(glyph_count_u16) = u16::try_from(glyphs.len()) else {
            panic!("sparse-union test fixture has too many glyphs");
        };
        let mut maxp = vec![0, 1, 0, 0, 0, glyph_count_u8];
        maxp.resize(32, 0);
        let mut hhea = vec![0_u8; 36];
        hhea[34..36].copy_from_slice(&glyph_count_u16.to_be_bytes());
        let hmtx = vec![0_u8; glyphs.len() * 4];

        let mut glyf = Vec::new();
        let mut offsets = Vec::new();
        for glyph in glyphs {
            offsets.push(glyf.len() as u32);
            if let Some(glyph) = glyph {
                glyf.extend_from_slice(glyph);
                while glyf.len() % 4 != 0 {
                    glyf.push(0);
                }
            }
        }
        offsets.push(glyf.len() as u32);
        let loca = offsets
            .iter()
            .flat_map(|offset| offset.to_be_bytes())
            .collect::<Vec<_>>();
        sfnt(&[
            (*b"head", &head),
            (*b"maxp", &maxp),
            (*b"hhea", &hhea),
            (*b"hmtx", &hmtx),
            (*b"loca", &loca),
            (*b"glyf", &glyf),
            (*b"cvt ", b"cvt"),
            (*b"fpgm", b"fpgm"),
            (*b"prep", b"prep"),
        ])
    }

    fn simple_test_glyph(marker: u8) -> Vec<u8> {
        let mut glyph = vec![0_u8; 12];
        glyph[0..2].copy_from_slice(&1_i16.to_be_bytes());
        glyph[10] = marker;
        glyph[11] = marker ^ 0x5a;
        glyph
    }

    #[test]
    fn sparse_cid_union_combines_disjoint_gids_and_preserves_head_bbox() {
        let bbox = [-123_i16, -456, 789, 1024];
        let glyph0 = simple_test_glyph(0x10);
        let glyph1 = simple_test_glyph(0x11);
        let glyph2 = simple_test_glyph(0x12);
        let left =
            sparse_union_test_font(&[Some(glyph0.clone()), Some(glyph1.clone()), None], bbox);
        let right = sparse_union_test_font(&[Some(glyph0), None, Some(glyph2.clone())], bbox);
        let left_used = BTreeSet::from([1_u16]);
        let right_used = BTreeSet::from([2_u16]);
        let Some(union) = sfnt_union_sparse_glyphs(&[
            (left.as_slice(), &left_used),
            (right.as_slice(), &right_used),
        ]) else {
            panic!("compatible sparse fonts should union");
        };
        let Some(head) = sfnt_table(&union, b"head") else {
            panic!("union should retain head");
        };
        assert_eq!(&head[36..38], &bbox[0].to_be_bytes());
        assert_eq!(&head[38..40], &bbox[1].to_be_bytes());
        assert_eq!(&head[40..42], &bbox[2].to_be_bytes());
        assert_eq!(&head[42..44], &bbox[3].to_be_bytes());

        let Some(offsets) = sfnt_glyph_offsets(&union) else {
            panic!("union should have valid loca");
        };
        let Some(glyf) = sfnt_table(&union, b"glyf") else {
            panic!("union should retain glyf");
        };
        assert_eq!(&glyf[offsets[1]..offsets[2]], glyph1.as_slice());
        assert_eq!(&glyf[offsets[2]..offsets[3]], glyph2.as_slice());
    }

    #[test]
    fn sparse_cid_union_rejects_same_gid_outline_conflict() {
        let bbox = [0_i16, 0, 100, 100];
        let a = sparse_union_test_font(
            &[Some(simple_test_glyph(0)), Some(simple_test_glyph(1))],
            bbox,
        );
        let b = sparse_union_test_font(
            &[Some(simple_test_glyph(0)), Some(simple_test_glyph(2))],
            bbox,
        );
        let used = BTreeSet::from([1_u16]);
        assert!(
            sfnt_union_sparse_glyphs(&[(a.as_slice(), &used), (b.as_slice(), &used)]).is_none()
        );
    }

    #[test]
    fn sparse_cid_union_rejects_different_head_bbox() {
        let glyphs = [Some(simple_test_glyph(0)), Some(simple_test_glyph(1))];
        let a = sparse_union_test_font(&glyphs, [0, 0, 100, 100]);
        let b = sparse_union_test_font(&glyphs, [0, 0, 101, 100]);
        let used = BTreeSet::from([1_u16]);
        assert!(
            sfnt_union_sparse_glyphs(&[(a.as_slice(), &used), (b.as_slice(), &used)]).is_none()
        );
    }

    fn tags(bytes: &[u8]) -> Vec<[u8; 4]> {
        let Some(count) = be16(bytes, 4).map(usize::from) else {
            return Vec::new();
        };
        (0..count)
            .filter_map(|index| bytes.get(12 + index * 16..16 + index * 16)?.try_into().ok())
            .collect()
    }

    fn cmap_table(platform: u16, encoding: u16, subtable: &[u8]) -> Vec<u8> {
        let mut cmap = Vec::new();
        cmap.extend_from_slice(&0_u16.to_be_bytes());
        cmap.extend_from_slice(&1_u16.to_be_bytes());
        cmap.extend_from_slice(&platform.to_be_bytes());
        cmap.extend_from_slice(&encoding.to_be_bytes());
        cmap.extend_from_slice(&12_u32.to_be_bytes());
        cmap.extend_from_slice(subtable);
        cmap
    }

    #[test]
    fn winansi_ascii_uses_unicode_cmap_format4_without_renumbering_gids() {
        // Two segments: A-C -> gids 5-7, then the required 0xffff sentinel.
        let mut format4 = Vec::new();
        format4.extend_from_slice(&4_u16.to_be_bytes());
        format4.extend_from_slice(&32_u16.to_be_bytes());
        format4.extend_from_slice(&0_u16.to_be_bytes());
        format4.extend_from_slice(&4_u16.to_be_bytes()); // segCountX2
        format4.extend_from_slice(&4_u16.to_be_bytes()); // searchRange
        format4.extend_from_slice(&1_u16.to_be_bytes()); // entrySelector
        format4.extend_from_slice(&0_u16.to_be_bytes()); // rangeShift
        format4.extend_from_slice(&67_u16.to_be_bytes());
        format4.extend_from_slice(&u16::MAX.to_be_bytes());
        format4.extend_from_slice(&0_u16.to_be_bytes()); // reservedPad
        format4.extend_from_slice(&65_u16.to_be_bytes());
        format4.extend_from_slice(&u16::MAX.to_be_bytes());
        format4.extend_from_slice(&(-60_i16).to_be_bytes()); // 65 + (-60) = gid 5
        format4.extend_from_slice(&1_i16.to_be_bytes());
        format4.extend_from_slice(&0_u16.to_be_bytes());
        format4.extend_from_slice(&0_u16.to_be_bytes());
        let cmap = cmap_table(3, 1, &format4);
        let font = sfnt(&[(*b"cmap", &cmap)]);
        assert_eq!(sfnt_unicode_gid(&font, 65), Some(5));
        assert_eq!(sfnt_unicode_gid(&font, 67), Some(7));
        assert_eq!(
            sfnt_winansi_ascii_glyph_ids(&font, &BTreeSet::from(*b"AC")),
            Some(BTreeSet::from([5_u16, 7_u16]))
        );
        assert!(sfnt_winansi_ascii_glyph_ids(&font, &BTreeSet::from([0x80])).is_none());
    }

    #[test]
    fn unicode_cmap_format12_maps_supplementary_codepoint() {
        let mut format12 = Vec::new();
        format12.extend_from_slice(&12_u16.to_be_bytes());
        format12.extend_from_slice(&0_u16.to_be_bytes());
        format12.extend_from_slice(&28_u32.to_be_bytes());
        format12.extend_from_slice(&0_u32.to_be_bytes());
        format12.extend_from_slice(&1_u32.to_be_bytes());
        format12.extend_from_slice(&0x1f600_u32.to_be_bytes());
        format12.extend_from_slice(&0x1f602_u32.to_be_bytes());
        format12.extend_from_slice(&42_u32.to_be_bytes());
        let cmap = cmap_table(3, 10, &format12);
        let font = sfnt(&[(*b"cmap", &cmap)]);
        assert_eq!(sfnt_unicode_gid(&font, 0x1f600), Some(42));
        assert_eq!(sfnt_unicode_gid(&font, 0x1f602), Some(44));
    }

    #[test]
    fn rendering_sfnt_drops_layout_and_vertical_metric_tables() {
        let head = [0_u8; 54];
        let source = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyphs"),
            (*b"hmtx", b"metrics"),
            (*b"GSUB", b"substitution"),
            (*b"GPOS", b"positioning"),
            (*b"vhea", b"vertical header"),
            (*b"vmtx", b"vertical metrics"),
        ]);
        let Some((trimmed, removed)) = sfnt_for_pdf_rendering(&source, FontProgramUsage::default())
        else {
            panic!("expected removable rendering-unused tables");
        };
        let remaining = tags(&trimmed);
        assert!(remaining.contains(b"head"));
        assert!(remaining.contains(b"glyf"));
        assert!(remaining.contains(b"hmtx"));
        assert!(!remaining.contains(b"GSUB"));
        assert!(!remaining.contains(b"GPOS"));
        assert!(!remaining.contains(b"vhea"));
        assert!(!remaining.contains(b"vmtx"));
        assert_eq!(removed, 12 + 11 + 15 + 16);
    }

    #[test]
    fn cidfont_type2_drops_cmap_and_post() {
        let head = [0_u8; 54];
        let source = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyphs"),
            (*b"cmap", b"mapping"),
            (*b"post", b"names"),
            (*b"name", b"editing metadata"),
        ]);
        let cid_usage = FontProgramUsage {
            simple_truetype: false,
            cidfont_type2: true,
        };
        let Some((trimmed, _)) = sfnt_for_pdf_rendering(&source, cid_usage) else {
            panic!("expected CID-only table removal");
        };
        let remaining = tags(&trimmed);
        assert!(!remaining.contains(b"cmap"));
        assert!(!remaining.contains(b"post"));

        let simple_usage = FontProgramUsage {
            simple_truetype: true,
            cidfont_type2: false,
        };
        let Some((simple, _)) = sfnt_for_pdf_rendering(&source, simple_usage) else {
            panic!("expected metadata table removal");
        };
        let simple_remaining = tags(&simple);
        assert!(simple_remaining.contains(b"cmap"));
        assert!(simple_remaining.contains(b"post"));
    }

    #[test]
    fn rendering_sfnt_is_noop_without_removable_tables() {
        let head = [0_u8; 54];
        let source = sfnt(&[(*b"head", &head), (*b"glyf", b"glyphs")]);
        assert!(sfnt_for_pdf_rendering(&source, FontProgramUsage::default()).is_none());
    }

    #[test]
    fn retain_gids_blanks_unused_outlines_and_keeps_composite_components() {
        let mut head = [0_u8; 54];
        head[50..52].copy_from_slice(&1_i16.to_be_bytes()); // long loca
        let mut maxp = vec![0, 1, 0, 0, 0, 4];
        maxp.resize(32, 0);

        let glyph0 = [0_u8; 10];
        let mut glyph1 = [0_u8; 10];
        glyph1[2..4].copy_from_slice(&1_i16.to_be_bytes());
        let mut glyph2 = Vec::new();
        glyph2.extend_from_slice(&(-1_i16).to_be_bytes());
        glyph2.extend_from_slice(&[0_u8; 8]);
        glyph2.extend_from_slice(&1_u16.to_be_bytes()); // ARG_1_AND_2_ARE_WORDS
        glyph2.extend_from_slice(&1_u16.to_be_bytes()); // component gid 1
        glyph2.extend_from_slice(&[0_u8; 4]);
        let glyph3 = vec![0x55_u8; 100];

        let mut glyf = Vec::new();
        let mut offsets = Vec::new();
        for glyph in [
            glyph0.as_slice(),
            glyph1.as_slice(),
            glyph2.as_slice(),
            glyph3.as_slice(),
        ] {
            offsets.push(glyf.len() as u32);
            glyf.extend_from_slice(glyph);
        }
        offsets.push(glyf.len() as u32);
        let loca = offsets
            .iter()
            .flat_map(|offset| offset.to_be_bytes())
            .collect::<Vec<_>>();
        let source = sfnt(&[
            (*b"head", &head),
            (*b"maxp", &maxp),
            (*b"loca", &loca),
            (*b"glyf", &glyf),
        ]);

        let requested = BTreeSet::from([2_u16]);
        let Some((subset, removed)) = sfnt_retain_glyph_ids(&source, &requested) else {
            panic!("expected a retain-GID subset");
        };
        assert_eq!(removed, glyph3.len());
        let Some(subset_loca) = sfnt_table(&subset, b"loca") else {
            panic!("subset should contain loca");
        };
        let Some(subset_glyf) = sfnt_table(&subset, b"glyf") else {
            panic!("subset should contain glyf");
        };
        let mut rebuilt = Vec::new();
        for index in 0..=4 {
            let Some(offset) = be32(subset_loca, index * 4) else {
                panic!("subset loca should contain all entries");
            };
            rebuilt.push(offset as usize);
        }
        assert!(rebuilt[1] > rebuilt[0]); // .notdef retained
        assert!(rebuilt[2] > rebuilt[1]); // composite dependency retained
        assert!(rebuilt[3] > rebuilt[2]); // requested composite retained
        assert_eq!(rebuilt[4], rebuilt[3]); // unused gid 3 has an empty outline
        assert_eq!(rebuilt[4], subset_glyf.len());
    }

    #[test]
    fn retain_gids_rejects_out_of_range_requests() {
        let mut head = [0_u8; 54];
        head[50..52].copy_from_slice(&1_i16.to_be_bytes());
        let maxp = [0, 1, 0, 0, 0, 1];
        let loca = [0_u8; 8];
        let source = sfnt(&[
            (*b"head", &head),
            (*b"maxp", &maxp),
            (*b"loca", &loca),
            (*b"glyf", b""),
        ]);
        assert!(sfnt_retain_glyph_ids(&source, &BTreeSet::from([1_u16])).is_none());
    }
}
