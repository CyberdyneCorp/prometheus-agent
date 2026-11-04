//! What an MCP server's tools cost (spec: plugin-system, MCP tool costs
//! and spend limits).
//!
//! `tool_costs` maps a tool to a number or `{credits = [min, max], note}`
//! in `cost_unit`. A costed call's approval prompt states its cost and the
//! conversation's running total; a call that would take the total past
//! `spend_limit` asks once-only approval whatever the trust or YOLO says
//! (and is refused, telling the model not to retry, where no one can
//! answer). The total is kept by the host per conversation (it follows
//! compression and branches); a call adds its upper cost once it ran.

use std::collections::BTreeMap;

use serde_json::Value;

/// One tool's cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    /// The lowest it costs.
    pub low: f64,
    /// The most it costs (what the total is charged).
    pub high: f64,
    /// What the server says about it.
    pub note: Option<String>,
}

/// A server's costs.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Costs {
    /// Per tool (the server's own names).
    pub tools: BTreeMap<String, Cost>,
    /// The conversation's limit on this server (`None`: none).
    pub limit: Option<f64>,
    /// `credits`, `USD`...
    pub unit: String,
}

impl Costs {
    /// The costs of `[mcp.servers.<name>]`'s `tool_costs`, `spend_limit`
    /// and `cost_unit`.
    ///
    /// # Errors
    ///
    /// `tool_costs.<tool>: ...` for the first malformed entry (the server
    /// is skipped).
    pub fn parse(
        tool_costs: &BTreeMap<String, Value>,
        spend_limit: f64,
        unit: &str,
    ) -> Result<Self, String> {
        let mut tools = BTreeMap::new();
        for (tool, value) in tool_costs {
            let cost = cost(value).map_err(|why| format!("tool_costs.{tool}: {why}"))?;
            tools.insert(tool.clone(), cost);
        }
        if !spend_limit.is_finite() || spend_limit < 0.0 {
            return Err("spend_limit: must be 0 or more".to_owned());
        }
        Ok(Self {
            tools,
            limit: (spend_limit > 0.0).then_some(spend_limit),
            unit: if unit.is_empty() { "credits" } else { unit }.to_owned(),
        })
    }

    /// `tool`'s cost, if it has one.
    #[must_use]
    pub fn of(&self, tool: &str) -> Option<&Cost> {
        self.tools.get(tool)
    }

    /// `about 20 credits`, `10-30 credits (per page)`.
    #[must_use]
    pub fn describe(&self, cost: &Cost) -> String {
        let amount = if (cost.high - cost.low).abs() < f64::EPSILON {
            format!("about {} {}", number(cost.high), self.unit)
        } else {
            format!("{}-{} {}", number(cost.low), number(cost.high), self.unit)
        };
        match &cost.note {
            Some(note) => format!("{amount} ({note})"),
            None => amount,
        }
    }

    /// `90 credits used in this conversation (limit 100)`.
    #[must_use]
    pub fn total_line(&self, spent: f64) -> String {
        let limit = self
            .limit
            .map(|l| format!(" (limit {})", number(l)))
            .unwrap_or_default();
        format!(
            "{} {} used in this conversation{limit}",
            number(spent),
            self.unit
        )
    }

    /// Whether a call costing `cost` after `spent` crosses the limit.
    #[must_use]
    pub fn crosses(&self, spent: f64, cost: &Cost) -> bool {
        self.limit.is_some_and(|limit| spent + cost.high > limit)
    }
}

fn cost(value: &Value) -> Result<Cost, String> {
    let non_negative = |n: Option<f64>| n.filter(|n| n.is_finite() && *n >= 0.0);
    if let Some(n) = non_negative(value.as_f64()) {
        return Ok(Cost {
            low: n,
            high: n,
            note: None,
        });
    }
    let Some(table) = value.as_object() else {
        return Err("expected a number or { credits = [min, max], note }".to_owned());
    };
    if let Some(key) = table
        .keys()
        .find(|k| !matches!(k.as_str(), "credits" | "note"))
    {
        return Err(format!("unknown key '{key}'"));
    }
    let range = table
        .get("credits")
        .and_then(Value::as_array)
        .filter(|r| r.len() == 2)
        .ok_or("credits: expected [min, max]")?;
    let (Some(low), Some(high)) = (
        non_negative(range[0].as_f64()),
        non_negative(range[1].as_f64()),
    ) else {
        return Err("credits: expected two numbers of 0 or more".to_owned());
    };
    if low > high {
        return Err("credits: min is above max".to_owned());
    }
    let note = match table.get("note") {
        None => None,
        Some(Value::String(note)) => Some(note.clone()),
        Some(_) => return Err("note: expected a string".to_owned()),
    };
    Ok(Cost { low, high, note })
}

/// `20`, `2.5`.
fn number(n: f64) -> String {
    let text = format!("{n:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn costs(entries: Value, limit: f64) -> Result<Costs, String> {
        let map: BTreeMap<String, Value> = serde_json::from_value(entries).unwrap();
        Costs::parse(&map, limit, "credits")
    }

    #[test]
    fn numbers_and_ranges_are_described_and_the_limit_crossing_found() {
        let c = costs(
            json!({"create_image": 20, "render": {"credits": [10, 30], "note": "per page"}}),
            100.0,
        )
        .unwrap();
        let image = c.of("create_image").unwrap();
        assert_eq!(c.describe(image), "about 20 credits");
        assert_eq!(
            c.describe(c.of("render").unwrap()),
            "10-30 credits (per page)"
        );
        assert_eq!(
            c.total_line(90.0),
            "90 credits used in this conversation (limit 100)"
        );
        assert!(c.crosses(90.0, image));
        assert!(!c.crosses(80.0, image));
    }

    #[test]
    fn a_malformed_entry_names_its_key() {
        let err = costs(json!({"x": "ten"}), 0.0).unwrap_err();
        assert!(err.starts_with("tool_costs.x: "), "{err}");
        let err = costs(json!({"y": {"credits": [5]}}), 0.0).unwrap_err();
        assert!(err.starts_with("tool_costs.y: credits"), "{err}");
        assert!(costs(json!({}), 0.0).unwrap().limit.is_none());
    }
}
