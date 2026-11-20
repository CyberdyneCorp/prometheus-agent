# Bot Mode

Bot Mode shows your profiles as a roster of named bots. A bot is exactly a
profile: its config, secrets, skills, memory and sessions are the
profile's own, and everything is reachable from the terminal. The daemon
must serve every profile (`gateway.multiplex = true`).

## Bots and their Bot Chat

Each bot has one canonical Bot Chat. The desktop opens it with
`bot.chat.ensure`; the terminal opens the same chat:

```text
prometheus -p research chat --bot-chat
```

Inside a Bot Chat `/new` and `/reset` compress the chat in place instead
of starting a new session, and the idle auto-archive never archives it.
An explicit archive retires it; the next open creates a new one.

A bot's metadata (title, description, avatar, hidden flag, roster
section) lives in its profile's `state.db`, so every client of the daemon
shows the same bot. Read and change it with `bot.meta.get` and
`bot.meta.update`, for example from the terminal:

```text
prometheus rpc call bot.meta.update '{"profile":"research","changes":{"title":"Research Buddy"}}'
```

Avatars are `blob` (the default face, derived from the bot's name),
`geometric`, `image` (PNG, JPEG or WebP up to 5 MiB, kept in the
profile's blobs), `generated` (drawn by the profile's image backend, then
kept as `image`) or `pet`. The roster names an `image` avatar by its blob;
clients read the picture with `bot.avatar.get`, and draw a `pet` avatar
from the companion's first idle frame (`pets.sprite`). The desktop's
bot editor uploads an avatar picture.

Every call names the profile it acts for, and the connection's grant
must cover every profile it acts on: a `clone_from` source, each local
member `groups.create` adds, the member `groups.grant` is for, and the
local members of any room it reads or drives (rooms with one outside the
grant are not listed). `POST /api/bots` creates the bot for the API
key's profile, which may clone only itself.

`bot.create` makes a new bot (fresh, cloned from another profile, or
without bundled skills) with its Bot Chat and an introduction. Static API
keys (`*_KEY` secrets) of the source are copied unless `copy_api_keys` is
false; subscription sign-ins (`OAUTH_*`) and gateway-owned secrets never
are. With `on` naming a registered peer the bot is created on that
machine. `bot.duplicate` (config, skills, `SOUL.md`, memory and
metadata, an image avatar included) and `bot.delete` (never `default`)
complete the set.

## The Bots pane

The desktop's **Bots** pane lists the connection's bots with their
avatar, latest message, unread count and an active dot (a running turn, a
Bot Chat message in the last 90 seconds, or a kanban worker of that bot
that heartbeat in the last two minutes), grouped under the section stored
on each bot. **Edit** changes a bot's title, description,
section and hidden flag (a hidden bot leaves the default roster but can
still be messaged; **Show hidden** brings it back), duplicates or deletes
it, and manages its routines. **Delete section** moves the section's bots
to *Unassigned*; it never deletes a bot. **New bot** runs `bot.create`.
Opening a bot opens its Bot Chat and marks it read.

## Messaging other bots

A canonical Bot Chat has the [`message_agent`](../reference/tools.md#message_agent)
tool and a protocol section listing its teammates: the other local bots,
each registered peer's agents (`@<peer>/<agent>`, from the daemon's peer
listing, refreshed every five minutes) and the agents a desktop reported
for its other connections (`@<agent>@<connection>`), each with its title,
role and machine. A target is a profile
name, the `@`-tag of a title (`@research-buddy`), `@prometheus` for the
default profile, `<peer>/<agent>` for a peer gateway, or
`<handle>@<connection>` for another desktop connection. The tool returns
`queued` at once; the reply, or the failure as `[reason: <code>]`,
arrives later in the Bot Chat. A message waits at most
`bot_mode.turn_wait_secs` for a busy target (`target_busy`), and a chain of
bot messages stops after `bot_mode.max_hops` hops (`hop_limit`).
Transient failures are retried once; auth, quota and configuration
failures never are.

Roster changes reach an open Bot Chat as one `roster.update` notice with
the next turn, never by rebuilding its prompt. An `@handle` you write in
any chat is resolved and named to the agent; your text is never
forwarded on its own.

```console
$ prometheus config get bot_mode.max_hops
4
$ prometheus config get bot_mode.turn_wait_secs
120
```

`bot_mode.enabled = false` takes a profile off every roster and refuses
messages to it with `missing_config`; its sessions, routines and room
history stay.

## Routines and voices

A routine is an ordinary scheduled job of the bot named
`[bot:<name>] <routine>` delivering to `bot_chat`:

```text
prometheus -p inbox cron add "0 8 * * *" --prompt "digest my mail" --name "[bot:inbox] morning digest" --deliver bot_chat --yes
```

`bot.routines.list` lists them for the routines pane, whose schedule
picker (every day, every weekday or every week at a time, every few hours
or minutes) writes only cron expressions and `every <n>h|m` intervals,
the same strings `prometheus cron` accepts. Reading a message
aloud (`voice.speak`) uses the `tts.*` settings of the profile that owns
the chat, so each bot speaks with its own voice.

## Group chats

A room has 2 to 6 member bots (`groups.create`); each member keeps its own
room session, `Group: <room> · main`. A message (`groups.post`) drives up
to `hosted_rooms.discussion.max_rounds` rounds and
`hosted_rooms.discussion.max_messages` member messages; members reply or
answer `[PASS]`, and a silent round settles the room. `@builder stop`
(also `halt`, `pause`) holds a member until it is mentioned again or the
room is addressed with `@all`; it then hears every message it missed.
The daemon drives rooms under a lease in `state.db`, so closing every
client never stops a discussion; read the room with `groups.log`.

A room with members on other gateways is driven by its authority. The
member gateway's owner issues a grant (`groups.grant`) that the
authority's owner keeps with `groups.member_grant`; without it the member
is unreachable (`missing_config`). The authority's runs and their state
(`/api/rooms/<id>/turns[/<run>]`) need that grant; a room's log
(`/api/rooms/<id>/log`) is served to the key of the profile holding the
home's rooms (`default`) or of a local member. Other gateways keep a
replica with `groups.replicate`. To move the authority, fence the old writer first,
then `groups.promote` the replacement with `confirm: true` and
`groups.demote` the old one: histories are never merged.
