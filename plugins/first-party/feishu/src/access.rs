//! Who may talk to the bot: `dm_policy` over `allow_from` for direct
//! chats, and for groups `group_policy` (`allowlist` by default, `open`,
//! `disabled`) over `group_allow_from`, overridden per chat by
//! `group_rules.<chat_id> = {policy, allowlist, blacklist,
//! require_mention}` whose policy may also be `blacklist` or
//! `admin_only`. `admins` pass every group rule. A group allowlist with no
//! entries admits nobody; the first message it drops in a chat logs one
//! WARN for that chat, so a fresh setup that hears nothing says why.

use adapter_kit::host;
use adapter_kit::policy::{self, Policy, Verdict};
use serde_json::Value;

/// A group rule's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// Everyone in the chat.
    Open,
    /// Only listed senders.
    Allowlist,
    /// Everyone but listed senders.
    Blacklist,
    /// Only `admins`.
    AdminOnly,
    /// Nobody.
    Disabled,
}

impl Group {
    fn parse(text: &str, key: &str) -> Result<Self, String> {
        Ok(match text {
            "open" => Self::Open,
            "allowlist" => Self::Allowlist,
            "blacklist" => Self::Blacklist,
            "admin_only" => Self::AdminOnly,
            "disabled" => Self::Disabled,
            other => {
                return Err(format!(
                    "{key}: '{other}' is not one of open, allowlist, blacklist, admin_only, disabled"
                ));
            }
        })
    }
}

/// One chat's rule.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    chat: String,
    policy: Group,
    allowlist: Vec<String>,
    blacklist: Vec<String>,
    require_mention: Option<bool>,
}

/// The access settings.
#[derive(Debug, Clone)]
pub struct Access {
    dm: Policy,
    allow_from: Vec<String>,
    group: Group,
    group_allow_from: Vec<String>,
    admins: Vec<String>,
    require_mention: bool,
    rules: Vec<Rule>,
    warned: Vec<String>,
}

/// What became of a group message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Admitted.
    Admit,
    /// Dropped by the chat's policy.
    Denied,
    /// Dropped: the chat wants a mention and has none.
    Unmentioned,
}

impl Access {
    /// The settings in `options`.
    ///
    /// # Errors
    ///
    /// A policy that is not one.
    pub fn new(options: &Value) -> Result<Self, String> {
        let group = match options["group_policy"].as_str() {
            None => Group::Allowlist,
            Some(p @ ("open" | "allowlist" | "disabled")) => Group::parse(p, "")?,
            Some(other) => {
                return Err(format!(
                    "options.group_policy: '{other}' is not one of allowlist, open, disabled"
                ));
            }
        };
        let mut rules = Vec::new();
        for (chat, rule) in options["group_rules"].as_object().into_iter().flatten() {
            let key = format!("options.group_rules.{chat}.policy");
            rules.push(Rule {
                chat: chat.clone(),
                policy: Group::parse(rule["policy"].as_str().unwrap_or("allowlist"), &key)?,
                allowlist: policy::strings(rule, "allowlist"),
                blacklist: policy::strings(rule, "blacklist"),
                require_mention: rule["require_mention"].as_bool(),
            });
        }
        Ok(Self {
            dm: Policy::option(options, "dm_policy", Policy::Pairing)?,
            allow_from: policy::strings(options, "allow_from"),
            group,
            group_allow_from: policy::strings(options, "group_allow_from"),
            admins: policy::strings(options, "admins"),
            require_mention: options["require_mention"].as_bool().unwrap_or(true),
            rules,
            warned: Vec::new(),
        })
    }

    /// The verdict on a direct-chat sender.
    #[must_use]
    pub fn dm(&self, sender: &str) -> Verdict {
        self.dm.verdict(&self.allow_from, sender)
    }

    fn rule(&self, chat: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.chat == chat)
    }

    /// Whether `sender` may speak in group `chat` (mentions aside).
    #[must_use]
    pub fn group_allows(&self, chat: &str, sender: &str) -> bool {
        let listed = |list: &[String]| list.iter().any(|a| a == sender || a == "*");
        if listed(&self.admins) {
            return true;
        }
        let (policy, allowlist, blacklist) = match self.rule(chat) {
            Some(rule) => (rule.policy, &rule.allowlist[..], &rule.blacklist[..]),
            None => (self.group, &self.group_allow_from[..], &[][..]),
        };
        match policy {
            Group::Open => true,
            Group::Allowlist => listed(allowlist),
            Group::Blacklist => !listed(blacklist),
            Group::AdminOnly | Group::Disabled => false,
        }
    }

    /// Decides on a group message, logging the one WARN per chat an empty
    /// allowlist earns.
    pub fn group(&mut self, chat: &str, sender: &str, mentioned: bool) -> Admission {
        if !self.group_allows(chat, sender) {
            self.warn_empty(chat);
            return Admission::Denied;
        }
        let wants = self
            .rule(chat)
            .and_then(|r| r.require_mention)
            .unwrap_or(self.require_mention);
        if wants && !mentioned {
            return Admission::Unmentioned;
        }
        Admission::Admit
    }

    fn warn_empty(&mut self, chat: &str) {
        let empty = match self.rule(chat) {
            Some(rule) => rule.policy == Group::Allowlist && rule.allowlist.is_empty(),
            None => self.group == Group::Allowlist && self.group_allow_from.is_empty(),
        };
        if !empty || self.warned.iter().any(|c| c == chat) {
            return;
        }
        self.warned.push(chat.to_owned());
        host::warn(&format!(
            "Feishu group {chat}: dropped a message because the group allowlist is empty \
             (group_policy = \"allowlist\"); list senders in options.group_allow_from or \
             group_rules.{chat}.allowlist, or set group_policy = \"open\""
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn group_rules_override_the_default_policy() {
        let options = json!({
            "group_allow_from": ["ou_a"],
            "admins": ["ou_admin"],
            "group_rules": {
                "oc_open": {"policy": "open", "require_mention": false},
                "oc_black": {"policy": "blacklist", "blacklist": ["ou_b"]},
                "oc_admins": {"policy": "admin_only"},
            },
        });
        let mut access = Access::new(&options).unwrap();
        assert_eq!(access.group("oc_x", "ou_a", true), Admission::Admit);
        assert_eq!(access.group("oc_x", "ou_a", false), Admission::Unmentioned);
        assert_eq!(access.group("oc_x", "ou_c", true), Admission::Denied);
        assert_eq!(access.group("oc_open", "ou_c", false), Admission::Admit);
        assert_eq!(access.group("oc_black", "ou_b", true), Admission::Denied);
        assert_eq!(access.group("oc_black", "ou_c", true), Admission::Admit);
        assert_eq!(access.group("oc_admins", "ou_a", true), Admission::Denied);
        assert_eq!(
            access.group("oc_admins", "ou_admin", true),
            Admission::Admit
        );
        assert!(Access::new(&json!({"group_policy": "blacklist"})).is_err());
        assert_eq!(access.dm("ou_z"), Verdict::Unknown);
    }
}
