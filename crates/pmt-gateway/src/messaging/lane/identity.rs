//! `/link` and `/unlink` (spec: messaging-gateway, Cross-platform
//! identity linking): linking two accounts of one person (each already
//! authorized: commands only reach an admitted sender) through a 6-digit
//! code valid for 600 seconds, handed out in a DM only.

use pmt_store::gateway::identity::Redeemed;

use super::Lane;
use crate::messaging::key::{ChatType, Routing};

impl Lane {
    /// `/link`: a code for this account; `/link <code>`: links this
    /// account to the one that asked for the code.
    pub(super) fn link_command(&self, routing: &Routing, args: &str) {
        if routing.chat_type != ChatType::Dm {
            self.say(self.strings().get("gateway.link_in_dm"));
            return;
        }
        let Some(store) = self.shared.store(&self.profile) else {
            return;
        };
        let strings = self.strings();
        let now = jiff::Timestamp::now();
        let account = (routing.platform.as_str(), routing.sender_id.as_str());
        let code = args.trim();
        let text = if code.is_empty() {
            store
                .link_code(account.0, account.1, now)
                .map(|code| pmt_i18n::t!(strings, "gateway.link_code", code = code))
        } else {
            store
                .redeem_link_code(code, account, now)
                .map(|redeemed| match redeemed {
                    Redeemed::Linked(_) => strings.get("gateway.link_done").to_owned(),
                    Redeemed::Expired => strings.get("gateway.link_expired").to_owned(),
                    Redeemed::Unknown => strings.get("gateway.link_unknown").to_owned(),
                })
        };
        match text {
            Ok(text) => self.say(&text),
            Err(err) => tracing::warn!("identity link failed: {err}"),
        }
    }

    /// `/unlink <platform>`: takes this person's account(s) on `platform`
    /// out of the link.
    pub(super) fn unlink_command(&self, routing: &Routing, args: &str) {
        let Some(store) = self.shared.store(&self.profile) else {
            return;
        };
        let strings = self.strings();
        let platform = args.trim();
        let platform = if platform.is_empty() {
            routing.platform.as_str()
        } else {
            platform
        };
        let removed = store
            .person_of(&routing.platform, &routing.sender_id)
            .and_then(|person| match person {
                Some(person) => store.unlink_identity(&person, platform, None),
                None => Ok(0),
            });
        match removed {
            Ok(0) => self.say(&pmt_i18n::t!(
                strings,
                "gateway.not_linked",
                platform = platform
            )),
            Ok(_) => self.say(&pmt_i18n::t!(
                strings,
                "gateway.unlinked",
                platform = platform
            )),
            Err(err) => tracing::warn!("identity unlink failed: {err}"),
        }
    }
}
