//! The right panel's **Search** tab: everything about one feature, in one folder.

pub mod engine;
pub mod exclude;
pub mod glob;
pub mod in_file;
pub mod state;

#[cfg(test)]
pub(crate) mod fixtures;
pub(crate) mod render;
