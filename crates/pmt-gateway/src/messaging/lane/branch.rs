//! `/branch [--here] [name]` (alias `/fork`) in a chat (spec:
//! messaging-gateway, Branching into a sibling thread): the session is
//! forked (lineage `fork`). Where the adapter opens threads the branch
//! gets a new sibling thread titled `name` (or the parent's title plus
//! " (branch)") bound to the fork, and this conversation keeps the
//! original session; with `--here`, or where threads cannot be opened,
//! this conversation switches to the fork instead. Refused while a turn
//! runs.

use pmt_rpc::contracts::ForkParams;
use pmt_store::Origin;
use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};
use pmt_store::gateway::routes::NewRoute;

use super::Lane;
use crate::messaging::adapter::SendError;
use crate::messaging::key::{Isolation, Routing};

/// What `/branch` answers while a turn runs.
pub(crate) const BRANCH_BUSY: &str =
    "/branch is unavailable while the agent is working; use /stop first";

/// `--here` and the branch's name from the arguments.
fn parse(args: &str) -> (bool, Option<String>) {
    let mut here = false;
    let mut words = Vec::new();
    for word in args.split_whitespace() {
        if word == "--here" {
            here = true;
        } else {
            words.push(word);
        }
    }
    let name = Some(words.join(" ")).filter(|n| !n.is_empty());
    (here, name)
}

impl Lane {
    /// `/branch [--here] [name]`.
    pub(super) async fn branch_command(&mut self, routing: &Routing, args: &str) {
        if self.running.is_some() {
            self.say(BRANCH_BUSY);
            return;
        }
        let (here, name) = parse(args);
        let fork = match self.fork_current(routing).await {
            Ok(fork) => fork,
            Err(message) => {
                self.say(&message);
                return;
            }
        };
        if !here {
            let title = name.unwrap_or_else(|| self.branch_title());
            match self.open_branch_thread(routing, &fork, &title).await {
                Ok(()) => return,
                Err(SendError::Unsupported) => {}
                Err(err) => {
                    self.say(&format!("No thread for the branch: {}", err.describe()));
                    return;
                }
            }
        }
        self.set_fork_origin(&fork, routing);
        match fork.parse() {
            Ok(id) => match self.route_to(routing, id) {
                Ok(()) => self.say(&format!(
                    "Switched to the branch {fork}; /resume goes back to the original."
                )),
                Err(err) => self.say(err.message()),
            },
            Err(err) => self.say(err.message()),
        }
    }

    /// Forks the conversation's session; the fork's id or the refusal.
    async fn fork_current(&mut self, routing: &Routing) -> Result<String, String> {
        let session = self
            .ensure_session(routing)
            .await
            .map_err(|e| e.message().to_owned())?;
        let params = ForkParams {
            session_id: session,
            at_message_id: None,
            title: None,
        };
        self.shared
            .agents
            .fork(&self.profile, params)
            .await
            .map(|r| r.session_id)
            .map_err(|e| e.message().to_owned())
    }

    /// The parent's title plus " (branch)".
    fn branch_title(&self) -> String {
        let title = self
            .shared
            .store(&self.profile)
            .zip(self.session.as_deref().and_then(|s| s.parse().ok()))
            .and_then(|(store, id)| store.session(id).ok())
            .and_then(|s| s.title);
        format!("{} (branch)", title.as_deref().unwrap_or("Conversation"))
    }

    /// Opens a sibling thread for `fork` and binds it there.
    async fn open_branch_thread(
        &mut self,
        routing: &Routing,
        fork: &str,
        title: &str,
    ) -> Result<(), SendError> {
        let adapter = self
            .shared
            .adapters
            .adapter(&self.profile, &routing.platform)
            .ok_or(SendError::Unsupported)?;
        if !adapter.describe().supports_threads {
            return Err(SendError::Unsupported);
        }
        let thread = adapter.open_thread(&routing.chat_id, title).await?;
        let branch = Routing {
            thread_id: Some(thread.clone()),
            ..routing.clone()
        };
        self.set_fork_origin(fork, &branch);
        let bound = self.bind_elsewhere(&branch, fork);
        if let Err(err) = bound {
            return Err(SendError::Permanent(err.message().to_owned()));
        }
        self.post_in(
            &branch,
            fork,
            &format!("Branch \"{title}\": this thread continues a copy of the conversation."),
        );
        self.say(&format!(
            "Branch \"{title}\" opened in a new thread; this one continues the original session."
        ));
        Ok(())
    }

    /// Routes `routing`'s conversation (not this lane's) to `session`.
    pub(super) fn bind_elsewhere(
        &self,
        routing: &Routing,
        session: &str,
    ) -> Result<(), pmt_core::error::Error> {
        let store = self.shared.store(&self.profile).ok_or_else(|| {
            pmt_core::error::Error::new(
                &pmt_core::error::codes::PROFILE_NOT_FOUND,
                "the profile is not served",
            )
        })?;
        let (group_per_user, thread_per_user) = self.config().map_or((true, false), |s| {
            s.config().gateway.sessions_for(&routing.platform)
        });
        let key = routing.key(Isolation {
            group_per_user,
            thread_per_user,
        });
        store.bind_route(
            &NewRoute {
                key: key.as_str().to_owned(),
                session_id: session.parse()?,
                platform: routing.platform.clone(),
                chat_id: routing.chat_id.clone(),
                thread_id: routing.thread_id.clone(),
            },
            None,
        )
    }

    /// Sends `text` to `routing`'s chat and thread (not this lane's).
    pub(super) fn post_in(&self, routing: &Routing, session: &str, text: &str) {
        let new = NewOutbound {
            idempotency_key: format!("system:{}", crate::tickets::random_hex(8)),
            session_id: Some(session.to_owned()),
            platform: routing.platform.clone(),
            chat_id: routing.chat_id.clone(),
            thread_id: routing.thread_id.clone(),
            reply_to: None,
            edit_of: None,
            kind: OutboundKind::System,
            body: text.to_owned(),
            attachments: Vec::new(),
        };
        self.shared.deliver(&self.profile, &new);
    }

    /// The fork now lives in `routing`'s chat and thread.
    fn set_fork_origin(&self, fork: &str, routing: &Routing) {
        let origin = Origin::Gateway {
            platform: routing.platform.clone(),
            chat: routing.chat_id.clone(),
            thread: routing.thread_id.clone(),
            user: Some(routing.sender_id.clone()),
        };
        if let (Some(store), Ok(id)) = (self.shared.store(&self.profile), fork.parse())
            && let Err(err) = store.set_origin(id, &origin)
        {
            tracing::warn!("the branch's origin was not recorded: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn here_and_the_name_come_from_the_arguments() {
        assert_eq!(parse("try-postgres"), (false, Some("try-postgres".into())));
        assert_eq!(parse("--here"), (true, None));
        assert_eq!(parse("a plan --here"), (true, Some("a plan".into())));
        assert_eq!(parse(""), (false, None));
    }
}
