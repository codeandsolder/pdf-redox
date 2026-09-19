//! qpdf correspondence: `QPDFObjectHandle::ParserCallbacks` and `QPDFParser::warn` content boundary.
//! Content-stream object callbacks (ISO 32000-1 §7.8.2).
//!
//! A PDF content stream is a sequence of operands followed by an operator,
//! interleaved with inline images and comments. This module routes the shared
//! tokenizer and [`crate::parser`] through qpdf-shaped
//! [`ObjectHandleParserCallbacks`]. It contains orchestration and event accumulation only;
//! lexical boundaries remain owned by the tokenizer.
//!
//! [`parse_content_operations`] provides the common operand/operator adapter
//! for consumers that do not need inline-image payload events.
//!
//! When parsing a document-owned handle, recoverable tokenizer/parser
//! diagnostics are delivered through the owning `DocumentResolver`. This
//! mirrors qpdf's `QPDFObjectHandle::warn` path. Detached parses have no qpdf
//! warning sink, so the first recoverable diagnostic is returned as the
//! corresponding `QPDFExc` error.

use crate::parser::{
    parse_integer_token, parse_live_content_stream_object_from_tokens, parse_real_token_value,
    ContentHandleResolver, LiveTokenSource, SliceLiveInput,
};
use crate::tokenizer::{Token, TokenType, Tokenizer, TokenizerStateError};
use crate::{
    object_handle::{DocumentResolver, ObjectHandle},
    Error, QpdfErrorCode, QpdfExc, Result,
};
use std::{cell::RefCell, rc::Rc};

/// Lightweight top-level scalar from a PDF content stream.
///
/// This is deliberately value-only: content-stream operands have no persistent
/// PDF object identity, so hot analysis callbacks can consume ordinary
/// numbers, names, strings, and operators without allocating a full handle.
/// Containers and malformed/recovery cases still use the canonical parser.
#[doc(hidden)]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ContentScalar {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Name(Vec<u8>),
    String(Vec<u8>),
    Operator(Vec<u8>),
}

impl ContentScalar {
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_real(&self) -> Option<f64> {
        match self {
            Self::Real(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_name(&self) -> Option<&[u8]> {
        match self {
            Self::Name(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&[u8]> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_operator(&self) -> Option<&[u8]> {
        match self {
            Self::Operator(value) => Some(value),
            _ => None,
        }
    }
}

fn content_scalar_from_token(token: Token) -> Option<ContentScalar> {
    if token.error_message.is_some() {
        return None;
    }
    match token.token_type {
        TokenType::Null => Some(ContentScalar::Null),
        TokenType::Bool => Some(ContentScalar::Boolean(token.value == b"true")),
        TokenType::Integer => parse_integer_token(&token).ok().map(ContentScalar::Integer),
        TokenType::Real => parse_real_token_value(&token).ok().map(ContentScalar::Real),
        TokenType::Name => {
            let mut value = token.value;
            if value.first().copied() != Some(b'/') {
                return None;
            }
            value.remove(0);
            Some(ContentScalar::Name(value))
        }
        TokenType::String => Some(ContentScalar::String(token.value)),
        TokenType::Word => Some(ContentScalar::Operator(token.value)),
        _ => None,
    }
}

/// Whether content-stream parsing should continue after an object callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseControl {
    /// Continue parsing the content stream.
    Continue,
    /// Stop immediately without calling [`ObjectHandleParserCallbacks::handle_eof`].
    Stop,
}

/// qpdf's `QPDFObjectHandle::ParserCallbacks` boundary
/// (`include/qpdf/QPDFObjectHandle.hh:204-226`).
///
/// Parsed values are canonical [`ObjectHandle`]s, so callback code
/// can inspect identity and parsed offsets without introducing an
/// ObjectHandle-to-Object consumer bridge.
pub trait ObjectHandleParserCallbacks {
    /// Whether this callback consumes lightweight top-level content scalars.
    ///
    /// Implementations that set this to true must override handle_scalar.
    /// The parser then moves each scalar directly into the callback instead of
    /// constructing a heavyweight ObjectHandle.
    #[doc(hidden)]
    const HANDLES_CONTENT_SCALARS: bool = false;

    /// Receive the full decoded content size before the first object.
    fn content_size(&mut self, _size: usize) -> Result<()> {
        Ok(())
    }

    /// Consume one lightweight scalar when HANDLES_CONTENT_SCALARS is enabled.
    #[doc(hidden)]
    fn handle_scalar(
        &mut self,
        _scalar: ContentScalar,
        _offset: usize,
        _length: usize,
    ) -> Result<ParseControl> {
        Err(Error::Internal(
            "content scalar callback invoked without HANDLES_CONTENT_SCALARS".into(),
        ))
    }

    /// Consume an operator without transferring ownership of its token bytes.
    ///
    /// The default preserves the old scalar callback contract. Hot callbacks
    /// can override this to fan out the borrowed operator without allocating
    /// or cloning the short operator token at every callback layer.
    #[doc(hidden)]
    fn handle_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
    ) -> Result<ParseControl> {
        self.handle_scalar(ContentScalar::Operator(operator.to_vec()), offset, length)
    }

    /// Receive one parsed ObjectHandle and its qpdf content span.
    fn handle_object(
        &mut self,
        object: ObjectHandle,
        offset: usize,
        length: usize,
    ) -> Result<ParseControl>;

    /// Receive normal content EOF. A [`ParseControl::Stop`] return from
    /// `handle_object` skips this callback, matching qpdf's
    /// `terminateParsing` path.
    fn handle_eof(&mut self) -> Result<()>;
}

fn deliver_diagnostic(
    context: Option<&Rc<dyn DocumentResolver>>,
    source_description: &str,
    object_description: &str,
    offset: usize,
    message: &str,
) -> Result<()> {
    let warning = QpdfExc::new(
        QpdfErrorCode::DamagedPdf,
        source_description.as_bytes(),
        object_description.as_bytes(),
        i64::try_from(offset).unwrap_or(i64::MAX),
        message.as_bytes(),
    );
    if let Some(context) = context {
        context.warn(warning)?;
        Ok(())
    } else {
        Err(Error::QpdfExc(warning))
    }
}

/// Parse an in-memory fragment with qpdf's warning-and-continue context.
///
/// qpdf's tolerant content consumers always parse through a document-owned
/// `QPDF`, even when the bytes are an in-memory fragment. This internal route
/// supplies only that warning boundary; the public detached route above still
/// throws when no context exists.
pub(crate) fn parse_content_stream_handles_with_recoverable_warnings<
    C: ObjectHandleParserCallbacks,
>(
    input: &[u8],
    source_description: &str,
    callbacks: &mut C,
) -> Result<()> {
    parse_content_stream_handles_with_recoverable_warnings_and_status(
        input,
        source_description,
        callbacks,
    )
    .map(|_| ())
}

/// Parse through a synthetic warning sink and report whether a container EOF
/// stopped the scan. Detached ResourceReplacer callers retain their historical
/// structural-failure fallback, while document-owned callers use the qpdf
/// warning-and-EOF path directly.
pub(crate) fn parse_content_stream_handles_with_recoverable_warnings_and_status<
    C: ObjectHandleParserCallbacks,
>(
    input: &[u8],
    source_description: &str,
    callbacks: &mut C,
) -> Result<bool> {
    let context: Rc<dyn DocumentResolver> = Rc::new(RecoverableWarningResolver::default());
    parse_content_stream_handles_internal(input, Some(context), source_description, callbacks)
}

/// Parse decoded content bytes into ObjectHandle callbacks.
pub(crate) fn parse_content_stream_handles<C: ObjectHandleParserCallbacks>(
    input: &[u8],
    context: Option<Rc<dyn DocumentResolver>>,
    source_description: &str,
    callbacks: &mut C,
) -> Result<()> {
    parse_content_stream_handles_internal(input, context, source_description, callbacks).map(|_| ())
}

/// Parse detached decoded content bytes through the qpdf-shaped object callback boundary.
///
/// This is intentionally document-agnostic: callers provide already-decoded bytes, and
/// malformed-content diagnostics are returned as errors because there is no owning PDF
/// warning sink. It exists so higher-level consumers can reuse flpdf's proven lexical/COS
/// content parser without adopting flpdf's mutable document model.
pub fn parse_detached_content_stream<C: ObjectHandleParserCallbacks>(
    input: &[u8],
    source_description: &str,
    callbacks: &mut C,
) -> Result<()> {
    parse_content_stream_handles(input, None, source_description, callbacks)
}

fn skip_content_ignorable(input: &[u8], mut position: usize) -> usize {
    while position < input.len() {
        match input[position] {
            0 | b'\t' | b'\n' | 0x0c | b'\r' | b' ' => position += 1,
            b'%' => {
                position += 1;
                while position < input.len() && !matches!(input[position], b'\n' | b'\r') {
                    position += 1;
                }
            }
            _ => break,
        }
    }
    position
}

fn parse_content_stream_handles_internal<C: ObjectHandleParserCallbacks>(
    input: &[u8],
    context: Option<Rc<dyn DocumentResolver>>,
    source_description: &str,
    callbacks: &mut C,
) -> Result<bool> {
    callbacks.content_size(input.len())?;

    let mut tokenizer = Tokenizer::new(input);
    tokenizer.allow_eof();
    // Ordinary content objects use the canonical live-input parser.
    // The pull tokenizer remains only for inline-image payload framing.
    let mut live_input = SliceLiveInput::new(input);
    let mut live_tokens = LiveTokenSource::new(&mut live_input);
    let mut resolver = ContentHandleResolver::new(context.clone());
    let mut stopped_on_container_eof = false;

    while usize::try_from(live_tokens.tell()?).unwrap_or(usize::MAX) < input.len() {
        let position = usize::try_from(live_tokens.tell()?).unwrap_or(usize::MAX);
        let offset = skip_content_ignorable(input, position);
        if offset >= input.len() {
            break;
        }
        live_tokens.seek(offset as u64)?;

        let mut scalar_result = None;
        if C::HANDLES_CONTENT_SCALARS {
            let token = live_tokens.next_scalar_token()?;
            let token_start = token.start;
            let token_end = token.end;
            if token.error_message.is_none() && token.token_type == TokenType::Word {
                let length = token_end.saturating_sub(token_start);
                let is_id = token.value == b"ID";
                let control = callbacks.handle_operator(&token.value, token_start, length)?;
                scalar_result = Some((is_id, control));
            } else if let Some(scalar) = content_scalar_from_token(token) {
                let length = token_end.saturating_sub(token_start);
                let control = callbacks.handle_scalar(scalar, token_start, length)?;
                scalar_result = Some((false, control));
            } else {
                // Containers and recovery cases retain the canonical
                // qpdf-shaped parser.
                live_tokens.seek(offset as u64)?;
            }
        }

        let (is_id, control) = if let Some(result) = scalar_result {
            result
        } else {
            let (object, length, diagnostics) = {
                let parsed =
                    parse_live_content_stream_object_from_tokens(&mut live_tokens, &mut resolver)?;
                let next = usize::try_from(live_tokens.tell()?).unwrap_or(usize::MAX);
                let length = next.saturating_sub(offset);
                (parsed.value, length, parsed.diagnostics)
            };
            for diagnostic in diagnostics {
                if diagnostic.message == "parse error while reading object" {
                    stopped_on_container_eof = true;
                }
                deliver_diagnostic(
                    context.as_ref(),
                    source_description,
                    "content",
                    diagnostic.relative_offset,
                    &diagnostic.message,
                )?;
            }
            if !object.is_initialized() {
                break;
            }
            let is_id = object.as_operator().as_deref() == Some(b"ID");
            let control = callbacks.handle_object(object, offset, length)?;
            (is_id, control)
        };

        let live_position = usize::try_from(live_tokens.tell()?).unwrap_or(usize::MAX);
        tokenizer.set_position(live_position)?;
        if control == ParseControl::Stop {
            return Ok(false);
        }

        if is_id {
            // qpdf discards the byte after ID without making a short read an
            // exception; the subsequent inline-image token read reports the
            // warning-only EOF case (QPDFObjectHandle.cc:1820-1848).
            if tokenizer.consume_one_byte().is_err() {
                deliver_diagnostic(
                    context.as_ref(),
                    source_description,
                    "stream data",
                    input.len(),
                    "EOF found while reading inline image",
                )?;
                break;
            }
            let inline_offset = tokenizer.position();
            // The shared tokenizer is reset by consume_one_byte, so this
            // state failure is unreachable through this pull route. Keep the
            // defensive mapping documented for callers that change tokenizer
            // state handling.
            // cov:ignore-start: consume_one_byte resets the shared tokenizer; qpdf state errors are unreachable here
            tokenizer.expect_inline_image().map_err(|error| {
                let message = match error {
                    TokenizerStateError::TokenWaiting => "tokenizer already has a token waiting",
                    TokenizerStateError::ImproperInlineImageState => {
                        "tokenizer is in an improper inline image state"
                    }
                };
                Error::parse(inline_offset, message)
            })?;
            // cov:ignore-end
            let image = tokenizer.read_token(true, 0)?;
            if image.token_type == TokenType::Bad {
                // QPDFObjectHandle::parseContentStream_data warns and lets the
                // surrounding parseContentStream_internal deliver handleEOF;
                // an incomplete inline image is not a parser exception on this
                // owning ObjectHandle route (QPDFObjectHandle.cc:1826-1848).
                let diagnostic = "EOF found while reading inline image";
                deliver_diagnostic(
                    context.as_ref(),
                    source_description,
                    "stream data",
                    image.end,
                    diagnostic,
                )?; // cov:ignore: LLVM attributes this successful diagnostic-delivery terminator to the fallible error edge.
                break;
            }
            let image_offset = image.start;
            let image_length = image.end - image.start;
            if callbacks.handle_object(
                ObjectHandle::inline_image(image.value),
                image_offset,
                image_length,
            )? == ParseControl::Stop
            {
                return Ok(false);
            }
            live_tokens.seek(tokenizer.position() as u64)?;
        }
    }

    callbacks.handle_eof()?;
    Ok(stopped_on_container_eof)
}

#[derive(Default)]
struct RecoverableWarningResolver {
    warnings: RefCell<Vec<QpdfExc>>,
}

impl DocumentResolver for RecoverableWarningResolver {
    fn resolve_indirect(
        &self,
        _object_ref: crate::ObjectRef,
        _handle: &ObjectHandle,
    ) -> Result<()> {
        Err(Error::Internal(
            "indirect resolution requested from an in-memory content warning sink".to_owned(),
        ))
    }

    fn warn(&self, warning: QpdfExc) -> Result<()> {
        self.warnings.borrow_mut().push(warning);
        Ok(())
    }
}

/// Accumulates content objects until an operator event is received.
///
/// This adapter deliberately sees only parser events. Lexical boundaries and
/// inline-image discovery remain owned by [`parse_content_stream_handles`].
pub(crate) struct OperationCallbacks<F> {
    operands: Vec<ObjectHandle>,
    on_operation: F,
}

pub(crate) fn parse_content_operations_with_recoverable_warnings<F>(
    input: &[u8],
    on_operation: F,
) -> Result<()>
where
    F: FnMut(&[ObjectHandle], &[u8]) -> Result<ParseControl>,
{
    let mut callbacks = OperationCallbacks {
        operands: Vec::new(),
        on_operation,
    };
    parse_content_stream_handles_with_recoverable_warnings(input, "", &mut callbacks)
}

impl<F> ObjectHandleParserCallbacks for OperationCallbacks<F>
where
    F: FnMut(&[ObjectHandle], &[u8]) -> Result<ParseControl>,
{
    fn handle_object(
        &mut self,
        object: ObjectHandle,
        _offset: usize,
        _length: usize,
    ) -> Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            let control = (self.on_operation)(&self.operands, &operator)?;
            self.operands.clear();
            Ok(control)
        } else if object.as_inline_image().is_some() {
            Ok(ParseControl::Continue)
        } else {
            self.operands.push(object);
            Ok(ParseControl::Continue)
        }
    }

    fn handle_eof(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Parse content and invoke `on_operation` with each operator's accumulated
/// operands.
///
/// Inline-image payload events are ignored by this convenience adapter.
/// Consumers that need inline-image headers or payloads should implement
/// [`ObjectHandleParserCallbacks`] directly.
///
/// # Errors
///
/// Recoverable object-token errors use qpdf's document warning sink when the
/// content belongs to a document, and become `Error::System` values carrying
/// qpdf's formatted `QPDFExc::what()` for detached parsing. Inline-image/
/// tokenizer state errors and callback errors are propagated.
pub fn parse_content_operations<F>(input: &[u8], on_operation: F) -> Result<()>
where
    F: FnMut(&[ObjectHandle], &[u8]) -> Result<ParseControl>,
{
    let mut callbacks = OperationCallbacks {
        operands: Vec::new(),
        on_operation,
    };
    parse_content_stream_handles(input, None, "", &mut callbacks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingResolver {
        warnings: RefCell<Vec<Vec<u8>>>,
    }

    impl DocumentResolver for RecordingResolver {
        fn resolve_indirect(
            &self,
            _object_ref: crate::ObjectRef,
            _handle: &ObjectHandle,
        ) -> Result<()> {
            Err(Error::Internal("unexpected indirect resolution".to_owned()))
        }

        fn warn(&self, warning: QpdfExc) -> Result<()> {
            self.warnings
                .borrow_mut()
                .push(warning.what_bytes().to_vec());
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingCallbacks {
        objects: usize,
        eof: bool,
    }

    impl ObjectHandleParserCallbacks for RecordingCallbacks {
        fn handle_object(
            &mut self,
            _object: ObjectHandle,
            _offset: usize,
            _length: usize,
        ) -> Result<ParseControl> {
            self.objects += 1;
            Ok(ParseControl::Continue)
        }

        fn handle_eof(&mut self) -> Result<()> {
            self.eof = true;
            Ok(())
        }
    }

    #[derive(Default)]
    struct SpanCallbacks {
        spans: Vec<(usize, usize)>,
        inline_images: usize,
        eof: bool,
    }

    impl ObjectHandleParserCallbacks for SpanCallbacks {
        fn handle_object(
            &mut self,
            object: ObjectHandle,
            offset: usize,
            length: usize,
        ) -> Result<ParseControl> {
            if object.as_inline_image().is_some() {
                self.inline_images += 1;
            }
            self.spans.push((offset, length));
            Ok(ParseControl::Continue)
        }

        fn handle_eof(&mut self) -> Result<()> {
            self.eof = true;
            Ok(())
        }
    }

    #[derive(Default)]
    struct ScalarNameCallbacks {
        names: Vec<Vec<u8>>,
    }

    impl ObjectHandleParserCallbacks for ScalarNameCallbacks {
        const HANDLES_CONTENT_SCALARS: bool = true;

        fn handle_scalar(
            &mut self,
            scalar: ContentScalar,
            _offset: usize,
            _length: usize,
        ) -> Result<ParseControl> {
            if let Some(name) = scalar.as_name() {
                self.names.push(name.to_vec());
            }
            Ok(ParseControl::Continue)
        }

        fn handle_object(
            &mut self,
            _object: ObjectHandle,
            _offset: usize,
            _length: usize,
        ) -> Result<ParseControl> {
            Ok(ParseControl::Continue)
        }

        fn handle_eof(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct ScalarFallbackCallbacks {
        scalar_count: usize,
        arrays: Vec<Vec<i64>>,
    }

    impl ObjectHandleParserCallbacks for ScalarFallbackCallbacks {
        const HANDLES_CONTENT_SCALARS: bool = true;

        fn handle_scalar(
            &mut self,
            _scalar: ContentScalar,
            _offset: usize,
            _length: usize,
        ) -> Result<ParseControl> {
            self.scalar_count += 1;
            Ok(ParseControl::Continue)
        }

        fn handle_object(
            &mut self,
            object: ObjectHandle,
            _offset: usize,
            _length: usize,
        ) -> Result<ParseControl> {
            if let Some(items) = object.as_array() {
                self.arrays.push(
                    items
                        .into_iter()
                        .filter_map(|item| item.as_integer())
                        .collect(),
                );
            }
            Ok(ParseControl::Continue)
        }

        fn handle_eof(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scalar_fast_path_falls_back_to_canonical_array_parser() {
        let mut callbacks = ScalarFallbackCallbacks::default();
        parse_detached_content_stream(b"[1 2 3] TJ 4 Tc", "scalar fallback", &mut callbacks)
            .expect("content parses");
        assert_eq!(callbacks.arrays, vec![vec![1, 2, 3]]);
        assert_eq!(callbacks.scalar_count, 4);
    }

    #[test]
    fn lightweight_name_scalar_matches_object_handle_name_semantics() {
        let mut callbacks = ScalarNameCallbacks::default();
        parse_detached_content_stream(b"/Im1 Do", "name scalar", &mut callbacks)
            .expect("content parses");
        assert_eq!(callbacks.names, vec![b"Im1".to_vec()]);
    }

    #[test]
    fn detached_content_skips_ignorable_bytes_without_probe_tokenization() {
        let input = b" \n% hi\r\n12% after number\n0 0 1 3 4 cm";
        let mut callbacks = SpanCallbacks::default();
        parse_detached_content_stream(input, "span test", &mut callbacks).expect("content parses");
        let slices = callbacks
            .spans
            .iter()
            .map(|&(offset, length)| &input[offset..offset + length])
            .collect::<Vec<_>>();
        assert_eq!(
            slices,
            vec![
                b"12".as_slice(),
                b"0".as_slice(),
                b"0".as_slice(),
                b"1".as_slice(),
                b"3".as_slice(),
                b"4".as_slice(),
                b"cm".as_slice(),
            ]
        );
        assert!(callbacks.eof);
    }

    #[test]
    fn detached_content_resynchronizes_after_inline_image_payload() {
        let input = b"BI /W 1 /H 1 /BPC 8 /CS /G ID \x7f EI Q";
        let mut callbacks = SpanCallbacks::default();
        parse_detached_content_stream(input, "inline span test", &mut callbacks)
            .expect("inline image and following operator parse");
        assert_eq!(callbacks.inline_images, 1);
        let &(offset, length) = callbacks.spans.last().expect("Q callback");
        assert_eq!(&input[offset..offset + length], b"Q");
        assert!(callbacks.eof);
    }

    #[derive(Default)]
    struct BorrowedOperatorCallbacks {
        operators: Vec<Vec<u8>>,
        scalars: usize,
    }

    impl ObjectHandleParserCallbacks for BorrowedOperatorCallbacks {
        const HANDLES_CONTENT_SCALARS: bool = true;

        fn handle_scalar(
            &mut self,
            _scalar: ContentScalar,
            _offset: usize,
            _length: usize,
        ) -> Result<ParseControl> {
            self.scalars += 1;
            Ok(ParseControl::Continue)
        }

        fn handle_operator(
            &mut self,
            operator: &[u8],
            _offset: usize,
            _length: usize,
        ) -> Result<ParseControl> {
            self.operators.push(operator.to_vec());
            Ok(ParseControl::Continue)
        }

        fn handle_object(
            &mut self,
            _object: ObjectHandle,
            _offset: usize,
            _length: usize,
        ) -> Result<ParseControl> {
            Ok(ParseControl::Continue)
        }

        fn handle_eof(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn lightweight_operators_use_borrowed_callback() {
        let mut callbacks = BorrowedOperatorCallbacks::default();
        parse_detached_content_stream(b"q 1 0 0 1 2 3 cm Q", "borrowed operators", &mut callbacks)
            .expect("content parses");
        assert_eq!(
            callbacks.operators,
            vec![b"q".to_vec(), b"cm".to_vec(), b"Q".to_vec()]
        );
        assert_eq!(callbacks.scalars, 6);
    }

    #[test]
    fn recoverable_warning_resolver_rejects_indirect_resolution() {
        let resolver = RecoverableWarningResolver::default();
        let object_ref = crate::ObjectRef::new(9, 0);
        let handle = ObjectHandle::new_indirect_unresolved(object_ref, -1);

        let error = resolver
            .resolve_indirect(object_ref, &handle)
            .expect_err("an in-memory warning sink cannot resolve indirect objects");
        assert!(matches!(
            error,
            Error::Internal(message)
                if message == "indirect resolution requested from an in-memory content warning sink"
        ));
    }

    #[test]
    fn recording_warning_resolver_rejects_indirect_resolution() {
        let resolver = RecordingResolver::default();
        let error = resolver
            .resolve_indirect(crate::ObjectRef::new(7, 0), &ObjectHandle::null())
            .expect_err("the test warning resolver must reject indirect resolution");
        assert!(matches!(
            error,
            Error::Internal(message) if message == "unexpected indirect resolution"
        ));
    }

    #[test]
    fn container_eof_warns_and_finishes_content_parsing_like_qpdf() {
        for input in [b"/F1 12 Tf [".as_slice(), b"/F1 12 Tf << /A 1".as_slice()] {
            let resolver = Rc::new(RecordingResolver::default());
            let context: Rc<dyn DocumentResolver> = resolver.clone();
            let mut callbacks = RecordingCallbacks::default();

            parse_content_stream_handles(
                input,
                Some(context),
                "page object 14 0 stream 14 0",
                &mut callbacks,
            )
            .expect("content EOF is a warning, not a hard parser error");

            assert_eq!(callbacks.objects, 3, "qpdf keeps the complete prefix");
            assert!(
                callbacks.eof,
                "qpdf invokes handleEOF after the truncated object"
            );
            let warnings = resolver.warnings.borrow();
            assert_eq!(warnings.len(), 1, "qpdf emits one container EOF warning");
            let warning = String::from_utf8_lossy(&warnings[0]);
            let expected = format!(
                "page object 14 0 stream 14 0 (content, offset {}): parse error while reading object",
                input.len()
            );
            assert!(
                warning.contains(&expected),
                "warning must use qpdf's content description: {warning}"
            );
        }
    }

    #[test]
    fn nested_container_eof_propagates_to_the_outer_content_parse() {
        for input in [
            b"<< /A [".as_slice(),
            b"<< 1 [".as_slice(),
            b"[[".as_slice(),
        ] {
            let resolver = Rc::new(RecordingResolver::default());
            let context: Rc<dyn DocumentResolver> = resolver.clone();
            let mut callbacks = RecordingCallbacks::default();

            parse_content_stream_handles(input, Some(context), "nested", &mut callbacks)
                .expect("nested content EOF is a warning, not a hard parser error");

            assert_eq!(
                callbacks.objects, 0,
                "the incomplete outer object is discarded"
            );
            assert!(callbacks.eof, "qpdf completes the scan after the warning");
            let warnings = resolver.warnings.borrow();
            assert_eq!(
                warnings.len(),
                1,
                "qpdf emits one nested container EOF warning"
            );
            assert!(String::from_utf8_lossy(&warnings[0]).contains(&format!(
                "nested (content, offset {}): parse error while reading object",
                input.len()
            )));
        }
    }
}
