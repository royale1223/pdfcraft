//! LitePDF-only additions to pdfcraft-export. Wrappers that expose
//! crate-private helpers through a stable public surface, so the mobile app
//! can detect tables and reading order without widening upstream visibility.

use crate::layout::{Block, Table};

/// Detect tables in a page's blocks from their text layout, as the engine's
/// own export does. Returns the tables and, per block, whether a table
/// consumed it (so the caller can drop it from the paragraph flow).
pub fn detect_tables(blocks: &[Block], rules: &[[f64; 4]]) -> (Vec<Table>, Vec<bool>) {
    crate::layout::tables(blocks, rules)
}
