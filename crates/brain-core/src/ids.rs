//! Stable identifiers.
//!
//! These are newtypes over SQLite rowids rather than bare `i64` so that a
//! `SectionId` can never be passed where a `DocumentId` is expected. Index
//! updates must preserve them (spec §10) — relationships hang off these.

use std::fmt;

macro_rules! rowid {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
            serde::Serialize, serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl $name {
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<i64> for $name {
            fn from(value: i64) -> Self {
                Self(value)
            }
        }
    };
}

rowid!(
    /// A single indexed file.
    DocumentId
);
rowid!(
    /// A heading-delimited slice of a document — the unit of retrieval.
    SectionId
);
rowid!(
    /// A trusted, executable jump target parsed from source metadata.
    ActionId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_transparent_over_the_wire() {
        let json = serde_json::to_string(&SectionId(42)).unwrap();
        assert_eq!(json, "42");
        assert_eq!(
            serde_json::from_str::<SectionId>("42").unwrap(),
            SectionId(42)
        );
    }

    #[test]
    fn ids_display_as_bare_numbers() {
        assert_eq!(DocumentId(7).to_string(), "7");
    }
}
