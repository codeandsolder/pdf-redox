//! Hayro-backed content-stream helpers shared by optimization passes.

use crate::Result;
use hayro_syntax::{content::UntypedIter, object::Object as HayroObject};
use smallvec::SmallVec;

/// Visit whole Hayro content instructions without recreating token callbacks.
///
/// Returns `true` when parsing ended before the physical end of the stream.
pub fn visit_instructions(
    input: &[u8],
    mut visit: impl FnMut(&hayro_syntax::content::Instruction<'_, '_>) -> Result<()>,
) -> Result<bool> {
    let mut iter = UntypedIter::new(input);
    while let Some(instruction) = iter.next() {
        visit(&instruction)?;
    }
    Ok(!iter.is_at_end())
}

/// Whether bytes consist solely of PDF content whitespace and comments.
pub const fn trivia_only(bytes: &[u8]) -> bool {
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            0 | b'\t' | b'\n' | 0x0c | b'\r' | b' ' => cursor += 1,
            b'%' => {
                cursor += 1;
                while cursor < bytes.len() && !matches!(bytes[cursor], b'\n' | b'\r') {
                    cursor += 1;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Resource dictionary category referenced by a content operator.
pub const fn resource_type_for_operator(operator: &[u8]) -> Option<&'static [u8]> {
    match operator {
        b"CS" | b"cs" => Some(b"ColorSpace"),
        b"gs" => Some(b"ExtGState"),
        b"Tf" => Some(b"Font"),
        b"SCN" | b"scn" => Some(b"Pattern"),
        b"BDC" | b"DP" => Some(b"Properties"),
        b"sh" => Some(b"Shading"),
        b"Do" => Some(b"XObject"),
        _ => None,
    }
}

/// Parsed scalar/name content operand with its byte offset in the source stream.
#[derive(Debug, Clone)]
pub struct InstructionOperand {
    pub number: Option<f64>,
    pub name: Option<Vec<u8>>,
    pub offset: usize,
}

/// Parse the scalar/name facets used by graphics-state and geometry scanners.
pub fn instruction_operands(
    input: &[u8],
    instruction: &hayro_syntax::content::Instruction<'_, '_>,
) -> Vec<InstructionOperand> {
    let mut operands = Vec::new();
    instruction_operands_into(input, instruction, &mut operands);
    operands
}

/// Parse the same operand facets into an existing buffer, retaining capacity
/// across instructions in long-running page scanners.
pub fn instruction_operands_into(
    input: &[u8],
    instruction: &hayro_syntax::content::Instruction<'_, '_>,
    operands: &mut Vec<InstructionOperand>,
) {
    operands.clear();
    operands.extend(instruction.operands().zip(instruction.operand_spans()).map(
        |(object, span)| InstructionOperand {
            number: operand_number(object, input.get(span.clone()).unwrap_or_default()),
            name: operand_name(object).map(ToOwned::to_owned),
            offset: span.start,
        },
    ));
}

/// Return all operands as numbers, or `None` when any operand is non-numeric.
pub fn operand_numbers(operands: &[InstructionOperand]) -> Option<SmallVec<[f64; 6]>> {
    operands.iter().map(|operand| operand.number).collect()
}

/// Borrow a content operand as a PDF name.
pub fn operand_name<'a>(object: &'a HayroObject<'_>) -> Option<&'a [u8]> {
    match object {
        HayroObject::Name(name) => Some(name.as_ref()),
        _ => None,
    }
}

/// Borrow a content operand as a PDF string.
pub fn operand_string<'a>(object: &'a HayroObject<'_>) -> Option<&'a [u8]> {
    match object {
        HayroObject::String(value) => Some(value.as_bytes()),
        _ => None,
    }
}

/// Read a numeric content operand without silently rounding an exactly parsed PDF integer.
pub fn operand_number(object: &HayroObject<'_>, raw: &[u8]) -> Option<f64> {
    let HayroObject::Number(value) = object else {
        return None;
    };
    if !raw.contains(&b'.')
        && let Ok(text) = std::str::from_utf8(raw)
        && let Ok(integer) = text.parse::<i64>()
    {
        crate::source::exact_i64_to_f64(integer)
    } else {
        Some(value.as_f64())
    }
}

/// Apply sorted, non-overlapping byte-range replacements to a content stream.
///
/// Returns `None` when any range is invalid or overlaps an earlier replacement.
pub fn apply_replacements_owned(
    input: &[u8],
    mut replacements: Vec<(usize, usize, Vec<u8>)>,
) -> Option<Vec<u8>> {
    replacements.sort_unstable_by_key(|(start, _, _)| *start);
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in replacements {
        if start < cursor || start > end || end > input.len() {
            return None;
        }
        output.extend_from_slice(input.get(cursor..start)?);
        output.extend_from_slice(&replacement);
        cursor = end;
    }
    output.extend_from_slice(input.get(cursor..)?);
    Some(output)
}

/// Borrowed convenience variant; the owned form avoids cloning replacement payloads.
pub fn apply_replacements(
    input: &[u8],
    replacements: &[(usize, usize, Vec<u8>)],
) -> Option<Vec<u8>> {
    apply_replacements_owned(input, replacements.to_vec())
}

/// Canonicalize ordinary content-stream token spacing using Hayro's parsed spans.
///
/// Streams with inline images or malformed trailing input are left unchanged so
/// arbitrary inline payload bytes and recovery cases remain byte-for-byte stable.
pub fn normalize_content_stream(input: &[u8]) -> Vec<u8> {
    let mut iter = UntypedIter::new(input);
    let mut output = Vec::with_capacity(input.len());
    while let Some(instruction) = iter.next() {
        if &instruction.operator[..] == b"BI" {
            return input.to_vec();
        }
        if !output.is_empty() {
            output.push(b'\n');
        }
        let mut first = true;
        for span in instruction.operand_spans() {
            if !first {
                output.push(b' ');
            }
            first = false;
            let raw = input.get(span).unwrap_or_default();
            let end = raw
                .iter()
                .rposition(|byte| !matches!(byte, 0x00 | 0x09 | 0x0a | 0x0c | 0x0d | 0x20))
                .map_or(0, |index| index + 1);
            output.extend_from_slice(&raw[..end]);
        }
        if !first {
            output.push(b' ');
        }
        output.extend_from_slice(&instruction.operator[..]);
    }
    if iter.is_at_end() {
        output
    } else {
        input.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_operator_categories_are_authoritative_for_pruning_and_inventory() {
        for (operator, category) in [
            (b"CS".as_slice(), b"ColorSpace".as_slice()),
            (b"cs".as_slice(), b"ColorSpace".as_slice()),
            (b"gs".as_slice(), b"ExtGState".as_slice()),
            (b"Tf".as_slice(), b"Font".as_slice()),
            (b"SCN".as_slice(), b"Pattern".as_slice()),
            (b"scn".as_slice(), b"Pattern".as_slice()),
            (b"BDC".as_slice(), b"Properties".as_slice()),
            (b"DP".as_slice(), b"Properties".as_slice()),
            (b"sh".as_slice(), b"Shading".as_slice()),
            (b"Do".as_slice(), b"XObject".as_slice()),
        ] {
            assert_eq!(resource_type_for_operator(operator), Some(category));
        }
        assert_eq!(resource_type_for_operator(b"Tj"), None);
    }

    #[test]
    fn pdf_trivia_accepts_comments_and_whitespace_but_not_operators() {
        assert!(trivia_only(b""));
        assert!(trivia_only(b"\0 \t\r\n\x0c% first\r% second"));
        assert!(trivia_only(b"% final comment without a newline"));
        assert!(!trivia_only(b" % comment\n/Im0 Do"));
        assert!(!trivia_only(b"% comment\nQ"));
    }

    #[test]
    fn reusable_operand_buffer_matches_owned_parsing_and_reuses_capacity() -> Result<()> {
        let input = b"/F1 12 Tf 5.123 8 m 1 0 0 rg q Q 10 20 l (literal text) Tj";
        let mut reused = Vec::with_capacity(16);
        let initial_pointer = reused.as_ptr();
        let mut instructions = 0;
        let incomplete = visit_instructions(input, |instruction| {
            let owned = instruction_operands(input, instruction);
            instruction_operands_into(input, instruction, &mut reused);
            assert_eq!(reused.len(), owned.len());
            for (actual, expected) in reused.iter().zip(&owned) {
                assert_eq!(actual.number, expected.number);
                assert_eq!(actual.name, expected.name);
                assert_eq!(actual.offset, expected.offset);
            }
            assert_eq!(reused.as_ptr(), initial_pointer);
            instructions += 1;
            Ok(())
        })?;
        assert!(!incomplete);
        assert_eq!(instructions, 7);
        Ok(())
    }

    #[test]
    fn owned_and_borrowed_replacements_agree_and_reject_overlaps() {
        let input = b"abcdefghij";
        let replacements = vec![(6, 8, b"Q".to_vec()), (1, 4, b"XX".to_vec())];
        let expected = Some(b"aXXefQij".to_vec());
        assert_eq!(apply_replacements(input, &replacements), expected);
        assert_eq!(apply_replacements_owned(input, replacements), expected);

        let overlap = vec![(4, 6, b"z".to_vec()), (1, 5, b"x".to_vec())];
        assert_eq!(apply_replacements(input, &overlap), None);
        assert_eq!(apply_replacements_owned(input, overlap), None);
        assert_eq!(
            apply_replacements_owned(input, vec![(7, 4, Vec::new())]),
            None
        );
        assert_eq!(
            apply_replacements_owned(input, vec![(0, 11, Vec::new())]),
            None
        );
        assert_eq!(
            apply_replacements_owned(input, Vec::new()),
            Some(input.to_vec())
        );
    }

    #[test]
    fn content_normalization_is_idempotent_for_composite_operand_spacing() {
        let input = b"/Span <</ActualText<FEFF2008>>>  BDC\n( ) Tj\nEMC";
        let normalized = normalize_content_stream(input);
        assert_eq!(
            normalized,
            b"/Span <</ActualText<FEFF2008>>> BDC\n( ) Tj\nEMC"
        );
        assert_eq!(normalize_content_stream(&normalized), normalized);

        let all_pdf_whitespace = b"/Span <</ActualText<FEFF2008>>>\0\t\n\x0c\r   BDC";
        assert_eq!(
            normalize_content_stream(all_pdf_whitespace),
            b"/Span <</ActualText<FEFF2008>>> BDC"
        );

        let with_comment = b"1 % discarded lexical comment\n 2 m";
        let comment_normalized = normalize_content_stream(with_comment);
        assert_eq!(comment_normalized, b"1 2 m");
        assert_eq!(
            normalize_content_stream(&comment_normalized),
            comment_normalized
        );
    }
}
