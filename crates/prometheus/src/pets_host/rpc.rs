//! The daemon's `pets.*` and `achievements.list` (spec: pets): what the
//! desktop and the dashboard read and change of a profile's pets, the
//! generate flow and the achievements snapshot. They run as the `pets`
//! commands do; no session is touched.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_gateway::pets::PetsCall;
use pmt_pets::PetState;
use pmt_pets::hatch::{Hatch, Step};
use pmt_rpc::contracts::{
    Achievements, HatchAnswerParams, HatchStartParams, HatchStep, PetGallery, PetGalleryParams,
    PetInfo, PetInstallParams, PetRow, PetSetParams, PetSprite, PetSpriteParams, PetsState,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{current, patch, pets};

/// Open generate flows by id.
static FLOWS: Mutex<Option<HashMap<String, Box<Hatch>>>> = Mutex::new(None);

fn params<T: DeserializeOwned>(call: &PetsCall) -> Result<T, Error> {
    serde_json::from_value(call.params.clone())
        .map_err(|e| Error::new(&codes::RPC_INVALID_PARAMS, format!("{}: {e}", call.method)))
}

fn value(result: impl serde::Serialize) -> Value {
    serde_json::to_value(result).unwrap_or(Value::Null)
}

/// Called with the tiers a page-triggered achievements scan reached, so
/// the host can celebrate them as the post-turn rescan does.
pub type OnUnlock = Box<dyn FnOnce(Vec<pmt_pets::achievements::Unlock>) + Send>;

/// Runs one call for `scope`'s profile (`store`: its session store;
/// `on_unlock`: what to do with tiers its achievements scan reaches).
///
/// # Errors
///
/// The method's errors (`pet.*`, configuration, `rpc.invalid_params`).
pub async fn handle(
    scope: &ProfileScope,
    store: &Arc<pmt_store::Store>,
    call: PetsCall,
    on_unlock: OnUnlock,
) -> Result<Value, Error> {
    match call.method {
        "pets.state" => Ok(value(state(scope, None))),
        "pets.sprite" => Ok(value(sprite(scope, &params(&call)?)?)),
        "pets.gallery" => Ok(value(gallery(scope, &params(&call)?).await?)),
        "pets.install" => Ok(value(install(scope, &params(&call)?).await?)),
        "pets.set" => Ok(value(set(scope, &params(&call)?)?)),
        "pets.hatch.start" => Ok(value(hatch_start(scope, &params(&call)?)?)),
        "pets.hatch.answer" => Ok(value(hatch_answer(&params(&call)?).await?)),
        "achievements.list" => Ok(value(achievements(scope, store, on_unlock))),
        other => Err(Error::new(
            &codes::RPC_NOT_SERVED,
            format!("{other} is not a pets method"),
        )),
    }
}

fn info(meta: pmt_pets::PetMeta) -> PetInfo {
    PetInfo {
        slug: meta.slug,
        name: meta.name,
        author: meta.author,
        installed: true,
    }
}

fn state(scope: &ProfileScope, notice: Option<String>) -> PetsState {
    let config = current(scope);
    let store = pets(scope);
    PetsState {
        enabled: config.enabled,
        active: store.active(&config.slug).map(|p| p.meta.slug),
        scale: config.scale,
        roam: config.roam,
        installed: store
            .installed()
            .into_iter()
            .map(|p| info(p.meta))
            .collect(),
        notice,
    }
}

fn sprite(scope: &ProfileScope, params: &PetSpriteParams) -> Result<PetSprite, Error> {
    let config = current(scope);
    let store = pets(scope);
    let pet = match &params.slug {
        Some(slug) => store
            .get(slug)
            .ok_or_else(|| pmt_pets::store::not_found(slug))?,
        None => store
            .active(&config.slug)
            .ok_or_else(|| Error::new(&codes::PET_NOT_FOUND, "no pet is installed"))?,
    };
    let atlas = pet.load_atlas()?;
    let bytes =
        std::fs::read(&pet.atlas).map_err(|e| Error::io(&codes::HOME_IO, &pet.atlas, &e))?;
    let counts: HashMap<PetState, usize> = pmt_pets::frames::counts(&atlas).into_iter().collect();
    let states: BTreeMap<String, PetRow> = PetState::ALL
        .into_iter()
        .map(|state| {
            let frames = counts.get(&state).copied().unwrap_or(0);
            let row = PetRow {
                row: state.row(),
                frames: u32::try_from(frames).unwrap_or(0),
            };
            (state.as_str().to_owned(), row)
        })
        .collect();
    Ok(PetSprite {
        slug: pet.meta.slug,
        name: pet.meta.name,
        atlas: format!(
            "data:{};base64,{}",
            atlas.format().mime(),
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
        frame_width: pmt_pets::atlas::FRAME_W,
        frame_height: pmt_pets::atlas::FRAME_H,
        states,
    })
}

fn client(scope: &ProfileScope) -> Result<reqwest::Client, Error> {
    let network = &scope.config().network;
    pmt_net::client(&pmt_net::OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    })
}

async fn gallery(scope: &ProfileScope, params: &PetGalleryParams) -> Result<PetGallery, Error> {
    let url = current(scope).gallery_url;
    let entries = pmt_pets::gallery::fetch(&client(scope)?, &url).await?;
    let limit = params
        .limit
        .map_or(100, |n| usize::try_from(n).unwrap_or(100));
    let store = pets(scope);
    let pets = pmt_pets::gallery::filter(&entries, &params.query, limit)
        .into_iter()
        .map(|e| PetInfo {
            installed: store.get(&e.slug).is_some(),
            slug: e.slug,
            name: e.name,
            author: e.author,
        })
        .collect();
    Ok(PetGallery { pets })
}

async fn install(scope: &ProfileScope, params: &PetInstallParams) -> Result<PetsState, Error> {
    let client = client(scope)?;
    let url = current(scope).gallery_url;
    let entries = pmt_pets::gallery::fetch(&client, &url).await?;
    let entry = entries
        .into_iter()
        .find(|e| e.slug == params.slug)
        .ok_or_else(|| pmt_pets::store::not_found(&params.slug))?;
    let bytes = pmt_pets::gallery::download(&client, &entry).await?;
    let meta = pmt_pets::PetMeta {
        slug: entry.slug,
        name: entry.name,
        author: entry.author,
        source: entry.atlas_url,
        sha256: String::new(),
        license: entry.license,
    };
    pets(scope).install(&meta, &bytes, true)?;
    if params.select {
        patch(
            scope,
            &[
                ("pets.slug", params.slug.clone()),
                ("pets.enabled", "true".to_owned()),
            ],
        )?;
    }
    Ok(state(scope, Some(format!("installed '{}'", params.slug))))
}

fn set(scope: &ProfileScope, params: &PetSetParams) -> Result<PetsState, Error> {
    use super::slash::{Pet, run, set_scale};
    let mut notice = None;
    if let Some(slug) = &params.slug {
        notice = Some(run(scope, &Pet::Adopt(slug.clone()))?);
    }
    match params.enabled {
        Some(false) => notice = Some(run(scope, &Pet::Off)?),
        Some(true) if !current(scope).enabled => notice = Some(run(scope, &Pet::Toggle)?),
        _ => {}
    }
    if let Some(scale) = params.scale {
        notice = Some(set_scale(scope, &scale.to_string())?);
    }
    if let Some(roam) = params.roam {
        patch(scope, &[("pets.roam", roam.to_string())])?;
    }
    Ok(state(scope, notice))
}

fn flows<R>(f: impl FnOnce(&mut HashMap<String, Box<Hatch>>) -> R) -> R {
    let mut flows = FLOWS.lock().unwrap_or_else(PoisonError::into_inner);
    f(flows.get_or_insert_with(HashMap::new))
}

fn drafts_of(step: &Step, hatch: &Hatch) -> Vec<String> {
    if !matches!(step, Step::Ask(_)) {
        return Vec::new();
    }
    hatch
        .drafts()
        .iter()
        .map(|png| {
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(png)
            )
        })
        .collect()
}

fn step_of(flow: &str, step: &Step, drafts: Vec<String>) -> HatchStep {
    match step {
        Step::Ask(text) => HatchStep {
            flow: flow.to_owned(),
            text: text.clone(),
            done: false,
            failed: false,
            drafts,
            slug: None,
        },
        Step::Done { text, slug, failed } => HatchStep {
            flow: flow.to_owned(),
            text: text.clone(),
            done: true,
            failed: *failed,
            drafts: Vec::new(),
            slug: slug.clone(),
        },
    }
}

fn hatch_start(scope: &ProfileScope, params: &HatchStartParams) -> Result<HatchStep, Error> {
    let backend = super::backend::select(scope)?;
    let drafts = scope.dir().join("cache/pets/hatch");
    let (hatch, step) = Hatch::start(&params.description, backend, pets(scope), drafts);
    let flow = ulid::Ulid::generate().to_string();
    let shown = step_of(&flow, &step, Vec::new());
    flows(|f| f.insert(flow, Box::new(hatch)));
    Ok(shown)
}

async fn hatch_answer(params: &HatchAnswerParams) -> Result<HatchStep, Error> {
    let mut hatch = flows(|f| f.remove(&params.flow)).ok_or_else(|| {
        Error::new(
            &codes::RPC_INVALID_PARAMS,
            format!("no pet generation flow '{}' is open", params.flow),
        )
    })?;
    let step = hatch.answer(&params.answer).await;
    let drafts = drafts_of(&step, &hatch);
    let shown = step_of(&params.flow, &step, drafts);
    if !hatch.over() {
        flows(|f| f.insert(params.flow.clone(), hatch));
    }
    Ok(shown)
}

/// The snapshot now; an incremental scan starts in the background when
/// none runs and the last is not recent (spec: pets, First scan does not
/// block). Tiers that scan reaches go to `on_unlock` (spec: pets,
/// Achievement unlocks celebrate): whichever scan records a tier first
/// is the one that announces it.
fn achievements(
    scope: &ProfileScope,
    store: &Arc<pmt_store::Store>,
    on_unlock: OnUnlock,
) -> Achievements {
    let profile = super::achievements::profile(scope);
    // What is known now, taken before a scan starts so the answer never
    // waits for it.
    let mut snapshot = profile.snapshot();
    // Each finished turn rescans; a page rescans what other processes
    // added when the last scan is older than this.
    if !snapshot.scanning && profile.stale(std::time::Duration::from_secs(30)) {
        pmt_pets::achievements::scan::mark_running(scope.dir());
        snapshot.scanning = true;
        let (scope, store) = (scope.clone(), Arc::clone(store));
        // Detached: the page polls while `scanning` is set.
        drop(pmt_core::spawn::blocking(move || {
            on_unlock(super::achievements::rescan(&scope, &store));
        }));
    }
    serde_json::from_value(value(snapshot)).unwrap_or(Achievements {
        badges: Vec::new(),
        sessions: 0,
        scanned_at: None,
        scanning: true,
    })
}
