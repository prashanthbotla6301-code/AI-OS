//! AgentOS model-router.
//!
//! The single abstraction through which the OS (and, later, agents) reach AI
//! models. No provider is hardcoded into the architecture: models are
//! *declared* as providers in JSON, classified Local / Cloud / Hybrid, with
//! capabilities, endpoints and cost metadata. Requests are routed by
//! capability + preference; responses carry token/cost usage.
//!
//! Phase 1 ships exactly one *working* provider: `echo-mock` (Local,
//! deterministic, offline). Cloud providers can be declared in the registry
//! and will be reported by `models.list`, but `models.route` to them returns
//! a clear "not configured in Phase 1" error rather than pretending. When a
//! real backend lands (Phase 2+) it implements the same registry entry shape
//! and nothing above this layer changes.

use agentos_ipc::{serve, Handler, RpcResponse};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Provider model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Local,
    Cloud,
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Provider {
    pub id: String,
    pub kind: ModelKind,
    /// Vendor-neutral provider label, e.g. "agentos", "openai", "ollama".
    pub provider: String,
    /// Provider-specific model identifier, e.g. "echo-1", "gpt-x", "llama-8b".
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Cost per 1M input/output tokens, provider currency units (USD assumed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_mtok_in: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_mtok_out: Option<f64>,
    #[serde(default)]
    pub max_context_tokens: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

pub fn builtin_providers() -> Vec<Provider> {
    vec![Provider {
        id: "echo-mock".into(),
        kind: ModelKind::Local,
        provider: "agentos".into(),
        model: "echo-1".into(),
        endpoint: None,
        capabilities: vec!["text.generate".into(), "text.echo".into()],
        cost_per_mtok_in: Some(0.0),
        cost_per_mtok_out: Some(0.0),
        max_context_tokens: 8192,
        enabled: true,
    }]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteRequest {
    pub text: String,
    #[serde(default)]
    pub capability: Option<String>,
    /// "local" | "hybrid" | "cloud" | "any" (default: local-first).
    #[serde(default)]
    pub prefer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Micro-currency units (1e-6 USD) to avoid floats in accounting.
    pub cost_micros: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteResponse {
    pub provider_id: String,
    pub provider_kind: ModelKind,
    pub model: String,
    pub completion: String,
    pub usage: Usage,
    pub latency_ms: u64,
    pub mock: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderUsage {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micros: u64,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub struct Router {
    providers: Vec<Provider>,
    usage: BTreeMap<String, ProviderUsage>,
}

fn approx_tokens(text: &str) -> u64 {
    // ~4 chars per token; honest about being an approximation for Phase 1.
    (text.chars().count() as u64 + 3) / 4
}

impl Router {
    pub fn new() -> Router {
        Router {
            providers: builtin_providers(),
            usage: BTreeMap::new(),
        }
    }

    /// Merge declared providers from config files. Duplicates by id replace
    /// builtins/earlier entries (so an operator can override `echo-mock`).
    pub fn load_registry(&mut self, etc_config: &Path, state_dir: &Path) {
        let mut merge_file = |path: &Path| {
            let Ok(body) = fs::read_to_string(path) else {
                return;
            };
            let Ok(root) = serde_json::from_str::<serde_json::Value>(&body) else {
                return;
            };
            let Some(providers) = root.get("providers").and_then(|value| value.as_array()) else {
                return;
            };
            for raw in providers {
                let Some(id) = raw.get("id").and_then(|value| value.as_str()) else {
                    continue;
                };
                if let Some(slot) = self.providers.iter_mut().find(|provider| provider.id == id) {
                    // models.d files may be sparse overrides, e.g.
                    // {"id":"echo-mock","enabled":false}. Merge over the
                    // existing typed provider, then deserialize again so the
                    // final registry still has a strict Provider shape.
                    let Ok(mut merged) = serde_json::to_value(&*slot) else {
                        continue;
                    };
                    if let (Some(dst), Some(src)) = (merged.as_object_mut(), raw.as_object()) {
                        for (key, value) in src {
                            dst.insert(key.clone(), value.clone());
                        }
                    }
                    if let Ok(provider) = serde_json::from_value::<Provider>(merged) {
                        *slot = provider;
                    }
                } else if let Ok(provider) = serde_json::from_value::<Provider>(raw.clone()) {
                    self.providers.push(provider);
                }
            }
        };
        merge_file(etc_config);
        if let Ok(entries) = fs::read_dir(state_dir.join("models.d")) {
            let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                merge_file(&p);
            }
        }
    }

    pub fn providers(&self) -> Vec<Provider> {
        self.providers.clone()
    }

    fn kind_rank(kind: ModelKind, prefer: &str) -> u32 {
        // lower = better
        match prefer {
            "cloud" => match kind {
                ModelKind::Cloud => 0,
                ModelKind::Hybrid => 1,
                ModelKind::Local => 2,
            },
            "hybrid" => match kind {
                ModelKind::Hybrid => 0,
                ModelKind::Local => 1,
                ModelKind::Cloud => 2,
            },
            // default "local"/"any": local-first posture (data stays on device)
            _ => match kind {
                ModelKind::Local => 0,
                ModelKind::Hybrid => 1,
                ModelKind::Cloud => 2,
            },
        }
    }

    pub fn route(&mut self, req: &RouteRequest) -> Result<RouteResponse, String> {
        if req.text.trim().is_empty() {
            return Err("text must be non-empty".into());
        }
        let prefer = req.prefer.as_deref().unwrap_or("local");
        let capability = req
            .capability
            .clone()
            .unwrap_or_else(|| "text.generate".to_string());

        let mut candidates: Vec<&Provider> = self
            .providers
            .iter()
            .filter(|p| p.enabled && p.capabilities.iter().any(|c| c == &capability))
            .collect();
        candidates.sort_by_key(|p| Self::kind_rank(p.kind, prefer));
        let chosen = candidates
            .first()
            .copied()
            .ok_or_else(|| format!("no enabled provider offers capability {capability:?}"))?;

        if chosen.provider != "agentos" {
            // Declared third-party/cloud model: refuse honestly in Phase 1.
            return Err(format!(
                "provider {:?} ({}/{}) is declared in the registry but model backends arrive in Phase 2; nothing is faked",
                chosen.id, chosen.provider, chosen.model
            ));
        }

        let started = std::time::Instant::now();
        // echo-1 deterministic completion
        let completion = mock_completion(&req.text);
        let usage = Usage {
            input_tokens: approx_tokens(&req.text),
            output_tokens: approx_tokens(&completion),
            cost_micros: 0,
        };
        let entry = self.usage.entry(chosen.id.clone()).or_default();
        entry.requests += 1;
        entry.input_tokens += usage.input_tokens;
        entry.output_tokens += usage.output_tokens;
        entry.cost_micros += usage.cost_micros;
        Ok(RouteResponse {
            provider_id: chosen.id.clone(),
            provider_kind: chosen.kind,
            model: chosen.model.clone(),
            completion,
            usage,
            latency_ms: started.elapsed().as_millis() as u64,
            mock: true,
        })
    }

    pub fn usage(&self) -> serde_json::Value {
        serde_json::json!({ "per_provider": self.usage })
    }
}

fn mock_completion(text: &str) -> String {
    let trimmed = text.trim();
    let mut chars = trimmed.chars();
    let capped: String = match chars.next() {
        Some(f) => f.to_uppercase().chain(chars).take(240).collect(),
        None => String::new(),
    };
    format!("[echo-1] {capped} — deterministic mock response from the local echo-1 model. Real model backends are a Phase 2 concern.")
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

fn main() {
    let paths = agentos_common::Paths::detect();
    let mut socket = paths.socket(agentos_common::sockets::MODELS);
    let mut state_dir = paths.state_dir;
    let mut etc_dir = paths.etc_dir;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from).unwrap_or_default(),
            "--state-dir" => state_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            "--etc-dir" => etc_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            _ => {}
        }
    }
    agentos_common::logger_init("model-router");

    let router = Arc::new(Mutex::new(Router::new()));
    router
        .lock()
        .unwrap()
        .load_registry(&etc_dir.join("models.json"), &state_dir);
    let n = router.lock().unwrap().providers().len();
    agentos_common::info(&format!(
        "model-router starting: {n} provider(s) registered"
    ));

    let handler: Handler = Arc::new(move |req| {
        let p = &req.params;
        match req.method.as_str() {
            "ping" => RpcResponse::ok(
                req.id,
                serde_json::json!({"service":"model-router","version":agentos_common::VERSION}),
            ),
            "models.list" => {
                let r = router.lock().unwrap();
                RpcResponse::ok(req.id, serde_json::json!({ "providers": r.providers() }))
            }
            "models.route" => {
                let Ok(rreq) = serde_json::from_value::<RouteRequest>(p.clone()) else {
                    return RpcResponse::err(
                        req.id,
                        agentos_ipc::ERR_INVALID_PARAMS,
                        "invalid route request",
                    );
                };
                let mut r = router.lock().unwrap();
                match r.route(&rreq) {
                    Ok(resp) => {
                        RpcResponse::ok(req.id, serde_json::to_value(resp).unwrap_or_default())
                    }
                    Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_INVALID_STATE, e),
                }
            }
            "models.usage" => RpcResponse::ok(req.id, router.lock().unwrap().usage()),
            _ => RpcResponse::err(
                req.id,
                agentos_ipc::ERR_UNKNOWN_METHOD,
                format!("unknown method {}", req.method),
            ),
        }
    });

    if let Err(e) = serve(&socket, handler) {
        agentos_common::error(&format!("model-router fatal: {e}"));
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_local_provider_routes_and_accounts() {
        let mut r = Router::new();
        let resp = r
            .route(&RouteRequest {
                text: "hello world".into(),
                capability: Some("text.echo".into()),
                prefer: None,
            })
            .unwrap();
        assert_eq!(resp.provider_id, "echo-mock");
        assert_eq!(resp.provider_kind, ModelKind::Local);
        assert!(resp.completion.starts_with("[echo-1] Hello world"));
        assert!(resp.usage.input_tokens > 0);
        assert!(resp.mock);
        let u = r.usage();
        assert_eq!(
            u["per_provider"]["echo-mock"]["requests"],
            serde_json::json!(1)
        );
        // second call accumulates
        r.route(&RouteRequest {
            text: "hello again".into(),
            capability: Some("text.echo".into()),
            prefer: None,
        })
        .unwrap();
        let u = r.usage();
        assert_eq!(
            u["per_provider"]["echo-mock"]["requests"],
            serde_json::json!(2)
        );
    }

    #[test]
    fn declared_cloud_provider_refuses_honestly() {
        let mut r = Router::new();
        r.providers.push(Provider {
            id: "cloud-x".into(),
            kind: ModelKind::Cloud,
            provider: "openai".into(),
            model: "gpt-future".into(),
            endpoint: Some("https://api.example.com".into()),
            capabilities: vec!["text.generate".into()],
            cost_per_mtok_in: Some(1.0),
            cost_per_mtok_out: Some(2.0),
            max_context_tokens: 128000,
            enabled: true,
        });
        // capability only the cloud provider offers
        let err = r
            .route(&RouteRequest {
                text: "hi".into(),
                capability: Some("text.generate".into()),
                prefer: Some("cloud".into()),
            })
            .unwrap_err();
        assert!(err.contains("Phase 2"), "must clearly refuse: {err}");
        // local-first default still picks echo-mock for text.generate
        let ok = r
            .route(&RouteRequest {
                text: "hi".into(),
                capability: Some("text.generate".into()),
                prefer: None,
            })
            .unwrap();
        assert_eq!(ok.provider_id, "echo-mock");
    }

    #[test]
    fn unknown_capability_errors() {
        let mut r = Router::new();
        assert!(r
            .route(&RouteRequest {
                text: "x".into(),
                capability: Some("video.render".into()),
                prefer: None
            })
            .is_err());
    }

    #[test]
    fn registry_files_merge() {
        let dir = std::env::temp_dir().join(format!("agentos-models-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("models.d")).unwrap();
        fs::write(
            dir.join("models.json"),
            r#"{"providers":[{"id":"local-llm","kind":"local","provider":"ollama","model":"llama-8b","capabilities":["text.generate"],"max_context_tokens":32000}]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("models.d/extra.json"),
            r#"{"providers":[{"id":"echo-mock","enabled":false}]}"#,
        )
        .unwrap();
        let mut r = Router::new();
        r.load_registry(&dir.join("models.json"), &dir);
        let ps = r.providers();
        assert!(ps.iter().any(|p| p.id == "local-llm"));
        // state-dir override disabled echo-mock => no provider for text.echo
        assert!(r
            .route(&RouteRequest {
                text: "x".into(),
                capability: Some("text.echo".into()),
                prefer: None
            })
            .is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
