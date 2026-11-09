//! `Session::cos` and `Session::apply_with`: an embedder's own change as one undoable step.

use std::sync::Arc;

use pdfcraft_engine::{EditError, Label, Session};

fn blank_session() -> (Session, pdfcraft_engine::DocId) {
    let cos = pdfcraft_create::blank(200.0, 300.0, 2).expect("blank document");
    let bytes = pdfcraft_cos::write_full(&cos, &pdfcraft_cos::SaveOptions::default()).expect("written");
    let mut s = Session::new();
    let id = s.open("blank.pdf", None, Arc::new(bytes), None).expect("opens");
    (s, id)
}

fn title(s: &Session, id: pdfcraft_engine::DocId) -> Option<String> {
    pdfcraft_organize::info(s.cos(id).expect("editable"), "Title")
}

#[test]
fn apply_with_is_undoable_and_redoable() {
    let (mut s, id) = blank_session();
    assert_eq!(title(&s, id), None);

    s.apply_with(id, Label::of("Set title"), |cos| pdfcraft_organize::set_info(cos, "Title", "Hello").map_err(|e| EditError::Invalid(e.to_string())))
        .expect("applies");
    assert_eq!(title(&s, id).as_deref(), Some("Hello"));
    let doc = s.get(id).expect("open");
    assert!(doc.dirty);
    assert_eq!(doc.undo_label().map(|l| l.as_str().to_owned()).as_deref(), Some("Set title"));

    s.undo(id).expect("undoes");
    assert_eq!(title(&s, id), None);
    s.redo(id).expect("redoes");
    assert_eq!(title(&s, id).as_deref(), Some("Hello"));
}

#[test]
fn apply_with_leaves_the_document_alone_on_failure() {
    let (mut s, id) = blank_session();
    let failed = s.apply_with(id, Label::of("Set title"), |cos| {
        pdfcraft_organize::set_info(cos, "Title", "Half done").map_err(|e| EditError::Invalid(e.to_string()))?;
        Err(EditError::Invalid("refused".into()))
    });
    assert!(failed.is_err());
    assert_eq!(title(&s, id), None, "the failed change ran on a copy");
    assert!(s.get(id).expect("open").can_undo().is_none());

    #[allow(clippy::panic)]
    let panicked = s.apply_with(id, Label::of("Set title"), |_| panic!("boom"));
    assert!(matches!(panicked, Err(EditError::Invalid(_))));
    assert!(s.get(id).expect("open").can_undo().is_none());
}

#[test]
fn apply_with_refreshes_the_view() {
    let (mut s, id) = blank_session();
    s.apply_with(id, Label::of("Delete page"), |cos| pdfcraft_organize::delete_pages(cos, &[1]).map_err(|e| EditError::Invalid(e.to_string())))
        .expect("applies");
    assert_eq!(s.get(id).expect("open").info.pages.len(), 1);
    s.undo(id).expect("undoes");
    assert_eq!(s.get(id).expect("open").info.pages.len(), 2);
}
