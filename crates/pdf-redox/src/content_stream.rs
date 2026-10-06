//! Hayro-backed content-stream helpers shared by optimization passes.

use crate::Result;
use hayro_syntax::{content::UntypedIter, object::Object as HayroObject};

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
            output.extend_from_slice(input.get(span).unwrap_or_default());
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
