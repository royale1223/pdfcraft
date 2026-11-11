//! Lay an OCR text layer on a page, fitted by [`pdfcraft_ocr::Fit`]. Built on
//! [`Session::apply_with`] so there is no upstream `Edit` variant to maintain
//! across merges.

use crate::{DocId, EditError, Label, Session};
use pdfcraft_ocr::{Fit, PlacedWord};

/// Put recognized words on `page` as invisible text over the image, sized by
/// `fit`. One undoable step labelled `"Recognize text"`. The engine's
/// [`Edit::AddOcrText`] (`fit = Fit::Ink`) stays available for the default
/// Helvetica path; this entry point is for apps that read words from a
/// platform recognizer and want the extracted glyph boxes to match the
/// recognized word boxes ([`Fit::Box`]).
///
/// [`Edit::AddOcrText`]: crate::Edit::AddOcrText
pub fn add_ocr_text_fitted(session: &mut Session, id: DocId, page: usize, words: Vec<PlacedWord>, fit: Fit) -> Result<(), EditError> {
    session.apply_with(id, Label::of("Recognize text"), move |doc| Ok(crate::ocr::layer::add_text(doc, page, &words, fit)?))
}
