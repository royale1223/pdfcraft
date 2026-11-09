//! Guarded text redaction: [`Session::add_redaction_mark`], [`Session::apply_redactions_guarded`]
//! and [`Session::delete_text`], each one undoable step.
//!
//! `pdfcraft-redact` removes a vector path a mark touches *whole* (it never keeps a path's data
//! hidden behind a clip). On many real pages one path is the page background or a large panel,
//! so marking a single word blanked the whole page. The guarded apply works on the staged copy:
//!
//! 1. [`split_partial_rect_fills`] rewrites an axis-aligned `re` fill that a mark only partly
//!    covers into the rectangles around the mark, so only the marked area goes;
//! 2. box-only marks keep the selection's full height ([`take_box_quads`], [`paint_boxes`])
//!    while a trimmed band ([`trim_text_quads`]) decides which glyphs go, so a word never takes
//!    the neighbouring line with it;
//! 3. an image or form that would be removed whole refuses the step;
//! 4. the pages are rendered before and after; when content outside the marks still disappears
//!    ([`lost_outside_marks`]), the step is refused and the document is unchanged.

use std::sync::Arc;

use pdfcraft_content::{Matrix, Pieces, contains, overlaps};
use pdfcraft_cos::{Document as CosDocument, ObjRef, Object, Stream};
use pdfcraft_redact::Mark;
use pdfcraft_render::{PageInfo, PageRenderer, RenderConfig, RenderRequest, RequestKind};

use crate::{DocId, EditError, Label, Session};

/// Mark rects grow by this before subtracting, so fragments never touch a mark (the hit test is
/// strict, but transforms round-trip in floating point).
const GAP: f64 = 0.001;
/// More fragments than this from one rectangle: leave it to `pdfcraft-redact`.
const MAX_FRAGMENTS: usize = 256;
/// Share of a page that may change outside redaction marks before the step is refused
/// (anti-aliasing at the margins is far below this).
const LOST_LIMIT: f64 = 0.01;
/// Share of a selection quad's height (from its bottom edge) that a mark keeps on text pages:
/// -0.05 em to 0.7 em around the baseline of the em-box selection (-0.2 em to 0.8 em).
const TEXT_MARK_BAND: (f64, f64) = (0.15, 0.9);
/// Private key on a Redact annotation: the untrimmed selection quads, so the applied box keeps
/// the selection's full height while only the trimmed band decides which glyphs go.
const BOX_QUADS_KEY: &[u8] = b"PCLiteBoxQuads";

fn cos_err(e: impl std::fmt::Display) -> EditError {
    EditError::Invalid(e.to_string())
}

/// Marks on the pages being applied, grouped by page index.
pub fn marks_by_page(cos: &CosDocument, pages: Option<&[usize]>) -> Vec<(usize, Vec<[f64; 4]>)> {
    let mut out: Vec<(usize, Vec<[f64; 4]>)> = Vec::new();
    for Mark { page, rects, .. } in pdfcraft_redact::marks(cos) {
        if pages.is_some_and(|p| !p.contains(&page)) {
            continue;
        }
        match out.iter_mut().find(|(p, _)| *p == page) {
            Some((_, r)) => r.extend(rects),
            None => out.push((page, rects)),
        }
    }
    out
}

/// `outer` minus every rect in `holes`, as disjoint rectangles. `None` when the result would be
/// too fragmented.
pub fn subtract(outer: [f64; 4], holes: &[[f64; 4]]) -> Option<Vec<[f64; 4]>> {
    let mut parts = vec![outer];
    for h in holes {
        let h = [h[0] - GAP, h[1] - GAP, h[2] + GAP, h[3] + GAP];
        let mut next = Vec::with_capacity(parts.len() + 4);
        for p in parts {
            if !overlaps(p, h, 0.0) {
                next.push(p);
                continue;
            }
            let (y0, y1) = (p[1].max(h[1]), p[3].min(h[3]));
            if h[1] > p[1] {
                next.push([p[0], p[1], p[2], h[1]]);
            }
            if h[3] < p[3] {
                next.push([p[0], h[3], p[2], p[3]]);
            }
            if h[0] > p[0] {
                next.push([p[0], y0, h[0], y1]);
            }
            if h[2] < p[2] {
                next.push([h[2], y0, p[2], y1]);
            }
        }
        if next.len() > MAX_FRAGMENTS {
            return None;
        }
        parts = next;
    }
    Some(parts)
}

fn fmt(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" || s.is_empty() { "0".into() } else { s.into() }
}

/// Rewrite partly marked rectangle fills in the content of the marked pages so the redactor only
/// removes what lies under a mark. Returns how many fills were split. Pages it cannot read safely
/// (a content stream shared with another page, undecodable data, ambiguous inline images) are
/// left as they are.
pub fn split_partial_rect_fills(cos: &mut CosDocument, marks: &[(usize, Vec<[f64; 4]>)]) -> usize {
    let pages = pdfcraft_model::pages(cos);
    // A content stream shared with another page would gain a hole there too.
    let mut use_count: std::collections::HashMap<ObjRef, usize> = Default::default();
    let contents_of = |dict: &pdfcraft_cos::Dict| -> Option<Vec<ObjRef>> {
        match dict.get(b"Contents")? {
            Object::Ref(r) => match &*cos.get(*r) {
                Object::Array(a) => a.iter().map(Object::as_ref).collect(),
                Object::Stream(_) => Some(vec![*r]),
                _ => None,
            },
            Object::Array(a) => a.iter().map(Object::as_ref).collect(),
            _ => None,
        }
    };
    let per_page: Vec<Option<Vec<ObjRef>>> = pages.iter().map(|p| contents_of(&p.dict)).collect();
    for refs in per_page.iter().flatten() {
        for r in refs {
            *use_count.entry(*r).or_default() += 1;
        }
    }
    let mut split = 0;
    for (pi, rects) in marks {
        let Some(Some(refs)) = per_page.get(*pi) else { continue };
        if refs.iter().any(|r| use_count.get(r).copied().unwrap_or(0) > 1) {
            continue;
        }
        let mut streams = Vec::with_capacity(refs.len());
        let mut data = Vec::with_capacity(refs.len());
        let mut budget = pdfcraft_cos::DecodeBudget::page();
        let mut readable = true;
        for r in refs {
            match &*cos.get(*r) {
                Object::Stream(s) => match budget.decode_strict(s) {
                    Ok(d) => {
                        streams.push(s.clone());
                        data.push(d);
                    }
                    Err(_) => readable = false,
                },
                _ => readable = false,
            }
        }
        if !readable {
            continue;
        }
        let pieces = Pieces::join(&data);
        let parsed = pieces.parse();
        if parsed.ambiguous_images > 0 {
            continue;
        }
        let mut edits = Vec::new();
        let mut ctm = Matrix::default();
        let mut stack = Vec::new();
        let mut path: Vec<usize> = Vec::new();
        let mut clips = false;
        for (i, op) in parsed.ops.iter().enumerate() {
            match op.op.as_slice() {
                b"q" => stack.push(ctm),
                b"Q" => ctm = stack.pop().unwrap_or_default(),
                b"cm" => {
                    if let Some(m) = op.nums::<6>() {
                        ctm = Matrix(m).then(&ctm);
                    }
                }
                b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" => path.push(i),
                b"W" | b"W*" => clips = true,
                b"f" | b"F" | b"f*" | b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*" | b"n" => {
                    let fill = matches!(op.op.as_slice(), b"f" | b"F" | b"f*");
                    let re = match path.as_slice() {
                        [only] => parsed.ops.get(*only).filter(|o| o.is("re")),
                        _ => None,
                    };
                    let [a, b, c, d, ..] = ctm.0;
                    let axis = b.abs() < 1e-9 && c.abs() < 1e-9 && a.abs() > 1e-9 && d.abs() > 1e-9;
                    if let Some(re) = re.filter(|_| fill && !clips && axis)
                        && let Some(edit) = split_rect(re, &op.op, &ctm, rects)
                    {
                        edits.push(edit);
                        split += 1;
                    }
                    path.clear();
                    clips = false;
                }
                _ => {}
            }
        }
        if edits.is_empty() {
            continue;
        }
        for ((r, old), new) in refs.iter().zip(streams).zip(pieces.splice(edits)) {
            let mut dict = old.dict.clone();
            dict.remove(b"Length");
            cos.set(*r, Stream::flate(dict, &new));
        }
    }
    split
}

/// The replacement for one `re` op whose fill a mark partly covers: one rectangle per fragment,
/// each filled on its own (`paint`) except the last, which the original paint operator still
/// ends. Separate paths matter: the redactor tests a path's whole bounding box against the marks.
fn split_rect(re: &pdfcraft_content::Op, paint: &[u8], ctm: &Matrix, rects: &[[f64; 4]]) -> Option<(std::ops::Range<usize>, Vec<u8>)> {
    let [x, y, w, h] = re.nums::<4>()?;
    let bbox = ctm.bbox([x.min(x + w), y.min(y + h), x.max(x + w), y.max(y + h)]);
    let hit: Vec<[f64; 4]> = rects.iter().copied().filter(|r| overlaps(*r, bbox, 0.0)).collect();
    if hit.is_empty() || hit.iter().any(|r| contains(*r, bbox, 0.01)) {
        return None;
    }
    let parts = subtract(bbox, &hit)?;
    if parts.is_empty() {
        return None;
    }
    let inv = ctm.invert()?;
    let mut out = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(paint);
            out.push(b'\n');
        }
        let l = inv.bbox(*p);
        out.extend(format!("{} {} {} {} re\n", fmt(l[0]), fmt(l[1]), fmt(l[2] - l[0]), fmt(l[3] - l[1])).as_bytes());
    }
    Some((re.span.clone(), out))
}

/// Trim text-selection quads to the core of their line before they become redaction marks.
///
/// Selections use the em box (one font size tall); the redactor measures glyphs by the font's
/// ascent/descent (about 1.15 em) and removes any glyph overlapping a mark by more than 1 pt. On
/// single-spaced text the next line's glyphs reach about 0.1 em into a full selection, so
/// deleting or redacting a word took the line below with it. The trimmed band still overlaps
/// every selected glyph by most of its height and only reaches a neighbouring line when line
/// spacing is under ~0.95 em. Quads are `[TL, TR, BL, BR]`; trimming runs along the quad's own
/// bottom→top edge, so rotated pages and text work too.
pub fn trim_text_quads(quads: Vec<[f64; 8]>) -> Vec<[f64; 8]> {
    let (lo, hi) = TEXT_MARK_BAND;
    quads
        .into_iter()
        .map(|q| {
            let lerp = |b: (f64, f64), t: (f64, f64), f: f64| (b.0 + (t.0 - b.0) * f, b.1 + (t.1 - b.1) * f);
            let (tl, tr, bl, br) = ((q[0], q[1]), (q[2], q[3]), (q[4], q[5]), (q[6], q[7]));
            let (ntl, nbl) = (lerp(bl, tl, hi), lerp(bl, tl, lo));
            let (ntr, nbr) = (lerp(br, tr, hi), lerp(br, tr, lo));
            let out = [ntl.0, ntl.1, ntr.0, ntr.1, nbl.0, nbl.1, nbr.0, nbr.1];
            if out.iter().all(|v| v.is_finite()) { out } else { q }
        })
        .collect()
}

/// Record `quads` as the box to paint for mark `obj`.
fn remember_box_quads(cos: &mut CosDocument, obj: ObjRef, quads: &[[f64; 8]]) -> Result<(), EditError> {
    let nums = quads.iter().flatten().map(|v| Object::Real(*v)).collect();
    cos.update_dict(obj, |d| d.set(BOX_QUADS_KEY.to_vec(), Object::Array(nums))).map_err(cos_err)
}

/// A box to paint after applying: page, fill colour, quads.
type BoxPaint = (usize, [f64; 3], Vec<[f64; 8]>);

/// For every mark being applied that carries full-height box quads and a fill: take the fill off
/// the mark (the redactor then removes without painting) and return what to paint instead.
fn take_box_quads(cos: &mut CosDocument, pages: Option<&[usize]>) -> Result<Vec<BoxPaint>, EditError> {
    let mut out = Vec::new();
    for m in pdfcraft_redact::marks(cos) {
        if pages.is_some_and(|p| !p.contains(&m.page)) {
            continue;
        }
        let Some(fill) = m.fill else { continue };
        let nums: Vec<f64> = match &*cos.get(m.obj) {
            Object::Dict(d) => match d.get(BOX_QUADS_KEY) {
                Some(Object::Array(a)) => a.iter().filter_map(Object::as_f64).collect(),
                _ => continue,
            },
            _ => continue,
        };
        let quads: Vec<[f64; 8]> = nums.as_chunks::<8>().0.to_vec();
        if quads.is_empty() || quads.iter().flatten().any(|v| !v.is_finite()) {
            continue;
        }
        cos.update_dict(m.obj, |d| {
            d.remove(b"IC");
        })
        .map_err(cos_err)?;
        out.push((m.page, fill, quads));
    }
    Ok(out)
}

/// Paint the boxes from [`take_box_quads`] on top of their pages.
fn paint_boxes(cos: &mut CosDocument, boxes: &[BoxPaint]) -> Result<(), EditError> {
    let mut pages: Vec<usize> = boxes.iter().map(|b| b.0).collect();
    pages.sort_unstable();
    pages.dedup();
    for page in pages {
        let mut c = String::from("q\n");
        for (_, fill, quads) in boxes.iter().filter(|b| b.0 == page) {
            let [r, g, b] = fill.map(|v| v.clamp(0.0, 1.0));
            c.push_str(&format!("{} {} {} rg\n", fmt(r), fmt(g), fmt(b)));
            for q in quads {
                // TL, TR, BR, BL: the quad's outline in order.
                c.push_str(&format!(
                    "{} {} m {} {} l {} {} l {} {} l h f\n",
                    fmt(q[0]),
                    fmt(q[1]),
                    fmt(q[2]),
                    fmt(q[3]),
                    fmt(q[6]),
                    fmt(q[7]),
                    fmt(q[4]),
                    fmt(q[5])
                ));
            }
        }
        c.push_str("Q\n");
        pdfcraft_edit::stamp(cos, page, "Redact", c.into_bytes())?;
    }
    Ok(())
}

/// A page rendered for the before/after comparison (straight RGBA).
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Render scale for the before/after comparison: about 400 px on the long side.
pub fn check_scale(info: &PageInfo) -> f32 {
    400.0 / info.width.max(info.height).max(1.0)
}

fn snapshot(renderer: &mut PageRenderer, page: usize, scale: f32) -> Option<Snapshot> {
    let out = renderer.render(RenderRequest { page, kind: RequestKind::Pixels, tile: None, scale, tag: 0, preview: false });
    if out.error.is_some() {
        return None;
    }
    let rgba = out.rgba.into_words().into_iter().flat_map(u32::to_ne_bytes).collect();
    Some(Snapshot { width: out.width, height: out.height, rgba })
}

/// Fraction of the page's pixels that changed outside the marks (each grown by a margin, since
/// a glyph the mark partly covers is removed whole).
pub fn lost_outside_marks(info: &PageInfo, scale: f32, rects: &[[f64; 4]], before: &Snapshot, after: &Snapshot) -> f64 {
    if before.width != after.width || before.height != after.height || before.rgba.len() != after.rgba.len() {
        return 1.0;
    }
    let (w, h) = (before.width as usize, before.height as usize);
    let px = |v: f32| (v * scale).max(0.0) as usize;
    let mut excluded = vec![false; w.saturating_mul(h)];
    for r in rects {
        let m = (r[3] - r[1]).abs().max(2.0);
        let (x0, y0, x1, y1) = ((r[0] - m) as f32, (r[1] - m) as f32, (r[2] + m) as f32, (r[3] + m) as f32);
        let corners = [info.user_to_view(x0, y0), info.user_to_view(x1, y1), info.user_to_view(x0, y1), info.user_to_view(x1, y0)];
        let vx0 = corners.iter().map(|c| c[0]).fold(f32::INFINITY, f32::min);
        let vx1 = corners.iter().map(|c| c[0]).fold(f32::NEG_INFINITY, f32::max);
        let vy0 = corners.iter().map(|c| c[1]).fold(f32::INFINITY, f32::min);
        let vy1 = corners.iter().map(|c| c[1]).fold(f32::NEG_INFINITY, f32::max);
        for yy in px(vy0).min(h)..(px(vy1) + 1).min(h) {
            for xx in px(vx0).min(w)..(px(vx1) + 1).min(w) {
                if let Some(e) = excluded.get_mut(yy * w + xx) {
                    *e = true;
                }
            }
        }
    }
    let changed = before
        .rgba
        .chunks_exact(4)
        .zip(after.rgba.chunks_exact(4))
        .zip(&excluded)
        .filter(|((a, b), ex)| !**ex && a.iter().zip(b.iter()).any(|(p, q)| p.abs_diff(*q) > 48))
        .count();
    changed as f64 / (w * h).max(1) as f64
}

/// The guarded apply on the staged `cos`: see the module docs. `before` holds the marked pages
/// as they looked (page, scale, render, info).
fn redact_checked(
    cos: &mut CosDocument,
    pages: Option<&[usize]>,
    before: &[(usize, f32, Snapshot, PageInfo)],
    config: &RenderConfig,
) -> Result<(), EditError> {
    let marks = marks_by_page(cos, pages);
    split_partial_rect_fills(cos, &marks);
    let boxes = take_box_quads(cos, pages)?;
    let report = pdfcraft_redact::apply(cos, pages)?;
    paint_boxes(cos, &boxes)?;
    if report.images_removed > 0 || report.forms_removed > 0 {
        return Err(EditError::Invalid(
            "This page contains an image that cannot be redacted without removing it entirely; nothing was changed".into(),
        ));
    }
    let bytes = Arc::new(pdfcraft_cos::write_incremental(cos, &pdfcraft_cos::SaveOptions::default()).map_err(cos_err)?);
    let mut renderer = PageRenderer::new(bytes, config.clone());
    for (page, rects) in &marks {
        let Some((_, scale, snap, info)) = before.iter().find(|b| b.0 == *page) else { continue };
        let lost = match snapshot(&mut renderer, *page, *scale) {
            Some(after) => lost_outside_marks(info, *scale, rects, snap, &after) > LOST_LIMIT,
            None => true,
        };
        if lost {
            return Err(EditError::Invalid(format!(
                "Redacting page {} would also erase content outside the marked area (a shape or drawing the mark overlaps); nothing was changed",
                page + 1
            )));
        }
    }
    Ok(())
}

impl Session {
    /// The marked pages of document `id` as they look now, and the settings to draw them with.
    fn before_pages(&self, id: DocId, pages: &[usize]) -> Result<(Vec<(usize, f32, Snapshot, PageInfo)>, RenderConfig), EditError> {
        let doc = self.get(id).ok_or(EditError::NoDocument)?;
        let config = RenderConfig { password: doc.password.as_deref().map(Arc::from), ..Default::default() };
        let mut renderer = PageRenderer::new(doc.bytes.clone(), config.clone());
        let mut out = Vec::new();
        for &page in pages {
            let info = doc.info.pages.get(page).cloned().ok_or_else(|| EditError::Invalid(format!("there is no page {}", page + 1)))?;
            let scale = check_scale(&info);
            let snap = snapshot(&mut renderer, page, scale).ok_or_else(|| EditError::Invalid(format!("page {} could not be drawn", page + 1)))?;
            out.push((page, scale, snap, info));
        }
        Ok((out, config))
    }

    /// The redaction annotation for text-selection `quads` on `page`, and the full-height box
    /// quads to remember on it: text pages trim the mark to the core of its line, box-only marks
    /// keep the selection's height to paint; pages with a scan image keep the full box (the image
    /// is cleared exactly to the mark, and a trimmed one would leave ink behind).
    fn redaction_mark(
        &self,
        id: DocId,
        page: usize,
        quads: Vec<[f64; 8]>,
        overlay: String,
        author: String,
    ) -> Result<(pdfcraft_annot::NewAnnotation, Option<Vec<[f64; 8]>>), EditError> {
        if quads.is_empty() {
            return Err(EditError::Invalid("a redaction mark needs at least one quad".into()));
        }
        let cos = self.cos(id)?;
        if page >= pdfcraft_model::pages(cos).len() {
            return Err(EditError::Invalid(format!("there is no page {}", page + 1)));
        }
        let has_image = !pdfcraft_create::extract_images(cos, &[page], 300).images.is_empty();
        let full = (!has_image && overlay.is_empty()).then(|| quads.clone());
        let quads = if has_image { quads } else { trim_text_quads(quads) };
        let shape = pdfcraft_annot::Shape::Redact { quads, overlay, look: pdfcraft_annot::OverlayLook::default() };
        let new = pdfcraft_annot::NewAnnotation { page, style: pdfcraft_annot::Style::default_for(&shape), shape, contents: String::new(), author };
        Ok((new, full))
    }

    fn mark_id(&self, id: DocId) -> String {
        format!("PCRedact-{}-{}", id.0, self.get(id).map_or(0, |d| d.generation))
    }

    fn add_mark(cos: &mut CosDocument, new: &pdfcraft_annot::NewAnnotation, full: Option<&[[f64; 8]]>, mark_id: String) -> Result<(), EditError> {
        let before: std::collections::HashSet<_> = pdfcraft_redact::marks(cos).into_iter().map(|m| m.obj).collect();
        pdfcraft_annot::add_annotation(cos, new, &pdfcraft_annot::Meta { date: None, id: mark_id })?;
        if let Some(full) = full {
            for m in pdfcraft_redact::marks(cos) {
                if !before.contains(&m.obj) {
                    remember_box_quads(cos, m.obj, full)?;
                }
            }
        }
        Ok(())
    }

    /// Mark text for redaction: `quads` are text-selection quads in user space (`[TL, TR, BL,
    /// BR]`). Nothing is removed until [`Session::apply_redactions_guarded`]. One step.
    pub fn add_redaction_mark(&mut self, id: DocId, page: usize, quads: Vec<[f64; 8]>, overlay: String, author: String) -> Result<(), EditError> {
        let (new, full) = self.redaction_mark(id, page, quads, overlay, author)?;
        let mark_id = self.mark_id(id);
        self.apply_with(id, Label::of("Mark for redaction"), move |cos| Self::add_mark(cos, &new, full.as_deref(), mark_id))
    }

    /// Apply the redaction marks on `pages` (all when `None`) with the safeguards in the module
    /// docs, as one step. A refused redaction leaves the document and its history unchanged.
    pub fn apply_redactions_guarded(&mut self, id: DocId, pages: Option<&[usize]>) -> Result<(), EditError> {
        let marked: Vec<usize> = marks_by_page(self.cos(id)?, pages).into_iter().map(|(p, _)| p).collect();
        if marked.is_empty() {
            return Err(EditError::Invalid("there are no redaction marks to apply".into()));
        }
        let (before, config) = self.before_pages(id, &marked)?;
        let pages = pages.map(<[usize]>::to_vec);
        self.apply_with(id, Label::of("Apply redactions"), move |cos| redact_checked(cos, pages.as_deref(), &before, &config))
    }

    /// Delete the text under `quads` (as for [`Session::add_redaction_mark`]) at once, leaving
    /// no box behind: a box-less redaction through the same safeguards, as one step. Refused
    /// while the page has pending redaction marks, since applying would burn those in too.
    pub fn delete_text(&mut self, id: DocId, page: usize, quads: Vec<[f64; 8]>) -> Result<(), EditError> {
        if pdfcraft_redact::marks(self.cos(id)?).iter().any(|m| m.page == page) {
            return Err(EditError::Invalid("This page has redaction marks; apply or clear them before deleting text".into()));
        }
        let (new, full) = self.redaction_mark(id, page, quads, String::new(), String::new())?;
        let (before, config) = self.before_pages(id, &[page])?;
        let mark_id = self.mark_id(id);
        self.apply_with(id, Label::of("Delete text"), move |cos| {
            Self::add_mark(cos, &new, full.as_deref(), mark_id)?;
            for m in pdfcraft_redact::marks(cos).into_iter().filter(|m| m.page == page) {
                cos.update_dict(m.obj, |d| {
                    d.remove(b"IC");
                })
                .map_err(cos_err)?;
            }
            redact_checked(cos, Some(&[page]), &before, &config)
        })
    }
}
