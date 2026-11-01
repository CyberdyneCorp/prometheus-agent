//! `prometheus update --check` and `--set-channel` (spec:
//! distribution-and-update, Update exit codes; Release channels).

use pmt_core::error::Error;

use super::{Install, Session, finish, self_updating};
use crate::receipts::{Kind, Outcome, ReceiptWriter};
use crate::version::{Channel, ReleaseVersion};

/// How the running build compares with its channel's newest release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    /// A newer release exists.
    Available {
        /// Running version.
        from: ReleaseVersion,
        /// Newest release.
        to: ReleaseVersion,
    },
    /// The running build is the channel's newest.
    Current(ReleaseVersion),
    /// The running build is newer than the channel's newest (a canary
    /// after switching to stable): it stays until the channel catches up.
    Ahead {
        /// The channel.
        channel: Channel,
        /// Its newest release.
        latest: ReleaseVersion,
    },
}

impl Checked {
    /// Compares the running `current` with `latest` of `channel`.
    #[must_use]
    pub fn compare(current: ReleaseVersion, channel: Channel, latest: ReleaseVersion) -> Self {
        match latest.cmp(&current) {
            std::cmp::Ordering::Greater => Self::Available {
                from: current,
                to: latest,
            },
            std::cmp::Ordering::Equal => Self::Current(current),
            std::cmp::Ordering::Less => Self::Ahead { channel, latest },
        }
    }

    /// The one-line result.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Available { from, to } => format!("update available: {from} → {to}"),
            Self::Current(v) => format!("prometheus {v} is up to date"),
            Self::Ahead { channel, latest } => format!(
                "current build is newer than {channel} {latest}; staying until {channel} catches up"
            ),
        }
    }

    /// Exit status of `--check`: 10 when an update is available.
    #[must_use]
    pub fn check_exit_code(&self) -> u8 {
        match self {
            Self::Available { .. } => 10,
            _ => 0,
        }
    }
}

/// `prometheus update --check [--channel c]`: fetches only the signed
/// channel index.
///
/// # Errors
///
/// `update.owned_elsewhere` (69), `update.signature` (77),
/// `update.failed` (1).
pub async fn check(session: &Session<'_>, channel: Option<Channel>) -> Result<Checked, Error> {
    let install = session.install;
    let receipt = self_updating(install, Kind::Check)?;
    let channel = channel.unwrap_or(receipt.channel);
    let mut writer = begin(install, Kind::Check, channel)?;
    let result = async {
        let index = session.releases.channel_index(channel).await?;
        let checked = Checked::compare(install.version, channel, index.version);
        writer.update(|r| {
            r.to = Some(index.version.to_string());
            r.stages.push("check".to_owned());
        })?;
        Ok(checked)
    }
    .await;
    finish(writer, result, Outcome::Success)
}

/// `prometheus update --set-channel c`: persists the channel in the
/// install receipt.
///
/// # Errors
///
/// `update.owned_elsewhere` (69), `update.failed` (1).
pub fn set_channel(install: &Install, channel: Channel) -> Result<(), Error> {
    let mut receipt = self_updating(install, Kind::SetChannel)?;
    let writer = begin(install, Kind::SetChannel, channel)?;
    receipt.channel = channel;
    let result = receipt.write(install.home.root()).map(drop);
    finish(writer, result, Outcome::Success)
}

pub(super) fn begin(
    install: &Install,
    kind: Kind,
    channel: Channel,
) -> Result<ReceiptWriter, Error> {
    ReceiptWriter::begin(
        install.home.root(),
        kind,
        &install.binary,
        &install.version.to_string(),
        channel.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::Checked;
    use crate::version::{Channel, ReleaseVersion};

    fn v(text: &str) -> ReleaseVersion {
        ReleaseVersion::parse(text).unwrap()
    }

    #[test]
    fn newer_release_is_exit_10_with_the_spec_line() {
        let checked = Checked::compare(v("v1.4.0"), Channel::Stable, v("v1.4.1"));
        assert_eq!(checked.message(), "update available: v1.4.0 → v1.4.1");
        assert_eq!(checked.check_exit_code(), 10);
    }

    #[test]
    fn canary_on_stable_stays() {
        let checked = Checked::compare(
            v("v1.5.0-canary.20261029064100"),
            Channel::Stable,
            v("v1.4.1"),
        );
        assert_eq!(
            checked.message(),
            "current build is newer than stable v1.4.1; staying until stable catches up"
        );
        assert_eq!(checked.check_exit_code(), 0);
    }
}
