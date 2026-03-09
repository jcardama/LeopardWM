//! LeopardWM Core Layout Engine
//!
//! Platform-agnostic scrollable tiling layout engine inspired by Niri.
//!
//! This crate implements the "infinite horizontal strip" paradigm where:
//! - Windows are arranged in columns on an infinite horizontal strip
//! - The monitor acts as a viewport/camera sliding over this strip
//! - New windows append without resizing existing ones

pub mod types;
pub mod animation;
pub mod column;
pub mod workspace;
#[cfg(test)]
mod tests;

// Re-export public API so downstream crates can `use leopardwm_core_layout::*`
pub use types::*;
pub use animation::*;
pub use column::*;
pub use workspace::*;
