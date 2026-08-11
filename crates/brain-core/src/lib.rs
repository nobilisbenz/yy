//! Shared domain types for Brain Dock.
//!
//! This crate holds the vocabulary every other crate speaks: identifiers,
//! documents, sections, actions, and answers. It has no I/O and no async, so
//! it stays cheap to depend on and easy to test.

pub mod config;
pub mod ids;

pub use config::{Config, ConfigError, Source};
pub use ids::{ActionId, DocumentId, SectionId};
