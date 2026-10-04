//! Conservative monochrome embedded-bitmap subsetting for TrueType EBDT/EBLC.

use std::collections::BTreeSet;

const EBLC_HEADER_SIZE: usize = 8;
const BITMAP_SIZE_TABLE_SIZE: usize = 48;
const INDEX_ARRAY_ENTRY_SIZE: usize = 8;
const INDEX_SUB_HEADER_SIZE: usize = 8;

fn be16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) -> Option<()> {
    bytes
        .get_mut(offset..offset.checked_add(2)?)?
        .copy_from_slice(&value.to_be_bytes());
    Some(())
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) -> Option<()> {
    bytes
        .get_mut(offset..offset.checked_add(4)?)?
        .copy_from_slice(&value.to_be_bytes());
    Some(())
}

#[derive(Debug, Clone)]
struct BitmapGlyph {
    gid: u16,
    image_format: u16,
    data: Vec<u8>,
    new_data_offset: u32,
}

#[derive(Debug, Clone)]
struct BitmapStrike {
    size_table: [u8; BITMAP_SIZE_TABLE_SIZE],
    glyphs: Vec<BitmapGlyph>,
}

#[derive(Debug, Clone, Copy)]
enum GlyphLocation {
    Outside,
    Found(usize, usize, u16),
}

fn format_1_or_3_location(
    eblc: &[u8],
    subtable: usize,
    first: u16,
    gid: u16,
    image_data_offset: usize,
    offset_size: usize,
) -> Option<(usize, usize)> {
    let index = usize::from(gid.checked_sub(first)?);
    let array = subtable.checked_add(INDEX_SUB_HEADER_SIZE)?;
    let offset_at = |slot: usize| -> Option<usize> {
        let position = array.checked_add(slot.checked_mul(offset_size)?)?;
        match offset_size {
            2 => Some(usize::from(be16(eblc, position)?)),
            4 => usize::try_from(be32(eblc, position)?).ok(),
            _ => None,
        }
    };
    let start = image_data_offset.checked_add(offset_at(index)?)?;
    let end = image_data_offset.checked_add(offset_at(index.checked_add(1)?)?)?;
    (start <= end).then_some((start, end))
}

fn format_2_location(
    eblc: &[u8],
    subtable: usize,
    first: u16,
    gid: u16,
    image_data_offset: usize,
) -> Option<(usize, usize)> {
    let image_size =
        usize::try_from(be32(eblc, subtable.checked_add(INDEX_SUB_HEADER_SIZE)?)?).ok()?;
    let index = usize::from(gid.checked_sub(first)?);
    let start = image_data_offset.checked_add(index.checked_mul(image_size)?)?;
    Some((start, start.checked_add(image_size)?))
}

fn glyph_location(
    eblc: &[u8],
    subtable: usize,
    first: u16,
    last: u16,
    gid: u16,
) -> Option<GlyphLocation> {
    if gid < first || gid > last {
        return Some(GlyphLocation::Outside);
    }
    let index_format = be16(eblc, subtable)?;
    let image_format = be16(eblc, subtable.checked_add(2)?)?;
    let image_data_offset = usize::try_from(be32(eblc, subtable.checked_add(4)?)?).ok()?;
    let location = match index_format {
        1 => format_1_or_3_location(eblc, subtable, first, gid, image_data_offset, 4)?,
        2 => format_2_location(eblc, subtable, first, gid, image_data_offset)?,
        3 => format_1_or_3_location(eblc, subtable, first, gid, image_data_offset, 2)?,
        _ => return None,
    };
    Some(GlyphLocation::Found(location.0, location.1, image_format))
}

fn collect_strikes(
    ebdt: &[u8],
    eblc: &[u8],
    requested_gids: &BTreeSet<u16>,
) -> Option<Vec<BitmapStrike>> {
    if ebdt.len() < 4 || eblc.len() < EBLC_HEADER_SIZE {
        return None;
    }
    let strike_count = usize::try_from(be32(eblc, 4)?).ok()?;
    let size_tables_end =
        EBLC_HEADER_SIZE.checked_add(strike_count.checked_mul(BITMAP_SIZE_TABLE_SIZE)?)?;
    if size_tables_end > eblc.len() {
        return None;
    }

    let mut strikes = Vec::new();
    for strike_index in 0..strike_count {
        let size_table_offset =
            EBLC_HEADER_SIZE.checked_add(strike_index.checked_mul(BITMAP_SIZE_TABLE_SIZE)?)?;
        let size_table: [u8; BITMAP_SIZE_TABLE_SIZE] = eblc
            .get(size_table_offset..size_table_offset + BITMAP_SIZE_TABLE_SIZE)?
            .try_into()
            .ok()?;
        let array_offset = usize::try_from(be32(&size_table, 0)?).ok()?;
        let index_tables_size = usize::try_from(be32(&size_table, 4)?).ok()?;
        let subtable_count = usize::try_from(be32(&size_table, 8)?).ok()?;
        let array_end = array_offset.checked_add(index_tables_size)?;
        if array_offset < size_tables_end || array_end > eblc.len() {
            return None;
        }
        let entries_end =
            array_offset.checked_add(subtable_count.checked_mul(INDEX_ARRAY_ENTRY_SIZE)?)?;
        if entries_end > array_end {
            return None;
        }

        let mut glyphs = Vec::new();
        for &gid in requested_gids {
            let mut found = None;
            for subtable_index in 0..subtable_count {
                let entry = array_offset
                    .checked_add(subtable_index.checked_mul(INDEX_ARRAY_ENTRY_SIZE)?)?;
                let first = be16(eblc, entry)?;
                let last = be16(eblc, entry.checked_add(2)?)?;
                let additional = usize::try_from(be32(eblc, entry.checked_add(4)?)?).ok()?;
                let subtable = array_offset.checked_add(additional)?;
                if subtable < entries_end || subtable >= array_end {
                    return None;
                }
                if let GlyphLocation::Found(start, end, image_format) =
                    glyph_location(eblc, subtable, first, last, gid)?
                {
                    found = Some((start, end, image_format));
                    break;
                }
            }
            let Some((start, end, image_format)) = found else {
                continue;
            };
            if start == end {
                continue;
            }
            let data = ebdt.get(start..end)?.to_vec();
            glyphs.push(BitmapGlyph {
                gid,
                image_format,
                data,
                new_data_offset: 0,
            });
        }
        if !glyphs.is_empty() {
            glyphs.sort_unstable_by_key(|glyph| glyph.gid);
            glyphs.dedup_by_key(|glyph| glyph.gid);
            strikes.push(BitmapStrike { size_table, glyphs });
        }
    }
    Some(strikes)
}

fn build_ebdt(original: &[u8], strikes: &mut [BitmapStrike]) -> Option<Vec<u8>> {
    let mut output = original.get(..4)?.to_vec();
    for strike in strikes {
        for glyph in &mut strike.glyphs {
            glyph.new_data_offset = u32::try_from(output.len()).ok()?;
            output.extend_from_slice(&glyph.data);
        }
    }
    Some(output)
}

fn subtable_bytes(glyph: &BitmapGlyph) -> Option<Vec<u8>> {
    let length = glyph.data.len();
    let use_u16 = u16::try_from(length).is_ok();
    let index_format = if use_u16 { 3_u16 } else { 1_u16 };
    let mut output = Vec::with_capacity(if use_u16 { 12 } else { 16 });
    output.extend_from_slice(&index_format.to_be_bytes());
    output.extend_from_slice(&glyph.image_format.to_be_bytes());
    output.extend_from_slice(&glyph.new_data_offset.to_be_bytes());
    if use_u16 {
        output.extend_from_slice(&0_u16.to_be_bytes());
        output.extend_from_slice(&u16::try_from(length).ok()?.to_be_bytes());
    } else {
        output.extend_from_slice(&0_u32.to_be_bytes());
        output.extend_from_slice(&u32::try_from(length).ok()?.to_be_bytes());
    }
    Some(output)
}

fn build_eblc(original: &[u8], strikes: &[BitmapStrike]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    output.extend_from_slice(original.get(..4)?);
    output.extend_from_slice(&u32::try_from(strikes.len()).ok()?.to_be_bytes());

    let size_table_start = output.len();
    output.resize(
        size_table_start.checked_add(strikes.len().checked_mul(BITMAP_SIZE_TABLE_SIZE)?)?,
        0,
    );

    for (strike_index, strike) in strikes.iter().enumerate() {
        let array_offset = output.len();
        let array_size = strike.glyphs.len().checked_mul(INDEX_ARRAY_ENTRY_SIZE)?;
        output.resize(output.len().checked_add(array_size)?, 0);

        let mut subtables = Vec::with_capacity(strike.glyphs.len());
        let mut additional = array_size;
        for glyph in &strike.glyphs {
            let subtable = subtable_bytes(glyph)?;
            subtables.push((additional, subtable));
            additional = additional.checked_add(subtables.last()?.1.len())?;
        }

        for (index, (additional_offset, _)) in subtables.iter().enumerate() {
            let entry = array_offset.checked_add(index.checked_mul(INDEX_ARRAY_ENTRY_SIZE)?)?;
            let glyph = &strike.glyphs[index];
            put_u16(&mut output, entry, glyph.gid)?;
            put_u16(&mut output, entry.checked_add(2)?, glyph.gid)?;
            put_u32(
                &mut output,
                entry.checked_add(4)?,
                u32::try_from(*additional_offset).ok()?,
            )?;
        }
        for (_, subtable) in &subtables {
            output.extend_from_slice(subtable);
        }

        let mut size_table = strike.size_table;
        put_u32(&mut size_table, 0, u32::try_from(array_offset).ok()?)?;
        put_u32(&mut size_table, 4, u32::try_from(additional).ok()?)?;
        put_u32(&mut size_table, 8, u32::try_from(strike.glyphs.len()).ok()?)?;
        put_u16(&mut size_table, 40, strike.glyphs.first()?.gid)?;
        put_u16(&mut size_table, 42, strike.glyphs.last()?.gid)?;
        let target =
            size_table_start.checked_add(strike_index.checked_mul(BITMAP_SIZE_TABLE_SIZE)?)?;
        output
            .get_mut(target..target + BITMAP_SIZE_TABLE_SIZE)?
            .copy_from_slice(&size_table);
    }

    Some(output)
}

/// Subset monochrome EBDT/EBLC strikes to the requested stable glyph IDs.
///
/// Index formats 1, 2, and 3 are accepted. Retained image records are copied
/// byte-for-byte and rebuilt as one-glyph format-1/3 index subtables.
pub fn subset_ebdt_eblc(
    ebdt: &[u8],
    eblc: &[u8],
    requested_gids: &BTreeSet<u16>,
) -> Option<(Vec<u8>, Vec<u8>, usize)> {
    if requested_gids.is_empty() {
        return None;
    }
    let mut strikes = collect_strikes(ebdt, eblc, requested_gids)?;
    if strikes.is_empty() {
        return None;
    }
    let new_ebdt = build_ebdt(ebdt, &mut strikes)?;
    let new_eblc = build_eblc(eblc, &strikes)?;
    let old_size = ebdt.len().checked_add(eblc.len())?;
    let new_size = new_ebdt.len().checked_add(new_eblc.len())?;
    let removed = old_size.checked_sub(new_size)?;
    (removed > 0).then_some((new_ebdt, new_eblc, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tables() -> (Vec<u8>, Vec<u8>) {
        let image_a = [1_u8, 2, 3];
        let image_b = [4_u8, 5, 6, 7, 8];

        let mut ebdt = vec![0, 2, 0, 0];
        ebdt.extend_from_slice(&image_a);
        ebdt.extend_from_slice(&image_b);

        let mut eblc = vec![0, 2, 0, 0];
        eblc.extend_from_slice(&1_u32.to_be_bytes());
        let size_table_at = eblc.len();
        eblc.resize(size_table_at + BITMAP_SIZE_TABLE_SIZE, 0);

        let array_offset = eblc.len();
        eblc.extend_from_slice(&3_u16.to_be_bytes());
        eblc.extend_from_slice(&4_u16.to_be_bytes());
        eblc.extend_from_slice(&8_u32.to_be_bytes());

        eblc.extend_from_slice(&3_u16.to_be_bytes());
        eblc.extend_from_slice(&7_u16.to_be_bytes());
        eblc.extend_from_slice(&4_u32.to_be_bytes());
        eblc.extend_from_slice(&0_u16.to_be_bytes());
        eblc.extend_from_slice(&3_u16.to_be_bytes());
        eblc.extend_from_slice(&8_u16.to_be_bytes());
        eblc.extend_from_slice(&0_u16.to_be_bytes());

        let table_size = eblc.len() - array_offset;
        let table = &mut eblc[size_table_at..size_table_at + BITMAP_SIZE_TABLE_SIZE];
        put_u32(table, 0, u32::try_from(array_offset).unwrap_or_default());
        put_u32(table, 4, u32::try_from(table_size).unwrap_or_default());
        put_u32(table, 8, 1);
        put_u16(table, 40, 3);
        put_u16(table, 42, 4);
        table[44] = 12;
        table[45] = 12;
        table[46] = 1;
        (ebdt, eblc)
    }

    #[test]
    fn subsets_format3_bitmap_range_to_requested_gid() {
        let (ebdt, eblc) = sample_tables();
        let result = subset_ebdt_eblc(&ebdt, &eblc, &BTreeSet::from([4_u16]));
        assert!(result.is_some(), "expected bitmap subset");
        let Some((new_ebdt, new_eblc, removed)) = result else {
            return;
        };
        assert!(removed > 0);
        assert_eq!(&new_ebdt[..4], &ebdt[..4]);
        assert_eq!(&new_ebdt[4..], &[4, 5, 6, 7, 8]);
        assert_eq!(be32(&new_eblc, 4), Some(1));
        let size = &new_eblc[8..8 + BITMAP_SIZE_TABLE_SIZE];
        assert_eq!(be16(size, 40), Some(4));
        assert_eq!(be16(size, 42), Some(4));
    }
}
