//! Variant plans: how a catalog `#variant` lands on each wire protocol.

use crate::domain::{protocol_for_entry, CatalogEntry, Protocol, ThinkingConfig};
use serde_json::Value;

/// Normalize a variant's reasoning label into the OpenAI `reasoning_effort`
/// enum. Anthropic's `max` label has no OpenAI equivalent and degrades to
/// `high`; an explicit `none` is preserved so it cannot become maximum
/// reasoning by accident.
pub fn normalize_reasoning(label: &str) -> &str {
    match label.to_ascii_lowercase().trim() {
        "none" => "none",
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" | "max" => "high",
        _ => "high",
    }
}

/// The `thinking` object sent when a Messages-API variant turns thinking on.
/// The catalog already fixed the display mode for these variants, so the
/// gateway forwards exactly that.
const THINKING_ADAPTIVE_JSON: &str = r#"{"type":"adaptive","display":"summarized"}"#;

/// A variant that has a catalog definition but no safe representation on the
/// Messages API. Surfaced as a 400 instead of a silent no-op, so a user who
/// asked for `#xhigh` on an Anthropic model learns it was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicVariantError {
    pub model: String,
    pub variant: String,
}

impl std::fmt::Display for AnthropicVariantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "variant '{}' on '{}' has no Messages API representation \
             (thinking/effort); it cannot be applied",
            self.variant, self.model
        )
    }
}

/// What a variant resolves to for its wire protocol, or an error when the
/// target protocol has no safe representation for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantPlan {
    /// No settings to apply (variant unknown, or declares nothing).
    None,
    /// Chat Completions: `reasoning_effort` string, or remove the key (`none`).
    ChatReasoning(String),
    /// Responses: `reasoning.effort` plus optional `include` parts.
    ResponsesReasoning {
        effort: String,
        include: Option<Vec<String>>,
    },
    /// Messages API: `thinking` object (`null` = the variant disables it),
    /// optional `include` parts.
    AnthropicThinking { thinking: Value },
}

/// Resolve how `variant` applies to `entry`'s protocol, or `Err` when the
/// Messages API cannot represent it safely.
pub fn variant_plan(
    entry: &CatalogEntry,
    variant: &str,
) -> Result<VariantPlan, AnthropicVariantError> {
    let Some(v) = entry.variant(variant) else {
        return Ok(VariantPlan::None);
    };
    let err = || AnthropicVariantError {
        model: entry.qualified(),
        variant: variant.to_string(),
    };
    match protocol_for_entry(entry) {
        Protocol::ChatCompletions => match v.settings.reasoning_effort.as_deref() {
            Some(r) => Ok(VariantPlan::ChatReasoning(
                normalize_reasoning(r).to_string(),
            )),
            None => Ok(VariantPlan::None),
        },
        Protocol::Responses => match v.settings.reasoning_effort.as_deref() {
            Some(r) => Ok(VariantPlan::ResponsesReasoning {
                effort: normalize_reasoning(r).to_string(),
                include: v.settings.include.clone(),
            }),
            None => Ok(VariantPlan::None),
        },
        // Messages API has no `reasoningEffort` parameter. A variant carries
        // either a `thinking` object (forwarded verbatim) and/or an `effort`
        // label (mapped to the closest thinking mode); anything else is not
        // representable and is rejected rather than silently dropped.
        Protocol::Anthropic => {
            let thinking = match &v.settings.thinking {
                Some(ThinkingConfig::Typed { kind, .. })
                    if kind.eq_ignore_ascii_case("disabled") =>
                {
                    Value::Null
                }
                Some(_) => serde_json::from_str(THINKING_ADAPTIVE_JSON).unwrap_or(Value::Null),
                None => match v.settings.effort.as_deref() {
                    // Explicit no-thinking variant.
                    Some(e) if e.eq_ignore_ascii_case("none") => Value::Null,
                    Some(e) => thinking_for_effort(e),
                    // `reasoningEffort` alone on a Messages-API model was the
                    // old silent no-op; keep rejecting it.
                    None if v.settings.reasoning_effort.is_some() => return Err(err()),
                    None => return Ok(VariantPlan::None),
                },
            };
            Ok(VariantPlan::AnthropicThinking { thinking })
        }
    }
}

/// Map a Messages-API effort label onto a `thinking` object. Unknown labels
/// degrade to adaptive thinking rather than dropping the request.
fn thinking_for_effort(effort: &str) -> Value {
    match effort.to_ascii_lowercase().trim() {
        // The catalog's `thinking: {"type":"disabled"}` equivalent.
        "none" => Value::Null,
        // Anthropic accepts `thinking: {"type":"enabled"}` at a lower effort.
        "minimal" | "low" => serde_json::json!({"type": "enabled"}),
        _ => serde_json::from_str(THINKING_ADAPTIVE_JSON).unwrap_or(Value::Null),
    }
}

/// Apply a selected variant to a translated request body.
///
/// The field that carries the variant depends on the wire protocol (same
/// table as `protocol_for_entry`):
///
/// - `ChatCompletions`: `reasoning_effort` (removed for an explicit `none`).
/// - `Responses`: `reasoning.effort` plus the variant's `include` parts.
/// - `Anthropic`: `thinking` (and `include` where the catalog lists it);
///   a variant with no representable fields is rejected by
///   [`apply_variant_checked`].
///
/// Unknown variant labels pass through unchanged (callers validate).
pub fn apply_variant(body: Value, entry: &CatalogEntry, variant: &str) -> Value {
    match variant_plan(entry, variant) {
        Ok(plan) => apply_plan(body, plan),
        // Direct (unchecked) callers keep the previous lenient behavior.
        Err(_) => body,
    }
}

/// Apply a variant, surfacing a Messages-API variant the gateway cannot
/// represent (see [`variant_plan`]).
pub fn apply_variant_checked(
    body: Value,
    entry: &CatalogEntry,
    variant: &str,
) -> Result<Value, AnthropicVariantError> {
    let plan = variant_plan(entry, variant)?;
    Ok(apply_plan(body, plan))
}

fn apply_plan(mut body: Value, plan: VariantPlan) -> Value {
    match plan {
        VariantPlan::None => {}
        VariantPlan::ChatReasoning(effort) => {
            if effort == "none" {
                if let Some(o) = body.as_object_mut() {
                    o.remove("reasoning_effort");
                }
            } else {
                body["reasoning_effort"] = Value::String(effort);
            }
        }
        VariantPlan::ResponsesReasoning { effort, include } => {
            body["reasoning"] = serde_json::json!({"effort": effort});
            if let Some(include) = include {
                body["include"] = Value::Array(include.into_iter().map(Value::String).collect());
            }
        }
        VariantPlan::AnthropicThinking { thinking } => {
            match thinking {
                // The variant turns thinking off explicitly.
                Value::Null => {
                    if let Some(o) = body.as_object_mut() {
                        o.insert("thinking".into(), serde_json::json!({"type":"disabled"}));
                    }
                }
                other => {
                    body["thinking"] = other;
                }
            }
        }
    }
    body
}

// ---------------------------------------------------------------------------
// OpenAI -> Anthropic (non-streaming)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CatalogEntry, CatalogSettings};

    #[test]
    fn normalize_reasoning_clamps_to_openai_enum() {
        assert_eq!(normalize_reasoning("low"), "low");
        assert_eq!(normalize_reasoning("medium"), "medium");
        assert_eq!(normalize_reasoning("high"), "high");
        assert_eq!(normalize_reasoning("none"), "none");
        assert_eq!(normalize_reasoning("MAX"), "high");
        assert_eq!(normalize_reasoning("garbage"), "high");
    }

    #[test]
    fn apply_variant_sets_reasoning_effort_for_chat() {
        let e = entry_with_variants(
            "@opencode/ai/providers/openai-compatible",
            vec![("high".to_string(), Some("high".to_string()))],
        );
        let body = serde_json::json!({"model": "x", "messages": []});
        let out = apply_variant(body, &e, "high");
        assert_eq!(out["reasoning_effort"], "high");
    }

    #[test]
    fn apply_variant_uses_reasoning_key_for_responses() {
        let e = entry_with_variants(
            "@opencode/ai/providers/openai",
            vec![("low".to_string(), Some("low".to_string()))],
        );
        let body = serde_json::json!({"model": "x", "input": []});
        let out = apply_variant(body, &e, "low");
        assert_eq!(out["reasoning"]["effort"], "low");
        assert!(out["reasoning"].is_object());
    }

    #[test]
    fn anthropic_variant_without_representation_is_rejected() {
        // `reasoningEffort` alone has no Messages API parameter: surfaced as
        // an error rather than a silent no-op.
        let e = entry_with_variants(
            "@opencode/ai/providers/anthropic",
            vec![("high".to_string(), Some("high".to_string()))],
        );
        let body = serde_json::json!({"model": "x"});
        assert!(apply_variant_checked(body.clone(), &e, "high").is_err());
        // Unknown variant label stays untouched for both entry points.
        assert_eq!(apply_variant(body.clone(), &e, "nope"), body);
        assert_eq!(
            apply_variant_checked(body.clone(), &e, "nope").unwrap(),
            body
        );
    }

    fn anthropic_variant(
        pkg: &str,
        id: &str,
        thinking: Option<crate::domain::ThinkingConfig>,
        effort: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> CatalogEntry {
        let mut e = entry_with_variants(pkg, vec![]);
        e.variants.push(crate::domain::ModelVariant {
            id: id.to_string(),
            settings: crate::domain::ModelVariantSettings {
                reasoning_effort: reasoning_effort.map(str::to_string),
                thinking,
                effort: effort.map(str::to_string),
                ..Default::default()
            },
        });
        e
    }

    #[test]
    fn anthropic_variant_maps_thinking_object_and_effort() {
        // Catalog `thinking` object is forwarded verbatim.
        let e = anthropic_variant(
            "@opencode/ai/providers/anthropic",
            "high",
            Some(crate::domain::ThinkingConfig::Typed {
                kind: "adaptive".to_string(),
                display: Some("summarized".to_string()),
            }),
            None,
            None,
        );
        let out = apply_variant_checked(serde_json::json!({"model": "x"}), &e, "high").unwrap();
        assert_eq!(out["thinking"]["type"], "adaptive");
        assert_eq!(out["thinking"]["display"], "summarized");

        // A `disabled` variant turns thinking off explicitly.
        let e = anthropic_variant(
            "@opencode/ai/providers/anthropic",
            "none",
            Some(crate::domain::ThinkingConfig::Typed {
                kind: "disabled".to_string(),
                display: None,
            }),
            None,
            None,
        );
        let out = apply_variant_checked(serde_json::json!({"model": "x"}), &e, "none").unwrap();
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn anthropic_variant_effort_only_maps_to_thinking() {
        let e = anthropic_variant(
            "@opencode/ai/providers/anthropic",
            "high",
            None,
            Some("high"),
            None,
        );
        let out = apply_variant_checked(serde_json::json!({"model": "x"}), &e, "high").unwrap();
        assert_eq!(out["thinking"]["type"], "adaptive");

        // `none` disables thinking.
        let e = anthropic_variant(
            "@opencode/ai/providers/anthropic",
            "none",
            None,
            Some("none"),
            None,
        );
        let out = apply_variant_checked(serde_json::json!({"model": "x"}), &e, "none").unwrap();
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn responses_variant_carries_include_parts() {
        let mut e = entry_with_variants(
            "@opencode/ai/providers/openai",
            vec![("high".to_string(), Some("high".to_string()))],
        );
        e.variants[0].settings.include = Some(vec!["reasoning.encrypted_content".to_string()]);
        let out = apply_variant(serde_json::json!({"model": "x"}), &e, "high");
        assert_eq!(out["reasoning"]["effort"], "high");
        assert_eq!(out["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn chat_variant_none_removes_reasoning_effort() {
        let e = entry_with_variants(
            "@opencode/ai/providers/openai-compatible",
            vec![("none".to_string(), Some("none".to_string()))],
        );
        let out = apply_variant(
            serde_json::json!({"model": "x", "reasoning_effort": "high"}),
            &e,
            "none",
        );
        assert!(out.get("reasoning_effort").is_none());
    }
}