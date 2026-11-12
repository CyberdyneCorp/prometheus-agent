//! Hermes `${NAME}` and `${env:NAME}` references in config values (spec:
//! hermes-migration, Config references to environment values). A
//! reference to a secret becomes a Prometheus secret reference, one to a
//! non-secret is inlined from the Hermes `.env`; other sources
//! (`${file:...}`, vault sources) and names `.env` does not set are
//! unsupported.

/// One `${...}` in a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    /// `${NAME}` or `${env:NAME}`.
    Env(String),
    /// `${<scheme>:...}` of any other source.
    Other(String),
}

/// Every reference in `text`, in order.
#[must_use]
pub fn find(text: &str) -> Vec<Ref> {
    let mut refs = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            break;
        };
        let inner = after[..end].trim();
        refs.push(match inner.split_once(':') {
            Some(("env", name)) => Ref::Env(name.trim().to_owned()),
            Some((scheme, _)) => Ref::Other(scheme.to_owned()),
            None => Ref::Env(inner.to_owned()),
        });
        rest = &after[end + 1..];
    }
    refs
}

/// `text` when it is exactly one `${NAME}`/`${env:NAME}` reference.
#[must_use]
pub fn whole(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with("${") || !trimmed.ends_with('}') {
        return None;
    }
    match find(trimmed).as_slice() {
        [Ref::Env(name)] if trimmed.matches("${").count() == 1 => Some(name.clone()),
        _ => None,
    }
}

/// `text` with every `${env:NAME}` written as `${NAME}` (the Prometheus
/// placeholder of an MCP header).
#[must_use]
pub fn as_placeholders(text: &str) -> String {
    text.replace("${env:", "${")
}

/// `text` with each `${NAME}` replaced by `value(NAME)`; `Err(name)` for
/// the first name `value` cannot give.
///
/// # Errors
///
/// The unresolved name.
pub fn inline(text: &str, value: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let inner = after[..end].trim();
        let name = inner.strip_prefix("env:").unwrap_or(inner).trim();
        out.push_str(&value(name).ok_or_else(|| name.to_owned())?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_found_and_resolved() {
        assert_eq!(
            find("Bearer ${GITHUB_TOKEN} ${env:X} ${file:/k}"),
            [
                Ref::Env("GITHUB_TOKEN".into()),
                Ref::Env("X".into()),
                Ref::Other("file".into())
            ]
        );
        assert_eq!(whole("${env:KEY}"), Some("KEY".into()));
        assert_eq!(whole("x ${KEY}"), None);
        let value = |n: &str| (n == "A").then(|| "1".to_owned());
        assert_eq!(inline("a=${A}!", &value), Ok("a=1!".into()));
        assert_eq!(inline("${B}", &value), Err("B".into()));
        assert_eq!(as_placeholders("Bearer ${env:T}"), "Bearer ${T}");
    }
}
