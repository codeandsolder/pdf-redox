//! Retain-GID subsetting for OpenType embedded monochrome/grayscale bitmap strikes.
//! PDF glyph usage and ownership must be proven by the caller. Every retained
//! bitmap and its metrics are copied verbatim; composite dependencies are closed
//! per strike. EBLC index formats 1–5 are supported; output uses singleton
//! format-1 (variable metrics) or format-2 (shared metrics) records.
//! Unsupported/invalid font tables reject the entire rewrite.

use std::collections::BTreeSet;
use write_fonts::{
    FontBuilder,
    read::{FontRef, TableProvider},
    types::Tag,
};

const EBDT: Tag = Tag::new(b"EBDT");
const EBLC: Tag = Tag::new(b"EBLC");

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn checked_slice(bytes: &[u8], offset: usize, len: usize) -> Option<&[u8]> {
    bytes.get(offset..offset.checked_add(len)?)
}
fn usize32(value: usize) -> Option<u32> {
    u32::try_from(value).ok()
}
fn push16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn push32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put16(out: &mut [u8], index: usize, value: u16) -> Option<()> {
    out.get_mut(index..index.checked_add(2)?)?
        .copy_from_slice(&value.to_be_bytes());
    Some(())
}
fn put32(out: &mut [u8], index: usize, value: u32) -> Option<()> {
    out.get_mut(index..index.checked_add(4)?)?
        .copy_from_slice(&value.to_be_bytes());
    Some(())
}
fn align4(out: &mut Vec<u8>) {
    out.resize((out.len() + 3) & !3, 0);
}

#[derive(Debug)]
enum LocationIndex {
    Offsets32(Vec<u32>),
    Offsets16(Vec<u16>),
    Fixed {
        len: usize,
        metrics: [u8; 8],
    },
    SparseOffsets16(Vec<(u16, u16)>),
    SparseFixed {
        len: usize,
        metrics: [u8; 8],
        gids: Vec<u16>,
    },
}
#[derive(Debug)]
struct Subtable {
    first: u16,
    last: u16,
    image_format: u16,
    base: usize,
    index: LocationIndex,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GlyphBitmap<'a> {
    bytes: &'a [u8],
    image_format: u16,
    shared_metrics: Option<[u8; 8]>,
}
impl Subtable {
    fn glyph<'a>(&self, gid: u16, ebdt: &'a [u8]) -> Option<GlyphBitmap<'a>> {
        if gid < self.first || gid > self.last {
            return None;
        }
        let index = usize::from(gid - self.first);
        let (relative, len, shared_metrics) = match &self.index {
            LocationIndex::Offsets32(offsets) => (
                usize::try_from(*offsets.get(index)?).ok()?,
                usize::try_from(offsets.get(index + 1)?.checked_sub(offsets[index])?).ok()?,
                None,
            ),
            LocationIndex::Offsets16(offsets) => (
                usize::from(*offsets.get(index)?),
                usize::from(offsets.get(index + 1)?.checked_sub(offsets[index])?),
                None,
            ),
            LocationIndex::Fixed { len, metrics } => {
                (index.checked_mul(*len)?, *len, Some(*metrics))
            }
            LocationIndex::SparseOffsets16(pairs) => {
                let place = pairs[..pairs.len().checked_sub(1)?]
                    .binary_search_by_key(&gid, |(key, _)| *key)
                    .ok()?;
                (
                    usize::from(pairs[place].1),
                    usize::from(pairs[place + 1].1.checked_sub(pairs[place].1)?),
                    None,
                )
            }
            LocationIndex::SparseFixed { len, metrics, gids } => {
                let place = gids.binary_search(&gid).ok()?;
                (place.checked_mul(*len)?, *len, Some(*metrics))
            }
        };
        // A zero-length variable-offset record means no bitmap. A zero-sized
        // fixed-metrics glyph still has meaningful EBLC metrics and must be
        // preserved with a legal format-2 index record.
        if len == 0 && shared_metrics.is_none() {
            return None;
        }
        let offset = self.base.checked_add(relative)?;
        Some(GlyphBitmap {
            bytes: checked_slice(ebdt, offset, len)?,
            image_format: self.image_format,
            shared_metrics,
        })
    }
}
struct Strike {
    record: [u8; 48],
    subtables: Vec<Subtable>,
}
impl Strike {
    fn glyph<'a>(&self, gid: u16, ebdt: &'a [u8]) -> Option<GlyphBitmap<'a>> {
        self.subtables
            .iter()
            .find(|entry| gid >= entry.first && gid <= entry.last)?
            .glyph(gid, ebdt)
    }
}
fn parse_variable_offsets(
    eblc: &[u8],
    ebdt: &[u8],
    offset: usize,
    base: usize,
    count: usize,
    fmt: u16,
) -> Option<LocationIndex> {
    let width = if fmt == 1 { 4 } else { 2 };
    let bytes = checked_slice(
        eblc,
        offset.checked_add(8)?,
        count.checked_add(1)?.checked_mul(width)?,
    )?;
    let mut values = Vec::with_capacity(count + 1);
    for pos in (0..bytes.len()).step_by(width) {
        values.push(if fmt == 1 {
            u32_at(bytes, pos)?
        } else {
            u32::from(u16_at(bytes, pos)?)
        });
    }
    if !values.windows(2).all(|v| v[0] <= v[1]) {
        return None;
    }
    checked_slice(ebdt, base, usize::try_from(*values.last()?).ok()?)?;
    if fmt == 1 {
        Some(LocationIndex::Offsets32(values))
    } else {
        Some(LocationIndex::Offsets16(
            values
                .into_iter()
                .map(|v| u16::try_from(v).ok())
                .collect::<Option<Vec<_>>>()?,
        ))
    }
}

fn parse_subtable(
    eblc: &[u8],
    ebdt: &[u8],
    first: u16,
    last: u16,
    offset: usize,
) -> Option<Subtable> {
    let fmt = u16_at(eblc, offset)?;
    let image_format = u16_at(eblc, offset + 2)?;
    let base = usize::try_from(u32_at(eblc, offset + 4)?).ok()?;
    if first > last || !matches!(image_format, 1 | 2 | 5 | 6 | 7 | 8 | 9) {
        return None;
    }
    // OpenType formats 3 and 4 are respectively obsolete and unsupported.
    // Shared metrics are only present for image format 5.
    if (matches!(fmt, 2 | 5) && image_format != 5)
        || (matches!(fmt, 1 | 3 | 4) && image_format == 5)
    {
        return None;
    }
    let count = usize::from(last - first) + 1;
    let index = match fmt {
        1 | 3 => parse_variable_offsets(eblc, ebdt, offset, base, count, fmt)?,
        2 => {
            let len = usize::try_from(u32_at(eblc, offset + 8)?).ok()?;
            let metrics: [u8; 8] = checked_slice(eblc, offset + 12, 8)?.try_into().ok()?;
            checked_slice(ebdt, base, count.checked_mul(len)?)?;
            LocationIndex::Fixed { len, metrics }
        }
        4 => {
            let n = usize::try_from(u32_at(eblc, offset + 8)?).ok()?;
            if n == 0 || n > count {
                return None;
            }
            let bytes = checked_slice(
                eblc,
                offset.checked_add(12)?,
                n.checked_add(1)?.checked_mul(4)?,
            )?;
            let mut pairs = Vec::with_capacity(n + 1);
            for pos in (0..bytes.len()).step_by(4) {
                pairs.push((u16_at(bytes, pos)?, u16_at(bytes, pos + 2)?));
            }
            if !pairs[..n]
                .iter()
                .all(|(gid, _)| *gid >= first && *gid <= last)
                || !pairs[..n].windows(2).all(|v| v[0].0 < v[1].0)
                || !pairs.windows(2).all(|v| v[0].1 <= v[1].1)
            {
                return None;
            }
            checked_slice(ebdt, base, usize::from(pairs.last()?.1))?;
            LocationIndex::SparseOffsets16(pairs)
        }
        5 => {
            let len = usize::try_from(u32_at(eblc, offset + 8)?).ok()?;
            let metrics: [u8; 8] = checked_slice(eblc, offset + 12, 8)?.try_into().ok()?;
            let n = usize::try_from(u32_at(eblc, offset + 20)?).ok()?;
            if n == 0 || n > count {
                return None;
            }
            let bytes = checked_slice(eblc, offset.checked_add(24)?, n.checked_mul(2)?)?;
            let mut gids = Vec::with_capacity(n);
            for pos in (0..bytes.len()).step_by(2) {
                gids.push(u16_at(bytes, pos)?);
            }
            if !gids.iter().all(|&gid| gid >= first && gid <= last)
                || !gids.windows(2).all(|v| v[0] < v[1])
            {
                return None;
            }
            checked_slice(ebdt, base, n.checked_mul(len)?)?;
            LocationIndex::SparseFixed { len, metrics, gids }
        }
        _ => return None,
    };
    Some(Subtable {
        first,
        last,
        image_format,
        base,
        index,
    })
}
fn parse_strikes(eblc: &[u8], ebdt: &[u8], num_glyphs: u16) -> Option<Vec<Strike>> {
    if u32_at(eblc, 0)? != 0x0002_0000 || u32_at(ebdt, 0)? != 0x0002_0000 {
        return None;
    }
    let n = usize::try_from(u32_at(eblc, 4)?).ok()?;
    checked_slice(eblc, 8, n.checked_mul(48)?)?;
    let mut strikes = Vec::with_capacity(n);
    for i in 0..n {
        let record: [u8; 48] = checked_slice(eblc, 8 + i * 48, 48)?.try_into().ok()?;
        let base = usize::try_from(u32_at(&record, 0)?).ok()?;
        let total = usize::try_from(u32_at(&record, 4)?).ok()?;
        let count = usize::try_from(u32_at(&record, 8)?).ok()?;
        checked_slice(eblc, base, total)?;
        let array = checked_slice(eblc, base, count.checked_mul(8)?)?;
        let mut subtables = Vec::with_capacity(count);
        let mut previous = None;
        for row in array.as_chunks::<8>().0 {
            let first = u16_at(row, 0)?;
            let last = u16_at(row, 2)?;
            if last >= num_glyphs || previous.is_some_and(|prev| first <= prev) {
                return None;
            }
            let sub_offset = usize::try_from(u32_at(row, 4)?).ok()?;
            if sub_offset < count.checked_mul(8)? || sub_offset >= total {
                return None;
            }
            // An index subtable is only valid inside its strike-owned region.
            // Never interpret bytes belonging to another strike as offsets.
            let strike_end = base.checked_add(total)?;
            subtables.push(parse_subtable(
                eblc.get(..strike_end)?,
                ebdt,
                first,
                last,
                base.checked_add(sub_offset)?,
            )?);
            previous = Some(last);
        }
        strikes.push(Strike { record, subtables });
    }
    Some(strikes)
}
fn composite_components(glyph: GlyphBitmap<'_>) -> Option<Vec<u16>> {
    match glyph.image_format {
        1 | 2 => {
            checked_slice(glyph.bytes, 0, 5)?;
            Some(Vec::new())
        }
        5 => {
            glyph.shared_metrics?;
            Some(Vec::new())
        }
        6 | 7 => {
            checked_slice(glyph.bytes, 0, 8)?;
            Some(Vec::new())
        }
        8 | 9 => {
            let header = if glyph.image_format == 8 { 8 } else { 10 };
            let count = usize::from(u16_at(glyph.bytes, header - 2)?);
            if glyph.bytes.len() != header.checked_add(count.checked_mul(4)?)? {
                return None;
            }
            let mut deps = Vec::with_capacity(count);
            for i in 0..count {
                deps.push(u16_at(glyph.bytes, header + i * 4)?);
            }
            Some(deps)
        }
        _ => None,
    }
}
// Iterative dependency traversal avoids stack overflow for untrusted nested
// glyphs. Missing component, cycle, or unsupported image rejects the rewrite.
fn required_bitmaps(strike: &Strike, ebdt: &[u8], live: &BTreeSet<u16>) -> Option<BTreeSet<u16>> {
    let mut finished = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut pending = live.iter().map(|&gid| (gid, false)).collect::<Vec<_>>();
    while let Some((gid, done)) = pending.pop() {
        if done {
            active.remove(&gid);
            finished.insert(gid);
            continue;
        }
        if finished.contains(&gid) {
            continue;
        }
        if !active.insert(gid) {
            return None;
        }
        pending.push((gid, true));
        if let Some(glyph) = strike.glyph(gid, ebdt) {
            for dep in composite_components(glyph)? {
                strike.glyph(dep, ebdt)?;
                pending.push((dep, false));
            }
        }
    }
    Some(finished)
}
fn rebuild_strikes(
    eblc: &[u8],
    ebdt: &[u8],
    strikes: &[Strike],
    live: &BTreeSet<u16>,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut new_eblc = eblc[..8].to_vec();
    new_eblc.resize(8 + strikes.len().checked_mul(48)?, 0);
    let mut new_ebdt = ebdt[..4].to_vec();
    for (strike_number, strike) in strikes.iter().enumerate() {
        let kept = required_bitmaps(strike, ebdt, live)?;
        let mut glyphs = Vec::new();
        for gid in kept {
            if let Some(glyph) = strike.glyph(gid, ebdt) {
                composite_components(glyph)?;
                glyphs.push((gid, glyph));
            }
        }
        // Keep all strike entries (even if empty) so EBSC scaling references
        // and the renderer's original choice of ppem strike remain unchanged.
        let array_start = new_eblc.len();
        let records_len = glyphs.len().checked_mul(8)?;
        new_eblc.resize(array_start.checked_add(records_len)?, 0);
        for (i, (gid, glyph)) in glyphs.iter().enumerate() {
            align4(&mut new_eblc);
            let sub_offset = new_eblc.len().checked_sub(array_start)?;
            let arr_offset = array_start.checked_add(i.checked_mul(8)?)?;
            put16(&mut new_eblc, arr_offset, *gid)?;
            put16(&mut new_eblc, arr_offset + 2, *gid)?;
            put32(&mut new_eblc, arr_offset + 4, usize32(sub_offset)?)?;
            let out_format = if glyph.shared_metrics.is_some() { 2 } else { 1 };
            push16(&mut new_eblc, out_format);
            push16(&mut new_eblc, glyph.image_format);
            push32(&mut new_eblc, usize32(new_ebdt.len())?);
            if let Some(metrics) = glyph.shared_metrics {
                push32(&mut new_eblc, usize32(glyph.bytes.len())?);
                new_eblc.extend_from_slice(&metrics);
            } else {
                push32(&mut new_eblc, 0);
                push32(&mut new_eblc, usize32(glyph.bytes.len())?);
            }
            new_ebdt.extend_from_slice(glyph.bytes);
        }
        align4(&mut new_eblc);
        let end = new_eblc.len();
        let record_offset = 8 + strike_number * 48;
        new_eblc[record_offset..record_offset + 48].copy_from_slice(&strike.record);
        put32(&mut new_eblc, record_offset, usize32(array_start)?)?;
        put32(
            &mut new_eblc,
            record_offset + 4,
            usize32(end.checked_sub(array_start)?)?,
        )?;
        put32(&mut new_eblc, record_offset + 8, usize32(glyphs.len())?)?;
        let first = glyphs.first().map_or(0, |(gid, _)| *gid);
        let last = glyphs.last().map_or(0, |(gid, _)| *gid);
        put16(&mut new_eblc, record_offset + 40, first)?;
        put16(&mut new_eblc, record_offset + 42, last)?;
    }
    Some((new_eblc, new_ebdt))
}
/// Retains bitmaps for all PDF-proven live GIDs (plus composite components)
/// with unchanged GID addressing, metrics, ppem, and image data. The caller
/// applies the strict final *encoded* size gate and handles PDF ownership.
pub fn subset_embedded_bitmaps(
    bytes: &[u8],
    used_gids: &BTreeSet<u16>,
) -> Option<(Vec<u8>, usize)> {
    let font = FontRef::new(bytes).ok()?;
    let eblc = font.data_for_tag(EBLC)?;
    let eblc = eblc.as_bytes();
    let ebdt = font.data_for_tag(EBDT)?;
    let ebdt = ebdt.as_bytes();
    let glyph_count = font.maxp().ok()?.num_glyphs();
    let mut live = used_gids.clone();
    live.insert(0);
    if live.iter().any(|&gid| gid >= glyph_count) {
        return None;
    }
    let strikes = parse_strikes(eblc, ebdt, glyph_count)?;
    let (new_eblc, new_ebdt) = rebuild_strikes(eblc, ebdt, &strikes, &live)?;
    if new_eblc.len() + new_ebdt.len() >= eblc.len() + ebdt.len() {
        return None;
    }
    let mut builder = FontBuilder::new();
    builder.add_raw(EBLC, new_eblc);
    builder.add_raw(EBDT, new_ebdt);
    builder.copy_missing_tables(font);
    let result = builder.build();
    let output = FontRef::new(&result).ok()?;
    let out_eblc = output.data_for_tag(EBLC)?;
    let out_eblc = out_eblc.as_bytes();
    let out_ebdt = output.data_for_tag(EBDT)?;
    let out_ebdt = out_ebdt.as_bytes();
    let new_strikes = parse_strikes(out_eblc, out_ebdt, glyph_count)?;
    if new_strikes.len() != strikes.len() {
        return None;
    }
    for (original, reduced) in strikes.iter().zip(&new_strikes) {
        if original.record[12..40] != reduced.record[12..40]
            || original.record[44..48] != reduced.record[44..48]
        {
            return None;
        }
        let keep = required_bitmaps(original, ebdt, &live)?;
        for gid in keep {
            let left = original.glyph(gid, ebdt);
            let right = reduced.glyph(gid, out_ebdt);
            if left != right {
                return None;
            }
        }
    }
    let removed_bytes = eblc.len() + ebdt.len() - (out_eblc.len() + out_ebdt.len());
    Some((result, removed_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font_from_tables(eblc: Vec<u8>, ebdt: Vec<u8>) -> Vec<u8> {
        let mut builder = FontBuilder::new();
        let mut maxp = vec![0_u8; 32];
        maxp[..6].copy_from_slice(&[0, 1, 0, 0, 0, 4]);
        builder.add_raw(Tag::new(b"maxp"), maxp);
        builder.add_raw(EBLC, eblc);
        builder.add_raw(EBDT, ebdt);
        builder.build()
    }

    fn image_data(image_format: u16, marker: u8) -> Vec<u8> {
        // Three bitmaps with actual ink. Correct 80x80 1-bit pixels require
        // 800 bytes; a glyph with 8-byte metrics precedes the pixel data.
        let mut result = match image_format {
            1 | 2 => vec![80, 80, 0, 0, 80],
            6 | 7 => vec![80, 80, 0, 0, 80, 0, 0, 80],
            5 => Vec::new(),
            8 => vec![80, 80, 0, 0, 80, 0, 0, 1, 0, 1, 0, 0],
            9 => vec![80, 80, 0, 0, 80, 0, 0, 80, 0, 1, 0, 1, 0, 0],
            _ => unreachable!(),
        };
        if !matches!(image_format, 8 | 9) {
            result.extend(std::iter::repeat_n(marker, 800));
        }
        result
    }

    fn fixture(index_format: u16, image_format: u16) -> (Vec<u8>, Vec<u8>) {
        let mut images = [
            image_data(image_format, 0xf0),
            image_data(image_format, 0x0f),
            image_data(image_format, 0x55),
        ];
        if matches!(image_format, 8 | 9) {
            // gid1 is the leaf (zero components); gid2 references gid1.
            let header = if image_format == 8 { 8 } else { 10 };
            images[0].truncate(header);
            images[0][header - 2..header].copy_from_slice(&0u16.to_be_bytes());
        }
        let len = images[0].len();
        let offsets = [
            0,
            images[0].len(),
            images[0].len() + images[1].len(),
            images.iter().map(Vec::len).sum::<usize>(),
        ];
        let mut ebdt = vec![0, 2, 0, 0];
        for b in &images {
            ebdt.extend_from_slice(b);
        }
        let mut table = Vec::new();
        push16(&mut table, index_format);
        push16(&mut table, image_format);
        push32(&mut table, 4);
        match index_format {
            1 => {
                for x in offsets {
                    push32(&mut table, u32::try_from(x).unwrap());
                }
            }
            3 => {
                for x in offsets {
                    push16(&mut table, u16::try_from(x).unwrap());
                }
            }
            2 => {
                push32(&mut table, usize32(len).unwrap());
                table.extend([80, 80, 0, 0, 80, 0, 0, 80]);
            }
            4 => {
                push32(&mut table, 3);
                for (gid, offset) in [
                    (1, offsets[0]),
                    (2, offsets[1]),
                    (3, offsets[2]),
                    (u16::MAX, offsets[3]),
                ] {
                    push16(&mut table, gid);
                    push16(&mut table, u16::try_from(offset).unwrap());
                }
            }
            5 => {
                push32(&mut table, usize32(len).unwrap());
                table.extend([80, 80, 0, 0, 80, 0, 0, 80]);
                push32(&mut table, 3);
                for gid in [1, 2, 3] {
                    push16(&mut table, gid);
                }
            }
            _ => unreachable!(),
        }
        align4(&mut table);
        let array_start = 8 + 48;
        let mut eblc = vec![0, 2, 0, 0];
        push32(&mut eblc, 1);
        let mut rec = [0_u8; 48];
        put32(&mut rec, 0, usize32(array_start).unwrap()).unwrap();
        put32(&mut rec, 4, usize32(8 + table.len()).unwrap()).unwrap();
        put32(&mut rec, 8, 1).unwrap();
        put16(&mut rec, 40, 1).unwrap();
        put16(&mut rec, 42, 3).unwrap();
        rec[44..48].copy_from_slice(&[12, 12, 1, 1]);
        eblc.extend_from_slice(&rec);
        push16(&mut eblc, 1);
        push16(&mut eblc, 3);
        push32(&mut eblc, 8);
        eblc.extend(table);
        (eblc, ebdt)
    }

    #[test]
    fn round_trip_all_five_index_formats_with_live_ink_and_orig_gids() {
        for (index_fmt, image_fmt) in [(1, 7), (2, 5), (3, 6), (4, 1), (5, 5)] {
            let (eblc, ebdt) = fixture(index_fmt, image_fmt);
            let source = parse_strikes(&eblc, &ebdt, 4).unwrap();
            let used = BTreeSet::from([2]);
            let (new_eblc, new_ebdt) = rebuild_strikes(&eblc, &ebdt, &source, &used).unwrap();
            let new = parse_strikes(&new_eblc, &new_ebdt, 4).unwrap();
            assert_eq!(new[0].glyph(2, &new_ebdt), source[0].glyph(2, &ebdt));
            assert!(new[0].glyph(1, &new_ebdt).is_none(), "{index_fmt}");
            assert!(new[0].glyph(3, &new_ebdt).is_none(), "{index_fmt}");
            assert_eq!(new_eblc[8 + 44], 12);
            let original_font = font_from_tables(eblc, ebdt);
            let subset = subset_embedded_bitmaps(&original_font, &used);
            assert!(subset.is_some(), "index {index_fmt} must be profitable");
            let (out, _) = subset.unwrap();
            let parsed = FontRef::new(&out).unwrap();
            assert!(parsed.maxp().is_ok());
            let out_ebdt = parsed.data_for_tag(EBDT).unwrap();
            let out_eblc = parsed.data_for_tag(EBLC).unwrap();
            let after = parse_strikes(out_eblc.as_bytes(), out_ebdt.as_bytes(), 4).unwrap();
            assert_eq!(
                after[0].glyph(2, out_ebdt.as_bytes()).unwrap().bytes.len(),
                len_for(image_fmt)
            );
        }
    }

    fn len_for(image_fmt: u16) -> usize {
        match image_fmt {
            5 => 800,
            1 | 2 => 805,
            6 | 7 => 808,
            _ => unreachable!(),
        }
    }

    #[test]
    fn composite_bitmaps_keep_recursive_components_and_reject_cycles() {
        for (fmt, index_fmt) in [(8, 1), (9, 4)] {
            let (eblc, mut ebdt) = fixture(index_fmt, fmt);
            // img format 8/9 uses gid2 as a composite of gid1, and gid3 is
            // irrelevant. The raw component bytes survive without remapping.
            let original = parse_strikes(&eblc, &ebdt, 4).unwrap();
            let keep = BTreeSet::from([2]);
            let (new_eblc, new_ebdt) = rebuild_strikes(&eblc, &ebdt, &original, &keep).unwrap();
            let result = parse_strikes(&new_eblc, &new_ebdt, 4).unwrap();
            for gid in [1, 2] {
                assert_eq!(
                    result[0].glyph(gid, &new_ebdt),
                    original[0].glyph(gid, &ebdt)
                );
            }
            assert!(result[0].glyph(3, &new_ebdt).is_none());
            // Make gid2 reference itself, creating a cyclic bitmap.
            let composite = original[0].glyph(2, &ebdt).unwrap();
            let offset = composite.bytes.as_ptr() as usize - ebdt.as_ptr() as usize;
            let header = if fmt == 8 { 8 } else { 10 };
            ebdt[offset + header..offset + header + 2].copy_from_slice(&2u16.to_be_bytes());
            let source = parse_strikes(&eblc, &ebdt, 4).unwrap();
            assert!(rebuild_strikes(&eblc, &ebdt, &source, &keep).is_none());
        }
    }

    #[test]
    fn zero_length_shared_metrics_are_still_a_present_glyph() {
        let (mut eblc, ebdt) = fixture(2, 5);
        // The three indexed glyphs have zero image bytes but retain shared
        // advances/bearings in EBLC. This is distinct from a missing glyph.
        put32(&mut eblc, 8 + 48 + 8 + 8, 0).unwrap();
        let src = parse_strikes(&eblc, &ebdt, 4).unwrap();
        let metrics = src[0].glyph(2, &ebdt).unwrap().shared_metrics.unwrap();
        let (new_eblc, new_ebdt) =
            rebuild_strikes(&eblc, &ebdt, &src, &BTreeSet::from([2])).unwrap();
        let dst = parse_strikes(&new_eblc, &new_ebdt, 4).unwrap();
        let glyph = dst[0].glyph(2, &new_ebdt).unwrap();
        assert_eq!(glyph.bytes.len(), 0);
        assert_eq!(glyph.shared_metrics, Some(metrics));
        assert!(dst[0].glyph(1, &new_ebdt).is_none());
    }

    #[test]
    fn truncated_offsets_and_unknown_formats_reject_whole_font() {
        let (mut eblc, ebdt) = fixture(1, 7);
        let table = 8 + 48 + 8;
        put32(&mut eblc, table + 8 + 3 * 4, u32::MAX).unwrap();
        assert!(parse_strikes(&eblc, &ebdt, 4).is_none());
        let (mut eblc, ebdt) = fixture(3, 7);
        put16(&mut eblc, 8 + 48 + 8 + 2, 3).unwrap();
        assert!(parse_strikes(&eblc, &ebdt, 4).is_none());
        let (mut eblc, ebdt) = fixture(1, 7);
        put16(&mut eblc, 8 + 48 + 8 + 2, 4).unwrap(); // image format 4 unsupported
        assert!(parse_strikes(&eblc, &ebdt, 4).is_none());
    }
}
