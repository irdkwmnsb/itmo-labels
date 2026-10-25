//! Label printing without the OS print stack:
//! `label` (what to print) → encoder such as `tspl` (printer language) → `transport` (how bytes get there).
//! `render` turns text into label bitmaps on the host.

pub mod fonts;
pub mod label;
pub mod render;
// USB needs an OS; the web build renders and encodes only.
#[cfg(not(target_arch = "wasm32"))]
pub mod transport;
pub mod tspl;
