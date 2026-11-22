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

/// An operator/name token ends before PDF whitespace or a delimiter.
pub const fn token_boundary(byte: u8) -> bool {
    matches!(
        byte,
        0 | b'\t'
            | b'\n'
            | 0x0c
            | b'\r'
            | b' '
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'/'
            | b'%'
    )
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
    instruction: &hayro_syntax::content::Instruction<'_, '_>,
) -> Vec<InstructionOperand> {
    let mut operands = Vec::new();
    instruction_operands_into(instruction, &mut operands);
    operands
}

/// Parse the same operand facets into an existing buffer, retaining capacity
/// across instructions in long-running page scanners.
pub fn instruction_operands_into(
    instruction: &hayro_syntax::content::Instruction<'_, '_>,
    operands: &mut Vec<InstructionOperand>,
) {
    operands.clear();
    operands.extend(instruction.operands().zip(instruction.operand_spans()).map(
        |(object, span)| InstructionOperand {
            number: operand_number(object),
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
pub fn operand_number(object: &HayroObject<'_>) -> Option<f64> {
    let HayroObject::Number(value) = object else {
        return None;
    };
    // The parser already determined the integer/real kind. Never rescan the
    // original token, or round an exact integer through the parser's f64 view.
    value
        .as_integer()
        .map_or_else(|| Some(value.as_f64()), crate::source::exact_i64_to_f64)
}

/// Read exactly N numeric operands without a heap allocation.
pub fn instruction_number_array<const N: usize>(
    instruction: &hayro_syntax::content::Instruction<'_, '_>,
) -> Option<[f64; N]> {
    if instruction.operand_spans().len() != N {
        return None;
    }
    let mut values = [0.0; N];
    let mut operands = instruction.operands();
    for value in &mut values {
        *value = operand_number(operands.next()?)?;
    }
    operands.next().is_none().then_some(values)
}

/// Assemble a checked, already ordered replacement stream. Arena-backed
/// callers can project payloads directly without allocating individual
/// replacement buffers. Validate and measure before allocating output.
pub fn apply_ordered_replacements<'a>(
    input: &[u8],
    replacements: impl Iterator<Item = (usize, usize, Option<&'a [u8]>)> + Clone,
) -> Option<Vec<u8>> {
    let mut cursor = 0;
    let mut length = 0_usize;
    for (start, end, bytes) in replacements.clone() {
        if start < cursor || start > end || end > input.len() {
            return None;
        }
        length = length
            .checked_add(start - cursor)?
            .checked_add(bytes?.len())?;
        cursor = end;
    }
    length = length.checked_add(input.len() - cursor)?;
    let mut output = Vec::new();
    output.try_reserve_exact(length).ok()?;
    cursor = 0;
    for (start, end, bytes) in replacements {
        output.extend_from_slice(input.get(cursor..start)?);
        output.extend_from_slice(bytes?);
        cursor = end;
    }
    output.extend_from_slice(input.get(cursor..)?);
    Some(output)
}

/// Apply non-overlapping byte-range replacements without copying payloads.
/// Already sorted spans require no auxiliary allocation. Insertions at an
/// identical offset retain input order and precede a nonempty replacement
/// beginning there; overlapping nonempty ranges are rejected.
pub fn apply_replacements<B: AsRef<[u8]>>(
    input: &[u8],
    replacements: &[(usize, usize, B)],
) -> Option<Vec<u8>> {
    fn project<B: AsRef<[u8]>>(
        (start, end, bytes): &(usize, usize, B),
    ) -> (usize, usize, Option<&[u8]>) {
        (*start, *end, Some(bytes.as_ref()))
    }
    if replacements.is_sorted_by_key(|(start, end, _)| (*start, *end)) {
        return apply_ordered_replacements(input, replacements.iter().map(project));
    }
    let mut ordered = replacements.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|(start, end, _)| (*start, *end));
    apply_ordered_replacements(input, ordered.into_iter().map(project))
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
    fn parsed_numeric_operands_distinguish_exact_integers_and_reals() -> Result<()> {
        let input = b"9007199254740993 0 m 9007199254740994 -0.0 l 1 0 0 1 1.25 -2.5 cm";
        let mut count = 0;
        let incomplete = visit_instructions(input, |instruction| {
            match count {
                0 => {
                    let values = instruction_operands(instruction);
                    assert_eq!(values[0].number, None);
                    assert_eq!(values[1].number, Some(0.0));
                    assert_eq!(instruction_number_array::<2>(instruction), None);
                }
                1 => {
                    let values = instruction_number_array::<2>(instruction);
                    assert_eq!(
                        values.map(|values| values[0]),
                        Some(9_007_199_254_740_994.0)
                    );
                    assert_eq!(
                        values.map(|values| values[1].to_bits()),
                        Some((-0.0_f64).to_bits())
                    );
                    assert_eq!(instruction_number_array::<6>(instruction), None);
                }
                2 => {
                    assert_eq!(
                        instruction_number_array::<6>(instruction),
                        Some([1.0, 0.0, 0.0, 1.0, 1.25, -2.5])
                    );
                    assert_eq!(instruction_number_array::<2>(instruction), None);
                }
                _ => return Err(crate::Error::Invalid("unexpected instruction".to_owned())),
            }
            count += 1;
            Ok(())
        })?;
        assert!(!incomplete);
        assert_eq!(count, 3);
        Ok(())
    }

    #[test]
    fn reusable_operand_buffer_matches_owned_parsing_and_reuses_capacity() -> Result<()> {
        let input = b"/F1 12 Tf 5.123 8 m 1 0 0 rg q Q 10 20 l (literal text) Tj";
        let mut reused = Vec::with_capacity(16);
        let initial_pointer = reused.as_ptr();
        let mut instructions = 0;
        let incomplete = visit_instructions(input, |instruction| {
            let owned = instruction_operands(instruction);
            instruction_operands_into(instruction, &mut reused);
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
    fn replacements_borrow_payloads_and_reject_overlaps() {
        let input = b"abcdefghij";
        let replacements = [(6, 8, b"Q".as_slice()), (1, 4, b"XX".as_slice())];
        let expected = Some(b"aXXefQij".to_vec());
        assert_eq!(apply_replacements(input, &replacements), expected);
        let owned = replacements.map(|(start, end, bytes)| (start, end, bytes.to_vec()));
        assert_eq!(apply_replacements(input, &owned), expected);

        assert_eq!(
            apply_replacements(input, &[(4, 6, b"z"), (1, 5, b"x")]),
            None
        );
        assert_eq!(apply_replacements(input, &[(7, 4, b"")]), None);
        assert_eq!(apply_replacements(input, &[(0, 11, b"")]), None);
        assert_eq!(
            apply_replacements::<&[u8]>(input, &[]),
            Some(input.to_vec())
        );
    }

    #[test]
    fn replacements_order_insertions_and_preserve_expansion_and_tail() {
        let input = b"abcdef";
        let edits = [
            (2, 4, b"replacement".as_slice()),
            (2, 2, b"first".as_slice()),
            (6, 6, b"tail".as_slice()),
            (2, 2, b"second".as_slice()),
            (0, 0, b"start".as_slice()),
        ];
        assert_eq!(
            apply_replacements(input, &edits).as_deref(),
            Some(b"startabfirstsecondreplacementeftail".as_slice())
        );
        assert_eq!(
            apply_replacements(
                input,
                &[
                    (1, 4, b"".as_slice()),
                    (1, 1, b"A".as_slice()),
                    (4, 4, b"B".as_slice())
                ]
            )
            .as_deref(),
            Some(b"aABef".as_slice())
        );
        assert_eq!(apply_replacements(input, &[(1, 4, b""), (2, 2, b"")]), None);
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
