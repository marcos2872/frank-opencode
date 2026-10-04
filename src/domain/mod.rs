//! Domain: pure types and rules. No tokio, axum, rusqlite here.

pub mod alias;
pub mod catalog;
pub mod model;
pub mod protocol;

pub use alias::{
    auto_aliases_for, evade_desktop_blocklist, shield_cli_family_match, slugify,
    strip_window_suffix, window_suffix, AliasEntry, AliasOptions,
};
pub use catalog::{
    CatalogEntry, CatalogLimit, CatalogSettings, ModelVariant, ModelVariantSettings, ThinkingConfig,
};
pub use model::ModelRef;
pub use protocol::{is_known_package, protocol_for, protocol_for_entry, Protocol};
