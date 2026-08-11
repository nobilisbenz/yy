//! FTS5 query construction — re-exported from `yalive`.
//!
//! This module used to hold the implementation, on the reasoning in `09-decisions.md` §0b
//! that the expression language is `yy`'s problem while the schema is `yalive`'s. That
//! split does not survive contact: what counts as a *term* is decided by the tokenizer
//! (`tokenchars '_-.'`), which is part of the schema, and `yalive`'s own TUI search needs
//! the same escaping. Two copies would be two things to keep agreeing with one tokenizer.
//!
//! So the implementation moved to [`yalive::search`], where the tokenizer is declared, and
//! `yalive`'s TUI stopped being able to crash on a query containing an apostrophe as a side
//! effect. This module stays as the name `yy` code refers to it by.
//!
//! The authoritative escaping test — every hostile query through a real FTS5 parser — lives
//! with the implementation, in `yalive`'s `db::tests::no_query_can_make_the_search_index_raise`.

pub use yalive::search::{Bm25Weights, Mode, expression, expression_with};
