//! Fork additions inside pdfcraft-edit. Background colour and underline for
//! added text items live here ([`decoration`]), so a future upstream merge
//! touches at most this `pub mod` line and the hooks in
//! `crates/edit/src/added/store.rs`.

pub mod decoration;
