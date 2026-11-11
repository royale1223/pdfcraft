//! LitePDF-only additions to pdfcraft-engine. Everything the mobile app needs
//! that upstream has not (yet) merged lives here, so the rest of the crate
//! stays an unmodified mirror of `storytold/pdfcraft` and upstream merges
//! touch at most this `mod` line in `lib.rs`.
//!
//! One exception: [`crate::Session::apply_with`] lives inline in
//! `session/editing.rs` because it uses `pub(super)` helpers that only the
//! session submodule can see. Everything else — present and future — can be
//! built on top of `apply_with` and the public engine surface, and lives
//! here.

pub mod redact_guard;
