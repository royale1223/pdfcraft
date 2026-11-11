//! Background colour and underline for added text items. The in-stream
//! rendering still lives in [`crate::added::store::write_with`] (two short
//! fork blocks that produce the fill/bar bytes and stamp `/Background` /
//! `/Underline` on the `/PCAdded` dict). Everything else — the public API,
//! the storage readers, and the parameter struct — lives here, so a future
//! upstream merge touches at most the hook in `write_with` and this one
//! `pub mod decoration;` line in `litepdf/mod.rs`.

use crate::added::Content;
use crate::added::{find, write_with};
use crate::{Document, EditError, ObjRef, Object};

/// A text item's background colour, as stored on its content stream.
pub(crate) fn stored_background(doc: &Document, obj: ObjRef) -> Option<[f64; 3]> {
    let o = doc.get(obj);
    let Object::Stream(s) = &*o else { return None };
    let added = s.dict.get(b"PCAdded").map(|a| doc.resolve(a)).and_then(|a| a.as_dict().cloned())?;
    let c: Vec<f64> = added
        .get(b"Background")
        .map(|o| doc.resolve(o))
        .and_then(|a| a.as_array().map(|a| a.iter().filter_map(Object::as_f64).collect()))
        .unwrap_or_default();
    (c.len() == 3 && c.iter().all(|v| v.is_finite())).then(|| [c[0], c[1], c[2]].map(|v| v.clamp(0.0, 1.0)))
}

/// Whether a text item is underlined, as stored on its content stream.
pub(crate) fn stored_underline(doc: &Document, obj: ObjRef) -> bool {
    let o = doc.get(obj);
    let Object::Stream(s) = &*o else { return false };
    let Some(added) = s.dict.get(b"PCAdded").map(|a| doc.resolve(a)).and_then(|a| a.as_dict().cloned()) else { return false };
    added.get(b"Underline").map(|u| doc.resolve(u)).is_some_and(|u| matches!(*u, Object::Bool(true)))
}

/// What [`write_with`] does with a text item's background colour and underline. `None` keeps what
/// the item has (nothing for a new item).
#[derive(Clone, Copy, Default)]
pub(crate) struct Decoration {
    /// Set (`Some(Some)`) or remove (`Some(None)`) the background colour.
    pub(crate) background: Option<Option<[f64; 3]>>,
    /// Underline (`Some(true)`) or not (`Some(false)`).
    pub(crate) underline: Option<bool>,
}

/// Set (`Some`, RGB 0–1) or remove (`None`) the background colour of text item `index` of `page`:
/// a filled box under the text that moves and resizes with the item.
pub fn set_added_background(doc: &mut Document, page: usize, index: usize, background: Option<[f64; 3]>) -> Result<(), EditError> {
    let a = find(doc, page, index)?;
    if !matches!(a.content, Content::Text(_)) {
        return Err(EditError::Invalid("only text items have a background".into()));
    }
    if background.is_some_and(|b| !b.iter().all(|v| v.is_finite())) {
        return Err(EditError::Invalid("the background colour must be finite".into()));
    }
    write_with(doc, page, &a.content, Some(a.obj), Decoration { background: Some(background), ..Decoration::default() })?;
    Ok(())
}

/// Underline (or stop underlining) text item `index` of `page`: a text-coloured bar under each
/// line that follows the item when it is moved, resized, retyped or restyled.
pub fn set_added_underline(doc: &mut Document, page: usize, index: usize, underline: bool) -> Result<(), EditError> {
    let a = find(doc, page, index)?;
    if !matches!(a.content, Content::Text(_)) {
        return Err(EditError::Invalid("only text items can be underlined".into()));
    }
    write_with(doc, page, &a.content, Some(a.obj), Decoration { underline: Some(underline), ..Decoration::default() })?;
    Ok(())
}

/// Whether added item `index` of `page` is underlined.
pub fn added_underline(doc: &Document, page: usize, index: usize) -> bool {
    find(doc, page, index).ok().is_some_and(|a| stored_underline(doc, a.obj))
}

/// The background colour of added item `index` of `page`, if it has one.
pub fn added_background(doc: &Document, page: usize, index: usize) -> Option<[f64; 3]> {
    find(doc, page, index).ok().and_then(|a| stored_background(doc, a.obj))
}
