/// Failures that can occur while loading the immutable Hayro source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SourceLoadError {
    /// The input does not form a structurally valid PDF.
    #[error("invalid PDF structure")]
    Invalid,
    /// An encrypted PDF does not provide the trailer ID required for key derivation.
    #[error("encrypted PDF is missing its /ID entry")]
    MissingEncryptionId,
    /// The PDF requires a password that pdf-redox does not have.
    #[error("PDF is password-protected")]
    PasswordProtected,
    /// The encryption dictionary is malformed or internally inconsistent.
    #[error("PDF encryption dictionary is invalid")]
    InvalidEncryption,
    /// The PDF selects an encryption algorithm that the source parser does not support.
    #[error("PDF uses an unsupported encryption algorithm")]
    UnsupportedEncryption,
}

impl From<hayro_syntax::LoadPdfError> for SourceLoadError {
    fn from(value: hayro_syntax::LoadPdfError) -> Self {
        match value {
            hayro_syntax::LoadPdfError::Invalid => Self::Invalid,
            hayro_syntax::LoadPdfError::Decryption(error) => match error {
                hayro_syntax::DecryptionError::MissingIDEntry => Self::MissingEncryptionId,
                hayro_syntax::DecryptionError::PasswordProtected => Self::PasswordProtected,
                hayro_syntax::DecryptionError::InvalidEncryption => Self::InvalidEncryption,
                hayro_syntax::DecryptionError::UnsupportedAlgorithm => Self::UnsupportedEncryption,
            },
        }
    }
}

/// Errors produced while parsing, transforming, or serializing a PDF.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A vendored flpdf helper rejected or failed to process PDF data.
    #[error("pdf error: {0}")]
    Pdf(#[from] flpdf::Error),
    /// An underlying I/O operation failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The document violates a structural or transformation invariant.
    #[error("invalid pdf structure: {0}")]
    Invalid(String),
    /// Hayro could not load the immutable source document.
    #[error("source parser failed to load PDF: {0}")]
    SourceLoad(#[from] SourceLoadError),
    /// A referenced source object is absent from the source cross-reference graph.
    #[error("source object {number} {generation} is missing")]
    MissingSourceObject {
        /// PDF object number.
        number: i32,
        /// PDF generation number.
        generation: i32,
    },
    /// An operation requiring a source stream was given a non-stream object.
    #[error("source object {number} {generation} is not a stream")]
    ExpectedSourceStream {
        /// PDF object number.
        number: i32,
        /// PDF generation number.
        generation: i32,
    },
    /// An operation attempted to access a source object already deleted by the overlay.
    #[error("source object {number} {generation} was deleted from the overlay")]
    DeletedSourceObject {
        /// PDF object number.
        number: i32,
        /// PDF generation number.
        generation: i32,
    },
    /// A deleted object remains reachable from the rewritten object graph.
    #[error("reachable object {number} {generation} was deleted from the overlay")]
    DeletedReferencedObject {
        /// PDF object number.
        number: i32,
        /// PDF generation number.
        generation: i32,
    },
    /// A temporary overlay identifier no longer resolves to a newly added object.
    #[error("overlay object {index} is missing")]
    MissingNewObject {
        /// Zero-based temporary overlay-object index.
        index: usize,
    },
    /// The writer has no output-number mapping for a reachable source object.
    #[error("output plan has no mapping for source object {number} {generation}")]
    MissingOutputSourceMapping {
        /// PDF object number.
        number: i32,
        /// PDF generation number.
        generation: i32,
    },
    /// The writer has no output-number mapping for a reachable overlay object.
    #[error("output plan has no mapping for overlay object {index}")]
    MissingOutputOverlayMapping {
        /// Zero-based temporary overlay-object index.
        index: usize,
    },
    /// The rewritten graph cannot be represented by the writer's object-number domain.
    #[error("pdf output contains too many indirect objects: {count}")]
    TooManyOutputObjects {
        /// Number of indirect objects planned for output.
        count: usize,
    },
    /// A byte offset cannot be represented by a classic cross-reference entry.
    #[error("pdf output offset {offset} exceeds the classic xref limit")]
    OutputOffsetTooLarge {
        /// Output byte offset that exceeded the classic xref limit.
        offset: usize,
    },
    /// A non-finite floating-point value was supplied where PDF requires a finite real.
    #[error("pdf real value is not finite")]
    InvalidReal,
}

/// Result type used by the pdf-redox core API.
pub type Result<T> = std::result::Result<T, Error>;
