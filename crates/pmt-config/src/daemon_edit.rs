//! Validated edits of `<home>/daemon.toml` (`prometheus dashboard register`
//! writes `dashboard.auth.portal.client_id` this way): the key is set with
//! the rest of the file (comments included) kept, the result is validated
//! as a whole, and the file is replaced atomically only when it is valid.

use std::io;
use std::path::Path;

use pmt_core::error::{Error, codes};

use crate::daemon::DaemonConfig;

/// Sets the string at dotted `key` (`dashboard.auth.portal.client_id`).
///
/// # Errors
///
/// `config.parse` when the file or the result is invalid (nothing is
/// written then), `home.io`.
pub fn set_string(home: &Path, key: &str, value: &str) -> Result<(), Error> {
    let path = home.join("daemon.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(Error::new(
                &codes::HOME_IO,
                format!("cannot read {}: {e}", path.display()),
            ));
        }
    };
    let parse = |message: String| {
        Error::new(
            &codes::CONFIG_PARSE,
            format!("{}: {message}", path.display()),
        )
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| parse(e.message().to_owned()))?;
    let (tables, leaf) = key.rsplit_once('.').map_or(("", key), |(t, l)| (t, l));
    let mut table = doc.as_table_mut();
    for name in tables.split('.').filter(|n| !n.is_empty()) {
        let entry = table.entry(name).or_insert_with(|| {
            let mut fresh = toml_edit::Table::new();
            fresh.set_implicit(true);
            toml_edit::Item::Table(fresh)
        });
        table = entry
            .as_table_mut()
            .ok_or_else(|| parse(format!("{name} is not a table")))?;
    }
    table[leaf] = toml_edit::value(value);
    let updated = doc.to_string();
    DaemonConfig::parse(&path, &updated)?;
    pmt_core::private_fs::write_atomic(&path, updated.as_bytes()).map_err(|e| {
        Error::new(
            &codes::HOME_IO,
            format!("cannot write {}: {e}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::set_string;
    use crate::DaemonConfig;

    #[test]
    fn a_key_is_set_keeping_the_rest_and_an_invalid_result_is_not_written() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("daemon.toml");
        std::fs::write(&file, "# mine\n[dashboard]\nport = 9200\n").unwrap();
        set_string(home.path(), "dashboard.auth.portal.client_id", "cl_1").unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# mine\n"), "{text}");
        let config = DaemonConfig::load(home.path()).unwrap();
        assert_eq!(
            config.dashboard.auth.portal.client_id.as_deref(),
            Some("cl_1")
        );
        assert_eq!(config.dashboard.port, 9200);
        assert!(set_string(home.path(), "dashboard.no_such_key", "x").is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "unchanged");
    }
}
