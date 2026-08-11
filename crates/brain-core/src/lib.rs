//! Shared domain types for Brain Dock.
//!
//! This crate holds the vocabulary every other crate speaks: identifiers,
//! documents, sections, actions, and answers. It has no I/O and no async, so
//! it stays cheap to depend on and easy to test.

pub mod ids;

pub use ids::{ActionId, DocumentId, SectionId};
