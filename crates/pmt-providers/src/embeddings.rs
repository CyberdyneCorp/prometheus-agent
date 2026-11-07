//! Text embeddings over a resolved route (spec: memory, Semantic recall):
//! the OpenAI-compatible `POST <base_url>/embeddings` call most providers
//! and local servers (Ollama, llama.cpp, vLLM) serve.

use pmt_core::error::{Error, codes};
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use serde_json::{Value, json};

use crate::resolve::Route;

fn failed(route: &Route, why: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::PROVIDER_CALL_FAILED,
        format!("embedding call to {} failed: {why}", route.label()),
    )
}

/// The vectors of `inputs`, in order, from `route`'s model.
///
/// # Errors
///
/// `provider.call_failed`: transport failure, timeout, an error status or
/// a body without one vector per input.
pub async fn embed(
    client: &reqwest::Client,
    route: &Route,
    inputs: &[String],
) -> Result<Vec<Vec<f32>>, Error> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("{}/embeddings", route.base_url.trim_end_matches('/'));
    let body = json!({"model": route.model, "input": inputs});
    let builder = client
        .post(url)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .timeout(route.timeouts.request)
        .body(body.to_string());
    let response = crate::transport::authenticate(builder, route)
        .send()
        .await
        .map_err(|e| failed(route, e))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| failed(route, e))?;
    if !status.is_success() {
        return Err(failed(route, format!("HTTP {status}")));
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| failed(route, e))?;
    vectors(&value, inputs.len())
        .ok_or_else(|| failed(route, "the response has no vector per input"))
}

/// `data[].embedding` ordered by `data[].index`, when there is one per input.
fn vectors(value: &Value, count: usize) -> Option<Vec<Vec<f32>>> {
    let mut rows: Vec<(u64, Vec<f32>)> = value["data"]
        .as_array()?
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let index = row["index"]
                .as_u64()
                .unwrap_or_else(|| u64::try_from(i).unwrap_or(u64::MAX));
            #[allow(clippy::cast_possible_truncation)]
            let vector = row["embedding"]
                .as_array()?
                .iter()
                .map(|x| x.as_f64().map(|x| x as f32))
                .collect::<Option<Vec<f32>>>()?;
            Some((index, vector))
        })
        .collect::<Option<_>>()?;
    rows.sort_by_key(|(index, _)| *index);
    (rows.len() == count).then(|| rows.into_iter().map(|(_, v)| v).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_follow_their_index_and_need_one_per_input() {
        let body = json!({"data": [
            {"index": 1, "embedding": [0.0, 1.0]},
            {"index": 0, "embedding": [1.0, 0.0]},
        ]});
        assert_eq!(
            vectors(&body, 2).unwrap(),
            vec![vec![1.0, 0.0], vec![0.0, 1.0]]
        );
        assert!(vectors(&body, 3).is_none());
    }
}
