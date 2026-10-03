//! Domain: pure types and rules. No tokio, axum, rusqlite here.

pub mod alias;
pub mod desktop;
pub mod error;
pub mod model;
pub mod protocol;
pub mod slug;

pub use alias::{auto_alias, auto_aliases_for, AliasEntry};
pub use desktop::evade_desktop_blocklist;
pub use error::GatewayError;
pub use model::{
    CatalogEntry, CatalogLimit, CatalogSettings, ModelRef, ModelVariant, ModelVariantSettings,
    ThinkingConfig,
};
pub use protocol::{is_known_package, protocol_for, protocol_for_entry, Protocol};
pub use slug::{slugify, strip_window_suffix, window_suffix};
