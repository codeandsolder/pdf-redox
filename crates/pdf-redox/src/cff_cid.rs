//! Conservative CID-keyed CFF1 subsetting for embedded PDF `FontFile3` streams.
//!
//! This intentionally handles only the structurally simple shape seen in PDF
//! producer subsets where `CharStrings` dominate storage: one font, one FD,
//! custom charset, no global or local subroutines. Selected Type 2 `CharStrings`
//! and private-dictionary bytes are copied verbatim; only CFF indexes and
//! absolute offsets are rebuilt.

use std::collections::BTreeSet;

const OP_CHARSET: u16 = 15;
const OP_ENCODING: u16 = 16;
const OP_CHAR_STRINGS: u16 = 17;
const OP_PRIVATE: u16 = 18;
const OP_SUBRS: u16 = 19;
const OP_ROS: u16 = 0x0c1e;
const OP_FD_ARRAY: u16 = 0x0c24;
const OP_FD_SELECT: u16 = 0x0c25;

#[derive(Debug)]
struct CffIndex<'a> {
    items: Vec<&'a [u8]>,
    end: usize,
}

#[derive(Debug, Clone, Copy)]
enum DictNumber {
    Integer(i32),
    Other,
}

#[derive(Debug)]
struct DictEntry<'a> {
    op: u16,
    raw: &'a [u8],
    numbers: Vec<DictNumber>,
}

fn read_offset(bytes: &[u8], offset: &mut usize, size: usize) -> Option<usize> {
    if !(1..=4).contains(&size) {
        return None;
    }
    let end = offset.checked_add(size)?;
    let mut value = 0usize;
    for byte in bytes.get(*offset..end)? {
        value = value.checked_shl(8)?.checked_add(usize::from(*byte))?;
    }
    *offset = end;
    Some(value)
}

fn parse_index(bytes: &[u8], start: usize) -> Option<CffIndex<'_>> {
    let count = usize::from(u16::from_be_bytes(
        bytes.get(start..start.checked_add(2)?)?.try_into().ok()?,
    ));
    if count == 0 {
        return Some(CffIndex {
            items: Vec::new(),
            end: start.checked_add(2)?,
        });
    }

    let off_size_pos = start.checked_add(2)?;
    let off_size = usize::from(*bytes.get(off_size_pos)?);
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let mut cursor = off_size_pos.checked_add(1)?;
    let mut offsets = Vec::with_capacity(count.checked_add(1)?);
    for _ in 0..=count {
        offsets.push(read_offset(bytes, &mut cursor, off_size)?);
    }
    if offsets.first().copied() != Some(1) {
        return None;
    }
    let data_start = cursor;
    let data_len = offsets.last()?.checked_sub(1)?;
    let end = data_start.checked_add(data_len)?;
    if end > bytes.len() {
        return None;
    }

    let mut items = Vec::with_capacity(count);
    for pair in offsets.windows(2) {
        let item_start = data_start.checked_add(pair[0].checked_sub(1)?)?;
        let item_end = data_start.checked_add(pair[1].checked_sub(1)?)?;
        if item_start > item_end || item_end > end {
            return None;
        }
        items.push(bytes.get(item_start..item_end)?);
    }
    Some(CffIndex { items, end })
}

fn parse_dict_number(bytes: &[u8], cursor: &mut usize) -> Option<DictNumber> {
    let first = *bytes.get(*cursor)?;
    match first {
        28 => {
            let start = cursor.checked_add(1)?;
            let end = start.checked_add(2)?;
            let value = i16::from_be_bytes(bytes.get(start..end)?.try_into().ok()?);
            *cursor = end;
            Some(DictNumber::Integer(i32::from(value)))
        }
        29 => {
            let start = cursor.checked_add(1)?;
            let end = start.checked_add(4)?;
            let value = i32::from_be_bytes(bytes.get(start..end)?.try_into().ok()?);
            *cursor = end;
            Some(DictNumber::Integer(value))
        }
        30 => {
            *cursor = cursor.checked_add(1)?;
            loop {
                let byte = *bytes.get(*cursor)?;
                *cursor = cursor.checked_add(1)?;
                if byte >> 4 == 0x0f || byte & 0x0f == 0x0f {
                    break;
                }
            }
            Some(DictNumber::Other)
        }
        32..=246 => {
            *cursor = cursor.checked_add(1)?;
            Some(DictNumber::Integer(i32::from(first) - 139))
        }
        247..=250 => {
            let second = i32::from(*bytes.get(cursor.checked_add(1)?)?);
            *cursor = cursor.checked_add(2)?;
            Some(DictNumber::Integer(
                (i32::from(first) - 247) * 256 + second + 108,
            ))
        }
        251..=254 => {
            let second = i32::from(*bytes.get(cursor.checked_add(1)?)?);
            *cursor = cursor.checked_add(2)?;
            Some(DictNumber::Integer(
                -(i32::from(first) - 251) * 256 - second - 108,
            ))
        }
        255 => {
            let start = cursor.checked_add(1)?;
            let end = start.checked_add(4)?;
            let _raw = i32::from_be_bytes(bytes.get(start..end)?.try_into().ok()?);
            *cursor = end;
            Some(DictNumber::Other)
        }
        _ => None,
    }
}

fn parse_dict(bytes: &[u8]) -> Option<Vec<DictEntry<'_>>> {
    let mut entries = Vec::new();
    let mut cursor = 0usize;
    let mut entry_start = 0usize;
    let mut numbers = Vec::new();

    while cursor < bytes.len() {
        let byte = *bytes.get(cursor)?;
        if byte <= 21 {
            let op = if byte == 12 {
                let escaped = u16::from(*bytes.get(cursor.checked_add(1)?)?);
                cursor = cursor.checked_add(2)?;
                0x0c00 | escaped
            } else {
                cursor = cursor.checked_add(1)?;
                u16::from(byte)
            };
            entries.push(DictEntry {
                op,
                raw: bytes.get(entry_start..cursor)?,
                numbers: std::mem::take(&mut numbers),
            });
            entry_start = cursor;
        } else {
            numbers.push(parse_dict_number(bytes, &mut cursor)?);
        }
    }
    if entry_start != bytes.len() || !numbers.is_empty() {
        return None;
    }
    Some(entries)
}

const fn integer(number: DictNumber) -> Option<i32> {
    match number {
        DictNumber::Integer(value) => Some(value),
        DictNumber::Other => None,
    }
}

fn one_integer(entry: &DictEntry<'_>) -> Option<usize> {
    if entry.numbers.len() != 1 {
        return None;
    }
    usize::try_from(integer(entry.numbers[0])?).ok()
}

fn two_integers(entry: &DictEntry<'_>) -> Option<(usize, usize)> {
    if entry.numbers.len() != 2 {
        return None;
    }
    Some((
        usize::try_from(integer(entry.numbers[0])?).ok()?,
        usize::try_from(integer(entry.numbers[1])?).ok()?,
    ))
}

fn find_entry<'a>(entries: &'a [DictEntry<'a>], op: u16) -> Option<&'a DictEntry<'a>> {
    let mut matches = entries.iter().filter(|entry| entry.op == op);
    let entry = matches.next()?;
    matches.next().is_none().then_some(entry)
}

fn parse_charset(bytes: &[u8], offset: usize, glyph_count: usize) -> Option<Vec<u16>> {
    if glyph_count == 0 || offset < 3 {
        return None;
    }
    let mut cids = Vec::with_capacity(glyph_count);
    cids.push(0);
    if glyph_count == 1 {
        return Some(cids);
    }
    let format = *bytes.get(offset)?;
    let mut cursor = offset.checked_add(1)?;
    match format {
        0 => {
            for _ in 1..glyph_count {
                let end = cursor.checked_add(2)?;
                cids.push(u16::from_be_bytes(bytes.get(cursor..end)?.try_into().ok()?));
                cursor = end;
            }
        }
        1 | 2 => {
            while cids.len() < glyph_count {
                let end = cursor.checked_add(2)?;
                let first = u16::from_be_bytes(bytes.get(cursor..end)?.try_into().ok()?);
                cursor = end;
                let left = if format == 1 {
                    let value = usize::from(*bytes.get(cursor)?);
                    cursor = cursor.checked_add(1)?;
                    value
                } else {
                    let end = cursor.checked_add(2)?;
                    let value =
                        usize::from(u16::from_be_bytes(bytes.get(cursor..end)?.try_into().ok()?));
                    cursor = end;
                    value
                };
                for delta in 0..=left {
                    if cids.len() >= glyph_count {
                        return None;
                    }
                    cids.push(first.checked_add(u16::try_from(delta).ok()?)?);
                }
            }
        }
        _ => return None,
    }
    (cids.len() == glyph_count).then_some(cids)
}

const fn encoded_offset_size(max_offset: usize) -> usize {
    if max_offset <= 0xff {
        1
    } else if max_offset <= 0xffff {
        2
    } else if max_offset <= 0x00ff_ffff {
        3
    } else {
        4
    }
}

fn write_offset(output: &mut Vec<u8>, value: usize, size: usize) -> Option<()> {
    let value = u32::try_from(value).ok()?;
    let bytes = value.to_be_bytes();
    output.extend_from_slice(bytes.get(4usize.checked_sub(size)?..)?);
    Some(())
}

fn build_index(items: &[&[u8]]) -> Option<Vec<u8>> {
    let count = u16::try_from(items.len()).ok()?;
    let mut output = Vec::new();
    output.extend_from_slice(&count.to_be_bytes());
    if items.is_empty() {
        return Some(output);
    }

    let data_len = items
        .iter()
        .try_fold(0usize, |sum, item| sum.checked_add(item.len()))?;
    let final_offset = data_len.checked_add(1)?;
    let off_size = encoded_offset_size(final_offset);
    output.push(u8::try_from(off_size).ok()?);
    let mut offset = 1usize;
    write_offset(&mut output, offset, off_size)?;
    for item in items {
        offset = offset.checked_add(item.len())?;
        write_offset(&mut output, offset, off_size)?;
    }
    for item in items {
        output.extend_from_slice(item);
    }
    Some(output)
}

fn encode_dict_integer(value: usize, output: &mut Vec<u8>) -> Option<()> {
    let value = i32::try_from(value).ok()?;
    if (-107..=107).contains(&value) {
        output.push(u8::try_from(value + 139).ok()?);
    } else if (108..=1131).contains(&value) {
        let adjusted = value - 108;
        output.push(u8::try_from(adjusted / 256 + 247).ok()?);
        output.push(u8::try_from(adjusted % 256).ok()?);
    } else if (-1131..=-108).contains(&value) {
        let adjusted = -value - 108;
        output.push(u8::try_from(adjusted / 256 + 251).ok()?);
        output.push(u8::try_from(adjusted % 256).ok()?);
    } else if let Ok(short) = i16::try_from(value) {
        output.push(28);
        output.extend_from_slice(&short.to_be_bytes());
    } else {
        output.push(29);
        output.extend_from_slice(&value.to_be_bytes());
    }
    Some(())
}

fn append_operator(output: &mut Vec<u8>, op: u16) -> Option<()> {
    if op & 0xff00 == 0x0c00 {
        output.push(12);
        output.push(u8::try_from(op & 0xff).ok()?);
    } else {
        output.push(u8::try_from(op).ok()?);
    }
    Some(())
}

fn rebuilt_top_dict(
    entries: &[DictEntry<'_>],
    charset: usize,
    char_strings: usize,
    fd_array: usize,
    fd_select: usize,
) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    for entry in entries {
        if matches!(
            entry.op,
            OP_CHARSET | OP_CHAR_STRINGS | OP_FD_ARRAY | OP_FD_SELECT
        ) {
            continue;
        }
        output.extend_from_slice(entry.raw);
    }
    for (value, op) in [
        (charset, OP_CHARSET),
        (char_strings, OP_CHAR_STRINGS),
        (fd_array, OP_FD_ARRAY),
        (fd_select, OP_FD_SELECT),
    ] {
        encode_dict_integer(value, &mut output)?;
        append_operator(&mut output, op)?;
    }
    Some(output)
}

fn rebuilt_fd_dict(
    entries: &[DictEntry<'_>],
    private_size: usize,
    private: usize,
) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    for entry in entries {
        if entry.op != OP_PRIVATE {
            output.extend_from_slice(entry.raw);
        }
    }
    encode_dict_integer(private_size, &mut output)?;
    encode_dict_integer(private, &mut output)?;
    append_operator(&mut output, OP_PRIVATE)?;
    Some(output)
}

fn build_charset(cids: &[u16]) -> Vec<u8> {
    let mut output = Vec::with_capacity(1 + cids.len().saturating_sub(1) * 2);
    output.push(0);
    for cid in cids.iter().skip(1) {
        output.extend_from_slice(&cid.to_be_bytes());
    }
    output
}

fn build_fd_select(glyph_count: usize) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(glyph_count.checked_add(1)?);
    output.push(0);
    output.resize(glyph_count.checked_add(1)?, 0);
    Some(output)
}

/// Subset a structurally simple CID-keyed CFF1 program by CID.
///
/// The current conservative gate accepts one font, one FD, no global or local
/// subroutines, and a custom charset. Selected `CharStrings` remain byte-for-byte
/// unchanged. Returns the rewritten CFF and decoded bytes removed.
#[expect(
    clippy::too_many_lines,
    reason = "CFF parsing, conservative shape validation, offset fixed-point layout, and rebuilding are one fail-closed transaction"
)]
pub fn subset_cid_font(bytes: &[u8], requested_cids: &BTreeSet<u16>) -> Option<(Vec<u8>, usize)> {
    if requested_cids.is_empty() || bytes.first().copied()? != 1 {
        return None;
    }
    let header_size = usize::from(*bytes.get(2)?);
    let header_off_size = *bytes.get(3)?;
    if header_size < 4 || header_size > bytes.len() || !(1..=4).contains(&header_off_size) {
        return None;
    }

    let name_start = header_size;
    let name_index = parse_index(bytes, name_start)?;
    if name_index.items.len() != 1 {
        return None;
    }
    let top_start = name_index.end;
    let top_index = parse_index(bytes, top_start)?;
    if top_index.items.len() != 1 {
        return None;
    }
    let string_start = top_index.end;
    let string_index = parse_index(bytes, string_start)?;
    let global_start = string_index.end;
    let global_subrs = parse_index(bytes, global_start)?;
    if !global_subrs.items.is_empty() {
        return None;
    }

    let top_entries = parse_dict(top_index.items[0])?;
    if find_entry(&top_entries, OP_ROS).is_none()
        || find_entry(&top_entries, OP_ENCODING).is_some()
        || find_entry(&top_entries, OP_PRIVATE).is_some()
    {
        return None;
    }
    let charset_offset = one_integer(find_entry(&top_entries, OP_CHARSET)?)?;
    let char_strings_offset = one_integer(find_entry(&top_entries, OP_CHAR_STRINGS)?)?;
    let fd_array_offset = one_integer(find_entry(&top_entries, OP_FD_ARRAY)?)?;
    let _fd_select_offset = one_integer(find_entry(&top_entries, OP_FD_SELECT)?)?;

    let char_strings = parse_index(bytes, char_strings_offset)?;
    if char_strings.items.is_empty() {
        return None;
    }
    let charset = parse_charset(bytes, charset_offset, char_strings.items.len())?;
    if !requested_cids.iter().all(|cid| charset.contains(cid)) {
        return None;
    }

    let fd_array = parse_index(bytes, fd_array_offset)?;
    if fd_array.items.len() != 1 {
        return None;
    }
    let fd_entries = parse_dict(fd_array.items[0])?;
    let (private_size, private_offset) = two_integers(find_entry(&fd_entries, OP_PRIVATE)?)?;
    let private_end = private_offset.checked_add(private_size)?;
    let private_bytes = bytes.get(private_offset..private_end)?;
    let private_entries = parse_dict(private_bytes)?;
    if private_entries.iter().any(|entry| entry.op == OP_SUBRS) {
        return None;
    }

    let mut kept_cids = Vec::new();
    let mut kept_charstrings = Vec::new();
    for (gid, (&cid, charstring)) in charset.iter().zip(&char_strings.items).enumerate() {
        if gid == 0 || requested_cids.contains(&cid) {
            kept_cids.push(cid);
            kept_charstrings.push(*charstring);
        }
    }
    if kept_charstrings.len() == char_strings.items.len() || kept_charstrings.is_empty() {
        return None;
    }

    let charset_bytes = build_charset(&kept_cids);
    let fd_select_bytes = build_fd_select(kept_charstrings.len())?;
    let char_strings_bytes = build_index(&kept_charstrings)?;

    let header = bytes.get(..header_size)?;
    let name_bytes = bytes.get(name_start..name_index.end)?;
    let string_bytes = bytes.get(string_start..string_index.end)?;
    let global_bytes = bytes.get(global_start..global_subrs.end)?;

    let mut top_bytes = build_index(&[rebuilt_top_dict(&top_entries, 0, 0, 0, 0)?.as_slice()])?;
    let mut fd_bytes = build_index(&[rebuilt_fd_dict(&fd_entries, private_size, 0)?.as_slice()])?;

    for _ in 0..8 {
        let prefix = header
            .len()
            .checked_add(name_bytes.len())?
            .checked_add(top_bytes.len())?
            .checked_add(string_bytes.len())?
            .checked_add(global_bytes.len())?;
        let new_charset_offset = prefix;
        let new_fd_select_offset = new_charset_offset.checked_add(charset_bytes.len())?;
        let new_char_strings_offset = new_fd_select_offset.checked_add(fd_select_bytes.len())?;
        let new_fd_array_offset = new_char_strings_offset.checked_add(char_strings_bytes.len())?;
        let new_private_offset = new_fd_array_offset.checked_add(fd_bytes.len())?;

        let next_top_dict = rebuilt_top_dict(
            &top_entries,
            new_charset_offset,
            new_char_strings_offset,
            new_fd_array_offset,
            new_fd_select_offset,
        )?;
        let next_top = build_index(&[next_top_dict.as_slice()])?;
        let next_fd_dict = rebuilt_fd_dict(&fd_entries, private_size, new_private_offset)?;
        let next_fd = build_index(&[next_fd_dict.as_slice()])?;

        let stable = next_top == top_bytes && next_fd == fd_bytes;
        top_bytes = next_top;
        fd_bytes = next_fd;
        if stable {
            break;
        }
    }

    let prefix = header
        .len()
        .checked_add(name_bytes.len())?
        .checked_add(top_bytes.len())?
        .checked_add(string_bytes.len())?
        .checked_add(global_bytes.len())?;
    let expected_charset = prefix;
    let expected_fd_select = expected_charset.checked_add(charset_bytes.len())?;
    let expected_char_strings = expected_fd_select.checked_add(fd_select_bytes.len())?;
    let expected_fd_array = expected_char_strings.checked_add(char_strings_bytes.len())?;
    let expected_private = expected_fd_array.checked_add(fd_bytes.len())?;
    let check_top = rebuilt_top_dict(
        &top_entries,
        expected_charset,
        expected_char_strings,
        expected_fd_array,
        expected_fd_select,
    )?;
    let check_fd = rebuilt_fd_dict(&fd_entries, private_size, expected_private)?;
    if build_index(&[check_top.as_slice()])? != top_bytes
        || build_index(&[check_fd.as_slice()])? != fd_bytes
    {
        return None;
    }

    let mut output = Vec::with_capacity(expected_private.checked_add(private_bytes.len())?);
    output.extend_from_slice(header);
    output.extend_from_slice(name_bytes);
    output.extend_from_slice(&top_bytes);
    output.extend_from_slice(string_bytes);
    output.extend_from_slice(global_bytes);
    output.extend_from_slice(&charset_bytes);
    output.extend_from_slice(&fd_select_bytes);
    output.extend_from_slice(&char_strings_bytes);
    output.extend_from_slice(&fd_bytes);
    output.extend_from_slice(private_bytes);

    let removed = bytes.len().checked_sub(output.len())?;
    (removed > 0).then_some((output, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cff_index_round_trip_shape() {
        let items = [b"abc".as_slice(), b"".as_slice(), b"defgh".as_slice()];
        let encoded = build_index(&items).unwrap_or_default();
        let parsed = parse_index(&encoded, 0);
        assert!(parsed.is_some());
        let parsed = parsed.unwrap_or(CffIndex {
            items: Vec::new(),
            end: 0,
        });
        assert_eq!(parsed.items, items);
        assert_eq!(parsed.end, encoded.len());
    }

    #[test]
    fn cff_dict_integer_encoding_round_trips() {
        for value in [0usize, 107, 108, 1131, 1132, 32_767, 32_768, 12_807_299] {
            let mut encoded = Vec::new();
            assert!(encode_dict_integer(value, &mut encoded).is_some());
            let mut cursor = 0usize;
            let decoded = parse_dict_number(&encoded, &mut cursor).and_then(integer);
            assert_eq!(decoded, i32::try_from(value).ok());
            assert_eq!(cursor, encoded.len());
        }
    }
}
