//! Hayro-backed content-stream parsing shared by optimization passes.

use crate::{Error, Result};
use hayro_syntax::{
    content::UntypedIter,
    object::{MaybeRef, Object as HayroObject},
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ContentScalar {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Name(Vec<u8>),
    String(Vec<u8>),
    Operator(Vec<u8>),
}

impl ContentScalar {
    pub(crate) fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn as_real(&self) -> Option<f64> {
        match self {
            Self::Real(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn as_name(&self) -> Option<&[u8]> {
        match self {
            Self::Name(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_string(&self) -> Option<&[u8]> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_operator(&self) -> Option<&[u8]> {
        match self {
            Self::Operator(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ContentObjectRef {
    pub(crate) number: i32,
    pub(crate) generation: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ContentObject {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Name(Vec<u8>),
    String(Vec<u8>),
    Array(Vec<Self>),
    Dictionary(BTreeMap<Vec<u8>, Self>),
    Reference(ContentObjectRef),
    InlineImage(Vec<u8>),
    Operator(Vec<u8>),
}

impl ContentObject {
    pub(crate) fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn as_real(&self) -> Option<f64> {
        match self {
            Self::Real(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn as_name(&self) -> Option<Vec<u8>> {
        match self {
            Self::Name(value) => Some(value.clone()),
            _ => None,
        }
    }

    pub(crate) fn as_string(&self) -> Option<Vec<u8>> {
        match self {
            Self::String(value) => Some(value.clone()),
            _ => None,
        }
    }

    pub(crate) fn as_operator(&self) -> Option<Vec<u8>> {
        match self {
            Self::Operator(value) => Some(value.clone()),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<Vec<Self>> {
        match self {
            Self::Array(value) => Some(value.clone()),
            _ => None,
        }
    }

    pub(crate) const fn as_dictionary(&self) -> Option<&BTreeMap<Vec<u8>, Self>> {
        match self {
            Self::Dictionary(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_inline_image(&self) -> Option<Vec<u8>> {
        match self {
            Self::InlineImage(value) => Some(value.clone()),
            _ => None,
        }
    }

    pub(crate) const fn object_ref(&self) -> Option<ContentObjectRef> {
        match self {
            Self::Reference(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn try_get_key(&self, key: &[u8]) -> Result<Self> {
        let key = key.strip_prefix(b"/").unwrap_or(key);
        self.as_dictionary()
            .and_then(|dictionary| dictionary.get(key))
            .cloned()
            .ok_or_else(|| Error::Invalid("content dictionary key is missing".to_owned()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseControl {
    Continue,
    Stop,
}

pub(crate) trait ObjectHandleParserCallbacks {
    const HANDLES_CONTENT_SCALARS: bool = false;

    fn content_size(&mut self, _size: usize) -> Result<()> {
        Ok(())
    }

    fn handle_scalar(
        &mut self,
        _scalar: ContentScalar,
        _offset: usize,
        _length: usize,
    ) -> Result<ParseControl> {
        Err(Error::Invalid(
            "content scalar callback is not implemented".to_owned(),
        ))
    }

    fn handle_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
    ) -> Result<ParseControl> {
        self.handle_scalar(ContentScalar::Operator(operator.to_vec()), offset, length)
    }

    fn handle_object(
        &mut self,
        object: ContentObject,
        offset: usize,
        length: usize,
    ) -> Result<ParseControl>;

    fn handle_eof(&mut self) -> Result<()>;
}

fn number_from_raw(number: hayro_syntax::object::Number, raw: &[u8]) -> ContentObject {
    if !raw.contains(&b'.')
        && let Ok(text) = std::str::from_utf8(raw)
        && let Ok(value) = text.parse::<i64>()
    {
        ContentObject::Integer(value)
    } else {
        ContentObject::Real(number.as_f64())
    }
}

fn scalar_from_hayro(object: &HayroObject<'_>, raw: &[u8]) -> Option<ContentScalar> {
    Some(match object {
        HayroObject::Null(_) => ContentScalar::Null,
        HayroObject::Boolean(value) => ContentScalar::Boolean(*value),
        HayroObject::Number(number) => match number_from_raw(*number, raw) {
            ContentObject::Integer(value) => ContentScalar::Integer(value),
            ContentObject::Real(value) => ContentScalar::Real(value),
            _ => return None,
        },
        HayroObject::String(value) => ContentScalar::String(value.as_bytes().to_vec()),
        HayroObject::Name(value) => ContentScalar::Name(value.as_ref().to_vec()),
        HayroObject::Dict(_) | HayroObject::Array(_) | HayroObject::Stream(_) => return None,
    })
}

fn object_from_maybe_ref(value: MaybeRef<HayroObject<'_>>) -> Option<ContentObject> {
    match value {
        MaybeRef::Ref(reference) => Some(ContentObject::Reference(ContentObjectRef {
            number: reference.obj_number,
            generation: reference.gen_number,
        })),
        MaybeRef::NotRef(value) => object_from_hayro(&value, None),
    }
}

fn object_from_hayro(object: &HayroObject<'_>, raw: Option<&[u8]>) -> Option<ContentObject> {
    Some(match object {
        HayroObject::Null(_) => ContentObject::Null,
        HayroObject::Boolean(value) => ContentObject::Boolean(*value),
        HayroObject::Number(number) => raw.map_or_else(
            || ContentObject::Real(number.as_f64()),
            |raw| number_from_raw(*number, raw),
        ),
        HayroObject::String(value) => ContentObject::String(value.as_bytes().to_vec()),
        HayroObject::Name(value) => ContentObject::Name(value.as_ref().to_vec()),
        HayroObject::Array(value) => {
            ContentObject::Array(value.raw_iter().filter_map(object_from_maybe_ref).collect())
        }
        HayroObject::Dict(value) => {
            let mut dictionary = BTreeMap::new();
            for (key, item) in value.entries() {
                if let Some(item) = object_from_maybe_ref(item) {
                    dictionary.insert(key.as_ref().to_vec(), item);
                }
            }
            ContentObject::Dictionary(dictionary)
        }
        HayroObject::Stream(value) => ContentObject::InlineImage(value.raw_data().into_owned()),
    })
}

const fn stopped(control: ParseControl) -> bool {
    matches!(control, ParseControl::Stop)
}

fn parse_internal<C: ObjectHandleParserCallbacks>(input: &[u8], callbacks: &mut C) -> Result<bool> {
    callbacks.content_size(input.len())?;
    let mut iter = UntypedIter::new(input);
    while let Some(instruction) = iter.next() {
        let operator = &instruction.operator[..];
        if operator == b"BI" {
            let span = instruction.span();
            if let Some(HayroObject::Stream(stream)) = instruction.operands().next() {
                let image = ContentObject::InlineImage(stream.raw_data().into_owned());
                if stopped(callbacks.handle_object(image, span.start, span.len())?) {
                    return Ok(false);
                }
            }
            continue;
        }

        for (object, span) in instruction.operands().zip(instruction.operand_spans()) {
            let raw = input.get(span.clone()).unwrap_or_default();
            let control = if C::HANDLES_CONTENT_SCALARS {
                if let Some(scalar) = scalar_from_hayro(object, raw) {
                    callbacks.handle_scalar(scalar, span.start, span.len())?
                } else if let Some(object) = object_from_hayro(object, Some(raw)) {
                    callbacks.handle_object(object, span.start, span.len())?
                } else {
                    ParseControl::Continue
                }
            } else if let Some(object) = object_from_hayro(object, Some(raw)) {
                callbacks.handle_object(object, span.start, span.len())?
            } else {
                ParseControl::Continue
            };
            if stopped(control) {
                return Ok(false);
            }
        }

        let span = instruction.operator_span();
        let control = if C::HANDLES_CONTENT_SCALARS {
            callbacks.handle_operator(operator, span.start, span.len())?
        } else {
            callbacks.handle_object(
                ContentObject::Operator(operator.to_vec()),
                span.start,
                span.len(),
            )?
        };
        if stopped(control) {
            return Ok(false);
        }
    }
    let incomplete = !iter.is_at_end();
    callbacks.handle_eof()?;
    Ok(incomplete)
}

pub(crate) fn parse_detached_content_stream<C: ObjectHandleParserCallbacks>(
    input: &[u8],
    _source_description: &str,
    callbacks: &mut C,
) -> Result<()> {
    parse_internal(input, callbacks).map(|_| ())
}

pub(crate) fn parse_detached_content_stream_recovering<C: ObjectHandleParserCallbacks>(
    input: &[u8],
    _source_description: &str,
    callbacks: &mut C,
) -> Result<bool> {
    parse_internal(input, callbacks)
}
