//! `${VAR}` expansion for MCP server settings, so tokens can live in the
//! environment instead of the config file.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

static VAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid pattern"));

/// Replace every `${NAME}` with the environment value. An unset variable is
/// an error that names it; nothing else is interpreted.
pub fn expand(text: &str) -> Result<String, String> {
    expand_with(text, |name| std::env::var(name).ok())
}

pub fn expand_map(map: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String> {
    map.iter()
        .map(|(key, value)| Ok((key.clone(), expand(value)?)))
        .collect()
}

fn expand_with(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for captures in VAR.captures_iter(text) {
        let whole = captures.get(0).expect("capture 0");
        let name = &captures[1];
        out.push_str(&text[last..whole.start()]);
        let value = lookup(name).ok_or_else(|| {
            format!("environment variable {name} is not set; export it before starting lh")
        })?;
        out.push_str(&value);
        last = whole.end();
    }
    out.push_str(&text[last..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_known_and_rejects_unset_variables() {
        let lookup = |name: &str| (name == "TOKEN").then(|| "abc".to_string());
        assert_eq!(
            expand_with("Bearer ${TOKEN}", lookup).unwrap(),
            "Bearer abc"
        );
        assert_eq!(expand_with("plain $TOKEN", lookup).unwrap(), "plain $TOKEN");
        let error = expand_with("${MISSING}", lookup).unwrap_err();
        assert!(error.contains("MISSING"), "{error}");
    }
}
