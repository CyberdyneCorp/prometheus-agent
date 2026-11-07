//! Typed blueprint slots: what a value may be and how it reads in a
//! schedule and in a prompt (spec: scheduled-jobs, Automation blueprint
//! catalog). `time` is `HH:MM` (or `8am`, `6:30pm`); `weekdays` is a preset
//! (`everyday`, `weekdays`, `weekends`) or day names (`mon,wed`); `enum` is
//! one of its options unless it is not strict; `text` is any non-empty
//! text.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What a slot holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    /// A time of day.
    Time,
    /// One of `options` (any text when not `strict`).
    Enum,
    /// Free text.
    Text,
    /// Days of the week.
    Weekdays,
}

/// One slot of a blueprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    /// The name `{name}` placeholders and `name=value` use.
    pub name: String,
    /// What it holds.
    pub kind: SlotKind,
    /// The question asked for it.
    #[serde(default)]
    pub question: String,
    /// The options of an `enum`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// An `enum` accepts only its options.
    #[serde(default = "yes")]
    pub strict: bool,
    /// The value used when none is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Asked for when it has no value and no default.
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
}

/// The presets of a `weekdays` slot.
pub const WEEKDAY_PRESETS: [&str; 3] = ["everyday", "weekdays", "weekends"];

/// A checked value: how it reads in a prompt and its schedule fields
/// (`{slot}` is the schedule form, `{slot.hour}`...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    /// The prompt form.
    pub display: String,
    /// The schedule forms by field (`""` is `{slot}` itself).
    pub fields: BTreeMap<&'static str, String>,
}

impl Value {
    fn plain(text: &str) -> Self {
        Self {
            display: text.to_owned(),
            fields: BTreeMap::from([("", text.to_owned())]),
        }
    }
}

/// Why a value does not fit its slot.
pub type Invalid = String;

impl Slot {
    /// Checks `text` against the slot.
    ///
    /// # Errors
    ///
    /// What the slot expects.
    pub fn check(&self, text: &str) -> Result<Value, Invalid> {
        let text = text.trim();
        match self.kind {
            SlotKind::Time => time(text),
            SlotKind::Weekdays => weekdays(text),
            SlotKind::Enum => self.option(text),
            SlotKind::Text if text.is_empty() => Err("expected some text".to_owned()),
            SlotKind::Text => Ok(Value::plain(text)),
        }
    }

    fn option(&self, text: &str) -> Result<Value, Invalid> {
        let known = self.options.iter().find(|o| o.eq_ignore_ascii_case(text));
        match known {
            Some(option) => Ok(Value::plain(option)),
            None if !self.strict && !text.is_empty() => Ok(Value::plain(text)),
            None => Err(format!("expected one of {}", self.options.join(", "))),
        }
    }

    /// The question asked for it, with what it accepts.
    #[must_use]
    pub fn prompt(&self) -> String {
        let question = if self.question.is_empty() {
            format!("{}?", self.name)
        } else {
            self.question.clone()
        };
        let hint = match self.kind {
            SlotKind::Time => "HH:MM, e.g. 08:00".to_owned(),
            SlotKind::Weekdays => "everyday, weekdays, weekends or days like mon,wed".to_owned(),
            SlotKind::Enum if self.strict => self.options.join(", "),
            SlotKind::Enum => format!("e.g. {}", self.options.join(", ")),
            SlotKind::Text => "any text".to_owned(),
        };
        format!("{question} ({hint})")
    }
}

/// `HH:MM`, `H:MM`, `8am`, `6:30pm`.
fn time(text: &str) -> Result<Value, Invalid> {
    let expected = || "expected a time of day as HH:MM (00:00-23:59)".to_owned();
    let lower = text.to_ascii_lowercase();
    let (clock, meridiem) = match lower
        .strip_suffix("am")
        .or_else(|| lower.strip_suffix("pm"))
    {
        Some(rest) => (rest.trim().to_owned(), Some(lower.ends_with("pm"))),
        None => (lower.clone(), None),
    };
    let (hour, minute) = clock.split_once(':').unwrap_or((clock.as_str(), "00"));
    let digits = |s: &str, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
    };
    if !digits(hour, 2) || !digits(minute, 2) || minute.len() != 2 {
        return Err(expected());
    }
    let (mut hour, minute): (u32, u32) = (
        hour.parse().map_err(|_| expected())?,
        minute.parse().map_err(|_| expected())?,
    );
    if let Some(pm) = meridiem {
        if !(1..=12).contains(&hour) {
            return Err(expected());
        }
        hour = match (pm, hour) {
            (false, 12) => 0,
            (true, 12) => 12,
            (true, h) => h + 12,
            (false, h) => h,
        };
    }
    if hour > 23 || minute > 59 {
        return Err(expected());
    }
    Ok(Value {
        display: format!("{hour:02}:{minute:02}"),
        fields: BTreeMap::from([
            ("", format!("{hour}:{minute:02}")),
            ("hour", hour.to_string()),
            ("minute", minute.to_string()),
        ]),
    })
}

/// The cron day number of a day name.
fn day(name: &str) -> Option<u8> {
    const DAYS: [(&str, &str); 7] = [
        ("sun", "sunday"),
        ("mon", "monday"),
        ("tue", "tuesday"),
        ("wed", "wednesday"),
        ("thu", "thursday"),
        ("fri", "friday"),
        ("sat", "saturday"),
    ];
    DAYS.iter()
        .position(|(short, long)| name == *short || name == *long)
        .and_then(|i| u8::try_from(i).ok())
}

/// A preset or a list of day names; the schedule form is a cron day field.
fn weekdays(text: &str) -> Result<Value, Invalid> {
    let lower = text.to_ascii_lowercase();
    let preset = match lower.as_str() {
        "everyday" | "daily" | "every day" => Some(("everyday", "*")),
        "weekdays" => Some(("weekdays", "1-5")),
        "weekends" => Some(("weekends", "0,6")),
        _ => None,
    };
    if let Some((name, field)) = preset {
        return Ok(Value {
            display: name.to_owned(),
            fields: BTreeMap::from([("", field.to_owned())]),
        });
    }
    let mut days = Vec::new();
    for name in lower.split([',', ' ']).filter(|n| !n.is_empty()) {
        let number = day(name).ok_or_else(|| {
            "expected everyday, weekdays, weekends or day names like mon,wed".to_owned()
        })?;
        if !days.contains(&number) {
            days.push(number);
        }
    }
    if days.is_empty() {
        return Err("expected everyday, weekdays, weekends or day names like mon,wed".to_owned());
    }
    days.sort_unstable();
    let field: Vec<String> = days.iter().map(u8::to_string).collect();
    let names: Vec<&str> = days
        .iter()
        .map(|d| ["sun", "mon", "tue", "wed", "thu", "fri", "sat"][usize::from(*d)])
        .collect();
    Ok(Value {
        display: names.join(", "),
        fields: BTreeMap::from([("", field.join(","))]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(kind: SlotKind) -> Slot {
        Slot {
            name: "x".to_owned(),
            kind,
            question: String::new(),
            options: vec!["1h".to_owned(), "6h".to_owned()],
            strict: true,
            default: None,
            required: true,
        }
    }

    #[test]
    fn times_read_in_24_and_12_hour_forms() {
        let time = slot(SlotKind::Time);
        for (text, display, hour, minute) in [
            ("08:00", "08:00", "8", "0"),
            ("8:30", "08:30", "8", "30"),
            ("6:30pm", "18:30", "18", "30"),
            ("12am", "00:00", "0", "0"),
            ("23:59", "23:59", "23", "59"),
        ] {
            let value = time.check(text).unwrap();
            assert_eq!(value.display, display, "{text}");
            assert_eq!(
                (
                    value.fields["hour"].as_str(),
                    value.fields["minute"].as_str()
                ),
                (hour, minute)
            );
        }
        for bad in ["25:00", "8:60", "13pm", "noon", "8:5", ""] {
            assert!(time.check(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn weekdays_take_presets_and_day_lists() {
        let days = slot(SlotKind::Weekdays);
        assert_eq!(days.check("weekdays").unwrap().fields[""], "1-5");
        assert_eq!(days.check("weekends").unwrap().fields[""], "0,6");
        assert_eq!(days.check("everyday").unwrap().fields[""], "*");
        let listed = days.check("Wed, mon friday").unwrap();
        assert_eq!(
            (listed.display.as_str(), listed.fields[""].as_str()),
            ("mon, wed, fri", "1,3,5")
        );
        assert!(days.check("someday").is_err());
    }

    #[test]
    fn strict_enums_take_only_their_options() {
        let mut choice = slot(SlotKind::Enum);
        assert_eq!(choice.check("6H").unwrap().display, "6h");
        assert!(choice.check("2h").is_err());
        choice.strict = false;
        assert_eq!(choice.check("2h").unwrap().display, "2h");
        assert!(slot(SlotKind::Text).check("  ").is_err());
    }
}
