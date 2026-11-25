//! Meeting invitations (`vc.bot.meeting_invited_v1`): an invitation with
//! an inviter and a meeting number becomes a turn in the inviter's
//! direct chat (their open id), subject to the same authorization as any
//! direct message; one without either is ignored.

use adapter_kit::message::Inbound;
use serde_json::Value;

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// The turn an invitation event starts (`event_id` for its message id).
#[must_use]
pub fn turn(event: &Value, event_id: &str) -> Option<Inbound> {
    let meeting = &event["meeting"];
    let inviter = &event["inviter"];
    let number = text(&meeting["meeting_no"]);
    let open_id = text(&inviter["id"]["open_id"]);
    if number.is_empty() || open_id.is_empty() {
        return None;
    }
    let name = |user: &Value| {
        Some(text(&user["user_name"]))
            .filter(|n| !n.is_empty())
            .unwrap_or("unknown")
            .to_owned()
    };
    let topic = Some(text(&meeting["topic"])).filter(|t| !t.is_empty());
    let prompt = format!(
        "You have been invited to a Feishu meeting: {}\n\nMeeting number: {number}\nTopic: {}\nInviter: {}\nHost: {}\n\n\
         Join it if you have a way to (a meeting skill or tool); otherwise tell the inviter briefly why you cannot.",
        topic.unwrap_or(number),
        topic.unwrap_or("unknown"),
        name(inviter),
        name(&meeting["host_user"]),
    );
    let id = if event_id.is_empty() {
        format!("invite-{}-{number}", text(&meeting["id"]))
    } else {
        format!("invite-{event_id}")
    };
    let mut message = Inbound::text(&id, open_id, "dm", open_id, &prompt);
    message.sender_name = Some(name(inviter));
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_invitation_needs_an_inviter_and_a_meeting_number() {
        let event = json!({
            "meeting": {"id": "m1", "topic": "Weekly", "meeting_no": "123456789", "host_user": {"user_name": "Ana"}},
            "inviter": {"id": {"open_id": "ou_ana"}, "user_name": "Ana"},
        });
        let turn = turn(&event, "ev1").unwrap();
        assert_eq!(
            (turn.chat.as_str(), turn.sender.as_str()),
            ("ou_ana", "ou_ana")
        );
        assert!(
            turn.text.contains("Meeting number: 123456789"),
            "{}",
            turn.text
        );
        assert_eq!(turn.id, "invite-ev1");
        let mut no_number = event.clone();
        no_number["meeting"]["meeting_no"] = json!("");
        assert!(super::turn(&no_number, "ev2").is_none());
        let mut no_inviter = event;
        no_inviter["inviter"] = json!({});
        assert!(super::turn(&no_inviter, "ev3").is_none());
    }
}
