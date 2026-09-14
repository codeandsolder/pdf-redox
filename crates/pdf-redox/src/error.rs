#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pdf error: {0}")]
    Pdf(#[from] flpdf::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid pdf structure: {0}")]
    Invalid(String),
    #[error("hayro failed to load PDF: {0:?}")]
    HayroLoad(hayro_syntax::LoadPdfError),
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
}

pub type Result<T> = std::result::Result<T, Error>;
