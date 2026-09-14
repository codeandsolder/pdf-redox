#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SourceLoadError {
    #[error("invalid PDF structure")]
    Invalid,
    #[error("encrypted PDF is missing its /ID entry")]
    MissingEncryptionId,
    #[error("PDF is password-protected")]
    PasswordProtected,
    #[error("PDF encryption dictionary is invalid")]
    InvalidEncryption,
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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pdf error: {0}")]
    Pdf(#[from] flpdf::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid pdf structure: {0}")]
    Invalid(String),
    #[error("source parser failed to load PDF: {0}")]
    SourceLoad(#[from] SourceLoadError),
    #[error("source object {number} {generation} is missing")]
    MissingSourceObject { number: i32, generation: i32 },
    #[error("source object {number} {generation} is not a stream")]
    ExpectedSourceStream { number: i32, generation: i32 },
    #[error("source object {number} {generation} was deleted from the overlay")]
    DeletedSourceObject { number: i32, generation: i32 },
    #[error("reachable object {number} {generation} was deleted from the overlay")]
    DeletedReferencedObject { number: i32, generation: i32 },
    #[error("overlay object {index} is missing")]
    MissingNewObject { index: usize },
    #[error("output plan has no mapping for source object {number} {generation}")]
    MissingOutputSourceMapping { number: i32, generation: i32 },
    #[error("output plan has no mapping for overlay object {index}")]
    MissingOutputOverlayMapping { index: usize },
    #[error("pdf output contains too many indirect objects: {count}")]
    TooManyOutputObjects { count: usize },
    #[error("pdf output offset {offset} exceeds the classic xref limit")]
    OutputOffsetTooLarge { offset: usize },
    #[error("pdf real value is not finite")]
    InvalidReal,
}

pub type Result<T> = std::result::Result<T, Error>;
