//! Catalog table tests (spec: model-providers, Built-in provider catalog).

use std::collections::BTreeMap;

use pmt_config::Config;
use pmt_config::provider_urls::host_of;
use pmt_core::secret::SecretString;

use super::*;
use crate::registry::ProviderRegistry;
use crate::resolve::{CredentialSource, NoExtras, Resolver, Target, Trace};

/// The spec table: id, aliases, secret names (ordered), default base URL
/// (or how the endpoint is found), wire format, primary credential kind.
#[rustfmt::skip]
const SPEC: &[(&str, &str, &str, &str, &str, &str)] = &[
    ("openrouter", "or", "OPENROUTER_API_KEY", "https://openrouter.ai/api/v1", "chat_completions", "api_key"),
    ("anthropic", "claude,claude-oauth,claude-code", "ANTHROPIC_API_KEY,ANTHROPIC_TOKEN", "https://api.anthropic.com", "anthropic_messages", "api_key"),
    ("openai-api", "openai", "OPENAI_API_KEY", "https://api.openai.com/v1", "responses", "api_key"),
    ("openai-codex", "codex,chatgpt,chatgpt-codex", "", "https://chatgpt.com/backend-api/codex", "responses", "oauth"),
    ("nous", "nous-portal,nousresearch", "NOUS_API_KEY", "https://inference-api.nousresearch.com/v1", "chat_completions", "api_key"),
    ("xai", "grok,x-ai,x.ai", "XAI_API_KEY", "https://api.x.ai/v1", "responses", "api_key"),
    ("xai-oauth", "grok-oauth,x-ai-oauth,xai-grok-oauth", "", "https://api.x.ai/v1", "responses", "oauth"),
    ("router", "ramp-router,ramp,router.com", "RAMP_ROUTER_API_KEY,ROUTER_API_KEY", "https://api.router.com/v1", "responses", "api_key"),
    ("fireworks", "fireworks-ai,fw", "FIREWORKS_API_KEY", "https://api.fireworks.ai/inference/v1", "chat_completions", "api_key"),
    ("novita", "novita-ai,novitaai", "NOVITA_API_KEY", "https://api.novita.ai/openai/v1", "chat_completions", "api_key"),
    ("ai-gateway", "vercel,vercel-ai-gateway,aigateway", "AI_GATEWAY_API_KEY", "https://ai-gateway.vercel.sh/v1", "chat_completions", "api_key"),
    ("zai", "glm,z-ai,z.ai,zhipu", "GLM_API_KEY,ZAI_API_KEY,Z_AI_API_KEY", "https://api.z.ai/api/paas/v4", "chat_completions", "api_key"),
    ("kimi-coding", "kimi,moonshot,kimi-for-coding", "KIMI_API_KEY,KIMI_CODING_API_KEY", "https://api.moonshot.ai/v1", "chat_completions", "api_key"),
    ("kimi-coding-cn", "kimi-cn,moonshot-cn", "KIMI_CN_API_KEY", "https://api.moonshot.cn/v1", "chat_completions", "api_key"),
    ("arcee", "arcee-ai,arceeai", "ARCEEAI_API_KEY", "https://api.arcee.ai/api/v1", "chat_completions", "api_key"),
    ("gmi", "gmi-cloud,gmicloud", "GMI_API_KEY", "https://api.gmi-serving.com/v1", "chat_completions", "api_key"),
    ("nebius-token-factory", "nebius,nebius-tf,token-factory,tokenfactory", "NEBIUS_API_KEY,NEBIUS_TOKEN_FACTORY_API_KEY", "https://api.tokenfactory.nebius.com/v1", "chat_completions", "api_key"),
    ("actual", "actual-computer,actualcomputer,aci", "ACTUAL_API_KEY", "https://api.actual.inc/v1", "chat_completions", "api_key"),
    ("minimax", "mini-max", "MINIMAX_API_KEY", "https://api.minimax.io/anthropic", "anthropic_messages", "api_key"),
    ("minimax-cn", "minimax-china", "MINIMAX_CN_API_KEY", "https://api.minimaxi.com/anthropic", "anthropic_messages", "api_key"),
    ("minimax-oauth", "minimax-portal,minimax-global", "", "https://api.minimax.io/anthropic", "anthropic_messages", "oauth"),
    ("alibaba", "dashscope,alibaba-cloud,qwen-dashscope,aliyun", "DASHSCOPE_API_KEY", "https://dashscope-intl.aliyuncs.com/compatible-mode/v1", "chat_completions", "api_key"),
    ("alibaba-cn", "dashscope-cn,alibaba-cloud-cn", "DASHSCOPE_API_KEY", "https://dashscope.aliyuncs.com/compatible-mode/v1", "chat_completions", "api_key"),
    ("alibaba-coding-plan", "alibaba-coding,dashscope-coding", "ALIBABA_CODING_PLAN_API_KEY,DASHSCOPE_API_KEY", "https://coding-intl.dashscope.aliyuncs.com/v1", "chat_completions", "api_key"),
    ("alibaba-coding-plan-cn", "alibaba-coding-cn,dashscope-coding-cn", "ALIBABA_CODING_PLAN_CN_API_KEY,ALIBABA_CODING_PLAN_API_KEY,DASHSCOPE_API_KEY", "https://coding.dashscope.aliyuncs.com/v1", "chat_completions", "api_key"),
    ("alibaba-token-plan", "dashscope-token-plan", "ALIBABA_TOKEN_PLAN_API_KEY", "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", "chat_completions", "api_key"),
    ("alibaba-token-plan-cn", "dashscope-token-plan-cn", "ALIBABA_TOKEN_PLAN_CN_API_KEY,ALIBABA_TOKEN_PLAN_API_KEY", "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1", "chat_completions", "api_key"),
    ("qwen-oauth", "qwen,qwen-portal,qwen-cli", "", "https://portal.qwen.ai/v1", "chat_completions", "oauth"),
    ("kilocode", "kilo-code,kilo,kilo-gateway", "KILOCODE_API_KEY", "https://api.kilo.ai/api/gateway", "chat_completions", "api_key"),
    ("xiaomi", "mimo,xiaomi-mimo", "XIAOMI_API_KEY", "https://api.xiaomimimo.com/v1", "chat_completions", "api_key"),
    ("tencent-tokenhub", "tokenhub", "TOKENHUB_API_KEY", "https://tokenhub.tencentmaas.com/v1", "chat_completions", "api_key"),
    ("tencent-tokenplan", "tokenplan", "TOKENPLAN_API_KEY", "https://api.lkeap.cloud.tencent.com/plan/anthropic", "anthropic_messages", "api_key"),
    ("opencode-zen", "opencode,zen", "OPENCODE_ZEN_API_KEY", "https://opencode.ai/zen/v1", "per-model", "api_key"),
    ("opencode-go", "go,opencode-go-sub", "OPENCODE_GO_API_KEY", "https://opencode.ai/zen/go/v1", "per-model", "api_key"),
    ("commandcode", "commandcode-chat", "COMMANDCODE_API_KEY", "https://api.commandcode.ai/provider/v1", "chat_completions", "api_key"),
    ("commandcode-anthropic", "commandcode-claude", "COMMANDCODE_API_KEY", "https://api.commandcode.ai/provider/v1", "anthropic_messages", "api_key"),
    ("deepseek", "deepseek-chat,deep-seek", "DEEPSEEK_API_KEY", "https://api.deepseek.com/v1", "chat_completions", "api_key"),
    ("huggingface", "hf,hugging-face", "HF_TOKEN", "https://router.huggingface.co/v1", "chat_completions", "api_key"),
    ("gemini", "google,google-gemini,google-ai-studio", "GOOGLE_API_KEY,GEMINI_API_KEY", "https://generativelanguage.googleapis.com/v1beta", "gemini_native", "api_key"),
    ("vertex", "google-vertex,vertex-ai,gcp-vertex", "", "computed from project and region", "chat_completions", "token_mint"),
    ("bedrock", "aws,aws-bedrock,amazon-bedrock", "", "https://bedrock-runtime.<region>.amazonaws.com", "bedrock_converse", "aws_chain"),
    ("azure-foundry", "azure,azure-ai-foundry,azure-ai", "AZURE_FOUNDRY_API_KEY", "per-resource (required)", "auto-detected", "api_key"),
    ("nvidia", "nvidia-nim,nim,nemotron", "NVIDIA_API_KEY", "https://integrate.api.nvidia.com/v1", "chat_completions", "api_key"),
    ("ollama-cloud", "ollama_cloud", "OLLAMA_API_KEY", "https://ollama.com/v1", "chat_completions", "api_key"),
    ("stepfun", "step,stepfun-coding-plan", "STEPFUN_API_KEY", "https://api.stepfun.ai/step_plan/v1", "chat_completions", "api_key"),
    ("meta-ai", "meta,muse,muse-spark,model-api", "MODEL_API_KEY,META_API_KEY,META_MODEL_API_KEY", "https://api.meta.ai/v1", "responses", "api_key"),
    ("deepinfra", "deep-infra", "DEEPINFRA_API_KEY", "https://api.deepinfra.com/v1/openai", "chat_completions", "api_key"),
    ("upstage", "solar", "UPSTAGE_API_KEY", "https://api.upstage.ai/v1", "chat_completions", "api_key"),
    ("lmstudio", "lm-studio,lm_studio", "LM_API_KEY", "http://127.0.0.1:1234/v1", "chat_completions", "none"),
    ("ollama", "", "", "http://127.0.0.1:11434/v1", "chat_completions", "none"),
    ("llamacpp", "llama.cpp,llama-cpp", "", "resolved by local-models", "chat_completions", "none"),
    ("vllm", "", "", "required", "chat_completions", "none"),
    ("sglang", "", "", "required", "chat_completions", "none"),
    ("moa", "mixture-of-agents", "", "moa://local", "aggregator's", "none"),
    ("custom", "local", "", "required (--base-url or config)", "chat_completions", "none"),
    ("copilot", "github-copilot", "COPILOT_GITHUB_TOKEN,GH_TOKEN,GITHUB_TOKEN", "https://api.githubcopilot.com", "chat_completions", "oauth"),
    ("copilot-acp", "", "COPILOT_GITHUB_TOKEN,GH_TOKEN,GITHUB_TOKEN", "https://api.githubcopilot.com", "external_process", "oauth"),
    ("solstice", "solstice-oauth", "", "https://generativelanguage.googleapis.com/v1alpha", "gemini_native", "oauth"),
];

static PROBE: crate::resolve::TcpProbe = crate::resolve::TcpProbe {
    timeout: std::time::Duration::from_millis(200),
};

fn list(text: &str) -> Vec<String> {
    text.split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_built_in_id_matches_its_spec_row() {
    let descriptors = descriptors();
    let ids: Vec<&str> = descriptors.iter().map(|d| d.id.as_str()).collect();
    let spec_ids: Vec<&str> = SPEC.iter().map(|row| row.0).collect();
    assert_eq!(ids, spec_ids);
    for (d, (id, aliases, secrets, endpoint, wire, auth)) in descriptors.iter().zip(SPEC) {
        assert_eq!(d.aliases, list(aliases), "{id} aliases");
        assert_eq!(d.secret_names, list(secrets), "{id} secret_names");
        let shown = d
            .base_url
            .as_deref()
            .or(d.endpoint_note.as_deref())
            .unwrap();
        assert_eq!(shown, *endpoint, "{id} endpoint");
        assert_eq!(d.wire_label(), *wire, "{id} wire format");
        assert_eq!(d.auth.as_str(), *auth, "{id} auth");
    }
}

#[test]
fn every_builtin_satisfies_the_descriptor_contract_and_binds_its_host() {
    // Registration refuses a name claimed twice, so this also proves the
    // aliases are unique.
    ProviderRegistry::for_profile(&Config::default(), Vec::new()).unwrap();
    for d in descriptors() {
        d.validate().unwrap();
        if d.auth == ProviderAuth::Oauth {
            assert!(!d.login_flows.is_empty(), "{}", d.id);
        }
        if let Some(url) = &d.base_url {
            assert_eq!(d.key_hosts[0], host_of(url).unwrap(), "{}", d.id);
        }
    }
    let by_id: BTreeMap<String, Descriptor> = descriptors()
        .into_iter()
        .map(|d| (d.id.clone(), d))
        .collect();
    assert!(
        by_id["zai"]
            .key_hosts
            .contains(&"open.bigmodel.cn".to_owned())
    );
    assert!(
        by_id["kimi-coding"]
            .key_hosts
            .contains(&"api.kimi.com".to_owned())
    );
    for listed in ["router", "fireworks", "gmi"] {
        assert!(by_id[listed].supports_model_listing, "{listed}");
    }
}

async fn credential_of(provider: &str, stored: &[&str]) -> (CredentialSource, String) {
    let config = Config::default();
    let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
    let secrets: BTreeMap<String, SecretString> = stored
        .iter()
        .map(|n| {
            (
                (*n).to_owned(),
                SecretString::new(format!("{n}-value-0123456789")),
            )
        })
        .collect();
    let resolver = Resolver {
        registry: &registry,
        config: &config,
        secrets: &secrets,
        extras: &NoExtras,
        probe: &PROBE,
        ad_hoc: None,
    };
    let target = Target {
        provider: provider.to_owned(),
        model: Some("m".to_owned()),
        ..Target::default()
    };
    let mut trace = Trace::default();
    let route = resolver.resolve_target(&target, &mut trace).await.unwrap();
    let step = trace.0.iter().find(|s| s.step == "credential").unwrap();
    assert_eq!(step.detail, route.credential.describe());
    (route.credential.source, route.base_url)
}

#[tokio::test]
async fn plan_key_preferred_over_shared_key() {
    let (source, _) = credential_of(
        "alibaba-coding-plan",
        &["ALIBABA_CODING_PLAN_API_KEY", "DASHSCOPE_API_KEY"],
    )
    .await;
    assert_eq!(
        source,
        CredentialSource::Secret {
            name: "ALIBABA_CODING_PLAN_API_KEY".to_owned()
        }
    );
}

#[tokio::test]
async fn shared_key_fallback_reaches_the_plan_endpoint() {
    let (source, base_url) = credential_of("alibaba-coding-plan", &["DASHSCOPE_API_KEY"]).await;
    assert_eq!(
        source,
        CredentialSource::Secret {
            name: "DASHSCOPE_API_KEY".to_owned()
        }
    );
    assert_eq!(
        host_of(&base_url).unwrap(),
        "coding-intl.dashscope.aliyuncs.com"
    );
}

#[tokio::test]
async fn actual_is_keyless_only_on_a_loopback_endpoint() {
    let config: Config =
        toml::from_str("[providers.actual]\nbase_url = \"http://127.0.0.1:7777/v1\"\n").unwrap();
    let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
    let secrets: BTreeMap<String, SecretString> = BTreeMap::new();
    let resolver = Resolver {
        registry: &registry,
        config: &config,
        secrets: &secrets,
        extras: &NoExtras,
        probe: &PROBE,
        ad_hoc: None,
    };
    let target = Target {
        provider: "actual".to_owned(),
        model: Some("m".to_owned()),
        ..Target::default()
    };
    let route = resolver
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap();
    assert_eq!(route.credential.source, CredentialSource::None);
    let hosted = Target {
        base_url: Some("https://api.actual.inc/v1".to_owned()),
        ..target
    };
    let err = resolver
        .resolve_target(&hosted, &mut Trace::default())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
}
