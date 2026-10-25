//! Fonts compiled into the binary.

use ab_glyph::FontRef;

pub static HELVETICA_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Helvetica-Regular.otf");
pub static HELVETICA_BOLD: &[u8] = include_bytes!("../../../assets/fonts/Helvetica-Bold.otf");

pub fn regular() -> FontRef<'static> {
    FontRef::try_from_slice(HELVETICA_REGULAR).expect("embedded font is valid")
}

pub fn bold() -> FontRef<'static> {
    FontRef::try_from_slice(HELVETICA_BOLD).expect("embedded font is valid")
}

pub use ab_glyph::FontRef as Font;
