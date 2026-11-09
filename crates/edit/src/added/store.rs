use super::draw::{draw, params, underline_bars};
use super::layout::{first_undrawable, is_thai, nums, shaped_arabic, thai_error};
use super::types::{Added, AddedImage, AddedText, CHAR_SPACING_LIMIT, Family, MarkerScan, TAG};
use super::{Content, Dict, Document, EditError, GlyphError, MarkKind, ObjRef, Object, Stream, align_from, check, contents, page_list, place_tagged};
use crate::marks::matrix_number;

fn parse(doc: &Document, d: &Dict) -> Option<Content> {
    let r = nums(doc, d.get(b"Rect"));
    let rect: [f64; 4] = r.try_into().ok()?;
    match d.name(b"Kind")? {
        b"Text" => {
            let (family, bold, italic) = Family::from_base(d.name(b"Font").unwrap_or(b"Helvetica"));
            let c = nums(doc, d.get(b"Color"));
            let text = d.get(b"Text").and_then(|t| doc.resolve(t).as_string().map(|s| s.to_text())).unwrap_or_default();
            let align = align_from(d.int(b"Align"));
            // At most one entry per paragraph, whatever the file says.
            let aligns = d
                .get(b"Aligns")
                .and_then(|a| {
                    doc.resolve(a)
                        .as_array()
                        .map(|a| a.iter().take(crate::paragraph_align::count(&text)).map(|v| v.as_int().map(|c| align_from(Some(c)))).collect())
                })
                .unwrap_or_default();
            Some(Content::Text(AddedText {
                rect,
                text,
                family,
                bold,
                italic,
                size: d.get(b"Size").and_then(Object::as_f64).unwrap_or(12.0),
                color: if c.len() == 3 { [c[0], c[1], c[2]] } else { [0.0; 3] },
                align,
                aligns: crate::paragraph_align::tidy(aligns, align),
                // Items written before character spacing have none.
                char_spacing: d.get(b"CharSpacing").and_then(Object::as_f64).filter(|v| v.is_finite()).unwrap_or(0.0),
            }))
        }
        b"Image" => {
            let crop = nums(doc, d.get(b"Crop"));
            Some(Content::Image(AddedImage {
                rect,
                image: d.get(b"Image")?.as_ref()?,
                rotation: (d.int(b"Rotate").unwrap_or(0).rem_euclid(360) / 90) as u8,
                flip_h: matches!(d.get(b"FlipH"), Some(Object::Bool(true))),
                flip_v: matches!(d.get(b"FlipV"), Some(Object::Bool(true))),
                crop: crop.try_into().unwrap_or([0.0; 4]),
            }))
        }
        _ => None,
    }
}

fn with_resolved<T>(doc: &Document, object: &Object, f: impl FnOnce(&Object) -> T) -> T {
    match object {
        Object::Ref(reference) => {
            let resolved = doc.get(*reference);
            f(&resolved)
        }
        direct => f(direct),
    }
}

fn for_each_content_entry(doc: &Document, page: &pdfcraft_model::Page, mut visit: impl FnMut(&Object)) {
    let Some(contents) = page.dict.get(b"Contents") else { return };
    with_resolved(doc, contents, |resolved| match resolved {
        Object::Array(entries) => {
            for entry in entries {
                visit(entry);
            }
        }
        _ => visit(contents),
    });
}

/// Finds page marks and editable added content in one pass over the page content entries.
pub fn scan_markers(doc: &Document) -> MarkerScan {
    let mut scan = MarkerScan::default();
    for (pi, page) in page_list(doc).iter().enumerate() {
        for_each_content_entry(doc, page, |entry| {
            with_resolved(doc, entry, |resolved| {
                let Some(dict) = resolved.as_dict() else { return };
                let tag = dict.name(b"PCMark");
                let kind = if tag == Some(&b"HeaderFooter"[..]) {
                    Some(MarkKind::HeaderFooter)
                } else if tag == Some(&b"Watermark"[..]) {
                    Some(MarkKind::Watermark)
                } else if tag == Some(&b"Background"[..]) {
                    Some(MarkKind::Background)
                } else {
                    None
                };
                if let Some(kind) = kind
                    && !scan.marks.contains(&kind)
                {
                    scan.marks.push(kind);
                }

                let Some(obj) = Object::as_ref(entry) else { return };
                if tag != Some(TAG.as_bytes()) {
                    return;
                }
                let Some(params_object) = dict.get(b"PCAdded") else { return };
                with_resolved(doc, params_object, |resolved_params| {
                    let Some(params) = resolved_params.as_dict() else { return };
                    if let Some(content) = parse(doc, params) {
                        // Display points depend on the page's /UserUnit. An item records the one it was
                        // written for; none means 1, as for items written before display space followed
                        // /UserUnit.
                        let written = params
                            .get(b"UserUnit")
                            .and_then(Object::as_f64)
                            .filter(|u| u.is_finite() && *u >= 1.0)
                            .map_or(1.0, |u| u.min(pdfcraft_model::MAX_USER_UNIT));
                        scan.added.push(Added { page: pi, obj, content: content.scaled(page.user_unit(doc) / written) });
                    }
                });
            });
        });
    }
    scan
}

fn validate(c: &Content) -> Result<(), EditError> {
    let r = c.rect();
    if !r.iter().all(|v| v.is_finite()) {
        return Err(EditError::Invalid("invalid position".into()));
    }
    match c {
        Content::Text(t) => {
            if t.text.trim().is_empty() {
                return Err(EditError::Invalid("type some text first".into()));
            }
            if !(t.size.is_finite() && (1.0..=500.0).contains(&t.size)) {
                return Err(EditError::Invalid("the font size must be between 1 and 500 points".into()));
            }
            if !(t.char_spacing.is_finite() && (-CHAR_SPACING_LIMIT..=CHAR_SPACING_LIMIT).contains(&t.char_spacing)) {
                return Err(EditError::Invalid(format!(
                    "the character spacing must be between -{CHAR_SPACING_LIMIT} and {CHAR_SPACING_LIMIT} points"
                )));
            }
            if (r[2] - r[0]).abs() < 1.0 {
                return Err(EditError::Invalid("the text box is too narrow".into()));
            }
            if t.text.chars().any(is_thai) && pdfcraft_fonts::document_thai_font().is_none() {
                return Err(thai_error(GlyphError::NoFont, &t.text));
            }
            // Without the Arabic face, `draw` decides for the Arabic letters themselves (#403): a new
            // item is refused with what's missing, and one that already holds Arabic stays movable.
            // Everything else in it is drawn in the standard font, so it still has to fit.
            let undrawable = if t.text.chars().any(shaped_arabic) && pdfcraft_fonts::document_arabic_font().is_none() {
                t.text.split('\n').find_map(|line| {
                    line.chars()
                        .find(|c| !(shaped_arabic(*c) || is_thai(*c) && pdfcraft_fonts::thai_has(*c) || pdfcraft_fonts::win_ansi_encodable(*c)))
                })
            } else {
                first_undrawable(t)
            };
            if let Some(c) = undrawable {
                return Err(crate::undrawable(c));
            }
        }
        Content::Image(_) => {
            if (r[2] - r[0]).abs() < 1.0 || (r[3] - r[1]).abs() < 1.0 {
                return Err(EditError::Invalid("the image is too small".into()));
            }
        }
    }
    Ok(())
}

/// Write an item's stream (new, or replacing `obj`) and make its resources available to the page.
/// A text item's background colour, as stored on its content stream.
fn stored_background(doc: &Document, obj: ObjRef) -> Option<[f64; 3]> {
    let o = doc.get(obj);
    let Object::Stream(s) = &*o else { return None };
    let added = s.dict.get(b"PCAdded").map(|a| doc.resolve(a)).and_then(|a| a.as_dict().cloned())?;
    let c = nums(doc, added.get(b"Background"));
    (c.len() == 3 && c.iter().all(|v| v.is_finite())).then(|| [c[0], c[1], c[2]].map(|v| v.clamp(0.0, 1.0)))
}

/// Whether a text item is underlined, as stored on its content stream.
fn stored_underline(doc: &Document, obj: ObjRef) -> bool {
    let o = doc.get(obj);
    let Object::Stream(s) = &*o else { return false };
    let Some(added) = s.dict.get(b"PCAdded").map(|a| doc.resolve(a)).and_then(|a| a.as_dict().cloned()) else { return false };
    added.get(b"Underline").map(|u| doc.resolve(u)).is_some_and(|u| matches!(*u, Object::Bool(true)))
}

/// What [`write`] does with a text item's background colour and underline. `None` keeps what the
/// item has (nothing for a new item).
#[derive(Clone, Copy, Default)]
struct Decoration {
    /// Set (`Some(Some)`) or remove (`Some(None)`) the background colour.
    background: Option<Option<[f64; 3]>>,
    /// Underline (`Some(true)`) or not (`Some(false)`).
    underline: Option<bool>,
}

fn write(doc: &mut Document, page: usize, c: &Content, obj: Option<ObjRef>) -> Result<ObjRef, EditError> {
    write_with(doc, page, c, obj, Decoration::default())
}

fn write_with(doc: &mut Document, page: usize, c: &Content, obj: Option<ObjRef>, change: Decoration) -> Result<ObjRef, EditError> {
    validate(c)?;
    let all = page_list(doc);
    check(&[page], all.len())?;
    let p = &all[page];
    let view = p.view_matrix(doc);
    let user_unit = p.user_unit(doc);
    // The page gets its own copy of its resources with the item's fonts and images added, and
    // without the Arabic fonts of the version it replaces.
    let mut pres = p.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    if let Some(r) = obj {
        drop_fonts(doc, &mut pres, &own_fonts(doc, r));
    }
    let taken = pres.get(b"Font").map(|f| doc.resolve(f)).and_then(|f| f.as_dict().cloned()).unwrap_or_default();
    let (mut content, res) = draw(doc, c, view, &taken, obj.is_some())?;
    // A text item's background: a filled box under the text, inside the item's own stream so it
    // moves, resizes and goes with it. Kept across rewrites (moves, retyping) unless changed.
    let background = match change.background {
        Some(b) => b,
        None => obj.and_then(|r| stored_background(doc, r)),
    }
    .filter(|_| matches!(c, Content::Text(_)));
    // Underline: a text-coloured bar under each line, drawn over the text in the same stream so
    // it follows rewrapping, moves and restyling.
    let underline = change.underline.unwrap_or_else(|| obj.is_some_and(|r| stored_underline(doc, r)));
    let underline = match c {
        Content::Text(t) if underline => Some(t),
        _ => None,
    };
    if let Some(t) = underline {
        let [r, g, b] = t.color.map(|v| v.clamp(0.0, 1.0));
        let mut bars = format!("q {} {} {} rg\n", matrix_number(r), matrix_number(g), matrix_number(b));
        for [x, y, w, h] in underline_bars(t) {
            bars.push_str(&format!("{} {} {} {} re\n", matrix_number(x), matrix_number(y), matrix_number(w), matrix_number(h)));
        }
        bars.push_str("f Q\n");
        // Before the closing `Q` of the item's display-space `q … cm`.
        let at = if content.ends_with(b"Q\n") { content.len() - 2 } else { content.len() };
        content.splice(at..at, bars.into_bytes());
    }
    if let Some([r, g, b]) = background {
        let [x0, y0, x1, y1] = c.rect();
        let fill = format!(
            "q {} {} {} rg {} {} {} {} re f Q\n",
            matrix_number(r),
            matrix_number(g),
            matrix_number(b),
            matrix_number(x0.min(x1)),
            matrix_number(y0.min(y1)),
            matrix_number((x1 - x0).abs()),
            matrix_number((y1 - y0).abs())
        );
        // After the opening `q … cm`, which puts the stream in display space.
        let at = content.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
        content.splice(at..at, fill.into_bytes());
    }
    if obj.is_none() {
        crate::check_on_top(doc, p)?;
    }
    for (k, v) in res.iter() {
        let mut sub = pres.get(k).map(|s| doc.resolve(s)).and_then(|s| s.as_dict().cloned()).unwrap_or_default();
        for (n2, o) in v.as_dict().into_iter().flat_map(|d| d.iter()) {
            sub.set(n2.clone(), o.clone());
        }
        pres.set(k.clone(), Object::Dict(sub));
    }
    doc.update_dict(p.obj, |d| d.set(b"Resources".to_vec(), Object::Dict(pres)))?;
    let mut sd = Dict::new();
    sd.set(b"PCMark".to_vec(), Object::name(TAG));
    let mut added = params(c);
    if underline.is_some() {
        added.set(b"Underline".to_vec(), Object::Bool(true));
    }
    if let Some(b) = background {
        added.set(b"Background".to_vec(), Object::Array(b.iter().map(|v| Object::Real(*v)).collect()));
    }
    if user_unit != 1.0 {
        added.set(b"UserUnit".to_vec(), Object::Real(user_unit));
    }
    sd.set(b"PCAdded".to_vec(), Object::Dict(added));
    let stream = Stream::flate(sd, &content);
    match obj {
        Some(r) => {
            doc.set(r, Object::Stream(stream));
            Ok(r)
        }
        None => {
            // place_tagged appends a new stream; recover its reference.
            let p = page_list(doc).swap_remove(page);
            place_tagged(doc, &p, TAG, content.clone(), false)?;
            let p = page_list(doc).swap_remove(page);
            let r = contents(doc, &p)?.last().and_then(Object::as_ref).ok_or_else(|| EditError::Invalid("could not add the content".into()))?;
            doc.set(r, Object::Stream(stream));
            Ok(r)
        }
    }
}

/// Every added item, page by page, in drawing order.
pub fn list_added(doc: &Document) -> Vec<Added> {
    scan_markers(doc).added
}

/// Add an item to a page (0-based). Returns its index among the page's added items.
pub fn add_content(doc: &mut Document, page: usize, c: &Content) -> Result<usize, EditError> {
    write(doc, page, c, None)?;
    Ok(list_added(doc).iter().filter(|a| a.page == page).count() - 1)
}

fn find(doc: &Document, page: usize, index: usize) -> Result<Added, EditError> {
    list_added(doc)
        .into_iter()
        .filter(|a| a.page == page)
        .nth(index)
        .ok_or_else(|| EditError::Invalid(format!("page {} has no added item {}", page + 1, index + 1)))
}

/// Replace item `index` of `page` (moved, resized, retyped or reformatted).
pub fn update_content(doc: &mut Document, page: usize, index: usize, c: &Content) -> Result<(), EditError> {
    let a = find(doc, page, index)?;
    if std::mem::discriminant(&a.content) != std::mem::discriminant(c) {
        return Err(EditError::Invalid("an item can't change between text and image".into()));
    }
    write(doc, page, c, Some(a.obj))?;
    Ok(())
}

/// Set (`Some`, RGB 0–1) or remove (`None`) the background colour of text item `index` of
/// `page`: a filled box under the text that moves and resizes with the item.
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

/// Remove item `index` of `page`.
pub fn delete_content(doc: &mut Document, page: usize, index: usize) -> Result<(), EditError> {
    let a = find(doc, page, index)?;
    let p = page_list(doc).swap_remove(page);
    let list: Vec<Object> = contents(doc, &p)?.into_iter().filter(|o| o.as_ref() != Some(a.obj)).collect();
    let fonts = own_fonts(doc, a.obj);
    // Only the page's own resources carry an item's fonts (`write` gives the page a copy).
    let res = match p.dict.get(b"Resources") {
        Some(Object::Dict(d)) if !fonts.is_empty() => Some(d.clone()),
        _ => None,
    };
    let res = res.map(|mut r| {
        drop_fonts(doc, &mut r, &fonts);
        r
    });
    doc.update_dict(p.obj, |d| {
        d.set(b"Contents".to_vec(), Object::Array(list));
        if let Some(r) = res {
            d.set(b"Resources".to_vec(), Object::Dict(r));
        }
    })?;
    Ok(())
}

/// Leave only item `index` on `page`: its contents become the item's own stream and its
/// annotations go, so a render of the page (on a transparent backdrop) is the item's pixels
/// alone. For previews on a copy of the document; the page's resources stay as they are.
pub fn isolate_content(doc: &mut Document, page: usize, index: usize) -> Result<(), EditError> {
    let a = find(doc, page, index)?;
    let p = page_list(doc).swap_remove(page);
    // An item's stream starts from the page's initial graphics state: `place_tagged` closes
    // everything drawn before it, so it draws the same on its own.
    doc.update_dict(p.obj, |d| {
        d.set(b"Contents".to_vec(), Object::Array(vec![Object::Ref(a.obj)]));
        d.remove(b"Annots");
    })?;
    Ok(())
}

/// The Type 3 fonts an Arabic item's stream shows text with. Each item has its own (`draw_shaped`
/// names them so), so they go when the item is replaced or deleted.
fn own_fonts(doc: &Document, obj: ObjRef) -> Vec<Vec<u8>> {
    let Object::Stream(s) = &*doc.get(obj) else { return Vec::new() };
    let Ok(data) = s.decoded() else { return Vec::new() };
    data.split(u8::is_ascii_whitespace).filter_map(|w| w.strip_prefix(b"/")).filter(|w| w.starts_with(b"PCAr")).map(<[u8]>::to_vec).collect()
}

fn drop_fonts(doc: &Document, res: &mut Dict, names: &[Vec<u8>]) {
    if names.is_empty() {
        return;
    }
    let Some(mut fonts) = res.get(b"Font").map(|f| doc.resolve(f)).and_then(|f| f.as_dict().cloned()) else { return };
    for name in names {
        fonts.remove(name);
    }
    res.set(b"Font".to_vec(), Object::Dict(fonts));
}
