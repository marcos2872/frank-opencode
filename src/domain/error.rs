//! Typed gateway errors.
//!
//! Internal `Result` plumbing with a single mapping to the Anthropic wire
//! shape. Handlers never format these into ad-hoc strings: `status_code` +
//! `error_type` + `Display` reproduce exactly the responses the previous
//! `Result<_, String>` code built, so the wire is unchanged.
//!
//! The `Catalog*` variants never reach HTTP directly (they land in
//! `last_error` for `/health` and in `anyhow` for `--refresh`); their
//! status/type exist only so the mapping stays total.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    CatalogSpawn { bin: String, source: String },
    CatalogFailed { stderr: String },
    CatalogJson { source: String },
    CatalogShape { source: String },
    CatalogTask { source: String },
    CatalogTimeout,
    UnknownModel { name: String },
    NoVariants { model: String, requested: String },
    UnknownVariant {
        model: String,
        requested: String,
        available: Vec<String>,
    },
    MissingModel,
    MissingMessages,
    NoCredential { provider: String },
    NoBaseUrl { provider: String },
    UpstreamUnreachable { source: String },
    InvalidUpstreamJson { source: String },
    VariantNotRepresentable { detail: String },
}

impl GatewayError {
    /// HTTP status for the Anthropic wire shape.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::CatalogSpawn { .. }
            | Self::CatalogFailed { .. }
            | Self::CatalogJson { .. }
            | Self::CatalogShape { .. }
            | Self::CatalogTask { .. }
            | Self::CatalogTimeout
            | Self::NoBaseUrl { .. }
            | Self::UpstreamUnreachable { .. }
            | Self::InvalidUpstreamJson { .. } => 502,
            Self::UnknownModel { .. } | Self::NoVariants { .. } | Self::UnknownVariant { .. } => {
                404
            }
            Self::MissingModel | Self::MissingMessages | Self::VariantNotRepresentable { .. } => {
                400
            }
            Self::NoCredential { .. } => 401,
        }
    }

    /// Anthropic error `type` for the wire shape.
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::UnknownModel { .. } | Self::NoVariants { .. } | Self::UnknownVariant { .. } => {
                "not_found_error"
            }
            Self::NoCredential { .. } => "authentication_error",
            Self::MissingModel | Self::MissingMessages | Self::VariantNotRepresentable { .. } => {
                "invalid_request_error"
            }
            _ => "api_error",
        }
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CatalogSpawn { bin, source } => {
                write!(f, "failed to run `{bin} api get /api/model`: {source}")
            }
            Self::CatalogFailed { stderr } => write!(f, "opencode api failed: {stderr}"),
            Self::CatalogJson { source } => write!(f, "invalid catalog JSON: {source}"),
            Self::CatalogShape { source } => write!(f, "catalog shape: {source}"),
            Self::CatalogTask { source } => write!(f, "catalog task failed: {source}"),
            Self::CatalogTimeout => write!(f, "catalog fetch timed out after 20s"),
            Self::UnknownModel { name } => {
                write!(f, "unknown model '{name}' (see GET /v1/models)")
            }
            Self::NoVariants { model, requested } => {
                write!(f, "model '{model}' has no variants; requested '{requested}'")
            }
            Self::UnknownVariant {
                model,
                requested,
                available,
            } => write!(
                f,
                "model '{model}' has no variant '{requested}' (available: {})",
                available.join(", ")
            ),
            Self::MissingModel => write!(f, "missing model (and no default_model configured)"),
            Self::MissingMessages => write!(f, "missing messages"),
            Self::NoCredential { provider } => write!(
                f,
                "no stored credential for '{provider}' (run `opencode auth login`)"
            ),
            Self::NoBaseUrl { provider } => write!(f, "provider '{provider}' has no baseURL"),
            Self::UpstreamUnreachable { source } => write!(f, "upstream unreachable: {source}"),
            Self::InvalidUpstreamJson { source } => write!(f, "invalid upstream JSON: {source}"),
            Self::VariantNotRepresentable { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for GatewayError {}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// The wire contract: every variant maps to the exact status/type/message
    /// the `String`-based code produced. A failure here is a wire change.
    #[test]
    fn wire_mapping_table() {
        let cases: Vec<(GatewayError, u16, &str, &str)> = vec![
            (
                GatewayError::CatalogSpawn {
                    bin: "opencode".into(),
                    source: "boom".into(),
                },
                502,
                "api_error",
                "failed to run `opencode api get /api/model`: boom",
            ),
            (
                GatewayError::CatalogFailed {
                    stderr: "nope".into(),
                },
                502,
                "api_error",
                "opencode api failed: nope",
            ),
            (
                GatewayError::CatalogJson {
                    source: "eof".into(),
                },
                502,
                "api_error",
                "invalid catalog JSON: eof",
            ),
            (
                GatewayError::CatalogShape {
                    source: "eof".into(),
                },
                502,
                "api_error",
                "catalog shape: eof",
            ),
            (
                GatewayError::CatalogTask {
                    source: "cancelled".into(),
                },
                502,
                "api_error",
                "catalog task failed: cancelled",
            ),
            (
                GatewayError::CatalogTimeout,
                502,
                "api_error",
                "catalog fetch timed out after 20s",
            ),
            (
                GatewayError::UnknownModel { name: "x".into() },
                404,
                "not_found_error",
                "unknown model 'x' (see GET /v1/models)",
            ),
            (
                GatewayError::NoVariants {
                    model: "p/m".into(),
                    requested: "high".into(),
                },
                404,
                "not_found_error",
                "model 'p/m' has no variants; requested 'high'",
            ),
            (
                GatewayError::UnknownVariant {
                    model: "p/m".into(),
                    requested: "ghost".into(),
                    available: vec!["low".into(), "high".into()],
                },
                404,
                "not_found_error",
                "model 'p/m' has no variant 'ghost' (available: low, high)",
            ),
            (
                GatewayError::MissingModel,
                400,
                "invalid_request_error",
                "missing model (and no default_model configured)",
            ),
            (
                GatewayError::MissingMessages,
                400,
                "invalid_request_error",
                "missing messages",
            ),
            (
                GatewayError::NoCredential {
                    provider: "p".into(),
                },
                401,
                "authentication_error",
                "no stored credential for 'p' (run `opencode auth login`)",
            ),
            (
                GatewayError::NoBaseUrl {
                    provider: "p".into(),
                },
                502,
                "api_error",
                "provider 'p' has no baseURL",
            ),
            (
                GatewayError::UpstreamUnreachable {
                    source: "dns".into(),
                },
                502,
                "api_error",
                "upstream unreachable: dns",
            ),
            (
                GatewayError::InvalidUpstreamJson {
                    source: "eof".into(),
                },
                502,
                "api_error",
                "invalid upstream JSON: eof",
            ),
            (
                GatewayError::VariantNotRepresentable {
                    detail: "variant 'x' on 'p/m' has no Messages API representation (thinking/effort); it cannot be applied".into(),
                },
                400,
                "invalid_request_error",
                "variant 'x' on 'p/m' has no Messages API representation (thinking/effort); it cannot be applied",
            ),
        ];
        for (e, status, err_type, msg) in cases {
            assert_eq!(e.status_code(), status, "{e:?}");
            assert_eq!(e.error_type(), err_type, "{e:?}");
            assert_eq!(e.to_string(), msg, "{e:?}");
        }
    }
}
