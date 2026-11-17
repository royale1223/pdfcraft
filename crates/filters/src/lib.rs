//! PDF stream filters (`craft-pdf::filters`).
//!
//! pdfcraft-mobile adds JPEG and JPEG 2000 decoding to samples ([`decode_dct`], [`decode_jpx`])
//! for redaction's in-place image clearing; the shared crate has no equivalent yet.
pub use craft_pdf::filters::*;

mod dct;
mod jpx;

pub use dct::decode_dct;
pub use jpx::{JpxSamples, decode_jpx};
