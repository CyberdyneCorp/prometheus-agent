//! Localized UI text seen from this crate: wording shared by several
//! surfaces (background task notices) is rendered through a [`Catalog`]
//! that `pmt_i18n::Strings` implements, so this leaf crate needs no
//! catalog of its own and the text follows the profile's language.

use std::fmt::Display;

/// UI text by dotted catalog key with named `{placeholders}`.
pub trait Catalog {
    /// The text of `key` with `args` substituted.
    fn text(&self, key: &str, args: &[(&str, &dyn Display)]) -> String;
}
