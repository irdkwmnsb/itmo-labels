//! Label printing without the OS print stack:
//! `label` (what to print) → encoder such as `tspl` (printer language) → `transport` (how bytes get there).
//! `render` turns text into label bitmaps on the host.

pub mod fonts;
pub mod label;
pub mod render;
pub mod transport;
pub mod tspl;
