//! Hugging Face search and community pulls (spec: local-models, Model
//! library). Both are explicit user actions; nothing else here contacts
//! the hub. Community models never carry the `validated` badge, and no
//! file below 4 bits per weight is offered.
//!
//! The fit of a file is an estimate made before download: its size plus a
//! sixth for the KV cache and buffers at the guaranteed window (the exact
//! shape is read from the file's own metadata once it is in the library).

use pmt_core::error::{Error, codes};
use serde::Serialize;
use serde_json::Value;

use crate::backend::Selection;
use crate::catalog::{Fit, MIN_BITS};
use crate::download::{FileSpec, Job, Jobs};
use crate::library::{Library, Model, ModelFile, Role, Source};

/// The default hub.
pub const HUB: &str = "https://huggingface.co";
/// Repositories a search returns.
pub const SEARCH_LIMIT: usize = 10;

/// One GGUF file of a repository.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HubFile {
    /// Path in the repository.
    pub path: String,
    /// Size in bytes (all parts of a split model).
    pub size: u64,
    /// The quantisation read from the file name.
    pub quant: Option<String>,
    /// The estimated fit on this machine.
    pub fit: Fit,
    #[serde(skip)]
    sha256: String,
    #[serde(skip)]
    parts: Vec<(String, u64, String)>,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Found {
    /// `owner/name`.
    pub id: String,
    /// Downloads reported by the hub.
    pub downloads: u64,
    /// Its GGUF files of 4 bits or more, with their fit.
    pub files: Vec<HubFile>,
    /// Always false: community models are not curated.
    pub validated: bool,
}

/// A hub client.
#[derive(Debug, Clone)]
pub struct Hub {
    base: String,
    client: reqwest::Client,
}

impl Hub {
    /// A client of the hub at `base`.
    #[must_use]
    pub fn new(base: &str, client: reqwest::Client) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            client,
        }
    }

    /// Searches GGUF repositories for `query`, most downloaded first, with
    /// each file's fit for `selection`.
    ///
    /// # Errors
    ///
    /// `runtime.download` (69) when the hub does not answer.
    pub async fn search(&self, query: &str, selection: &Selection) -> Result<Vec<Found>, Error> {
        let q = percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC);
        let url = format!(
            "{}/api/models?search={q}&filter=gguf&sort=downloads&direction=-1&limit={SEARCH_LIMIT}",
            self.base
        );
        let list = self.get(&url).await?;
        let mut found = Vec::new();
        for repo in list.as_array().into_iter().flatten() {
            let Some(id) = repo.get("id").and_then(Value::as_str) else {
                continue;
            };
            found.push(Found {
                id: id.to_owned(),
                downloads: repo.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                files: self.files(id, selection).await?,
                validated: false,
            });
        }
        Ok(found)
    }

    /// The GGUF files of `repo` (4 bits or more, projectors excluded),
    /// split parts grouped under their first part.
    ///
    /// # Errors
    ///
    /// `runtime.download` (69).
    pub async fn files(&self, repo: &str, selection: &Selection) -> Result<Vec<HubFile>, Error> {
        let tree = self
            .get(&format!(
                "{}/api/models/{repo}/tree/main?recursive=true",
                self.base
            ))
            .await?;
        let raw = gguf_entries(&tree);
        let mut files: Vec<HubFile> = Vec::new();
        for (path, size, sha) in &raw {
            if is_projector(path) || part_number(path).is_some_and(|n| n > 1) {
                continue;
            }
            let quant = quant_of(path);
            if quant
                .as_deref()
                .and_then(bits)
                .is_some_and(|b| b < MIN_BITS)
            {
                continue;
            }
            let parts: Vec<_> = match part_number(path) {
                Some(_) => raw
                    .iter()
                    .filter(|(p, ..)| same_split(p, path))
                    .cloned()
                    .collect(),
                None => vec![(path.clone(), *size, sha.clone())],
            };
            let total = parts.iter().map(|p| p.1).sum();
            files.push(HubFile {
                path: path.clone(),
                size: total,
                quant,
                fit: fit(total, selection),
                sha256: sha.clone(),
                parts,
            });
        }
        Ok(files)
    }

    /// The download job of `spec` (`owner/name` or `owner/name:<quant or
    /// file>`): the named file, else the highest-bit file that fits the
    /// GPU, else the smallest; plus the repository's vision projector.
    ///
    /// # Errors
    ///
    /// `local.not_found` (66) when no file matches; `runtime.download`.
    pub async fn job(
        &self,
        spec: &str,
        selection: &Selection,
        library: &Library,
        jobs: &Jobs,
    ) -> Result<Job, Error> {
        let (repo, wanted) = spec
            .split_once(':')
            .map_or((spec, None), |(r, w)| (r, Some(w)));
        let id = model_id(repo)?;
        if let Some(job) = jobs.unfinished(&id) {
            return Ok(job);
        }
        let files = self.files(repo, selection).await?;
        let chosen = choose(&files, wanted).ok_or_else(|| {
            Error::new(
                &codes::LOCAL_NOT_FOUND,
                format!(
                    "{repo} has no GGUF file of 4 bits or more matching '{}'",
                    wanted.unwrap_or("*")
                ),
            )
        })?;
        let dir = library.dir(&id)?;
        let url = |path: &str| format!("{}/{repo}/resolve/main/{path}", self.base);
        let mut specs: Vec<(FileSpec, Role)> = chosen
            .parts
            .iter()
            .enumerate()
            .map(|(i, (path, size, sha))| {
                let role = if i == 0 { Role::Weights } else { Role::Part };
                Ok((crate::pull::spec(url(path), path, *size, sha, &dir)?, role))
            })
            .collect::<Result<_, Error>>()?;
        let tree = self
            .get(&format!(
                "{}/api/models/{repo}/tree/main?recursive=true",
                self.base
            ))
            .await?;
        if let Some((path, size, sha)) = gguf_entries(&tree)
            .into_iter()
            .find(|(p, ..)| is_projector(p))
        {
            specs.push((
                crate::pull::spec(url(&path), &path, size, &sha, &dir)?,
                Role::Projector,
            ));
        }
        let model = Model {
            id,
            source: Source::Hub,
            files: specs
                .iter()
                .map(|(s, role)| ModelFile {
                    path: s.target.clone(),
                    role: *role,
                    size: s.size,
                })
                .collect(),
            quant: chosen.quant.clone(),
            context: 0,
            context_max: None,
            added_at: String::new(),
        };
        crate::pull::create(jobs, &model, specs.into_iter().map(|(s, _)| s).collect())
    }

    async fn get(&self, url: &str) -> Result<Value, Error> {
        let failed = |reason: String| {
            Error::new(
                &codes::RUNTIME_DOWNLOAD,
                format!("cannot reach {url}: {reason}"),
            )
        };
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| failed(e.to_string()))?;
        if !response.status().is_success() {
            return Err(failed(format!("HTTP {}", response.status())));
        }
        let body = response.bytes().await.map_err(|e| failed(e.to_string()))?;
        serde_json::from_slice(&body).map_err(|e| failed(e.to_string()))
    }
}

/// `owner/Name-GGUF` -> `name`.
///
/// # Errors
///
/// `config.invalid_argument` (2) when the last segment is not a plain
/// library id (`..`, empty, odd characters): it names a directory.
pub fn model_id(repo: &str) -> Result<String, Error> {
    let name = repo.rsplit('/').next().unwrap_or(repo).to_lowercase();
    let id = name.strip_suffix("-gguf").unwrap_or(&name);
    if crate::library::valid_id(id) {
        Ok(id.to_owned())
    } else {
        Err(Error::new(
            &codes::CONFIG_INVALID_ARGUMENT,
            format!("'{repo}' does not end in a usable model name"),
        ))
    }
}

fn choose<'a>(files: &'a [HubFile], wanted: Option<&str>) -> Option<&'a HubFile> {
    if let Some(wanted) = wanted {
        let w = wanted.to_lowercase();
        return files.iter().find(|f| {
            f.path.to_lowercase() == w || f.quant.as_deref().is_some_and(|q| q.to_lowercase() == w)
        });
    }
    let by_bits = |f: &&HubFile| f.quant.as_deref().and_then(bits).unwrap_or(16.0);
    files
        .iter()
        .filter(|f| f.fit == Fit::FitsGpu)
        .max_by(|a, b| by_bits(a).total_cmp(&by_bits(b)))
        .or_else(|| files.iter().min_by_key(|f| f.size))
}

fn fit(size: u64, selection: &Selection) -> Fit {
    let need = size + size / 6 + crate::hardware::GIB / 2;
    if selection.gpu_budget > 0 && need <= selection.gpu_budget {
        Fit::FitsGpu
    } else if need <= selection.gpu_budget + selection.ram_budget {
        Fit::UsesSystemRam
    } else {
        Fit::TooBig
    }
}

fn gguf_entries(tree: &Value) -> Vec<(String, u64, String)> {
    tree.as_array()
        .into_iter()
        .flatten()
        .filter(|f| f.get("type").and_then(Value::as_str) == Some("file"))
        .filter_map(|f| {
            let path = f.get("path")?.as_str()?;
            let sha = f.pointer("/lfs/oid")?.as_str()?;
            path.to_lowercase().ends_with(".gguf").then(|| {
                (
                    path.to_owned(),
                    f.get("size").and_then(Value::as_u64).unwrap_or(0),
                    sha.to_owned(),
                )
            })
        })
        .collect()
}

fn is_projector(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .to_lowercase()
        .starts_with("mmproj")
}

/// `n` of `...-0000n-of-0000m.gguf`.
fn part_number(path: &str) -> Option<u32> {
    let stem = path.strip_suffix(".gguf")?;
    let (head, _total) = stem.rsplit_once("-of-")?;
    head.rsplit_once('-')?.1.parse().ok()
}

fn same_split(path: &str, first: &str) -> bool {
    let base = |p: &str| {
        p.strip_suffix(".gguf")
            .and_then(|s| s.rsplit_once("-of-"))
            .and_then(|(h, t)| Some((h.rsplit_once('-')?.0.to_owned(), t.to_owned())))
    };
    base(path).is_some() && base(path) == base(first)
}

/// The quantisation named in a GGUF file name (`Q4_K_M`, `IQ4_XS`, `BF16`,
/// `MXFP4`).
#[must_use]
pub fn quant_of(path: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.strip_suffix(".gguf").unwrap_or(name);
    stem.split(['-', '.'])
        .rev()
        .find(|part| bits(part).is_some())
        .map(str::to_owned)
}

/// Bits per weight of a quantisation name.
#[must_use]
pub fn bits(quant: &str) -> Option<f64> {
    let q = quant.to_uppercase();
    let known = [
        ("F32", 32.0),
        ("BF16", 16.0),
        ("F16", 16.0),
        ("MXFP4", 4.25),
        ("Q8", 8.5),
        ("Q6", 6.6),
        ("Q5", 5.5),
        ("Q4", 4.5),
        ("IQ4", 4.3),
        ("Q3", 3.5),
        ("IQ3", 3.3),
        ("Q2", 2.6),
        ("IQ2", 2.3),
        ("IQ1", 1.6),
        ("TQ", 1.7),
    ];
    known
        .iter()
        .find(|(prefix, _)| q == *prefix || q.starts_with(&format!("{prefix}_")))
        .map(|(_, b)| *b)
}

#[cfg(test)]
mod tests {
    use super::{bits, model_id, part_number, quant_of};

    #[test]
    fn file_names_give_their_quantisation_and_split_part() {
        assert_eq!(quant_of("Qwen3-8B-Q4_K_M.gguf").as_deref(), Some("Q4_K_M"));
        assert_eq!(
            quant_of("x/gpt-oss-20b-MXFP4.gguf").as_deref(),
            Some("MXFP4")
        );
        assert_eq!(quant_of("model-IQ2_XXS.gguf").as_deref(), Some("IQ2_XXS"));
        assert!(bits("IQ2_XXS").unwrap() < 4.0);
        assert!(bits("Q4_0").unwrap() >= 4.0);
        assert_eq!(part_number("Big-Q4_K_M-00002-of-00003.gguf"), Some(2));
        assert_eq!(part_number("Small-Q4_K_M.gguf"), None);
        assert_eq!(model_id("unsloth/Qwen3-8B-GGUF").unwrap(), "qwen3-8b");
        assert!(model_id("owner/..").is_err());
    }
}
