//! The built-in blueprint catalog (spec: scheduled-jobs, Automation
//! blueprint catalog). The first four are also the curated starters of the
//! suggestions inbox ([`STARTERS`]).

use super::{Blueprint, Origin, Slot, SlotKind};

/// The curated starters: daily briefing, important-mail monitor, weekly
/// review, workday-start reminder.
pub const STARTERS: [&str; 4] = [
    "morning-brief",
    "important-mail",
    "weekly-review",
    "workday-start",
];

fn slot(name: &str, kind: SlotKind, question: &str, default: Option<&str>) -> Slot {
    Slot {
        name: name.to_owned(),
        kind,
        question: question.to_owned(),
        options: Vec::new(),
        strict: true,
        default: default.map(str::to_owned),
        required: true,
    }
}

fn time(default: &str) -> Slot {
    slot("time", SlotKind::Time, "At what time?", Some(default))
}

fn days(default: &str) -> Slot {
    slot("days", SlotKind::Weekdays, "On which days?", Some(default))
}

fn text(name: &str, question: &str, default: Option<&str>) -> Slot {
    slot(name, SlotKind::Text, question, default)
}

fn choice(name: &str, question: &str, options: &[&str], default: &str, strict: bool) -> Slot {
    Slot {
        options: options.iter().map(|o| (*o).to_owned()).collect(),
        strict,
        ..slot(name, SlotKind::Enum, question, Some(default))
    }
}

/// At `{time}` on `{days}`.
const DAILY: &str = "{time.minute} {time.hour} * * {days}";

/// One built-in.
fn blueprint(
    key: &str,
    (title, category, description): (&str, &str, &str),
    schedule: &str,
    prompt: &str,
    slots: Vec<Slot>,
) -> Blueprint {
    Blueprint {
        key: key.to_owned(),
        title: title.to_owned(),
        description: description.to_owned(),
        category: category.to_owned(),
        schedule_template: schedule.to_owned(),
        prompt_template: prompt.to_owned(),
        slots,
        origin: Origin::Builtin,
    }
}

/// Every built-in blueprint, in catalog order.
#[must_use]
pub fn catalog() -> Vec<Blueprint> {
    let mut all = starters();
    all.extend(watches());
    all.extend(routines());
    all
}

/// The curated starters and the reminder.
fn starters() -> Vec<Blueprint> {
    vec![
        blueprint(
            "morning-brief",
            (
                "Morning brief",
                "daily",
                "Calendar, important mail and news before the day starts.",
            ),
            DAILY,
            "Prepare my morning brief: today's calendar, important unread email, the weather and the news on {topics}. Keep it short.",
            vec![
                time("08:00"),
                days("weekdays"),
                text("topics", "Which news topics?", Some("my usual topics")),
            ],
        ),
        blueprint(
            "important-mail",
            (
                "Important-mail monitor",
                "monitor",
                "Watches the inbox and reports only what needs attention.",
            ),
            "every {interval}",
            "Check my inbox for new email that needs my attention ({focus}). Report each with sender, subject and why it matters; answer exactly [SILENT] when there is none.",
            vec![
                choice(
                    "interval",
                    "How often?",
                    &["15m", "30m", "1h", "2h"],
                    "30m",
                    true,
                ),
                text(
                    "focus",
                    "What counts as important?",
                    Some("people I work with, deadlines, payments"),
                ),
            ],
        ),
        blueprint(
            "weekly-review",
            (
                "Weekly review",
                "review",
                "Looks back at the week and plans the next one.",
            ),
            DAILY,
            "Run my weekly review: what I finished this week, what slipped, open commitments, and the three priorities for next week.",
            vec![time("17:00"), days("fri")],
        ),
        blueprint(
            "workday-start",
            (
                "Workday-start reminder",
                "daily",
                "A nudge to start the day with clear priorities.",
            ),
            DAILY,
            "It is the start of my workday. Remind me of today's meetings and my top three priorities, and ask what I want to finish first.",
            vec![time("09:00"), days("weekdays")],
        ),
        blueprint(
            "custom-reminder",
            (
                "Reminder",
                "reminder",
                "Reminds you of something on a schedule.",
            ),
            DAILY,
            "Remind me: {message}",
            vec![
                time("09:00"),
                days("everyday"),
                text("message", "What should I remind you of?", None),
            ],
        ),
    ]
}

/// Digests and watches.
fn watches() -> Vec<Blueprint> {
    vec![
        blueprint(
            "evening-winddown",
            (
                "Evening wind-down",
                "wellbeing",
                "Closes the day: what went well and what is set for tomorrow.",
            ),
            DAILY,
            "Help me wind down: summarise what I got done today, list anything left open for tomorrow and suggest one thing to let go of tonight.",
            vec![time("21:00"), days("everyday")],
        ),
        blueprint(
            "news-digest",
            (
                "News digest",
                "daily",
                "A digest of the news on your topics.",
            ),
            DAILY,
            "Give me a {depth} news digest on {topics}, with a link for each item.",
            vec![
                time("07:30"),
                days("everyday"),
                text("topics", "Which topics?", None),
                choice(
                    "depth",
                    "How detailed?",
                    &["brief", "detailed"],
                    "brief",
                    false,
                ),
            ],
        ),
        blueprint(
            "bill-renewal-watch",
            (
                "Bill and renewal watch",
                "monitor",
                "Warns about bills and renewals coming due.",
            ),
            DAILY,
            "Look for bills, subscriptions and renewals due in the next {horizon} days in my email and calendar. List each with amount and date; answer exactly [SILENT] when there is none.",
            vec![
                time("09:00"),
                days("mon"),
                choice(
                    "horizon",
                    "How many days ahead?",
                    &["3", "7", "14", "30"],
                    "7",
                    true,
                ),
            ],
        ),
        blueprint(
            "price-watch",
            (
                "Price watch",
                "monitor",
                "Watches the price of something you want.",
            ),
            "every {interval}",
            "Check the current price of {item}. Tell me when it is at or below {target}; otherwise answer exactly [SILENT].",
            vec![
                choice(
                    "interval",
                    "How often?",
                    &["1h", "6h", "12h", "1d"],
                    "6h",
                    true,
                ),
                text("item", "What should I watch (name or link)?", None),
                text("target", "At what price should I tell you?", None),
            ],
        ),
        blueprint(
            "competitor-watch",
            (
                "Competitor watch",
                "monitor",
                "Notable news and releases from competitors.",
            ),
            DAILY,
            "Check for notable news, releases and pricing changes from {competitors} since the last check. Answer exactly [SILENT] when there is nothing new.",
            vec![
                time("10:00"),
                days("mon"),
                text("competitors", "Which competitors?", None),
            ],
        ),
    ]
}

/// Wellbeing, home and learning routines.
fn routines() -> Vec<Blueprint> {
    vec![
        blueprint(
            "habit-checkin",
            (
                "Habit check-in",
                "wellbeing",
                "Asks how a habit went today.",
            ),
            DAILY,
            "Check in on my habit: {habit}. Ask whether I did it today and keep a short streak count in your notes.",
            vec![
                time("20:00"),
                days("everyday"),
                text("habit", "Which habit?", None),
            ],
        ),
        blueprint(
            "hydration-move",
            (
                "Hydration and movement",
                "wellbeing",
                "Regular nudges to drink water and move.",
            ),
            "every {interval}",
            "Remind me to drink a glass of water and move for two minutes. Keep it to one line.",
            vec![choice(
                "interval",
                "How often?",
                &["30m", "1h", "2h"],
                "1h",
                true,
            )],
        ),
        blueprint(
            "meal-plan",
            (
                "Meal plan",
                "home",
                "Plans the week's meals and the shopping list.",
            ),
            DAILY,
            "Plan my meals for the coming week ({diet}) and write the shopping list grouped by aisle.",
            vec![
                time("10:00"),
                days("sun"),
                text("diet", "Any diet or preferences?", Some("no restrictions")),
            ],
        ),
        blueprint(
            "learn-daily",
            (
                "Learn something daily",
                "learning",
                "A short daily lesson on a subject.",
            ),
            DAILY,
            "Teach me one short lesson about {subject}, building on the earlier ones, and end with one question to check I understood.",
            vec![
                time("08:30"),
                days("weekdays"),
                text("subject", "What do you want to learn?", None),
            ],
        ),
        blueprint(
            "gratitude-journal",
            (
                "Gratitude journal",
                "wellbeing",
                "An evening prompt to note three good things.",
            ),
            DAILY,
            "Ask me for three things I am grateful for today and keep them in a running journal in your notes.",
            vec![time("21:30"), days("everyday")],
        ),
        blueprint(
            "on-this-day",
            (
                "On this day",
                "learning",
                "A historical event that happened on today's date.",
            ),
            DAILY,
            "Tell me one interesting historical event that happened on today's date, in three sentences.",
            vec![time("09:00"), days("everyday")],
        ),
    ]
}
