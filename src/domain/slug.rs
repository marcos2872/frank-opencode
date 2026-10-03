//! Slugs and context-window suffixes for gateway ids.

/// Strip a context-window hint suffix such as `[1m]` / `[200k]` / `[500k]`
/// that Claude Code appends to unknown gateway model ids. Returns the base id.
/// Any trailing `[<digits>k|m]` (case-insensitive) is stripped; anything else
/// (e.g. `[foo]`, `[12]`) is left untouched.
pub fn strip_window_suffix(s: &str) -> &str {
    if !s.ends_with(']') {
        return s;
    }
    let Some(open) = s.rfind('[') else {
        return s;
    };
    let inner = &s[open + 1..s.len() - 1];
    if inner.is_empty() || !inner.is_ascii() {
        return s;
    }
    let inner = inner.to_ascii_lowercase();
    let (digits, unit) = inner.split_at(inner.len() - 1);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && matches!(unit, "k" | "m")
    {
        &s[..open]
    } else {
        s
    }
}
/// The `[1m]` suffix for a context window announced on `/v1/models`.
/// Mainline Claude Code only reads a window from the literal `[1m]` suffix
/// (regex `/\[1m\]/i`), never from arbitrary `[<digits>k|m]` — so a window
/// below 1M yields no suffix (`None`) and a window at/above 1M announces
/// `[1m]` (rounded down, never claiming more than the real window).
pub fn window_suffix(tokens: u64) -> Option<String> {
    (tokens >= 1_000_000).then(|| "[1m]".to_string())
}

/// Sanitize a model id into a URL/claude-safe slug.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn slugify_collapses_separators() {
        assert_eq!(
            slugify("OpenRouter/Claude Sonnet 4.5"),
            "openrouter-claude-sonnet-4-5"
        );
    }

    #[test]
    fn strips_window_hint_suffix() {
        assert_eq!(strip_window_suffix("claude-x[1m]"), "claude-x");
        assert_eq!(strip_window_suffix("claude-x[200K]"), "claude-x");
        assert_eq!(strip_window_suffix("claude-x[500k]"), "claude-x");
        assert_eq!(strip_window_suffix("claude-x[2M]"), "claude-x");
        assert_eq!(strip_window_suffix("claude-x"), "claude-x");
        assert_eq!(strip_window_suffix("a/b[1m]"), "a/b");
        // Not a window hint: left untouched.
        assert_eq!(strip_window_suffix("claude-x[foo]"), "claude-x[foo]");
        assert_eq!(strip_window_suffix("claude-x[12]"), "claude-x[12]");
        assert_eq!(strip_window_suffix("claude-x[]"), "claude-x[]");
        assert_eq!(strip_window_suffix("claude-x[k]"), "claude-x[k]");
        assert_eq!(strip_window_suffix("claude-x"), "claude-x");
        assert_eq!(strip_window_suffix("no-bracket"), "no-bracket");
    }

    #[test]
    fn window_suffix_with_unicode_is_unchanged() {
        let input = "claude-x[é]";
        assert_eq!(strip_window_suffix(input), input);
    }

    #[test]
    fn window_suffix_only_1m() {
        // Mainline Claude Code reads a window only from the literal `[1m]`
        // suffix; sub-1M windows announce nothing.
        assert_eq!(window_suffix(1_000_000), Some("[1m]".to_string()));
        assert_eq!(window_suffix(1_050_000), Some("[1m]".to_string()));
        assert_eq!(window_suffix(1_999_999), Some("[1m]".to_string()));
        assert_eq!(window_suffix(2_000_000), Some("[1m]".to_string()));
        assert_eq!(window_suffix(200_000), None);
        assert_eq!(window_suffix(128_000), None);
        assert_eq!(window_suffix(105_000), None);
        assert_eq!(window_suffix(0), None);
        // Round trip: what we announce is what the gateway strips again.
        assert_eq!(strip_window_suffix("claude-x[1m]"), "claude-x");
    }
}