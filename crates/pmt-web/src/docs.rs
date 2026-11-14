//! The embedded documentation (spec: web-dashboard, Embedded
//! documentation): the user guide and the generated reference of the
//! running build, compiled into the binary by `build.rs`, listed, served
//! and searched offline. A query naming a configuration key finds its row
//! in the generated configuration reference first, so `/docs?key=<key>`
//! (where the dashboard links every message that names a key) shows that
//! key's entry.

use serde_json::{Value, json};

use crate::http::{Reply, Req, error, json_reply};

include!(concat!(env!("OUT_DIR"), "/docs.rs"));

/// The most hits one search answers.
const MAX_HITS: usize = 50;
/// The reference pages whose table rows document keys.
const KEY_PAGES: &[&str] = &[
    "reference/configuration.md",
    "reference/daemon-configuration.md",
];

fn title(text: &str) -> &str {
    text.lines()
        .find_map(|l| l.strip_prefix("# "))
        .unwrap_or_default()
        .trim()
}

/// `GET /api/docs`: every page.
pub(crate) fn index() -> Reply {
    let pages: Vec<Value> = PAGES
        .iter()
        .map(|(path, text)| json!({"path": path, "title": title(text)}))
        .collect();
    json_reply(200, &json!({"pages": pages}))
}

/// `GET /api/docs/page?path=`.
pub(crate) fn page(req: &Req) -> Reply {
    let wanted = req.param("path").unwrap_or_default();
    match PAGES.iter().find(|(path, _)| *path == wanted) {
        Some((path, text)) => json_reply(
            200,
            &json!({"path": path, "title": title(text), "markdown": text}),
        ),
        None => error(404, "not_found", &format!("no documentation page {wanted}")),
    }
}

/// The key a reference table row documents (the first cell's code span).
fn row_key(line: &str) -> Option<&str> {
    line.strip_prefix("| `")?.split('`').next()
}

fn hit(path: &str, text: &str, number: usize, line: &str) -> Value {
    json!({"path": path, "title": title(text), "line": number + 1, "text": line.trim(),
           "key": row_key(line)})
}

/// `GET /api/docs/search?q=`: the reference entry of a key first, then
/// every line that contains the query (case-insensitive).
pub(crate) fn search(req: &Req) -> Reply {
    let query = req.param("q").unwrap_or_default();
    let query = query.trim();
    if query.is_empty() {
        return json_reply(200, &json!({"query": query, "hits": []}));
    }
    let needle = query.to_lowercase();
    let mut hits: Vec<Value> = PAGES
        .iter()
        .filter(|(path, _)| KEY_PAGES.contains(path))
        .flat_map(|(path, text)| {
            text.lines()
                .enumerate()
                .filter(|(_, line)| row_key(line) == Some(query))
                .map(|(n, line)| hit(path, text, n, line))
        })
        .collect();
    let exact = hits.len();
    hits.extend(
        PAGES
            .iter()
            .flat_map(|(path, text)| {
                text.lines()
                    .enumerate()
                    .filter(|(_, line)| line.to_lowercase().contains(&needle))
                    .filter(|(_, line)| row_key(line) != Some(query))
                    .map(|(n, line)| hit(path, text, n, line))
            })
            .take(MAX_HITS.saturating_sub(exact)),
    );
    json_reply(200, &json!({"query": query, "hits": hits, "exact": exact}))
}

#[cfg(test)]
mod tests {
    use super::{PAGES, row_key};

    #[test]
    fn the_configuration_reference_is_embedded_with_its_key_rows() {
        let (_, text) = PAGES
            .iter()
            .find(|(path, _)| *path == "reference/configuration.md")
            .expect("the generated reference is embedded");
        assert!(text.lines().any(|l| row_key(l) == Some("busy.input_mode")));
        assert!(PAGES.iter().any(|(path, _)| *path == "guide/dashboard.md"));
    }
}
