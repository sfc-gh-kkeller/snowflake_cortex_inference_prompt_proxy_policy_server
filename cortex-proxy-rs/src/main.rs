//! High-performance Snowflake Cortex Proxy with Tool Support
//!
//! Supports both:
//!   - Anthropic API (Claude Code) -> /v1/messages
//!   - OpenAI API (Continue.dev)   -> /chat/completions

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Router,
};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{env, fs, net::ToSocketAddrs, path::PathBuf, sync::Arc, time::Instant};
use tower_http::cors::{Any, CorsLayer};

#[derive(Deserialize)]
struct Config {
    proxy: ProxyConfig,
    snowflake: SnowflakeConfig,
    #[serde(default)]
    model_map: std::collections::HashMap<String, String>,
    #[serde(default)]
    policy: Option<PolicyConfig>,
}

#[derive(Deserialize)]
struct ProxyConfig {
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_log_level")]
    log_level: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default = "default_pool_size")]
    connection_pool_size: usize,
}

#[derive(Deserialize)]
struct SnowflakeConfig {
    base_url: String,
    pat: String,
    #[serde(default = "default_model")]
    default_model: String,
}

fn default_port() -> u16 { 8766 }
fn default_log_level() -> String { "info".to_string() }
fn default_model() -> String { "claude-4-sonnet".to_string() }
fn default_timeout() -> u64 { 300 }
fn default_pool_size() -> usize { 10 }

#[derive(Deserialize, Clone, Debug)]
struct PolicyConfig {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_judge_model")]
    judge_model: String,
    #[serde(default = "default_action")]
    action: String,
    #[serde(default = "default_max_eval_tokens")]
    max_evaluation_tokens: u64,
    #[serde(default = "default_policy_source")]
    source: String,
    #[serde(default)]
    policies_file: Option<String>,
    #[serde(default)]
    rules: std::collections::HashMap<String, PolicyRule>,
}

#[derive(Deserialize, Clone, Debug)]
struct PolicyRule {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_severity")]
    severity: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    examples: Vec<String>,
}

fn default_true() -> bool { true }
fn default_judge_model() -> String { "claude-4-sonnet".to_string() }
fn default_action() -> String { "block".to_string() }
fn default_max_eval_tokens() -> u64 { 1024 }
fn default_policy_source() -> String { "local".to_string() }
fn default_severity() -> String { "medium".to_string() }

#[derive(Clone, Debug)]
struct PolicyState {
    enabled: bool,
    judge_model: String,
    action: String,
    max_evaluation_tokens: u64,
    rules: Vec<(String, PolicyRule)>,
}

#[derive(Clone)]
struct AppState {
    client: Client,
    base_url: String,
    upstream_host: Option<String>,
    auth_header: String,
    default_model: String,
    log_level: LogLevel,
    model_map: std::collections::HashMap<String, String>,
    policy: Option<PolicyState>,
}

#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum LogLevel {
    Debug = 0,
    Info = 1,
    Quiet = 2,
}

impl AppState {
    fn log(&self, level: LogLevel, msg: &str) {
        if level >= self.log_level {
            println!("{}", msg);
        }
    }
    fn apply_host_header(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref h) = self.upstream_host {
            builder.header("Host", h.as_str())
        } else {
            builder
        }
    }
}

fn find_config_path() -> Option<PathBuf> {
    let args: Vec<String> = env::args().collect();
    if let Some(idx) = args.iter().position(|a| a == "--config") {
        if let Some(path) = args.get(idx + 1) {
            let p = PathBuf::from(path);
            if p.exists() { return Some(p); }
        }
    }
    if let Ok(path) = env::var("CORTEX_PROXY_CONFIG") {
        let p = PathBuf::from(path);
        if p.exists() { return Some(p); }
    }
    for path_opt in [
        dirs::config_dir().map(|d| d.join("cortex-proxy/config.toml")),
        dirs::home_dir().map(|d| d.join(".config/cortex-proxy/config.toml")),
        Some(PathBuf::from("cortex-proxy.toml")),
    ] {
        if let Some(p) = path_opt {
            if p.exists() { return Some(p); }
        }
    }
    None
}

fn load_config() -> Result<Config, String> {
    let config_path = find_config_path().ok_or("Config not found")?;
    let content = fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let config: Config = toml::from_str(&content).map_err(|e| e.to_string())?;
    println!("📄 Config: {}", config_path.display());
    Ok(config)
}

fn load_policy(config: &Config) -> Option<PolicyState> {
    let policy_cfg = config.policy.as_ref()?;
    if !policy_cfg.enabled {
        println!("🛡️  Policy: disabled");
        return None;
    }

    let mut rules: Vec<(String, PolicyRule)> = vec![];

    if policy_cfg.source == "local" {
        if let Some(ref path_str) = policy_cfg.policies_file {
            match load_policy_rules_from_file(path_str) {
                Ok(file_rules) => rules = file_rules,
                Err(e) => eprintln!("⚠️  Failed to load policies file {}: {}", path_str, e),
            }
        } else {
            let default_paths = vec!["policies.toml", "config/policies.toml"];
            for p in &default_paths {
                if let Ok(r) = load_policy_rules_from_file(p) {
                    rules = r;
                    break;
                }
            }
        }
    }

    for (name, rule) in &policy_cfg.rules {
        if !rules.iter().any(|(n, _)| n == name) {
            rules.push((name.clone(), rule.clone()));
        }
    }

    let enabled_count = rules.iter().filter(|(_, r)| r.enabled).count();
    println!("🛡️  Policy: enabled ({} rules, action={})", enabled_count, policy_cfg.action);

    Some(PolicyState {
        enabled: true,
        judge_model: policy_cfg.judge_model.clone(),
        action: policy_cfg.action.clone(),
        max_evaluation_tokens: policy_cfg.max_evaluation_tokens,
        rules,
    })
}

fn load_policy_rules_from_file(path: &str) -> Result<Vec<(String, PolicyRule)>, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let raw: toml::Value = toml::from_str(&content).map_err(|e| e.to_string())?;
    let mut rules = vec![];
    if let Some(policy) = raw.get("policy") {
        if let Some(rules_table) = policy.get("rules").and_then(|r| r.as_table()) {
            for (name, rule_val) in rules_table {
                let rule: PolicyRule = toml::from_str(&toml::to_string(rule_val).unwrap_or_default())
                    .unwrap_or(PolicyRule {
                        enabled: true,
                        severity: "medium".to_string(),
                        description: String::new(),
                        examples: vec![],
                    });
                rules.push((name.clone(), rule));
            }
        }
    }
    Ok(rules)
}

#[tokio::main]
async fn main() {
    let config = load_config().unwrap_or_else(|e| { eprintln!("Error: {}", e); std::process::exit(1); });
    let log_level = match config.proxy.log_level.as_str() {
        "debug" => LogLevel::Debug,
        "quiet" => LogLevel::Quiet,
        _ => LogLevel::Info,
    };

    let base_url_str = config.snowflake.base_url.trim_end_matches('/');
    let is_https = base_url_str.starts_with("https://");
    let without_scheme = base_url_str.trim_start_matches("https://").trim_start_matches("http://");
    let host = without_scheme.split('/').next().unwrap_or("localhost");
    let host_no_port = host.split(':').next().unwrap_or(host);
    let port: u16 = host.split(':').nth(1).and_then(|p| p.parse().ok()).unwrap_or(if is_https { 443 } else { 80 });

    let resolved_base_url;
    let client_builder = Client::builder()
        .pool_max_idle_per_host(config.proxy.connection_pool_size)
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .timeout(std::time::Duration::from_secs(config.proxy.timeout_secs))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .tcp_nodelay(true)
        .gzip(true);

    if host_no_port.contains('_') {
        if let Ok(mut addrs) = format!("{}:{}", host_no_port, port).to_socket_addrs() {
            if let Some(addr) = addrs.next() {
                eprintln!("📡 Hostname has underscores (IDNA-incompatible), resolved {} -> {}", host_no_port, addr.ip());
                let ip_host = if addr.ip().is_ipv6() {
                    format!("[{}]", addr.ip())
                } else {
                    addr.ip().to_string()
                };
                resolved_base_url = base_url_str.replace(host_no_port, &ip_host);
            } else {
                resolved_base_url = base_url_str.to_string();
            }
        } else {
            eprintln!("⚠️  Failed to resolve {}, using as-is", host_no_port);
            resolved_base_url = base_url_str.to_string();
        }
    } else {
        resolved_base_url = base_url_str.to_string();
    }

    let client = client_builder.build().unwrap();

    let policy = load_policy(&config);

    let upstream_host = if resolved_base_url != base_url_str {
        Some(host_no_port.to_string())
    } else {
        None
    };

    let state = Arc::new(AppState {
        client,
        base_url: resolved_base_url,
        upstream_host,
        auth_header: format!("Bearer {}", config.snowflake.pat),
        default_model: config.snowflake.default_model,
        log_level,
        model_map: config.model_map,
        policy,
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(|| async { "OK" }))
        .route("/health", get(health_handler))
        .route("/v1/messages", post(anthropic_handler))
        .route("/*path", any(openai_handler))
        .layer(cors)
        .with_state(state.clone());

    let port = config.proxy.port;
    println!("🚀 Cortex Proxy on http://localhost:{}", port);
    println!("   /v1/messages (Anthropic) | /chat/completions (OpenAI)");
    println!();

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ============ Model Mapping ============

fn map_model(model: &str, model_map: &std::collections::HashMap<String, String>) -> String {
    if let Some(mapped) = model_map.get(model) {
        return mapped.clone();
    }
    let m = model.to_lowercase();
    if m.contains("opus-4-5") || m.contains("4-5-opus") {
        "claude-opus-4-5".to_string()
    } else if m.contains("4-opus") || m.contains("opus-4") {
        "claude-4-opus".to_string()
    } else if m.contains("haiku") {
        "claude-haiku-4-5".to_string()
    } else if m.contains("3-5") && m.contains("sonnet") {
        "claude-3-5-sonnet".to_string()
    } else {
        "claude-4-sonnet".to_string()
    }
}

// ============ Tool Conversation Validation ============

/// Validates that every tool_call in assistant messages has a matching tool result
fn validate_tool_conversation(messages: &[Value]) -> Result<(), String> {
    let mut pending_tool_ids: Vec<String> = vec![];
    
    for (idx, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        
        if role == "assistant" {
            // Check if there are unfulfilled tool calls from a previous assistant message
            // (this can happen in multi-turn conversations)
            if !pending_tool_ids.is_empty() {
                eprintln!("DEBUG VALIDATION: Warning - unfulfilled tool calls before new assistant message at idx {}: {:?}", idx, pending_tool_ids);
            }
            // Clear pending and collect new tool_call IDs
            pending_tool_ids.clear();
            if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        pending_tool_ids.push(id.to_string());
                        eprintln!("DEBUG VALIDATION: Found tool_call id={} at message idx {}", id, idx);
                    }
                }
            }
        } else if role == "tool" {
            if let Some(id) = msg.get("tool_call_id").and_then(|i| i.as_str()) {
                if pending_tool_ids.contains(&id.to_string()) {
                    pending_tool_ids.retain(|x| x != id);
                    eprintln!("DEBUG VALIDATION: Matched tool result for id={} at message idx {}", id, idx);
                } else {
                    eprintln!("DEBUG VALIDATION: Warning - tool result for unknown id={} at message idx {}", id, idx);
                }
            }
        } else if role == "user" || role == "system" {
            // User/system messages don't affect tool call tracking
            // But if we have pending tool calls and hit a user message, that's unusual
            if !pending_tool_ids.is_empty() && role == "user" {
                eprintln!("DEBUG VALIDATION: Warning - user message at idx {} with pending tool calls: {:?}", idx, pending_tool_ids);
            }
        }
    }
    
    if !pending_tool_ids.is_empty() {
        let err = format!("Unfulfilled tool calls at end of conversation: {:?}", pending_tool_ids);
        eprintln!("DEBUG VALIDATION ERROR: {}", err);
        return Err(err);
    }
    
    eprintln!("DEBUG VALIDATION: All tool calls have matching results");
    Ok(())
}

// ============ Anthropic -> OpenAI Conversion ============

fn anthropic_to_openai(
    body: &[u8],
    default_model: &str,
    model_map: &std::collections::HashMap<String, String>,
) -> Result<(Value, bool), String> {
    let req: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    
    let model = req.get("model").and_then(|m| m.as_str()).unwrap_or(default_model);
    let snowflake_model = map_model(model, model_map);
    let is_streaming = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let max_tokens = req.get("max_tokens").and_then(|m| m.as_u64()).unwrap_or(4096);
    
    let mut messages: Vec<Value> = vec![];
    
    // Handle system prompt
    if let Some(system) = req.get("system") {
        let system_text = match system {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr.iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !system_text.is_empty() {
            messages.push(json!({"role": "system", "content": system_text}));
        }
    }
    
    // Track tool_call info (ID -> name) for tool_results and reordering
    let mut pending_tool_calls: Vec<Value> = vec![];
    let mut pending_tool_call_ids: Vec<String> = vec![];
    let mut tool_id_to_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    
    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content");
            
            match content {
                Some(Value::String(s)) => {
                    // Simple string content - just add it
                    messages.push(json!({"role": role, "content": s}));
                }
                Some(Value::Array(blocks)) => {
                    // Handle content blocks (text, tool_use, tool_result)
                    let mut text_parts: Vec<String> = vec![];
                    let mut tool_calls: Vec<Value> = vec![];
                    let mut tool_results: Vec<(String, String)> = vec![]; // (tool_use_id, content)
                    
                    for block in blocks {
                        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        
                        match block_type {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    text_parts.push(text.to_string());
                                }
                            }
                            "tool_use" => {
                                // Anthropic tool_use -> OpenAI tool_calls
                                let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                let input = block.get("input").cloned().unwrap_or(json!({}));
                                
                                eprintln!("DEBUG TOOL_USE: Found tool_use block - id={} name={}", id, name);
                                
                                // Store mapping from ID to name
                                tool_id_to_name.insert(id.clone(), name.clone());
                                
                                tool_calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(&input).unwrap_or_default()
                                    }
                                }));
                            }
                            "tool_result" => {
                                // Collect tool_result for later
                                let tool_use_id = block.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let result_content = block.get("content");
                                let result_text = match result_content {
                                    Some(Value::String(s)) => s.clone(),
                                    Some(Value::Array(arr)) => {
                                        let mut parts: Vec<String> = vec![];
                                        for item in arr {
                                            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                                    parts.push(text.to_string());
                                                }
                                            } else {
                                                parts.push(serde_json::to_string(item).unwrap_or_default());
                                            }
                                        }
                                        parts.join("\n")
                                    }
                                    Some(other) => serde_json::to_string(other).unwrap_or_default(),
                                    None => String::new(),
                                };
                                
                                eprintln!("DEBUG TOOL_RESULT: Found tool_result block - tool_use_id={} content_len={}", 
                                    tool_use_id, result_text.len());
                                
                                tool_results.push((tool_use_id, result_text));
                            }
                            _ => {}
                        }
                    }
                    
                    // For assistant messages: add with tool_calls if present
                    if role == "assistant" {
                        let content = text_parts.join("");
                        if !tool_calls.is_empty() {
                            // If assistant includes text plus tool calls, emit text first
                            if !content.is_empty() {
                                messages.push(json!({"role": "assistant", "content": content.clone()}));
                            }
                            pending_tool_call_ids = tool_calls.iter()
                                .filter_map(|tc| tc.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                                .collect();
                            pending_tool_calls = tool_calls;
                            
                            eprintln!("DEBUG ASSISTANT: Queued {} tool_calls for sequential emit", pending_tool_calls.len());
                        } else if !content.is_empty() {
                            messages.push(json!({"role": "assistant", "content": content}));
                        }
                    } else {
                        // User message - emit tool_results as OpenAI tool messages
                        if !tool_results.is_empty() {
                            // Reorder tool_results to match pending tool_calls order (if present)
                            let mut ordered_results: Vec<(String, String)> = vec![];
                            if !pending_tool_call_ids.is_empty() {
                                for id in &pending_tool_call_ids {
                                    if let Some(result) = tool_results.iter().find(|(tid, _)| tid == id) {
                                        ordered_results.push(result.clone());
                                    }
                                }
                                // Add any tool_results not in the order list (shouldn't happen, but be safe)
                                for result in &tool_results {
                                    if !ordered_results.iter().any(|(tid, _)| tid == &result.0) {
                                        ordered_results.push(result.clone());
                                    }
                                }
                            } else {
                                ordered_results = tool_results.clone();
                            }
                            
                            if !pending_tool_calls.is_empty() {
                                eprintln!("DEBUG TOOL_MSGS: Sequentially emitting {} tool_calls with results", ordered_results.len());
                                for (tool_use_id, result_text) in ordered_results.iter() {
                                    if let Some(tc) = pending_tool_calls.iter().find(|tc| {
                                        tc.get("id").and_then(|i| i.as_str()) == Some(tool_use_id.as_str())
                                    }).cloned() {
                                        messages.push(json!({
                                            "role": "assistant",
                                            "content": Value::Null,
                                            "tool_calls": [tc]
                                        }));
                                    }
                                    let tool_name = tool_id_to_name.get(tool_use_id).cloned().unwrap_or_default();
                                    eprintln!("DEBUG TOOL_MSG: Adding tool message - tool_call_id={} name={}", tool_use_id, tool_name);
                                    messages.push(json!({
                                        "role": "tool",
                                        "tool_call_id": tool_use_id,
                                        "name": tool_name,
                                        "content": result_text
                                    }));
                                }
                                pending_tool_calls.clear();
                                pending_tool_call_ids.clear();
                            } else {
                                eprintln!("DEBUG TOOL_MSGS: Pushing {} tool messages in order: {:?}", 
                                    ordered_results.len(), 
                                    ordered_results.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>());
                                
                                for (tool_use_id, result_text) in ordered_results {
                                    let tool_name = tool_id_to_name.get(&tool_use_id).cloned().unwrap_or_default();
                                    eprintln!("DEBUG TOOL_MSG: Adding tool message - tool_call_id={} name={}", tool_use_id, tool_name);
                                    messages.push(json!({
                                        "role": "tool",
                                        "tool_call_id": tool_use_id,
                                        "name": tool_name,
                                        "content": result_text
                                    }));
                                }
                            }
                        }
                        let combined_text = text_parts.join("");
                        let trimmed_text = combined_text.trim();
                        if !trimmed_text.is_empty() && tool_results.is_empty() {
                            messages.push(json!({"role": role, "content": combined_text}));
                        } else if !trimmed_text.is_empty() && !tool_results.is_empty() {
                            // User message has both tool_results and meaningful text
                            messages.push(json!({"role": "user", "content": combined_text}));
                        }
                    }
                }
                _ => {
                    messages.push(json!({"role": role, "content": ""}));
                }
            }
        }
    }
    
    let mut openai_req = json!({
        "model": snowflake_model,
        "messages": messages,
        "stream": is_streaming,
        "max_completion_tokens": max_tokens
    });
    
    // Convert tools from Anthropic format to OpenAI format
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let openai_tools: Vec<Value> = tools.iter().map(|tool| {
            let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let description = tool.get("description").and_then(|d| d.as_str()).unwrap_or("");
            let input_schema = tool.get("input_schema").cloned().unwrap_or(json!({"type": "object"}));
            
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": input_schema
                }
            })
        }).collect();
        
        if !openai_tools.is_empty() {
            openai_req["tools"] = json!(openai_tools);
        }
    }
    
    // Copy other params
    if let Some(temp) = req.get("temperature") { openai_req["temperature"] = temp.clone(); }
    if let Some(top_p) = req.get("top_p") { openai_req["top_p"] = top_p.clone(); }
    if let Some(stop) = req.get("stop_sequences") { openai_req["stop"] = stop.clone(); }
    
    // Debug logging
    eprintln!("DEBUG CONVERTED OpenAI messages: {}", serde_json::to_string_pretty(&messages).unwrap_or_default());
    eprintln!("DEBUG TOOL_ID_MAP: Known tool IDs -> names: {:?}", tool_id_to_name);
    
    // Validate tool call/result pairing before sending to Snowflake
    if let Err(e) = validate_tool_conversation(&messages) {
        eprintln!("DEBUG VALIDATION FAILED: {}", e);
    }
    
    Ok((openai_req, is_streaming))
}

// ============ OpenAI -> Anthropic Response Conversion ============

fn openai_to_anthropic(openai_resp: &Value, model: &str, req_id: u128) -> Value {
    let choice = &openai_resp["choices"][0];
    let message = &choice["message"];
    let finish_reason = choice.get("finish_reason").and_then(|f| f.as_str());
    
    let mut content: Vec<Value> = vec![];
    
    // Add text content if present
    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }
    
    // Convert tool_calls to tool_use blocks
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let func = &tc["function"];
            let name = func.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args_str = func.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input
            }));
        }
    }
    
    // Map stop reason - check if there are tool_use blocks
    let has_tool_use = content.iter().any(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
    let stop_reason = if has_tool_use {
        "tool_use"
    } else {
        match finish_reason {
            Some("stop") => "end_turn",
            Some("tool_calls") => "tool_use",
            Some("length") | Some("max_tokens") => "max_tokens",
            _ => "end_turn",
        }
    };
    
    let usage = &openai_resp["usage"];
    
    json!({
        "id": format!("msg_{:06}", req_id),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(|p| p.as_u64()).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(|c| c.as_u64()).unwrap_or(0)
        }
    })
}

// ============ Policy Evaluation ============

fn extract_user_prompt(messages: &[Value]) -> String {
    let mut parts = vec![];
    for msg in messages.iter().rev() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "user" || role == "system" {
            if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                parts.push(content.to_string());
            }
        }
        if parts.len() >= 3 { break; }
    }
    parts.reverse();
    parts.join("\n")
}

fn check_policy_patterns(prompt: &str, policy: &PolicyState) -> Option<(String, String, String)> {
    let prompt_lower = prompt.to_lowercase();
    for (name, rule) in &policy.rules {
        if !rule.enabled { continue; }
        for example in &rule.examples {
            if prompt_lower.contains(&example.to_lowercase()) {
                return Some((
                    name.clone(),
                    format!("Prompt matches known pattern: {}", example),
                    rule.severity.clone(),
                ));
            }
        }
    }
    None
}

async fn evaluate_policy_with_judge(
    state: &AppState,
    prompt: &str,
    policy: &PolicyState,
) -> Option<(String, String, String)> {
    let mut rules_text = String::new();
    for (name, rule) in &policy.rules {
        if !rule.enabled { continue; }
        rules_text.push_str(&format!("- {}: {} (severity: {})\n", name, rule.description, rule.severity));
    }

    let judge_prompt = format!(
        "You are a security policy evaluator. Analyze the following user prompt against these security rules:\n\n\
         RULES:\n{}\n\n\
         USER PROMPT:\n{}\n\n\
         Respond with EXACTLY one line in this format:\n\
         ALLOWED - if the prompt does not violate any rules\n\
         BLOCKED|<rule_name>|<severity>|<reason> - if the prompt violates a rule\n\n\
         Be precise. Only block clearly malicious or policy-violating content.",
        rules_text,
        &prompt[..prompt.len().min(2000)]
    );

    let judge_req = json!({
        "model": policy.judge_model,
        "messages": [{"role": "user", "content": judge_prompt}],
        "max_completion_tokens": policy.max_evaluation_tokens,
        "stream": false
    });

    let url = format!("{}/chat/completions", state.base_url);
    let mut builder = state.client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", &state.auth_header)
        .header("X-Snowflake-Authorization-Token-Type", "PROGRAMMATIC_ACCESS_TOKEN")
        .json(&judge_req);
    builder = state.apply_host_header(builder);
    let resp = builder.send()
        .await
        .ok()?;

    if !resp.status().is_success() { return None; }

    let body: Value = resp.json().await.ok()?;
    let text = body["choices"][0]["message"]["content"].as_str()?;
    let trimmed = text.trim();

    if trimmed.starts_with("BLOCKED") {
        let parts: Vec<&str> = trimmed.splitn(4, '|').collect();
        let rule = parts.get(1).unwrap_or(&"unknown").trim().to_string();
        let severity = parts.get(2).unwrap_or(&"medium").trim().to_string();
        let reason = parts.get(3).unwrap_or(&"Policy violation").trim().to_string();
        Some((rule, reason, severity))
    } else {
        None
    }
}

async fn evaluate_policy(
    state: &AppState,
    messages: &[Value],
) -> Result<(), (String, String, String)> {
    let policy = match &state.policy {
        Some(p) if p.enabled => p,
        _ => return Ok(()),
    };

    let prompt = extract_user_prompt(messages);
    if prompt.is_empty() { return Ok(()); }

    if let Some(violation) = check_policy_patterns(&prompt, policy) {
        state.log(LogLevel::Info, &format!("🛡️  Policy BLOCKED (pattern): rule={} severity={}", violation.0, violation.2));
        return Err(violation);
    }

    if policy.action != "log" {
        if let Some(violation) = evaluate_policy_with_judge(state, &prompt, policy).await {
            state.log(LogLevel::Info, &format!("🛡️  Policy BLOCKED (judge): rule={} severity={}", violation.0, violation.2));
            return Err(violation);
        }
    }

    Ok(())
}

fn policy_block_anthropic(rule: &str, reason: &str, severity: &str, is_streaming: bool, model: &str) -> Response {
    let msg = format!("Request blocked by policy. Rule: {} (severity: {}). {}", rule, severity, reason);
    if is_streaming {
        let body = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_policy\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"{}\",\"stop_reason\":null}}}}\n\n\
             event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
             event: content_block_delta\ndata: {}\n\n\
             event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
             event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            model,
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": msg}})
        );
        (StatusCode::OK, [(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
    } else {
        (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")],
         json!({"id":"msg_policy","type":"message","role":"assistant","content":[{"type":"text","text": msg}],"model":model,"stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0}}).to_string()
        ).into_response()
    }
}

fn policy_block_openai(rule: &str, reason: &str, severity: &str, is_streaming: bool) -> Response {
    let msg = format!("Request blocked by policy. Rule: {} (severity: {}). {}", rule, severity, reason);
    if is_streaming {
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content": msg},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
        );
        (StatusCode::OK, [(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
    } else {
        (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")],
         json!({"choices":[{"message":{"role":"assistant","content": msg},"finish_reason":"stop"}]}).to_string()
        ).into_response()
    }
}

// ============ Health Check Handler ============

async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let policy_status = match &state.policy {
        Some(p) if p.enabled => json!({"enabled": true, "action": p.action, "rules": p.rules.len(), "judge_model": p.judge_model}),
        _ => json!({"enabled": false}),
    };
    axum::Json(json!({
        "status": "ok",
        "service": "cortex-proxy",
        "default_model": state.default_model,
        "policy": policy_status,
    }))
}

// ============ Anthropic API Handler ============

async fn anthropic_handler(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let req_id = (start.elapsed().as_nanos() % 1_000_000) as u128;
    
    // Log request immediately
    eprintln!("DEBUG [{:06}] Request received, body size: {} bytes", req_id, body.len());
    
    // Convert Anthropic -> OpenAI format
    let (openai_req, is_streaming) = match anthropic_to_openai(&body, &state.default_model, &state.model_map) {
        Ok(r) => r,
        Err(e) => {
            state.log(LogLevel::Info, &format!("[{:06}] Parse error: {}", req_id, e));
            return anthropic_error(400, &e);
        }
    };
    
    let model = openai_req.get("model").and_then(|m| m.as_str()).unwrap_or("claude-4-sonnet");
    state.log(LogLevel::Debug, &format!("[{:06}] OpenAI req: {}", req_id, openai_req));
    
    if let Some(msgs) = openai_req.get("messages").and_then(|m| m.as_array()) {
        if let Err((rule, reason, severity)) = evaluate_policy(&state, msgs).await {
            return policy_block_anthropic(&rule, &reason, &severity, is_streaming, model);
        }
    }
    
    // Forward to Snowflake
    let url = format!("{}/chat/completions", state.base_url);
    let accept = if is_streaming { "text/event-stream" } else { "application/json" };
    
    let resp = match state.apply_host_header(state.client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", accept)
        .header("Accept-Encoding", "gzip")
        .header("User-Agent", "cortex-proxy/1.0")
        .header("Authorization", &state.auth_header)
        .header("X-Snowflake-Authorization-Token-Type", "PROGRAMMATIC_ACCESS_TOKEN")
        .json(&openai_req))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            state.log(LogLevel::Info, &format!("[{:06}] Upstream error: {}", req_id, e));
            return anthropic_error(502, &format!("Upstream error: {}", e));
        }
    };
    
    let status = resp.status();
    let elapsed = start.elapsed().as_millis();
    
    if !status.is_success() {
        let error_body = resp.text().await.unwrap_or_default();
        state.log(LogLevel::Info, &format!("[{:06}] HTTP {}: {}", req_id, status.as_u16(), &error_body[..error_body.len().min(200)]));
        
        // Handle conversation complete
        let error_lower = error_body.to_lowercase();
        if status.as_u16() == 400 && (
            error_lower.contains("final position") || error_lower.contains("tool_result")
        ) {
            return anthropic_complete(is_streaming, model);
        }
        return anthropic_error(status.as_u16(), &error_body);
    }
    
    if is_streaming {
        // Streaming response
        eprintln!("DEBUG [{:06}] Starting streaming response", req_id);
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        
        let state_clone = state.clone();
        let model_owned = model.to_string();
        
        let stream = async_stream::stream! {
            // Send message_start
            yield Ok::<_, std::io::Error>(Bytes::from(format!(
                "event: message_start\ndata: {}\n\n",
                json!({"type": "message_start", "message": {
                    "id": format!("msg_{:06}", req_id),
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model_owned,
                    "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }})
            )));
            
            let mut had_text_content = false;
            let mut final_events_sent = false;
            let mut buffer = String::new();
            // Track tools by ID (since Snowflake returns all tools with index=0)
            // Map: tool_id -> anthropic_index
            let mut tool_indices: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            // Track the current (most recent) tool being streamed for argument association
            let mut current_tool_id: Option<String> = None;
            let mut last_content_index: i32 = -1; // -1 = nothing started yet
            let mut tool_count = 0usize;
            
            let mut byte_stream = resp.bytes_stream();
            while let Some(chunk) = byte_stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        
                        while let Some(pos) = buffer.find("\n\n") {
                            let line = buffer[..pos].to_string();
                            buffer = buffer[pos + 2..].to_string();
                            
                            if !line.starts_with("data: ") { continue; }
                            let data = &line[6..];
                            if data == "[DONE]" { continue; }
                            
                            if let Ok(chunk_data) = serde_json::from_str::<Value>(data) {
                                eprintln!("DEBUG OPENAI CHUNK: {}", data);
                                let delta = &chunk_data["choices"][0]["delta"];
                                let finish = chunk_data["choices"][0].get("finish_reason");
                                
                                // Handle text content
                                if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                                    if !text.is_empty() {
                                        if !had_text_content {
                                            yield Ok(Bytes::from(format!(
                                                "event: content_block_start\ndata: {}\n\n",
                                                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
                                            )));
                                            had_text_content = true;
                                            last_content_index = 0;
                                        }
                                        yield Ok(Bytes::from(format!(
                                            "event: content_block_delta\ndata: {}\n\n",
                                            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}})
                                        )));
                                    }
                                }
                                
                                // Handle tool calls
                                // NOTE: Snowflake returns ALL tools with index=0, so we track by ID instead
                                if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                                    for tc in tool_calls {
                                        let tc_id = tc.get("id").and_then(|i| i.as_str()).filter(|s| !s.is_empty());
                                        let tc_func = tc.get("function");
                                        let tc_name = tc_func.and_then(|f| f.get("name")).and_then(|n| n.as_str()).filter(|s| !s.is_empty());
                                        
                                        // Check if this is a NEW tool call (has id AND name)
                                        if let (Some(id), Some(name)) = (tc_id, tc_name) {
                                            // Close previous content block if any
                                            if last_content_index >= 0 {
                                                yield Ok(Bytes::from(format!(
                                                    "event: content_block_stop\ndata: {}\n\n",
                                                    json!({"type": "content_block_stop", "index": last_content_index})
                                                )));
                                            }
                                            
                                            // Calculate this tool's Anthropic index
                                            let anthropic_index = if had_text_content { 1 + tool_count } else { tool_count };
                                            tool_count += 1;
                                            
                                            // Store tool index by ID and set as current
                                            tool_indices.insert(id.to_string(), anthropic_index);
                                            current_tool_id = Some(id.to_string());
                                            last_content_index = anthropic_index as i32;
                                            
                                            eprintln!("DEBUG STREAM: tool_use id={} name={} anthropic_index={}", id, name, anthropic_index);
                                            yield Ok(Bytes::from(format!(
                                                "event: content_block_start\ndata: {}\n\n",
                                                json!({"type": "content_block_start", "index": anthropic_index, "content_block": {
                                                    "type": "tool_use",
                                                    "id": id,
                                                    "name": name,
                                                    "input": {}
                                                }})
                                            )));
                                        }
                                        
                                        // Stream argument chunks - associate with current tool
                                        if let Some(args) = tc_func.and_then(|f| f.get("arguments")).and_then(|a| a.as_str()) {
                                            if !args.is_empty() {
                                                // Use the current tool's index (arguments follow their tool immediately)
                                                if let Some(ref tool_id) = current_tool_id {
                                                    if let Some(&anthropic_index) = tool_indices.get(tool_id) {
                                                        yield Ok(Bytes::from(format!(
                                                            "event: content_block_delta\ndata: {}\n\n",
                                                            json!({"type": "content_block_delta", "index": anthropic_index, "delta": {"type": "input_json_delta", "partial_json": args}})
                                                        )));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                
                                // Handle finish
                                if let Some(reason) = finish.and_then(|r| r.as_str()) {
                                    // Close the last content block
                                    if last_content_index >= 0 && !final_events_sent {
                                        yield Ok(Bytes::from(format!(
                                            "event: content_block_stop\ndata: {}\n\n",
                                            json!({"type": "content_block_stop", "index": last_content_index})
                                        )));
                                    }
                                    
                                    let stop_reason = if tool_count > 0 || reason == "tool_calls" {
                                        "tool_use"
                                    } else {
                                        match reason {
                                            "stop" => "end_turn",
                                            "length" | "max_tokens" => "max_tokens",
                                            _ => "end_turn",
                                        }
                                    };
                                    
                                    eprintln!("DEBUG STREAM: message_delta stop_reason={} tool_count={}", stop_reason, tool_count);
                                    yield Ok(Bytes::from(format!(
                                        "event: message_delta\ndata: {}\n\n",
                                        json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 0}})
                                    )));
                                    final_events_sent = true;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        state_clone.log(LogLevel::Info, &format!("[{:06}] Stream error: {}", req_id, e));
                        break;
                    }
                }
            }
            
            // Ensure we always emit content_block_stop and message_delta if content was started
            if !final_events_sent {
                if last_content_index >= 0 {
                    yield Ok(Bytes::from(format!(
                        "event: content_block_stop\ndata: {}\n\n",
                        json!({"type": "content_block_stop", "index": last_content_index})
                    )));
                }
                // If we streamed any tool calls, stop_reason should be "tool_use"
                let stop_reason = if tool_count > 0 { "tool_use" } else { "end_turn" };
                yield Ok(Bytes::from(format!(
                    "event: message_delta\ndata: {}\n\n",
                    json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 0}})
                )));
            }
            
            yield Ok(Bytes::from("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
            state_clone.log(LogLevel::Info, &format!("[{:06}] /v1/messages stream=true {}ms", req_id, start.elapsed().as_millis()));
        };
        
        return (StatusCode::OK, headers, Body::from_stream(stream)).into_response();
    } else {
        // Non-streaming
        let openai_resp: Value = match resp.json().await {
            Ok(r) => r,
            Err(e) => return anthropic_error(502, &format!("Invalid response: {}", e)),
        };
        
        state.log(LogLevel::Debug, &format!("[{:06}] OpenAI resp: {}", req_id, openai_resp));
        
        let anthropic_resp = openai_to_anthropic(&openai_resp, model, req_id);
        state.log(LogLevel::Info, &format!("[{:06}] /v1/messages stream=false {}ms", req_id, elapsed));
        
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&anthropic_resp).unwrap_or_default(),
        ).into_response();
    }
}

fn anthropic_error(code: u16, msg: &str) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [(header::CONTENT_TYPE, "application/json")],
        json!({"type": "error", "error": {"type": "api_error", "message": msg}}).to_string(),
    ).into_response()
}

fn anthropic_complete(is_streaming: bool, model: &str) -> Response {
    if is_streaming {
        (StatusCode::OK, [(header::CONTENT_TYPE, "text/event-stream")],
         format!("event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_done\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"{}\",\"stop_reason\":null}}}}\n\n\
                  event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
                  event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"Done.\"}}}}\n\n\
                  event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
                  event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}}}}\n\n\
                  event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n", model)
        ).into_response()
    } else {
        (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")],
         json!({"id":"msg_done","type":"message","role":"assistant","content":[{"type":"text","text":"Done."}],"model":model,"stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":1}}).to_string()
        ).into_response()
    }
}

// ============ OpenAI API Handler ============

async fn openai_handler(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let start = Instant::now();
    let req_id = (start.elapsed().as_nanos() % 1_000_000) as u128;
    let method = req.method().clone();
    let mut path = req.uri().path().to_string();
    
    if path.ends_with("/completions") && !path.contains("/chat/") {
        path = path.replace("/completions", "/chat/completions");
    }
    
    let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return error_response(500, &e.to_string()),
    };
    
    let (transformed, is_streaming) = transform_openai(&body, &state.model_map);
    
    if state.policy.is_some() {
        if let Ok(data) = serde_json::from_slice::<Value>(&body) {
            if let Some(msgs) = data.get("messages").and_then(|m| m.as_array()) {
                if let Err((rule, reason, severity)) = evaluate_policy(&state, msgs).await {
                    return policy_block_openai(&rule, &reason, &severity, is_streaming);
                }
            }
        }
    }
    
    let normalized_path = if path.starts_with("/v1/") { &path[3..] } else { &path };
    let normalized_path = if normalized_path.starts_with("chat/") {
        format!("/{}", normalized_path)
    } else if normalized_path.starts_with('/') {
        normalized_path.to_string()
    } else {
        format!("/{}", normalized_path)
    };
    let url = format!("{}{}", state.base_url, normalized_path);
    let accept = if is_streaming { "text/event-stream" } else { "application/json" };
    
    let mut req_builder = state.apply_host_header(state.client.request(method.clone(), &url)
        .header("Content-Type", "application/json")
        .header("Accept", accept)
        .header("Accept-Encoding", "gzip")
        .header("User-Agent", "cortex-proxy/1.0")
        .header("Authorization", &state.auth_header)
        .header("X-Snowflake-Authorization-Token-Type", "PROGRAMMATIC_ACCESS_TOKEN"));
    
    if method != Method::GET && method != Method::HEAD && !transformed.is_empty() {
        req_builder = req_builder.body(transformed);
    }
    
    let resp = match req_builder.send().await
    {
        Ok(r) => r,
        Err(e) => {
            return error_response(502, &format!("upstream error: {}", e));
        },
    };
    
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 400 && (body.contains("final position") || body.contains("tool_result")) {
            return synthetic_complete(is_streaming);
        }
        state.log(LogLevel::Info, &format!("[{:06}] HTTP {}", req_id, status.as_u16()));
        return (StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY), [(header::CONTENT_TYPE, "application/json")], body).into_response();
    }
    
    if is_streaming {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        let state_clone = state.clone();
        let stream = async_stream::stream! {
            let mut s = resp.bytes_stream();
            while let Some(chunk) = s.next().await {
                match chunk {
                    Ok(b) => yield Ok::<_, std::io::Error>(b),
                    Err(_) => break,
                }
            }
            state_clone.log(LogLevel::Info, &format!("[{:06}] {} stream {}ms", req_id, path, start.elapsed().as_millis()));
        };
        (StatusCode::OK, headers, Body::from_stream(stream)).into_response()
    } else {
        let body = resp.bytes().await.unwrap_or_default();
        state.log(LogLevel::Info, &format!("[{:06}] {} {}ms", req_id, path, start.elapsed().as_millis()));
        (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], body).into_response()
    }
}

fn transform_openai(
    body: &[u8],
    model_map: &std::collections::HashMap<String, String>,
) -> (Bytes, bool) {
    if body.is_empty() { return (Bytes::new(), false); }
    let mut data: Value = match serde_json::from_slice(body) { Ok(v) => v, Err(_) => return (Bytes::from(body.to_vec()), false) };
    let is_streaming = data.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    if let Some(model) = data.get("model").and_then(|m| m.as_str()) {
        let mapped = map_model(model, model_map);
        data["model"] = Value::String(mapped);
    }
    if let Some(mt) = data.get("max_tokens").cloned() { data["max_completion_tokens"] = mt; data.as_object_mut().map(|o| o.remove("max_tokens")); }
    for key in ["reasoning", "reasoningBudgetTokens", "service_tier", "parallel_tool_calls", "logprobs", "seed"] {
        data.as_object_mut().map(|o| o.remove(key));
    }
    (Bytes::from(serde_json::to_vec(&data).unwrap_or_default()), is_streaming)
}

fn error_response(code: u16, msg: &str) -> Response {
    (StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), [(header::CONTENT_TYPE, "application/json")], json!({"error": msg}).to_string()).into_response()
}

fn synthetic_complete(is_streaming: bool) -> Response {
    if is_streaming {
        (StatusCode::OK, [(header::CONTENT_TYPE, "text/event-stream")], "data: {\"choices\":[{\"delta\":{\"content\":\"Done.\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").into_response()
    } else {
        (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], json!({"choices":[{"message":{"role":"assistant","content":"Done."},"finish_reason":"stop"}]}).to_string()).into_response()
    }
}
