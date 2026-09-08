#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("PDF error: {0}")]
    Pdf(#[from] flpdf::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid PDF structure: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;
