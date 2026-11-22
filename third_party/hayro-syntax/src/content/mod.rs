//! PDF content operators.

#[allow(missing_docs)]
pub mod ops;

use crate::content::ops::TypedInstruction;
use crate::object;
use crate::object::dict::InlineImageDict;
use crate::object::name::{Name, skip_name_like};
use crate::object::{Array, Number, Object, Stream};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::trivia::{is_regular_character, is_white_space_character};
use crate::util::find_needle;
use alloc::vec::Vec;
use core::fmt::{Debug, Display, Formatter};
use core::ops::{Deref, Range};
use smallvec::SmallVec;

// `scn` with 32 DeviceN components and a pattern name.
const OPERANDS_THRESHOLD: usize = 33;
const INLINE_COLOR_COMPONENTS: usize = 4;

impl Debug for Operator<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0.as_str())
    }
}

impl Display for Operator<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0.as_str())
    }
}

/// A content stream operator.
#[derive(Clone, PartialEq)]
pub struct Operator<'a>(Name<'a>);

impl Deref for Operator<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl Skippable for Operator<'_> {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        skip_name_like(r, false).map(|_| ())
    }
}

impl<'a> Readable<'a> for Operator<'a> {
    fn read(r: &mut Reader<'a>, _: &ReaderContext<'a>) -> Option<Self> {
        let start = r.offset();
        let mut escaped = false;
        while let Some(b) = r.peek_byte().filter(|b| is_regular_character(*b)) {
            escaped |= b == b'#';
            r.forward();
        }

        let data = r.range(start..r.offset())?;
        if data.is_empty() {
            return None;
        }

        let name = if escaped {
            Name::new_escaped(data)?
        } else {
            Name::new_unescaped(data)
        };

        Some(Self(name))
    }
}

/// An untyped iterator over the instructions of a content stream.
#[derive(Debug, Clone)]
pub struct UntypedIter<'a> {
    reader: Reader<'a>,
    stack: Stack<'a>,
    operator: Option<Operator<'a>>,
}

impl<'a> UntypedIter<'a> {
    /// Create a new untyped iterator.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            reader: Reader::new(data),
            stack: Stack::new(),
            operator: None,
        }
    }

    /// Create a new empty untyped iterator.
    pub fn empty() -> Self {
        Self {
            reader: Reader::new(&[]),
            stack: Stack::new(),
            operator: None,
        }
    }

    /// Whether the reader has reached the end of the content stream.
    pub fn is_at_end(&self) -> bool {
        self.reader.at_end()
    }

    /// Current byte offset in the original content stream.
    pub fn offset(&self) -> usize {
        self.reader.offset()
    }

    /// Return the next instruction.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Instruction<'_, 'a>> {
        self.stack.clear();

        let (operator, operator_span) = read_instruction(&mut self.reader, &mut self.stack)?;
        let start = self
            .stack
            .data
            .first()
            .map_or(operator_span.start, |entry| entry.span.start);
        let span = start..self.reader.offset();
        self.operator = Some(operator);
        Some(Instruction {
            operands: &self.stack,
            operator: self.operator.as_ref().unwrap(),
            operator_span,
            span,
        })
    }
}

fn read_instruction<'a>(
    r: &mut Reader<'a>,
    stack: &mut Stack<'a>,
) -> Option<(Operator<'a>, Range<usize>)> {
    // Keeps the read position in a register; must be written back before calling out.
    let mut cur = r.clone();

    loop {
        cur.skip_white_spaces_and_comments();

        let Some(first) = cur.peek_byte() else {
            *r = cur;
            return None;
        };

        if matches!(first, b'.' | b'+' | b'-' | b'0'..=b'9') {
            let start = cur.offset();
            match Number::read_simple(&mut cur) {
                Some(number) => {
                    if stack
                        .push(Object::Number(number), start..cur.offset())
                        .is_none()
                    {
                        *r = cur;
                        return None;
                    }
                    continue;
                }
                None => cur.jump(start),
            }
        }

        match first {
            b'.' | b'+' | b'-' | b'0'..=b'9' | b'/' | b'[' | b'<' | b'(' => {
                *r = cur;
                read_operand(r, stack)?;
                cur = r.clone();
            }
            _ => {
                *r = cur;
                let start = r.offset();
                let Some(operator) = r.read_without_context::<Operator<'a>>() else {
                    r.jump_to_end();
                    return None;
                };

                let operator_span = start..r.offset();
                if operator.as_ref() == b"BI" {
                    read_inline_image(r, stack, start)?;
                }

                return Some((operator, operator_span));
            }
        }
    }
}

fn read_operand<'a>(r: &mut Reader<'a>, stack: &mut Stack<'a>) -> Option<()> {
    // A number glued to regular characters acts as an unknown operator; skip it (issue 994).
    let start = r.offset();
    if let Some(object) = r.read_without_context::<Object<'a>>() {
        stack.push(object, start..r.offset())
    } else if r.read_without_context::<Operator<'a>>().is_some() {
        stack.clear();
        Some(())
    } else {
        None
    }
}

fn read_inline_image<'a>(
    r: &mut Reader<'a>,
    stack: &mut Stack<'a>,
    operator_start: usize,
) -> Option<()> {
    let inline_dict = r.read_without_context::<InlineImageDict<'_>>()?;
    let dict = inline_dict.get_dict().clone();

    r.read_white_space()?;

    let stream_data = r.tail()?;
    let start_offset = r.offset();

    'outer: while let Some(pos) = find_needle(r.tail()?, b"EI") {
        r.read_bytes(pos)?;

        if r.peek_bytes(2) == Some(b"EI") {
            if r.peek_bytes(3)
                .is_some_and(|b| !is_white_space_character(b[2]))
            {
                r.read_bytes(3)?;

                continue;
            }

            let end_offset = r.offset() - start_offset;
            let image_data = &stream_data[..end_offset];

            let stream = Stream::new(image_data, dict.clone());

            // `EI` can appear inside the image data, so check that content follows.
            let tail = &r.tail()?[2..];
            let mut find_reader = Reader::new(tail);
            let mut binary = BinaryTracker::default();
            let mut is_content = None;
            let mut next_bi = find_needle(tail, b"BI");

            while !find_reader.at_end() {
                let offset = find_reader.offset();

                if next_bi.is_some_and(|bi| bi < offset) {
                    next_bi = find_needle(&tail[offset..], b"BI").map(|bi| offset + bi);
                }

                let next_ei = find_needle(&tail[offset..], b"EI").map(|ei| offset + ei);

                let (next_pos, is_ei) = match (next_ei, next_bi) {
                    (Some(ei), Some(bi)) if ei <= bi => (ei, true),
                    (Some(_), Some(bi)) => (bi, false),
                    (Some(ei), None) => (ei, true),
                    (None, Some(bi)) => (bi, false),
                    (None, None) => break,
                };

                find_reader.read_bytes(next_pos - offset)?;

                if is_ei {
                    if binary.found_before(tail, next_pos) {
                        r.read_bytes(2)?;
                        continue 'outer;
                    }

                    if !*is_content.get_or_insert_with(|| starts_with_content(tail)) {
                        r.read_bytes(2)?;
                        continue 'outer;
                    }
                } else {
                    let mut cloned = find_reader.clone();
                    cloned.read_bytes(2)?;
                    if cloned
                        .read_without_context::<InlineImageDict<'_>>()
                        .is_some()
                    {
                        break;
                    }
                }

                find_reader.read_byte()?;
            }

            r.read_bytes(2)?;
            stack.push(Object::Stream(stream), operator_start..r.offset())?;
            r.skip_white_spaces();

            break;
        }
    }

    Some(())
}

fn starts_with_content(data: &[u8]) -> bool {
    let mut iter = TypedIter::new(data);
    let mut counter = 0;

    while let Some(op) = iter.next() {
        // 20 valid operators in a row are very unlikely in image data.
        if counter >= 20 {
            return true;
        }

        if matches!(
            op,
            TypedInstruction::NextLineAndShowText(_)
                | TypedInstruction::ShowText(_)
                | TypedInstruction::ShowTexts(_)
                | TypedInstruction::ShowTextWithParameters(_)
        ) {
            return true;
        }

        counter += 1;
    }

    false
}

// Non-ASCII bytes outside strings mean we're still in image data.
#[derive(Default)]
struct BinaryTracker {
    pos: usize,
    string_depth: usize,
    escaped: bool,
    found: bool,
}

impl BinaryTracker {
    fn found_before(&mut self, data: &[u8], end: usize) -> bool {
        if self.found || end <= self.pos {
            return self.found;
        }

        for (i, &b) in data[self.pos..end].iter().enumerate() {
            if self.string_depth > 0 {
                if self.escaped {
                    self.escaped = false;
                } else if b == b'\\' {
                    self.escaped = true;
                } else if b == b'(' {
                    self.string_depth += 1;
                } else if b == b')' {
                    self.string_depth -= 1;
                }
            } else if !b.is_ascii() {
                self.found = true;
                self.pos += i;
                return true;
            } else if b == b'(' {
                self.string_depth = 1;
            }
        }

        self.pos = end;
        false
    }
}

/// A typed iterator over the instructions of a content stream.
#[derive(Clone)]
pub struct TypedIter<'a> {
    untyped: UntypedIter<'a>,
}

impl<'a> TypedIter<'a> {
    /// Create a new typed iterator.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            untyped: UntypedIter::new(data),
        }
    }

    pub(crate) fn from_untyped(untyped: UntypedIter<'a>) -> Self {
        Self { untyped }
    }

    /// Return the next typed instruction.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<TypedInstruction<'_, 'a>> {
        let op = self.untyped.next()?;
        // TODO: Explore whether dispatching can be made more efficient.
        match TypedInstruction::dispatch(&op) {
            Some(op) => Some(op),
            // Abort on invalid path operators to avoid drawing garbage, and skip other operators.
            None => {
                if [
                    &b"m"[..],
                    &b"l"[..],
                    &b"c"[..],
                    &b"v"[..],
                    &b"y"[..],
                    &b"h"[..],
                    &b"re"[..],
                ]
                .contains(&op.operator.0.deref())
                {
                    None
                } else {
                    Some(TypedInstruction::Fallback(op.operator))
                }
            }
        }
    }
}

/// An operator and its operands.
#[derive(Debug)]
pub struct Instruction<'b, 'a> {
    /// The stack containing the operands.
    pub operands: &'b Stack<'a>,
    /// The operator.
    pub operator: &'b Operator<'a>,
    operator_span: Range<usize>,
    span: Range<usize>,
}

impl<'b, 'a> Instruction<'b, 'a> {
    /// An iterator over the operands of the instruction.
    pub fn operands(&self) -> OperandIterator<'b, 'a> {
        OperandIterator::new(self.operands)
    }

    /// Byte span of the operator token in the original content stream.
    pub fn operator_span(&self) -> Range<usize> {
        self.operator_span.clone()
    }

    /// Byte span covering the operands and operator in the original content stream.
    pub fn span(&self) -> Range<usize> {
        self.span.clone()
    }

    /// Byte spans for each operand, in the same order as the operands.
    pub fn operand_spans(&self) -> impl ExactSizeIterator<Item = Range<usize>> + '_ {
        self.operands.data.iter().map(|entry| entry.span.clone())
    }
}

#[derive(Clone)]
struct SpannedOperand<'a> {
    object: Object<'a>,
    span: Range<usize>,
}

/// A stack holding the arguments of an operator.
#[derive(Clone)]
pub struct Stack<'a> {
    // An instruction reuses this allocation. Keep each span beside its
    // object so insertion has one capacity check and one length update.
    data: Vec<SpannedOperand<'a>>,
}

impl<'a> Default for Stack<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Stack<'a> {
    /// Create a new, empty stack.
    pub fn new() -> Self {
        Self { data: Vec::new() }
    }

    #[inline]
    fn push(&mut self, object: Object<'a>, span: Range<usize>) -> Option<()> {
        if self.data.len() >= OPERANDS_THRESHOLD {
            return None;
        }

        self.data.push(SpannedOperand { object, span });
        Some(())
    }

    fn clear(&mut self) {
        self.data.clear();
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn iter(&self) -> impl ExactSizeIterator<Item = &Object<'a>> {
        self.data.iter().map(|entry| &entry.object)
    }

    fn get<'b, T>(&'b self, index: usize) -> Option<T>
    where
        T: Operand<'b, 'a>,
    {
        self.data
            .get(index)
            .and_then(|entry| T::from_object(&entry.object))
    }

    fn get_all<'b, T>(&'b self) -> Option<SmallVec<[T; INLINE_COLOR_COMPONENTS]>>
    where
        T: Operand<'b, 'a>,
    {
        let mut operands = SmallVec::new();

        for op in self.iter() {
            let converted = T::from_object(op)?;
            operands.push(converted);
        }

        Some(operands)
    }
}

impl PartialEq for Stack<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl Debug for Stack<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

trait Operand<'b, 'a>: Sized {
    fn from_object(object: &'b Object<'a>) -> Option<Self>;
}

impl<'b, 'a> Operand<'b, 'a> for Number {
    fn from_object(object: &'b Object<'a>) -> Option<Self> {
        match object {
            Object::Number(n) => Some(*n),
            _ => None,
        }
    }
}

impl<'b, 'a> Operand<'b, 'a> for &'b object::String<'a> {
    fn from_object(object: &'b Object<'a>) -> Option<Self> {
        match object {
            Object::String(s) => Some(s),
            _ => None,
        }
    }
}

impl<'b, 'a> Operand<'b, 'a> for &'b Name<'a> {
    fn from_object(object: &'b Object<'a>) -> Option<Self> {
        match object {
            Object::Name(n) => Some(n),
            _ => None,
        }
    }
}

impl<'b, 'a> Operand<'b, 'a> for &'b Array<'a> {
    fn from_object(object: &'b Object<'a>) -> Option<Self> {
        match object {
            Object::Array(a) => Some(a),
            _ => None,
        }
    }
}

impl<'b, 'a> Operand<'b, 'a> for &'b Stream<'a> {
    fn from_object(object: &'b Object<'a>) -> Option<Self> {
        match object {
            Object::Stream(s) => Some(s),
            _ => None,
        }
    }
}

impl<'b, 'a> Operand<'b, 'a> for &'b Object<'a> {
    fn from_object(object: &'b Object<'a>) -> Option<Self> {
        Some(object)
    }
}

/// An iterator over the operands of an operator.
pub struct OperandIterator<'b, 'a> {
    entries: core::slice::Iter<'b, SpannedOperand<'a>>,
}

impl<'b, 'a> OperandIterator<'b, 'a> {
    fn new(stack: &'b Stack<'a>) -> Self {
        Self {
            entries: stack.data.iter(),
        }
    }
}

impl<'b, 'a> Iterator for OperandIterator<'b, 'a> {
    type Item = &'b Object<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(|entry| &entry.object)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl ExactSizeIterator for OperandIterator<'_, '_> {}

pub(crate) trait OperatorTrait<'b, 'a>: Sized {
    fn from_stack(stack: &'b Stack<'a>) -> Option<Self>;
}

mod macros {
    macro_rules! display_op {
        ($t:ident $(<$($l:lifetime),+>)?;) => {
            impl$(<$($l),+>)? core::fmt::Display for $t$(<$($l),+>)? {
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    f.write_str(stringify!($t))
                }
            }
        };
        ($t:ident $(<$($l:lifetime),+>)?; $first:tt $(, $field:tt)*) => {
            impl$(<$($l),+>)? core::fmt::Display for $t$(<$($l),+>)? {
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    f.write_str(concat!(stringify!($t), "("))?;
                    core::fmt::Display::fmt(&self.$first, f)?;
                    $(
                        f.write_str(" ")?;
                        core::fmt::Display::fmt(&self.$field, f)?;
                    )*
                    f.write_str(")")
                }
            }
        };
    }

    macro_rules! display_variadic_op {
        ($t:ident $(<$($l:lifetime),+>)?) => {
            impl$(<$($l),+>)? core::fmt::Display for $t$(<$($l),+>)? {
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    f.write_str(stringify!($t))?;
                    if let Some((first, rest)) = self.0.split_first() {
                        f.write_str("(")?;
                        core::fmt::Display::fmt(first, f)?;
                        for value in rest {
                            f.write_str(" ")?;
                            core::fmt::Display::fmt(value, f)?;
                        }
                        f.write_str(")")?;
                    }
                    Ok(())
                }
            }
        };
    }

    macro_rules! op_impl {
        ($t:ident $(<$($l:lifetime),+>)?, $e:expr, $n:expr, |$stack:ident : $stack_ty:ty| $body:block) => {
            impl<'b, 'a> OperatorTrait<'b, 'a> for $t$(<$($l),+>)? {
                #[inline(always)]
                fn from_stack($stack: $stack_ty) -> Option<Self> {
                    $body
                }
            }

            impl<'b, 'a> From<$t$(<$($l),+>)?> for TypedInstruction<'b, 'a> {
                fn from(value: $t$(<$($l),+>)?) -> Self {
                    TypedInstruction::$t(value)
                }
            }

            impl<'b, 'a> TryFrom<TypedInstruction<'b, 'a>> for $t$(<$($l),+>)? {
                type Error = ();

                fn try_from(value: TypedInstruction<'b, 'a>) -> core::result::Result<Self, Self::Error> {
                    match value {
                        TypedInstruction::$t(e) => Ok(e),
                        _ => Err(())
                    }
                }
            }
        };
    }

    // With garbage operands, prefer those closest to the operator.

    macro_rules! op0 {
        ($t:ident $(<$($l:lifetime),+>)?, $e:expr) => {
            crate::content::macros::display_op!($t$(<$($l),+>)?;);
            crate::content::macros::op_impl!($t$(<$($l),+>)?, $e, 0, |_stack: &'b Stack<'a>| {
                Some(Self)
            });
        }
    }

    macro_rules! op1 {
        ($t:ident $(<$($l:lifetime),+>)?, $e:expr) => {
            crate::content::macros::display_op!($t$(<$($l),+>)?; 0);
            crate::content::macros::op_impl!($t$(<$($l),+>)?, $e, 1, |stack: &'b Stack<'a>| {
                let shift = stack.len().saturating_sub(1);
                Some(Self(stack.get(0 + shift)?))
            });
        }
    }

    macro_rules! op_all {
        ($t:ident $(<$($l:lifetime),+>)?, $e:expr) => {
            crate::content::macros::display_variadic_op!($t$(<$($l),+>)?);
            crate::content::macros::op_impl!($t$(<$($l),+>)?, $e, u8::MAX as usize, |stack: &'b Stack<'a>| {
                Some(Self(stack.get_all()?))
            });
        }
    }

    macro_rules! op2 {
        ($t:ident $(<$($l:lifetime),+>)?, $e:expr) => {
            crate::content::macros::display_op!($t$(<$($l),+>)?; 0, 1);
            crate::content::macros::op_impl!($t$(<$($l),+>)?, $e, 2, |stack: &'b Stack<'a>| {
                let shift = stack.len().saturating_sub(2);
                Some(Self(stack.get(0 + shift)?, stack.get(1 + shift)?))
            });
        }
    }

    macro_rules! op3 {
        ($t:ident $(<$($l:lifetime),+>)?, $e:expr) => {
            crate::content::macros::display_op!($t$(<$($l),+>)?; 0, 1, 2);
            crate::content::macros::op_impl!($t$(<$($l),+>)?, $e, 3, |stack: &'b Stack<'a>| {
                let shift = stack.len().saturating_sub(3);
                Some(Self(stack.get(0 + shift)?, stack.get(1 + shift)?,
                stack.get(2 + shift)?))
            });
        }
    }

    macro_rules! op4 {
        ($t:ident $(<$($l:lifetime),+>)?, $e:expr) => {
            crate::content::macros::display_op!($t$(<$($l),+>)?; 0, 1, 2, 3);
            crate::content::macros::op_impl!($t$(<$($l),+>)?, $e, 4, |stack: &'b Stack<'a>| {
               let shift = stack.len().saturating_sub(4);
            Some(Self(stack.get(0 + shift)?, stack.get(1 + shift)?,
            stack.get(2 + shift)?, stack.get(3 + shift)?))
            });
        }
    }

    macro_rules! op6 {
        ($t:ident $(<$($l:lifetime),+>)?, $e:expr) => {
            crate::content::macros::display_op!($t$(<$($l),+>)?; 0, 1, 2, 3, 4, 5);
            crate::content::macros::op_impl!($t$(<$($l),+>)?, $e, 6, |stack: &'b Stack<'a>| {
                let shift = stack.len().saturating_sub(6);
            Some(Self(stack.get(0 + shift)?, stack.get(1 + shift)?,
            stack.get(2 + shift)?, stack.get(3 + shift)?,
            stack.get(4 + shift)?, stack.get(5 + shift)?))
            });
        }
    }

    pub(crate) use display_op;
    pub(crate) use display_variadic_op;
    pub(crate) use op_all;
    pub(crate) use op_impl;
    pub(crate) use op0;
    pub(crate) use op1;
    pub(crate) use op2;
    pub(crate) use op3;
    pub(crate) use op4;
    pub(crate) use op6;
}

#[cfg(test)]
mod span_tests {
    use super::UntypedIter;
    use alloc::{vec, vec::Vec};

    #[test]
    fn exposes_inline_image_spans() {
        let input = b"q BI /W 1 /H 1 /BPC 8 /CS /G ID x EI Q";
        let mut iter = UntypedIter::new(input);
        assert_eq!(iter.next().unwrap().operator_span(), 0..1);
        let image = iter.next().unwrap();
        assert_eq!(&input[image.operator_span()], b"BI");
        let operand = image.operand_spans().next().unwrap();
        assert_eq!(&input[operand], b"BI /W 1 /H 1 /BPC 8 /CS /G ID x EI");
        let last = iter.next().unwrap();
        assert_eq!(&input[last.operator_span()], b"Q");
    }

    #[test]
    fn spanned_stack_keeps_mixed_operands_aligned_at_capacity() {
        let mut input = b"0 ".repeat(32);
        input.extend_from_slice(b"/Spot scn (text) Tj q");
        let mut iter = UntypedIter::new(&input);
        let colors = iter.next().unwrap();
        let mut operands = colors.operands();
        assert_eq!(operands.len(), 33);
        for (index, span) in colors.operand_spans().enumerate() {
            let operand = operands.next().unwrap();
            if index < 32 {
                assert_eq!(operand, &super::Object::Number(super::Number::ZERO));
                assert_eq!(&input[span], b"0");
            } else {
                assert!(matches!(operand, super::Object::Name(_)));
                assert_eq!(&input[span], b"/Spot");
            }
        }
        assert_eq!(operands.len(), 0);
        let text = iter.next().unwrap();
        assert_eq!(text.operands().len(), 1);
        assert_eq!(&input[text.operand_spans().next().unwrap()], b"(text)");
        let save = iter.next().unwrap();
        assert_eq!(save.operands().len(), 0);
        assert_eq!(&save.operator[..], b"q");
        assert!(iter.next().is_none());
    }

    #[test]
    fn exposes_instruction_and_operand_spans() {
        let input = b"  12 -3.5 /Name  cm\nq";
        let mut iter = UntypedIter::new(input);
        let first = iter.next().unwrap();
        assert_eq!(&first.operator[..], b"cm");
        assert_eq!(first.span(), 2..19);
        assert_eq!(first.operator_span(), 17..19);
        assert_eq!(
            first.operand_spans().collect::<Vec<_>>(),
            vec![2..4, 5..9, 10..15]
        );
        let second = iter.next().unwrap();
        assert_eq!(&second.operator[..], b"q");
        assert_eq!(second.span(), 20..21);
        assert_eq!(second.operator_span(), 20..21);
        assert!(second.operand_spans().next().is_none());
        assert!(iter.next().is_none());
        assert!(iter.is_at_end());
        assert_eq!(iter.offset(), input.len());
    }
}
