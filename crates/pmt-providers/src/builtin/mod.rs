//! Built-in provider descriptors (spec: model-providers, Built-in provider
//! catalog): one row per canonical id of the spec table, with aliases,
//! secret names in resolution order (plan-specific before shared), default
//! endpoint, wire format and credential kind. `key_hosts` are the default
//! endpoint's host plus the row's extra hosts.

use pmt_config::schema::{LoginFlow, ProviderAuth, WireFormat};

use crate::descriptor::{Descriptor, HostRule, WireNeed};

/// A wire format beyond the native one (const-friendly [`WireNeed`]).
#[derive(Clone, Copy)]
enum Need {
    Native,
    PerModel,
    Detected,
    Plugin {
        wire: &'static str,
        plugin: &'static str,
        hint: &'static str,
    },
    Virtual {
        wire: &'static str,
        owner: &'static str,
    },
}

/// One catalog row; [`BASE`] holds the defaults.
#[derive(Clone, Copy)]
// The flags are the descriptor contract's own fields.
#[allow(clippy::struct_excessive_bools)]
struct Row {
    id: &'static str,
    aliases: &'static [&'static str],
    secrets: &'static [&'static str],
    base_url: Option<&'static str>,
    endpoint_note: Option<&'static str>,
    wire: WireFormat,
    need: Need,
    auth: ProviderAuth,
    flows: &'static [LoginFlow],
    extra_hosts: &'static [&'static str],
    local: bool,
    keyless_on_loopback: bool,
    hidden: bool,
    prompt_cache: bool,
    listing: bool,
    default_model: Option<&'static str>,
}

use LoginFlow::{Browser, Device};
use ProviderAuth::{ApiKey, AwsChain, None as Keyless, Oauth, TokenMint};
use WireFormat::{AnthropicMessages, ChatCompletions, Responses};

const BASE: Row = Row {
    id: "",
    aliases: &[],
    secrets: &[],
    base_url: None,
    endpoint_note: None,
    wire: ChatCompletions,
    need: Need::Native,
    auth: ApiKey,
    flows: &[],
    extra_hosts: &[],
    local: false,
    keyless_on_loopback: false,
    hidden: false,
    prompt_cache: false,
    listing: true,
    default_model: None,
};

/// A hosted API-key provider speaking `wire`.
const fn key(
    id: &'static str,
    aliases: &'static [&'static str],
    secrets: &'static [&'static str],
    base_url: &'static str,
    wire: WireFormat,
) -> Row {
    Row {
        id,
        aliases,
        secrets,
        base_url: Some(base_url),
        wire,
        ..BASE
    }
}

/// A subscription sign-in provider.
const fn signin(
    id: &'static str,
    aliases: &'static [&'static str],
    base_url: &'static str,
    wire: WireFormat,
    flows: &'static [LoginFlow],
) -> Row {
    Row {
        id,
        aliases,
        base_url: Some(base_url),
        wire,
        auth: Oauth,
        flows,
        ..BASE
    }
}

/// A local inference server (keyless, probed, never routed remotely).
const fn local(
    id: &'static str,
    aliases: &'static [&'static str],
    base_url: Option<&'static str>,
    note: Option<&'static str>,
) -> Row {
    Row {
        id,
        aliases,
        base_url,
        endpoint_note: note,
        auth: Keyless,
        local: true,
        ..BASE
    }
}

const fn model(mut row: Row, model: &'static str) -> Row {
    row.default_model = Some(model);
    row
}

const fn cached(mut row: Row) -> Row {
    row.prompt_cache = true;
    row
}

const fn flows(mut row: Row, flows: &'static [LoginFlow]) -> Row {
    row.flows = flows;
    row
}

const fn hosts(mut row: Row, extra: &'static [&'static str]) -> Row {
    row.extra_hosts = extra;
    row
}

const fn need(mut row: Row, need: Need) -> Row {
    row.need = need;
    row
}

const GEMINI_HINT: &str = "until the bundled plugin ships, use Gemini's OpenAI-compatible endpoint: prometheus config set providers.gemini.base_url https://generativelanguage.googleapis.com/v1beta/openai";
const COPILOT_HINT: &str = "prometheus plugins install github-copilot";

const ROWS: &[Row] = &[
    cached(model(
        key(
            "openrouter",
            &["or"],
            &["OPENROUTER_API_KEY"],
            "https://openrouter.ai/api/v1",
            ChatCompletions,
        ),
        "anthropic/claude-sonnet-4.5",
    )),
    flows(
        model(
            key(
                "anthropic",
                &["claude", "claude-oauth", "claude-code"],
                &["ANTHROPIC_API_KEY", "ANTHROPIC_TOKEN"],
                "https://api.anthropic.com",
                AnthropicMessages,
            ),
            "claude-sonnet-4-5",
        ),
        &[Browser],
    ),
    cached(model(
        key(
            "openai-api",
            &["openai"],
            &["OPENAI_API_KEY"],
            "https://api.openai.com/v1",
            Responses,
        ),
        "gpt-5",
    )),
    signin(
        "openai-codex",
        &["codex", "chatgpt", "chatgpt-codex"],
        "https://chatgpt.com/backend-api/codex",
        Responses,
        &[Device, Browser],
    ),
    flows(
        model(
            key(
                "nous",
                &["nous-portal", "nousresearch"],
                &["NOUS_API_KEY"],
                "https://inference-api.nousresearch.com/v1",
                ChatCompletions,
            ),
            "Hermes-4-405B",
        ),
        &[Device],
    ),
    cached(model(
        key(
            "xai",
            &["grok", "x-ai", "x.ai"],
            &["XAI_API_KEY"],
            "https://api.x.ai/v1",
            Responses,
        ),
        "grok-4",
    )),
    signin(
        "xai-oauth",
        &["grok-oauth", "x-ai-oauth", "xai-grok-oauth"],
        "https://api.x.ai/v1",
        Responses,
        &[Browser],
    ),
    key(
        "router",
        &["ramp-router", "ramp", "router.com"],
        &["RAMP_ROUTER_API_KEY", "ROUTER_API_KEY"],
        "https://api.router.com/v1",
        Responses,
    ),
    key(
        "fireworks",
        &["fireworks-ai", "fw"],
        &["FIREWORKS_API_KEY"],
        "https://api.fireworks.ai/inference/v1",
        ChatCompletions,
    ),
    key(
        "novita",
        &["novita-ai", "novitaai"],
        &["NOVITA_API_KEY"],
        "https://api.novita.ai/openai/v1",
        ChatCompletions,
    ),
    key(
        "ai-gateway",
        &["vercel", "vercel-ai-gateway", "aigateway"],
        &["AI_GATEWAY_API_KEY"],
        "https://ai-gateway.vercel.sh/v1",
        ChatCompletions,
    ),
    hosts(
        key(
            "zai",
            &["glm", "z-ai", "z.ai", "zhipu"],
            &["GLM_API_KEY", "ZAI_API_KEY", "Z_AI_API_KEY"],
            "https://api.z.ai/api/paas/v4",
            ChatCompletions,
        ),
        &["open.bigmodel.cn"],
    ),
    hosts(
        key(
            "kimi-coding",
            &["kimi", "moonshot", "kimi-for-coding"],
            &["KIMI_API_KEY", "KIMI_CODING_API_KEY"],
            "https://api.moonshot.ai/v1",
            ChatCompletions,
        ),
        &["api.kimi.com"],
    ),
    key(
        "kimi-coding-cn",
        &["kimi-cn", "moonshot-cn"],
        &["KIMI_CN_API_KEY"],
        "https://api.moonshot.cn/v1",
        ChatCompletions,
    ),
    key(
        "arcee",
        &["arcee-ai", "arceeai"],
        &["ARCEEAI_API_KEY"],
        "https://api.arcee.ai/api/v1",
        ChatCompletions,
    ),
    key(
        "gmi",
        &["gmi-cloud", "gmicloud"],
        &["GMI_API_KEY"],
        "https://api.gmi-serving.com/v1",
        ChatCompletions,
    ),
    key(
        "nebius-token-factory",
        &["nebius", "nebius-tf", "token-factory", "tokenfactory"],
        &["NEBIUS_API_KEY", "NEBIUS_TOKEN_FACTORY_API_KEY"],
        "https://api.tokenfactory.nebius.com/v1",
        ChatCompletions,
    ),
    Row {
        keyless_on_loopback: true,
        ..key(
            "actual",
            &["actual-computer", "actualcomputer", "aci"],
            &["ACTUAL_API_KEY"],
            "https://api.actual.inc/v1",
            ChatCompletions,
        )
    },
    model(
        key(
            "minimax",
            &["mini-max"],
            &["MINIMAX_API_KEY"],
            "https://api.minimax.io/anthropic",
            AnthropicMessages,
        ),
        "MiniMax-M2",
    ),
    key(
        "minimax-cn",
        &["minimax-china"],
        &["MINIMAX_CN_API_KEY"],
        "https://api.minimaxi.com/anthropic",
        AnthropicMessages,
    ),
    signin(
        "minimax-oauth",
        &["minimax-portal", "minimax-global"],
        "https://api.minimax.io/anthropic",
        AnthropicMessages,
        &[Browser],
    ),
    key(
        "alibaba",
        &["dashscope", "alibaba-cloud", "qwen-dashscope", "aliyun"],
        &["DASHSCOPE_API_KEY"],
        "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        ChatCompletions,
    ),
    key(
        "alibaba-cn",
        &["dashscope-cn", "alibaba-cloud-cn"],
        &["DASHSCOPE_API_KEY"],
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        ChatCompletions,
    ),
    key(
        "alibaba-coding-plan",
        &["alibaba-coding", "dashscope-coding"],
        &["ALIBABA_CODING_PLAN_API_KEY", "DASHSCOPE_API_KEY"],
        "https://coding-intl.dashscope.aliyuncs.com/v1",
        ChatCompletions,
    ),
    key(
        "alibaba-coding-plan-cn",
        &["alibaba-coding-cn", "dashscope-coding-cn"],
        &[
            "ALIBABA_CODING_PLAN_CN_API_KEY",
            "ALIBABA_CODING_PLAN_API_KEY",
            "DASHSCOPE_API_KEY",
        ],
        "https://coding.dashscope.aliyuncs.com/v1",
        ChatCompletions,
    ),
    key(
        "alibaba-token-plan",
        &["dashscope-token-plan"],
        &["ALIBABA_TOKEN_PLAN_API_KEY"],
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        ChatCompletions,
    ),
    key(
        "alibaba-token-plan-cn",
        &["dashscope-token-plan-cn"],
        &[
            "ALIBABA_TOKEN_PLAN_CN_API_KEY",
            "ALIBABA_TOKEN_PLAN_API_KEY",
        ],
        "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        ChatCompletions,
    ),
    signin(
        "qwen-oauth",
        &["qwen", "qwen-portal", "qwen-cli"],
        "https://portal.qwen.ai/v1",
        ChatCompletions,
        &[Browser],
    ),
    key(
        "kilocode",
        &["kilo-code", "kilo", "kilo-gateway"],
        &["KILOCODE_API_KEY"],
        "https://api.kilo.ai/api/gateway",
        ChatCompletions,
    ),
    key(
        "xiaomi",
        &["mimo", "xiaomi-mimo"],
        &["XIAOMI_API_KEY"],
        "https://api.xiaomimimo.com/v1",
        ChatCompletions,
    ),
    key(
        "tencent-tokenhub",
        &["tokenhub"],
        &["TOKENHUB_API_KEY"],
        "https://tokenhub.tencentmaas.com/v1",
        ChatCompletions,
    ),
    key(
        "tencent-tokenplan",
        &["tokenplan"],
        &["TOKENPLAN_API_KEY"],
        "https://api.lkeap.cloud.tencent.com/plan/anthropic",
        AnthropicMessages,
    ),
    need(
        key(
            "opencode-zen",
            &["opencode", "zen"],
            &["OPENCODE_ZEN_API_KEY"],
            "https://opencode.ai/zen/v1",
            ChatCompletions,
        ),
        Need::PerModel,
    ),
    need(
        key(
            "opencode-go",
            &["go", "opencode-go-sub"],
            &["OPENCODE_GO_API_KEY"],
            "https://opencode.ai/zen/go/v1",
            ChatCompletions,
        ),
        Need::PerModel,
    ),
    key(
        "commandcode",
        &["commandcode-chat"],
        &["COMMANDCODE_API_KEY"],
        "https://api.commandcode.ai/provider/v1",
        ChatCompletions,
    ),
    key(
        "commandcode-anthropic",
        &["commandcode-claude"],
        &["COMMANDCODE_API_KEY"],
        "https://api.commandcode.ai/provider/v1",
        AnthropicMessages,
    ),
    model(
        key(
            "deepseek",
            &["deepseek-chat", "deep-seek"],
            &["DEEPSEEK_API_KEY"],
            "https://api.deepseek.com/v1",
            ChatCompletions,
        ),
        "deepseek-chat",
    ),
    key(
        "huggingface",
        &["hf", "hugging-face"],
        &["HF_TOKEN"],
        "https://router.huggingface.co/v1",
        ChatCompletions,
    ),
    need(
        key(
            "gemini",
            &["google", "google-gemini", "google-ai-studio"],
            &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
            "https://generativelanguage.googleapis.com/v1beta",
            ChatCompletions,
        ),
        Need::Plugin {
            wire: "gemini_native",
            plugin: "gemini",
            hint: GEMINI_HINT,
        },
    ),
    Row {
        id: "vertex",
        aliases: &["google-vertex", "vertex-ai", "gcp-vertex"],
        endpoint_note: Some("computed from project and region"),
        auth: TokenMint,
        ..BASE
    },
    Row {
        id: "bedrock",
        aliases: &["aws", "aws-bedrock", "amazon-bedrock"],
        endpoint_note: Some("https://bedrock-runtime.<region>.amazonaws.com"),
        need: Need::Plugin {
            wire: "bedrock_converse",
            plugin: "bedrock",
            hint: "the bundled bedrock provider plugin is not part of this build yet",
        },
        auth: AwsChain,
        listing: false,
        ..BASE
    },
    Row {
        id: "azure-foundry",
        aliases: &["azure", "azure-ai-foundry", "azure-ai"],
        secrets: &["AZURE_FOUNDRY_API_KEY"],
        endpoint_note: Some("per-resource (required)"),
        need: Need::Detected,
        extra_hosts: &[
            "*.services.ai.azure.com",
            "*.openai.azure.com",
            "*.cognitiveservices.azure.com",
        ],
        ..BASE
    },
    key(
        "nvidia",
        &["nvidia-nim", "nim", "nemotron"],
        &["NVIDIA_API_KEY"],
        "https://integrate.api.nvidia.com/v1",
        ChatCompletions,
    ),
    key(
        "ollama-cloud",
        &["ollama_cloud"],
        &["OLLAMA_API_KEY"],
        "https://ollama.com/v1",
        ChatCompletions,
    ),
    key(
        "stepfun",
        &["step", "stepfun-coding-plan"],
        &["STEPFUN_API_KEY"],
        "https://api.stepfun.ai/step_plan/v1",
        ChatCompletions,
    ),
    key(
        "meta-ai",
        &["meta", "muse", "muse-spark", "model-api"],
        &["MODEL_API_KEY", "META_API_KEY", "META_MODEL_API_KEY"],
        "https://api.meta.ai/v1",
        Responses,
    ),
    key(
        "deepinfra",
        &["deep-infra"],
        &["DEEPINFRA_API_KEY"],
        "https://api.deepinfra.com/v1/openai",
        ChatCompletions,
    ),
    key(
        "upstage",
        &["solar"],
        &["UPSTAGE_API_KEY"],
        "https://api.upstage.ai/v1",
        ChatCompletions,
    ),
    Row {
        secrets: &["LM_API_KEY"],
        ..local(
            "lmstudio",
            &["lm-studio", "lm_studio"],
            Some("http://127.0.0.1:1234/v1"),
            None,
        )
    },
    local("ollama", &[], Some("http://127.0.0.1:11434/v1"), None),
    local(
        "llamacpp",
        &["llama.cpp", "llama-cpp"],
        None,
        Some("resolved by local-models"),
    ),
    local("vllm", &[], None, Some("required")),
    local("sglang", &[], None, Some("required")),
    Row {
        id: "moa",
        aliases: &["mixture-of-agents"],
        endpoint_note: Some("moa://local"),
        need: Need::Virtual {
            wire: "aggregator's",
            owner: "mixture-of-agents",
        },
        auth: Keyless,
        listing: false,
        ..BASE
    },
    Row {
        id: "custom",
        aliases: &["local"],
        endpoint_note: Some("required (--base-url or config)"),
        auth: Keyless,
        ..BASE
    },
    Row {
        id: "copilot",
        aliases: &["github-copilot"],
        secrets: &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
        base_url: Some("https://api.githubcopilot.com"),
        need: Need::Plugin {
            wire: "chat_completions",
            plugin: "github-copilot",
            hint: COPILOT_HINT,
        },
        auth: Oauth,
        flows: &[Device],
        ..BASE
    },
    Row {
        id: "copilot-acp",
        secrets: &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
        base_url: Some("https://api.githubcopilot.com"),
        need: Need::Plugin {
            wire: "external_process",
            plugin: "github-copilot",
            hint: COPILOT_HINT,
        },
        auth: Oauth,
        flows: &[Device],
        ..BASE
    },
    Row {
        id: "solstice",
        aliases: &["solstice-oauth"],
        base_url: Some("https://generativelanguage.googleapis.com/v1alpha"),
        need: Need::Plugin {
            wire: "gemini_native",
            plugin: "solstice",
            hint: "prometheus plugins install solstice",
        },
        auth: Oauth,
        flows: &[Browser],
        hidden: true,
        ..BASE
    },
];

/// Every secret name a built-in provider reads: the provider-native
/// environment variables the launch environment treats as secrets.
#[must_use]
pub fn secret_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = ROWS
        .iter()
        .flat_map(|r| r.secrets.iter().copied())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Every built-in descriptor, in catalog order.
#[must_use]
pub fn descriptors() -> Vec<Descriptor> {
    ROWS.iter().map(descriptor).collect()
}

fn descriptor(row: &Row) -> Descriptor {
    let mut d = Descriptor::new(row.id, row.wire, row.base_url);
    d.aliases = owned(row.aliases);
    d.secret_names = owned(row.secrets);
    d.key_hosts.extend(owned(row.extra_hosts));
    d.auth = row.auth;
    d.login_flows = row.flows.to_vec();
    d.local = row.local;
    d.keyless_on_loopback = row.keyless_on_loopback;
    d.hidden = row.hidden;
    d.supports_prompt_cache = row.prompt_cache;
    d.supports_model_listing = row.listing;
    d.default_model = row.default_model.map(str::to_owned);
    d.endpoint_note = row.endpoint_note.map(str::to_owned);
    d.wire_need = wire_need(row.need);
    if row.id == "gemini" {
        // Gemini's OpenAI-compatible endpoint (`.../openai`) speaks Chat
        // Completions natively.
        d.host_rules.push(HostRule {
            host: None,
            path_suffixes: vec!["/openai".to_owned()],
            wire_format: ChatCompletions,
        });
    }
    d
}

fn wire_need(need: Need) -> WireNeed {
    match need {
        Need::Native => WireNeed::Native,
        Need::PerModel => WireNeed::PerModel,
        Need::Detected => WireNeed::Detected,
        Need::Plugin { wire, plugin, hint } => WireNeed::Plugin {
            wire: wire.to_owned(),
            plugin: plugin.to_owned(),
            hint: hint.to_owned(),
        },
        Need::Virtual { wire, owner } => WireNeed::Virtual {
            wire: wire.to_owned(),
            owner: owner.to_owned(),
        },
    }
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[cfg(test)]
mod tests;
