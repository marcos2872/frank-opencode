//! Claude Desktop picker evasion (advertised ids only).

/// Substrings the Claude Desktop app refuses anywhere in a gateway model
/// id: any discovered id containing one is dropped from the picker, even
/// with `claude` in it. Extracted from the denylist baked into the Desktop
/// bundle (`XSe`): only plain slug-compatible tokens are listed here
/// (dotted/bounded fragments like `k2\.` or `\bling\b` can never occur in
/// our slugs, which use `-` separators and no dots).
const DESKTOP_BLOCKED_TOKENS: &[&str] = &[
    "ark-code",
    "astron",
    "command-r",
    "deepseek",
    "doubao",
    "gemini",
    "gemma",
    "glm",
    "gpt",
    "grok",
    "hermes",
    "hy3",
    "kimi",
    "lfm",
    "llama",
    "longcat",
    "mimo",
    "minimax",
    "mistral",
    "mixtral",
    "moonshot",
    "nemotron",
    "openai",
    "qianfan",
    "qwen",
    "trinity",
    "abab",
    "jamba",
    "arctic",
    "solar",
    "mercury",
    "zamba",
    "ernie",
    "arcee",
    "nova-",
    "phi-",
    "tc-code",
    "kat-coder",
    "yi-",
    "devstral",
    "ministral",
    "stepfun",
    "bytedance",
    "hunyuan",
    "granite",
    "codex",
    "step-3",
    "seed-",
];

/// Rewrite a model/id fragment so no `DESKTOP_BLOCKED_TOKENS` entry survives
/// as a substring: a `-` is inserted after the first character of each
/// (case-insensitive) occurrence (`deepseek` → `d-eepseek`). The Desktop
/// filter only inspects the alias `id`, so display names and `opencode_ref`
/// resolution are untouched. Deterministic and slug-safe.
/// Blocklist tokens longest-first, cached process-wide: `evade_*` is called
/// once per alias at catalog refresh, and no longer needs to copy + sort
/// the table on every call.
fn desktop_tokens_by_length() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static SORTED: OnceLock<Vec<&'static str>> = OnceLock::new();
    SORTED.get_or_init(|| {
        let mut tokens: Vec<&'static str> = DESKTOP_BLOCKED_TOKENS.to_vec();
        tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
        tokens
    })
}

pub fn evade_desktop_blocklist(s: &str) -> String {
    let mut out = s.to_string();
    for &token in desktop_tokens_by_length() {
        let mut search_from = 0;
        loop {
            let lower = out.to_lowercase();
            let Some(rel) = lower[search_from..].find(token) else {
                break;
            };
            let at = search_from + rel;
            // Split "deepseek" into "d-eepseek". Byte-safe: tokens are ASCII.
            out.insert(at + 1, '-');
            search_from = at + token.len() + 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn evade_breaks_blocked_substrings() {
        assert_eq!(
            evade_desktop_blocklist("deepseek-v4.1-flash"),
            "d-eepseek-v4.1-flash"
        );
        assert_eq!(evade_desktop_blocklist("kimi-k2.7-code"), "k-imi-k2.7-code");
        assert_eq!(evade_desktop_blocklist("qwen3.8-max"), "q-wen3.8-max");
        assert_eq!(evade_desktop_blocklist("hy3"), "h-y3");
        // Unblocked names pass through untouched.
        assert_eq!(
            evade_desktop_blocklist("muse-spark-1.3-contributor"),
            "muse-spark-1.3-contributor"
        );
        assert_eq!(
            evade_desktop_blocklist("space-bunny-free"),
            "space-bunny-free"
        );
    }
}