//! Guarded redaction (`Session::apply_redactions_guarded`): a word redaction keeps the page
//! background a mark only partly covers, and refuses rather than erase a shape beyond the mark.

use std::sync::Arc;

use pdfcraft_engine::{DocId, Session};
use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest, RequestKind};

/// A one-page Letter PDF whose content is `content` (Helvetica as /F1).
fn pdf_with_content(content: &str) -> Vec<u8> {
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
        format!("<< /Length {} >>\nstream\n{}\nendstream", content.len() + 1, content),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{}\nendobj\n", i + 1, o).as_bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn open(content: &str) -> (Session, DocId) {
    let mut s = Session::new();
    let id = s.open("doc.pdf", None, Arc::new(pdf_with_content(content)), None).expect("opens");
    (s, id)
}

/// "SECRET" in `(Public SECRET)` at 24 pt from x 72: "Public " is 72 pt wide.
fn secret_quad() -> Vec<[f64; 8]> {
    let (x0, x1, y0, y1) = (146.0, 240.0, 695.2, 719.2);
    vec![[x0, y1, x1, y1, x0, y0, x1, y0]]
}

fn render(s: &Session, id: DocId) -> Vec<u32> {
    let doc = s.get(id).expect("open");
    let mut r = PageRenderer::new(doc.bytes.clone(), RenderConfig::default());
    r.render(RenderRequest { page: 0, kind: RequestKind::Pixels, tile: None, scale: 0.5, tag: 0, preview: false }).rgba.into_words()
}

#[test]
fn a_word_redaction_keeps_the_page_background() {
    let (mut s, id) = open("0.9 0.8 0.3 rg 0 0 612 792 re f 0 g BT /F1 24 Tf 72 700 Td (Public SECRET) Tj ET");
    let before = render(&s, id);
    s.add_redaction_mark(id, 0, secret_quad(), String::new(), String::new()).expect("marked");
    s.apply_redactions_guarded(id, None).expect("applied");
    let after = render(&s, id);
    let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
    assert!((changed as f64) < 0.02 * before.len() as f64, "only the word changes: {changed} of {}", before.len());
    s.undo(id).expect("one step");
    assert_eq!(render(&s, id).len(), before.len());
}

#[test]
fn a_shape_beyond_the_mark_refuses_the_redaction() {
    let (mut s, id) = open("0.2 0.5 0.8 rg 0 0 m 612 0 l 612 792 l 0 792 l h f 0 g BT /F1 24 Tf 72 700 Td (Public SECRET) Tj ET");
    s.add_redaction_mark(id, 0, secret_quad(), String::new(), String::new()).expect("marked");
    let label = s.get(id).and_then(|d| d.undo_label().map(|l| l.as_str().to_owned()));
    let err = s.apply_redactions_guarded(id, None).expect_err("refused").to_string();
    assert!(err.contains("outside the marked area"), "{err}");
    assert_eq!(s.get(id).and_then(|d| d.undo_label().map(|l| l.as_str().to_owned())), label, "nothing entered the history");
}

#[test]
fn subtract_leaves_only_the_surroundings() {
    let parts = pdfcraft_engine::redact_guard::subtract([0.0, 0.0, 10.0, 10.0], &[[4.0, 4.0, 6.0, 6.0]]).expect("parts");
    let area: f64 = parts.iter().map(|p| (p[2] - p[0]) * (p[3] - p[1])).sum();
    assert!((area - 96.0).abs() < 0.1, "{area}");
}
