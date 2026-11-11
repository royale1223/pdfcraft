//! A font file's family and style, and the standard styles of a variable font.
//!
//! PDF has no variable fonts: an embedded program shows one fixed design, and every viewer draws
//! a variable font's default outlines whatever weight was chosen. [`standard_styles`] turns a
//! variable font into static fonts for the named instances among Regular, Bold, Italic and Bold
//! Italic (allsorts' instancer; TrueType `gvar` and CFF2 outlines), so each can be embedded and
//! prints as named. [`style_of`] reads a static font's family and style, so an app can group a
//! family's files (`Inter-Regular.ttf`, `Inter-Bold.ttf`) under one name.

use allsorts::binary::read::ReadScope;
use allsorts::tables::variable_fonts::fvar::FvarTable;
use allsorts::tables::{FontTableProvider, NameTable, OpenTypeFont};
use allsorts::tag;
use skrifa::raw::TableProvider;
use skrifa::{FontRef, MetadataProvider, attribute::Style, string::StringId};

/// A font file's family and style, from the program itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontStyle {
    /// The typographic family (`name` ID 16, else ID 1): "Inter", not "Inter Bold".
    pub family: String,
    pub bold: bool,
    pub italic: bool,
    /// The font has variation axes (`fvar`).
    pub variable: bool,
}

/// One static font cut from a variable font.
#[derive(Clone, Debug)]
pub struct StaticStyle {
    pub bold: bool,
    pub italic: bool,
    pub bytes: Vec<u8>,
}

/// At or above this `wght` / OS/2 weight a style counts as bold.
const BOLD_WEIGHT: f32 = 600.0;

fn english(f: &FontRef<'_>, id: StringId) -> Option<String> {
    f.localized_strings(id)
        .find(|s| s.language() == Some("en"))
        .or_else(|| f.localized_strings(id).next())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty() && s.len() <= 128)
}

/// The family and style of the font in `bytes`.
pub fn style_of(bytes: &[u8]) -> Result<FontStyle, String> {
    let f = FontRef::new(bytes).map_err(|_| "select a standalone TrueType/OpenType font (not a font collection)".to_string())?;
    let family =
        english(&f, StringId::TYPOGRAPHIC_FAMILY_NAME).or_else(|| english(&f, StringId::FAMILY_NAME)).ok_or("the font has no family name")?;
    let a = f.attributes();
    Ok(FontStyle { family, bold: a.weight.value() >= BOLD_WEIGHT, italic: !matches!(a.style, Style::Normal), variable: f.fvar().is_ok() })
}

/// Which standard style a named instance's subfamily name is, if any.
fn standard(subfamily: &str) -> Option<(bool, bool)> {
    match subfamily.to_ascii_lowercase().replace([' ', '-', '_'], "").as_str() {
        "regular" | "roman" | "normal" | "book" => Some((false, false)),
        "bold" => Some((true, false)),
        "italic" | "oblique" | "regularitalic" => Some((false, true)),
        "bolditalic" | "boldoblique" => Some((true, true)),
        _ => None,
    }
}

/// Static fonts for the standard styles (Regular, Bold, Italic, Bold Italic) a variable font
/// names, with its family. A variable font without a "Regular" instance gets its default
/// design as Regular. Each result passes [`crate::embedding::validate`]. Fails for a static font,
/// or when allsorts cannot instance the outlines.
pub fn standard_styles(bytes: &[u8]) -> Result<(String, Vec<StaticStyle>), String> {
    let style = style_of(bytes)?;
    if !style.variable {
        return Err("this font is not variable".into());
    }
    let data = ReadScope::new(bytes).read::<OpenTypeFont<'_>>().map_err(|e| e.to_string())?;
    let provider = data.table_provider(0).map_err(|e| e.to_string())?;
    let fvar_data = provider.read_table_data(tag::FVAR).map_err(|e| e.to_string())?;
    let fvar = ReadScope::new(&fvar_data).read::<FvarTable<'_>>().map_err(|e| e.to_string())?;
    let name_data = provider.read_table_data(tag::NAME).map_err(|e| e.to_string())?;
    let name = ReadScope::new(&name_data).read::<NameTable<'_>>().map_err(|e| e.to_string())?;

    let mut wanted: Vec<((bool, bool), Vec<allsorts::tables::Fixed>)> = Vec::new();
    for record in fvar.instances() {
        let record = record.map_err(|e| e.to_string())?;
        let Some(sub) = name.string_for_id(record.subfamily_name_id) else { continue };
        let Some(key) = standard(&sub) else { continue };
        if wanted.iter().any(|(k, _)| *k == key) {
            continue;
        }
        wanted.push((key, record.coordinates.iter().collect()));
    }
    if !wanted.iter().any(|(k, _)| *k == (false, false)) {
        wanted.insert(0, ((false, false), fvar.axes().map(|a| a.default_value).collect()));
    }

    let mut out = Vec::new();
    for ((bold, italic), coords) in wanted {
        let (instance, _) =
            allsorts::variations::instance(&provider, &coords).map_err(|e| format!("cannot make a static style of this font: {e}"))?;
        crate::embedding::validate(&instance)?;
        out.push(StaticStyle { bold, italic, bytes: instance });
    }
    Ok((style.family, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> Vec<u8> {
        std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts").join(name)).unwrap()
    }

    #[test]
    fn a_static_font_reports_its_family_and_style() {
        let s = style_of(&asset("JetBrainsMono-Regular.ttf")).unwrap();
        assert_eq!(s, FontStyle { family: "JetBrains Mono".into(), bold: false, italic: false, variable: false });
        let s = style_of(&asset("Inter-SemiBold.ttf")).unwrap();
        assert_eq!(s.family, "Inter");
        assert!(s.bold, "semibold counts as bold");
    }

    #[test]
    fn a_variable_font_gives_static_regular_and_bold() {
        let bytes = asset("DancingScript.ttf");
        assert!(style_of(&bytes).unwrap().variable);
        let (family, styles) = standard_styles(&bytes).unwrap();
        assert_eq!(family, "Dancing Script");
        let keys: Vec<_> = styles.iter().map(|s| (s.bold, s.italic)).collect();
        assert!(keys.contains(&(false, false)) && keys.contains(&(true, false)), "{keys:?}");
        for s in &styles {
            let st = style_of(&s.bytes).unwrap();
            assert!(!st.variable, "static");
            assert_eq!(st.bold, s.bold, "the bold instance is heavier");
        }
        assert!(standard_styles(&asset("JetBrainsMono-Regular.ttf")).is_err(), "static fonts are not instanced");
    }
}
