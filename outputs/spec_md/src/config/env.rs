//! `${NAME}` environment variable substitution inside attribute values.

/// Replace every `${NAME}` reference in `value` using `lookup`.
/// Returns the names that could not be resolved as the error.
pub fn substitute(
    value: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, SubstError> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(SubstError::Unterminated);
        };
        let name = &after[..end];
        if !is_valid_name(name) {
            return Err(SubstError::InvalidName(name.to_string()));
        }
        match lookup(name) {
            Some(v) => out.push_str(&v),
            None => return Err(SubstError::Unset(name.to_string())),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Whether the value contains at least one `${...}` reference.
pub fn has_reference(value: &str) -> bool {
    value
        .find("${")
        .is_some_and(|i| value[i + 2..].contains('}'))
}

fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubstError {
    Unset(String),
    InvalidName(String),
    Unterminated,
}

impl std::fmt::Display for SubstError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubstError::Unset(n) => write!(f, "environment variable '{n}' is not set"),
            SubstError::InvalidName(n) => write!(f, "invalid environment variable name '{n}'"),
            SubstError::Unterminated => write!(f, "unterminated '${{' reference"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "KEY" => Some("secret".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn substitutes() {
        assert_eq!(substitute("Bearer ${KEY}", &env).unwrap(), "Bearer secret");
        assert_eq!(substitute("${KEY}${KEY}", &env).unwrap(), "secretsecret");
        assert_eq!(substitute("plain", &env).unwrap(), "plain");
        assert_eq!(substitute("a${EMPTY}b", &env).unwrap(), "ab");
    }

    #[test]
    fn reports_errors() {
        assert_eq!(
            substitute("${NOPE}", &env),
            Err(SubstError::Unset("NOPE".into()))
        );
        assert_eq!(substitute("${KEY", &env), Err(SubstError::Unterminated));
        assert!(matches!(
            substitute("${1BAD}", &env),
            Err(SubstError::InvalidName(_))
        ));
    }

    #[test]
    fn detects_reference() {
        assert!(has_reference("${A}"));
        assert!(!has_reference("literal"));
        assert!(!has_reference("${unterminated"));
    }
}
