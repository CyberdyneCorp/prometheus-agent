//! One Hermes config key to its disposition and (maybe) a Prometheus
//! value.

use pmt_config::catalog;
use serde_json::Value;

use super::refs;
use super::table::{self, Transform};
use crate::component::Component;
use crate::data;
use crate::env::{self, Class};
use crate::host::Registry;
use crate::plan::Source;
use crate::report::{Disposition, Entry};

/// A key's entry and the `(target, value)` to write.
pub(super) type Planned = (Entry, Option<(String, toml_edit::Value)>);

/// Plans Hermes key `key` set to `value` (`coerced`: a YAML-1.1-only
/// reading).
pub(super) fn plan_key(src: &Source<'_>, key: &str, value: &Value, coerced: bool) -> Planned {
    let entry = |d| Entry::new(Component::Config, key, d);
    let row = table::row_of(key);
    let target = row
        .map(|r| r.target.to_owned())
        .or_else(|| same_key(key, value))
        .or_else(|| {
            value
                .as_str()
                .and_then(refs::whole)
                .and_then(|_| secret_sibling(key))
        });
    let Some(target) = target else {
        if data::default_of(key) == Some(value) {
            return (entry(Disposition::DefaultOmitted), None);
        }
        let mut e = entry(Disposition::Unsupported).because("no Prometheus equivalent");
        if let Some(shown) = show(key, value) {
            e = e.showing(shown);
        }
        return (e, None);
    };
    if coerced && let Some(allowed) = enum_values(&target) {
        return ambiguous(key, &target, value, &allowed);
    }
    if data::default_of(key) == Some(value) || value.is_null() {
        let e = entry(Disposition::DefaultOmitted)
            .to(&target)
            .because("equal to the Hermes default; the Prometheus default applies");
        return (e, None);
    }
    if let Some(var) = env_override(src, &target) {
        let e = entry(Disposition::Skipped)
            .to(&target)
            .because(format!("the Hermes .env sets {var}, which wins"));
        return (e, None);
    }
    if let Value::String(text) = value
        && text.contains("${")
    {
        return reference(src, key, &target, text);
    }
    let transform = row.map_or(Transform::Same, |r| r.transform);
    match convert(src, value, transform) {
        Ok((converted, note)) => accept(key, &target, converted, note),
        Err(why) => {
            let mut e = entry(Disposition::Unsupported).to(&target).because(why);
            if let Some(shown) = show(key, value) {
                e = e.showing(shown);
            }
            (e, None)
        }
    }
}

/// The Prometheus key named like `key`, when its value is a scalar or a
/// list of scalars and the Prometheus schema has the key.
fn same_key(key: &str, value: &Value) -> Option<String> {
    let scalar = |v: &Value| !v.is_object() && !v.is_array();
    let plain = scalar(value) || value.as_array().is_some_and(|a| a.iter().all(scalar));
    let info = catalog::lookup(key)?;
    (plain && !info.section).then(|| key.to_owned())
}

fn enum_values(target: &str) -> Option<Vec<String>> {
    let info = catalog::lookup(target)?;
    let values = catalog::allowed_values(info.schema)?;
    Some(
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

/// An unquoted YAML 1.1 boolean (or octal) on an enum key: the most
/// restrictive value on a security key, else what Hermes applied.
fn ambiguous(key: &str, target: &str, value: &Value, allowed: &[String]) -> Planned {
    let entry = Entry::new(Component::Config, key, Disposition::Ambiguous).to(target);
    let pick = if table::security_sensitive(key) || table::security_sensitive(target) {
        table::RESTRICTIVE
            .iter()
            .find(|v| allowed.iter().any(|a| a == *v))
            .map(|v| ((*v).to_owned(), "the most restrictive value"))
    } else {
        let runtime = match value {
            Value::Bool(false) => Some("off"),
            Value::Bool(true) => Some("on"),
            _ => None,
        };
        runtime
            .filter(|v| allowed.iter().any(|a| a == v))
            .map(|v| (v.to_owned(), "the value Hermes applied"))
    };
    let shown = value.to_string();
    if let Some((chosen, why)) = pick {
        let e = entry.showing(&shown).because(format!(
            "unquoted YAML 1.1 value read as {shown}; took {why} '{chosen}'; set {target} explicitly to review"
        ));
        return (e, Some((target.to_owned(), toml_edit::Value::from(chosen))));
    }
    let e = entry.showing(&shown).because(format!(
        "unquoted YAML 1.1 value read as {shown}; no Prometheus value matches; set {target} explicitly"
    ));
    (e, None)
}

/// The `.env` setting that writes `target` and that the profile sets.
fn env_override<'a>(src: &'a Source<'_>, target: &str) -> Option<&'a str> {
    src.env.iter().map(|e| e.name.as_str()).find(
        |name| matches!(env::classify(name, &|_| false), Class::Setting(key, _) if key == target),
    )
}

/// A `${NAME}` value: a secret reference in the key's `*_secret`
/// sibling, or the `.env` value inlined.
fn reference(src: &Source<'_>, key: &str, target: &str, text: &str) -> Planned {
    let entry = |d| Entry::new(Component::Config, key, d).to(target);
    let refs = refs::find(text);
    if let Some(refs::Ref::Other(scheme)) = refs.iter().find(|r| matches!(r, refs::Ref::Other(_))) {
        let e = entry(Disposition::Unsupported).because(format!(
            "${{{scheme}:...}} sources are not supported; set the value in Prometheus"
        ));
        return (e, None);
    }
    let known = |n: &str| src.host.known_secret(n);
    if let Some(name) = refs::whole(text)
        && matches!(
            env::classify(&name, &known),
            Class::Secret | Class::Unclassified
        )
    {
        if let Some(sibling) = secret_sibling(target) {
            let e = entry(Disposition::Transformed)
                .to(&sibling)
                .because(format!("reference to secret {name}"));
            return (e, Some((sibling, toml_edit::Value::from(name))));
        }
        let e = entry(Disposition::Unsupported)
            .because(format!("{target} cannot reference secret {name}"));
        return (e, None);
    }
    let value = |n: &str| src.env_value(n).map(str::to_owned);
    match refs::inline(text, &value) {
        Ok(inlined) => {
            let (e, write) = accept(key, target, toml_edit::Value::from(inlined), None);
            let e = Entry {
                disposition: if e.disposition == Disposition::Mapped {
                    Disposition::Transformed
                } else {
                    e.disposition
                },
                reason: if e.reason.is_empty() {
                    "inlined from the Hermes .env".to_owned()
                } else {
                    e.reason
                },
                ..e
            };
            (e, write)
        }
        Err(name) => {
            let e = entry(Disposition::Unsupported).because(format!(
                "unresolved reference ${{{name}}}: the Hermes .env does not set it"
            ));
            (e, None)
        }
    }
}

/// The key that names the secret holding `target`'s value: `target`
/// itself when it is a secret reference, else `<target>_secret`
/// (`api_key` -> `api_key_secret`) or the sibling `key_secret`.
fn secret_sibling(target: &str) -> Option<String> {
    let candidates = [target.to_owned(), format!("{target}_secret"), {
        let (parent, _) = target.rsplit_once('.').unwrap_or(("", target));
        if parent.is_empty() {
            "key_secret".to_owned()
        } else {
            format!("{parent}.key_secret")
        }
    }];
    candidates
        .into_iter()
        .find(|k| catalog::lookup(k).is_some_and(|i| i.secret_ref))
}

/// Checks `value` against the Prometheus schema of `target`.
fn accept(key: &str, target: &str, value: toml_edit::Value, note: Option<String>) -> Planned {
    let entry = |d| Entry::new(Component::Config, key, d).to(target);
    if let Err(e) = pmt_config::layers::typed_value(target, &value.to_string()) {
        let first = e.message().lines().next().unwrap_or_default().to_owned();
        return (
            entry(Disposition::Unsupported)
                .because(format!("Prometheus refuses the value: {first}")),
            None,
        );
    }
    let changed = target != key || note.is_some();
    let e = if changed {
        entry(Disposition::Transformed).because(note.unwrap_or_else(|| "renamed".to_owned()))
    } else {
        entry(Disposition::Mapped)
    };
    (e, Some((target.to_owned(), value)))
}

/// `value` through `transform`; `Err` says why it cannot map.
fn convert(
    src: &Source<'_>,
    value: &Value,
    transform: Transform,
) -> Result<(toml_edit::Value, Option<String>), String> {
    match transform {
        Transform::Same => json_to_toml(value)
            .map(|v| (v, None))
            .ok_or_else(not_a_value),
        Transform::Scale(factor) => {
            let n = value.as_f64().ok_or_else(|| "not a number".to_owned())?;
            #[allow(clippy::cast_possible_truncation)] // config values are small
            let scaled = (n * factor).round() as i64;
            let note = (factor > 1.0).then(|| format!("unit changed (x{factor})"));
            Ok((toml_edit::Value::from(scaled), note))
        }
        Transform::Lower => {
            let text = value.as_str().ok_or_else(not_a_value)?;
            Ok((toml_edit::Value::from(text.to_lowercase()), None))
        }
        Transform::Name(registry) => {
            let name = value.as_str().ok_or_else(not_a_value)?;
            match name_of(src, registry, name) {
                Named::Name(mapped) => Ok((toml_edit::Value::from(mapped), None)),
                Named::Default => Err(format!("'{name}' is covered by the Prometheus default")),
                Named::Unknown => Err(format!("no Prometheus {} named '{name}'", label(registry))),
            }
        }
        Transform::Names(registry) => names(src, registry, value),
    }
}

fn names(
    src: &Source<'_>,
    registry: Registry,
    value: &Value,
) -> Result<(toml_edit::Value, Option<String>), String> {
    let list: Vec<&str> = match value {
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        Value::String(one) => vec![one.as_str()],
        _ => return Err(not_a_value()),
    };
    let mut kept = toml_edit::Array::new();
    let mut dropped = Vec::new();
    for name in list {
        match name_of(src, registry, name) {
            Named::Name(mapped) => kept.push(mapped),
            Named::Default => {}
            Named::Unknown => dropped.push(name.to_owned()),
        }
    }
    if kept.is_empty() {
        let why = if dropped.is_empty() {
            "covered by the Prometheus default".to_owned()
        } else {
            format!(
                "no Prometheus {} named {}",
                label(registry),
                dropped.join(", ")
            )
        };
        return Err(why);
    }
    let note = (!dropped.is_empty()).then(|| {
        format!(
            "unsupported {}s dropped: {}",
            label(registry),
            dropped.join(", ")
        )
    });
    Ok((toml_edit::Value::Array(kept), note))
}

/// What a Hermes name is in a Prometheus registry.
enum Named {
    /// This name.
    Name(String),
    /// Nothing: the Prometheus default covers it.
    Default,
    /// The registry has no such name.
    Unknown,
}

fn name_of(src: &Source<'_>, registry: Registry, name: &str) -> Named {
    if table::covered_by_default(registry, name) {
        Named::Default
    } else if src.host.known(registry, name, None) {
        Named::Name(name.to_owned())
    } else {
        Named::Unknown
    }
}

fn label(registry: Registry) -> &'static str {
    match registry {
        Registry::Toolset => "toolset",
        Registry::Provider => "provider",
        Registry::Theme => "theme",
        Registry::Platform => "platform",
        Registry::MemoryProvider => "memory provider",
    }
}

fn not_a_value() -> String {
    "not a value Prometheus can hold here".to_owned()
}

/// `value` as a TOML value (scalars, lists of scalars and tables).
#[must_use]
pub fn json_to_toml(value: &Value) -> Option<toml_edit::Value> {
    Some(match value {
        Value::Bool(b) => toml_edit::Value::from(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => toml_edit::Value::from(i),
            None => toml_edit::Value::from(n.as_f64()?),
        },
        Value::String(s) => toml_edit::Value::from(s.as_str()),
        Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(json_to_toml(item)?);
            }
            toml_edit::Value::Array(array)
        }
        Value::Object(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, v) in map {
                if !v.is_null() {
                    table.insert(k, json_to_toml(v)?);
                }
            }
            toml_edit::Value::InlineTable(table)
        }
        Value::Null => return None,
    })
}

/// `value` for the report, unless `key` looks like it holds a credential.
#[must_use]
pub fn show(key: &str, value: &Value) -> Option<String> {
    const WORDS: &[&str] = &["KEY", "APIKEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"];
    let leaf = key.rsplit('.').next().unwrap_or(key).to_ascii_uppercase();
    if leaf.split(['_', '-']).any(|w| WORDS.contains(&w)) {
        return None;
    }
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Some(text.chars().take(120).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Fixture;

    fn plan(f: &Fixture, key: &str) -> Planned {
        let src = f.source();
        let value = src.get(key).unwrap().clone();
        let coerced = src.config.coerced.contains(key);
        plan_key(&src, key, &value, coerced)
    }

    #[test]
    fn references_inline_settings_and_refuse_other_sources() {
        let f = Fixture::new(
            "terminal:\n  cwd: ${env:HERMES_WORKDIR}\n  backend: ${file:/etc/backend}\nskills:\n  external_dirs: ${HERMES_MISSING}\n",
            "HERMES_WORKDIR=/srv/work\n",
        );
        let (e, write) = plan(&f, "terminal.cwd");
        assert_eq!(e.disposition, Disposition::Transformed, "{e:?}");
        assert_eq!(write.unwrap().1.as_str(), Some("/srv/work"));
        let (e, write) = plan(&f, "terminal.backend");
        assert_eq!(e.disposition, Disposition::Unsupported);
        assert!(write.is_none());
        let (e, _) = plan(&f, "skills.external_dirs");
        assert!(
            e.reason.contains("unresolved reference ${HERMES_MISSING}"),
            "{e:?}"
        );
    }

    #[test]
    fn a_secret_reference_names_the_secret_in_the_sibling_key() {
        let f = Fixture::new(
            "delegation:\n  api_key: ${DELEGATE_API_KEY}\n",
            "DELEGATE_API_KEY=k\n",
        );
        let src = f.source();
        let (e, write) = plan_key(
            &src,
            "delegation.api_key",
            &Value::from("${DELEGATE_API_KEY}"),
            false,
        );
        assert_eq!(e.disposition, Disposition::Transformed, "{e:?}");
        let (key, value) = write.unwrap();
        assert_eq!(key, "delegation.api_key_secret");
        assert_eq!(value.as_str(), Some("DELEGATE_API_KEY"));
    }

    #[test]
    fn toolsets_keep_known_names_and_report_the_rest() {
        let f = Fixture::new("toolsets: [hermes-cli, terminal, web, teleport]\n", "");
        let (e, write) = plan(&f, "toolsets");
        assert_eq!(e.disposition, Disposition::Transformed);
        assert!(e.reason.contains("teleport"), "{e:?}");
        let (_, value) = write.unwrap();
        assert_eq!(value.to_string().trim(), r#"["terminal", "web"]"#);
    }

    #[test]
    fn a_non_security_enum_takes_what_hermes_applied() {
        let f = Fixture::new("display:\n  tool_progress: off\n", "");
        let (e, write) = plan(&f, "display.tool_progress");
        assert_eq!(e.disposition, Disposition::Ambiguous);
        assert_eq!(write.unwrap().1.as_str(), Some("off"));
    }

    #[test]
    fn every_explicit_row_writes_a_prometheus_key() {
        for row in table::ROWS {
            assert!(
                catalog::lookup(row.target).is_some(),
                "{} -> {}",
                row.hermes,
                row.target
            );
        }
    }
}
