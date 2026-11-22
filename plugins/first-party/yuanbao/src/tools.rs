//! The `yuanbao` toolset's calls, run by the adapter (they need its
//! connection): the host sends `op = "tool"` with `{name, args}` as the
//! text and returns the JSON answer to the model. Arguments are checked
//! by the host against the tool schemas first (`limit` at most 50).

use adapter_kit::message::Outgoing;
use serde_json::{Value, json};

use crate::proto::{Fields, Writer};
use crate::{Yuanbao, element, stickers, text_element};

/// Member roles as the platform numbers them.
fn role(member: &Fields) -> &'static str {
    match member.int(6) {
        2 => "yuanbao_ai",
        3 => "bot",
        _ => match member.int(3) {
            1 => "admin",
            2 => "owner",
            _ => "member",
        },
    }
}

fn members(yb: &mut Yuanbao, group: &str) -> Result<Vec<Value>, String> {
    let answer = yb.request(
        "get_group_member_list",
        "gml",
        &Writer::default().string(1, group).int(3, 200),
    )?;
    Ok(answer
        .messages(3)
        .iter()
        .map(|m| {
            let card = m.string(5);
            json!({
                "user_id": m.string(1),
                "nickname": if card.is_empty() { m.string(2) } else { card },
                "role": role(m),
            })
        })
        .collect())
}

fn text(args: &Value, key: &str) -> Result<String, String> {
    args[key]
        .as_str()
        .filter(|v| !v.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("permanent: {key} is required"))
}

/// The chat a call without one acts in: the last chat a turn ran in.
fn current_chat(yb: &Yuanbao, args: &Value) -> Result<String, String> {
    args["chat"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| yb.typing.last().map(|(c, ..)| c.clone()))
        .or_else(|| yb.seen_chat.clone())
        .ok_or_else(|| "permanent: no Yuanbao conversation to send in (pass chat)".to_owned())
}

fn group_info(yb: &mut Yuanbao, args: &Value) -> Result<Value, String> {
    let group = text(args, "group_code")?;
    let answer = yb.request(
        "query_group_info",
        "qgi",
        &Writer::default().string(1, &group),
    )?;
    let info = answer.message(3);
    Ok(json!({
        "group_code": group,
        "group_name": info.string(1),
        "owner": {"user_id": info.string(2), "nickname": info.string(3)},
        "member_count": info.int(4),
    }))
}

fn group_members(yb: &mut Yuanbao, args: &Value) -> Result<Value, String> {
    let group = text(args, "group_code")?;
    let all = members(yb, &group)?;
    let listed: Vec<Value> = match args["action"].as_str().unwrap_or("list_all") {
        "list_bots" => all
            .into_iter()
            .filter(|m| matches!(m["role"].as_str(), Some("bot" | "yuanbao_ai")))
            .collect(),
        "find" => {
            let name = args["name"].as_str().unwrap_or_default().to_lowercase();
            all.into_iter()
                .filter(|m| {
                    m["nickname"]
                        .as_str()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&name)
                })
                .collect()
        }
        "list_all" => all,
        other => {
            return Err(format!(
                "permanent: unknown action '{other}' (find, list_bots, list_all)"
            ));
        }
    };
    Ok(json!({"count": listed.len(), "members": listed}))
}

fn send_dm(yb: &mut Yuanbao, args: &Value) -> Result<Value, String> {
    let name = text(args, "name")?;
    let body = text(args, "text")?;
    let group = args["group_code"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            yb.seen_chat
                .as_deref()
                .and_then(|c| c.strip_prefix("group:"))
                .map(str::to_owned)
        })
        .ok_or("permanent: group_code is required to find the member")?;
    let wanted = name.to_lowercase();
    let found: Vec<Value> = members(yb, &group)?
        .into_iter()
        .filter(|m| {
            m["nickname"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains(&wanted)
        })
        .collect();
    let member = match found.as_slice() {
        [one] => one.clone(),
        [] => {
            return Err(format!(
                "permanent: no member of group {group} is called '{name}'"
            ));
        }
        many => {
            return Err(format!(
                "permanent: {} members match '{name}'; be more specific",
                many.len()
            ));
        }
    };
    let account = member["user_id"].as_str().unwrap_or_default().to_owned();
    let mut elements = vec![text_element(&body)];
    if let Some(sticker) = args["media"].as_str().and_then(stickers::find) {
        elements.push(element(
            "TIMFaceElem",
            &Writer::default().string(4, &stickers::face_data(sticker)),
        ));
    }
    let id = yb.send_body(&format!("direct:{account}"), &elements)?;
    Ok(json!({"sent": true, "to": member, "message_id": id}))
}

fn search_sticker(args: &Value) -> Value {
    let limit = usize::try_from(args["limit"].as_u64().unwrap_or(10)).unwrap_or(10);
    let found = stickers::search(args["query"].as_str().unwrap_or_default(), limit);
    let results: Vec<Value> = found
        .iter()
        .map(|(name, id, description)| json!({"sticker_id": id, "name": name, "description": description}))
        .collect();
    json!({"count": results.len(), "results": results})
}

fn send_sticker(yb: &mut Yuanbao, args: &Value) -> Result<Value, String> {
    let wanted = text(args, "sticker")?;
    let sticker = stickers::find(&wanted)
        .ok_or_else(|| format!("permanent: no sticker '{wanted}' (search first)"))?;
    let chat = current_chat(yb, args)?;
    let face = element(
        "TIMFaceElem",
        &Writer::default().string(4, &stickers::face_data(sticker)),
    );
    let id = yb.send_body(&chat, &[face])?;
    Ok(json!({"sent": true, "sticker": sticker.0, "chat": chat, "message_id": id}))
}

/// Runs one tool call: its JSON answer.
pub(crate) fn call(yb: &mut Yuanbao, out: &Outgoing) -> Result<String, String> {
    let call: Value = serde_json::from_str(&out.text).map_err(|e| format!("permanent: {e}"))?;
    let args = &call["args"];
    let answer = match call["name"].as_str().unwrap_or_default() {
        "yb_query_group_info" => group_info(yb, args)?,
        "yb_query_group_members" => group_members(yb, args)?,
        "yb_send_dm" => send_dm(yb, args)?,
        "yb_search_sticker" => search_sticker(args),
        "yb_send_sticker" => send_sticker(yb, args)?,
        other => return Err(format!("permanent: unknown Yuanbao tool '{other}'")),
    };
    Ok(answer.to_string())
}
