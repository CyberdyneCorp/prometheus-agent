//! Whole-config checks of the session automation sections the schema
//! cannot express (spec: session-automation, Automation configuration): a
//! self-paced loop's floor is not above its ceiling.

use pmt_core::error::{Error, codes};

use crate::schema::Config;

/// Checks `[loops]`.
///
/// # Errors
///
/// `config.invalid` naming both keys.
pub fn check(config: &Config) -> Result<(), Error> {
    let loops = &config.loops;
    if loops.self_paced_floor_secs > loops.self_paced_ceiling_secs {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!(
                "loops.self_paced_floor_secs ({}) is above loops.self_paced_ceiling_secs ({}): \
                 the floor must not exceed the ceiling",
                loops.self_paced_floor_secs, loops.self_paced_ceiling_secs
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inverted_self_paced_range_names_both_keys() {
        let mut config = Config::default();
        assert!(check(&config).is_ok());
        config.loops.self_paced_floor_secs = 600;
        config.loops.self_paced_ceiling_secs = 300;
        let err = check(&config).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(err.message().contains("loops.self_paced_floor_secs"));
        assert!(err.message().contains("loops.self_paced_ceiling_secs"));
    }
}
