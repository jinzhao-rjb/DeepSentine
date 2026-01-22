use axum::{
    extract::{State, Path as AxumPath, WebSocketUpgrade},
      extract::ws::{Message, WebSocket},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use tokio::sync::broadcast;
use dotenv::dotenv;
use tiktoken_rs::cl100k_base;

mod client;
mod types;

use client::Client;

// ✅ [核心修正 1] 定义必须与初始化完全一致
struct AppState {
    client: Arc<Client>,
    ws_tx: broadcast::Sender<Value>,
    price_cache: Arc<Mutex<HashMap<String, types::PriceInfo>>>,
    total_cost: Arc<AtomicU64>,
    budget_limit: Arc<Mutex<f64>>, // 🆕 新增：熔断警戒线
    // 🆕 [性能优化] 全局复用 Tiktoken 编码器，避免重复加载
    bpe: Arc<tiktoken_rs::CoreBPE>,
}

#[tokio::main]
async fn main() {
    dotenv().ok();

    // 1. 初始化 Client
    let client = Client::create_default_client();
    let shared_client = Arc::new(client);

    // 2. 异步启动 Redis 并等待连接成功
    let client_for_redis = shared_client.clone();
    tokio::spawn(async move {
        let _ = client_for_redis.init_redis().await;
    });
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await; // 等待 Redis 连接

    // 3. 启动价格同步定时任务（24 小时一次）
    let client_for_sync = shared_client.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(86400));
        loop {
            interval.tick().await;
            println!("🔄 [哨兵] 开始定时同步 LiteLLM 价格...");
            if let Err(e) = client_for_sync.sync_litellm_prices().await {
                println!("⚠️ [哨兵] 价格同步失败: {}", e);
            }
        }
    });

    // 4. 启动时立即同步一次价格（从 LiteLLM 获取）
    let client_for_initial_sync = shared_client.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        println!("🔄 [哨兵] 启动时从 LiteLLM 同步最新价格...");
        if let Err(e) = client_for_initial_sync.sync_litellm_prices().await {
            println!("⚠️ [哨兵] 初始价格同步失败: {}", e);
        }
    });

    // 5. 冷启动：先从 Redis 加载存量数据到内存缓存
    let initial_prices = match shared_client.get_all_prices_from_redis().await {
        Ok(prices) => prices,
        Err(e) => {
            println!("⚠️ [哨兵] 从 Redis 加载初始价格失败: {}", e);
            HashMap::new()
        }
    };
    println!("🚀 [哨兵] 冷启动完成，已加载 {} 个模型价格到内存", initial_prices.len());

    // 6. 启动定时刷新内存缓存任务（1 小时一次）
    let client_for_cache = shared_client.clone();
    let price_cache = Arc::new(Mutex::new(initial_prices));
    let cache_for_task = price_cache.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(prices) = client_for_cache.get_all_prices_from_redis().await {
                let mut guard = cache_for_task.lock().unwrap();
                *guard = prices;
                println!("🔄 [哨兵] 内存价格缓存已刷新，当前支持 {} 个模型", guard.len());
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
        }
    });

    // 7. 准备所有零件
    let (tx, _) = broadcast::channel(100);
    let total_cost = Arc::new(AtomicU64::new(0));
    let budget_limit = Arc::new(Mutex::new(10.0)); // 默认熔断值：10元
    
    // 🆕 [性能优化] 初始化 Tiktoken 编码器（全局复用，避免重复加载）
    let bpe = Arc::new(cl100k_base().unwrap());

    // ✅ [核心修正 2] 初始化 AppState，确保不多不少，正好这六个字段
    let app_state = Arc::new(AppState {
        client: shared_client,
        ws_tx: tx,
        price_cache,
        total_cost,
        budget_limit,
        bpe,
    });

    // 6. 构建路由：使用 nest 确保 /v1 前缀绝对生效
    let api_routes = Router::new()
        .route("/sessions/:session_id/messages", get(get_chat_history))
        .route("/chat/completions", post(chat_handler))
        .route("/config/limit", post(update_limit))
        .route("/config/reset_cost", post(reset_cost))
        .route("/status", get(get_status))
        .route("/check_gate", get(check_gate))
        .route("/admin/refresh_prices", get(refresh_prices))
        .route("/ws", get(ws_handler)); // ✅ 将 WebSocket 也移到 /v1 命名空间内

    let app = Router::new()
        .route("/status", get(get_status))
        .route("/check_gate", get(check_gate))
        .nest("/v1", api_routes) // ✅ 使用 nest 确保 /v1 前缀绝对生效
        .with_state(app_state);

    // 7. 启动服务器
    let addr = "127.0.0.1:3001";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("🚀 [Sentinel] 哨兵核心已就位: http://{}", addr);
    
    // 🆕 [优雅停机] 捕获 Ctrl+C 信号
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install CTRL+C handler");
        println!("\n🛑 [Sentinel] 收到 Ctrl+C 信号，准备优雅停机...");
    };
    
    tokio::select! {
        _ = ctrl_c => {
            println!("🛑 [Sentinel] 开始优雅停机...");
            // 这里可以添加清理逻辑，比如关闭 Redis 连接等
            println!("✅ [Sentinel] 优雅停机完成");
            std::process::exit(0);
        }
        result = axum::serve(listener, app) => {
            result.unwrap();
        }
    }
}

// --- Handler 逻辑 ---

// ✅ 哨兵状态查询接口：获取当前费用和限额（单位统一为元）
#[axum::debug_handler]
async fn get_status(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let current = state.total_cost.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1_000_000_000_000.0;
    let limit = *state.budget_limit.lock().unwrap();
    
    Json(json!({
        "total_cost": current,
        "limit": limit
    }))
}

// ✅ 哨兵预检接口：让前端"预检"是否允许发送请求（单位统一为元）
#[axum::debug_handler]
async fn check_gate(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let current = state.total_cost.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1_000_000_000_000.0;
    let limit = *state.budget_limit.lock().unwrap();
    
    let allowed = current < limit;
    
    Json(json!({
        "allowed": allowed,
        "current_cost": current,
        "limit": limit
    }))
}

// ✅ 使用 impl IntoResponse 是解决所有 E0277 的终极良药
#[axum::debug_handler]
async fn chat_handler(
    State(state): State<Arc<AppState>>,
    Json(mut payload): Json<Value>,
) -> Result<Response, axum::http::StatusCode> {
    // ✅ 第一时间打印请求信息，避免静默失败
    println!("📨 [DEBUG] 收到新请求");
    if let Some(model) = payload.get("model").and_then(|m| m.as_str()) {
        println!("🔍 [DEBUG] 请求模型: {}", model);
    } else {
        println!("⚠️ [DEBUG] 请求中缺少 model 字段");
    }
    
    // A. 从请求体中获取 session_id（如果没有则使用默认值）
    let session_id = payload["session_id"].as_str().unwrap_or("default").to_string();
    
    // 🆕 [可选历史] 从请求体中获取是否加载历史对话的参数（默认为 false）
    let load_history = payload["load_history"].as_bool().unwrap_or(false);
    
    // B. 获取模型信息
    let model = payload["model"].as_str().unwrap_or("default").to_string();
    let simplified_model = state.client.simplify_model_id(&model);
    println!("🔍 [DEBUG] 原始模型名: {}, 简化后: {}", model, simplified_model);
    
    // 🆕 [累计熔断] 检查累计成本是否超过预算
    let current_cost = state.total_cost.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1_000_000_000_000.0;
    let budget_limit = *state.budget_limit.lock().unwrap();
    
    if current_cost >= budget_limit {
        println!("🛡️ [累计熔断生效] 累计成本 ￥{:.4} 已达到预算限额 ￥{:.4}", current_cost, budget_limit);
        return Ok((axum::http::StatusCode::PAYMENT_REQUIRED, 
                 json!({"error": "预算已耗尽", "current_cost": current_cost, "limit": budget_limit}).to_string()).into_response());
    }
    
    // 🆕 [单次计费模式 1] 重置计费逻辑：初始化临时计数器
    let request_cost = Arc::new(AtomicU64::new(0));
    
    // C. 注入记忆（只有当 load_history 为 true 时才加载历史对话）
    if load_history {
        let history = state.client.get_messages_from_redis(&session_id).await.unwrap_or_default();
        if let Some(messages) = payload.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for (i, msg) in history.into_iter().enumerate() {
                messages.insert(i, msg);
            }
        }
    }
    
    // D. 如果不是视觉模型，过滤掉历史记录中的图片内容
    if !simplified_model.contains("vl") {
        if let Some(messages) = payload.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for msg in messages.iter_mut() {
                if let Some(content) = msg.get_mut("content") {
                    if content.is_array() {
                        // 将多模态列表简化为纯文本字符串
                        if let Some(text_obj) = content.as_array().and_then(|a| a.iter().find(|i| i["type"] == "text")) {
                            *content = text_obj["text"].clone();
                        }
                    }
                }
            }
        }
    }
    
    match state.client.chat_completion(&model, payload.clone(), &session_id).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            
            // ✅ 检查是否为流式响应
            let is_stream = payload.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
            
            if is_stream {
                // 流式模式处理
                let stream = resp.bytes_stream();
                let _user_limit = *state.budget_limit.lock().unwrap();
                let model_for_cost = model.clone();
                let price_cache_for_cost = state.price_cache.lock().unwrap().clone();
                let state_for_billing = state.clone();
                let request_cost_for_ws = request_cost.clone();
                
                // 🆕 [异步旁路] 准备异步保存消息到 Redis
                let client_for_redis = state.client.clone();
                let session_id_for_redis = session_id.clone();
                
                // 🆕 [性能优化] 使用全局复用的 Tiktoken 编码器
                let bpe = state.bpe.clone();
                let completion_tokens = Arc::new(AtomicUsize::new(0));
                let completion_tokens_clone = completion_tokens.clone();
                
                // 🆕 [节流阀] 添加 token 计数器（每 10 个 token 才发一次计费）
                let token_emit_counter = Arc::new(AtomicUsize::new(0));
                let token_emit_counter_clone = token_emit_counter.clone();
                
                // 🆕 [铁血熔断] 添加熔断标志
                let is_fused = Arc::new(AtomicBool::new(false));
                let is_fused_clone = is_fused.clone();
                
                // 🆕 [优化发送频率] 添加发送计时器
                let last_emit = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
                let last_emit_clone = last_emit.clone();
                
                // 🆕 [节流阀] 添加上次发送金额跟踪
                let last_emitted_cost = Arc::new(std::sync::Mutex::new(0.0));
                let last_emitted_cost_clone = last_emitted_cost.clone();

            let mapped_stream = stream.map(move |item| {
                if is_fused_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(anyhow::anyhow!("Budget limit exceeded"));
                }
                
                match item {
                    Ok(chunk) => {
                        // 🆕 [零拷贝优化] 直接使用 chunk 引用，避免不必要的克隆
                        let chunk_str = std::str::from_utf8(&chunk).unwrap_or("");
                        
                        // 解析 SSE 格式：data: {...}\n\n
                        let json_opt = chunk_str
                            .lines()
                            .filter(|line| line.starts_with("data: "))
                            .filter_map(|line| {
                                let json_str = line.trim_start_matches("data: ");
                                if json_str == "[DONE]" {
                                    None
                                } else {
                                    serde_json::from_str::<Value>(json_str).ok()
                                }
                            })
                            .next();
                        
                        if let Some(json) = json_opt {
                            // 实时提取并计数 completion tokens
                            if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
                                if let Some(delta) = choices.first().and_then(|c| c.get("delta")) {
                                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                        let tokens = bpe.encode_with_special_tokens(content);
                                        let token_count = tokens.len();
                                        completion_tokens_clone.fetch_add(token_count, std::sync::atomic::Ordering::Relaxed);
                                        
                                        // 🆕 [节流阀] 累加 token 计数
                                        let total_tokens = token_emit_counter_clone.fetch_add(token_count, std::sync::atomic::Ordering::Relaxed) + token_count;
                                        
                                        // 🆕 [实时跳钱] 使用 tiktoken 精确计算成本
                                        let (estimated_chunk_cost, currency) = types::calculate_real_time_cost(
                                            &json,
                                            &model_for_cost,
                                            &price_cache_for_cost,
                                            &bpe
                                        );
                                        
                                        let cost_in_cents = (estimated_chunk_cost * 1_000_000_000_000.0) as u64;
                                        state_for_billing.total_cost.fetch_add(cost_in_cents, std::sync::atomic::Ordering::SeqCst);
                                        
                                        let _currency_symbol = if currency == "USD" { "$" } else { "￥" };
                                        println!("🔍 [DEBUG] 实时计数: 新增 {} tokens, 累计 {} tokens", token_count, completion_tokens_clone.load(std::sync::atomic::Ordering::Relaxed));
                                        println!("💰 [DEBUG] 实时计费: 本次估算 {}{:.9}, 累计 {}{:.6}", _currency_symbol, estimated_chunk_cost, _currency_symbol, state_for_billing.total_cost.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1_000_000_000_000.0);
                                        
                                        // 🆕 [流式熔断] 检查是否超过预算
                                        let current_total = state_for_billing.total_cost.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1_000_000_000_000.0;
                                        let budget_limit = *state_for_billing.budget_limit.lock().unwrap();
                                        
                                        if current_total >= budget_limit {
                                            println!("🛡️ [流式熔断生效] 累计成本 {}{:.4} 已达到预算限额 {}{:.4}", _currency_symbol, current_total, _currency_symbol, budget_limit);
                                            
                                            // 🆕 [铁血熔断] 设置熔断标志，立即中断流
                                            is_fused.store(true, std::sync::atomic::Ordering::SeqCst);
                                            
                                            // 🆕 [熔断处理] 发送熔断消息给灵动岛（确保立即发送）
                                            let fuse_msg = json!({
                                                "type": "billing",
                                                "model": model_for_cost.clone(),
                                                "cost": current_total,
                                                "currency": currency,
                                                "fused": true
                                            });
                                            
                                            // 🆕 [熔断处理] 发送错误信号（确保前端立即响应）
                                            let error_msg = json!({
                                                "type": "error",
                                                "reason": "budget_exceeded",
                                                "cost": current_total,
                                                "currency": currency
                                            });
                                            
                                            // 确保两个消息都发送成功
                                            let fuse_result = state_for_billing.ws_tx.send(fuse_msg);
                                            let error_result = state_for_billing.ws_tx.send(error_msg);
                                            
                                            if let Err(e) = fuse_result {
                                                println!("❌ [DEBUG] 熔断消息发送失败: {}", e);
                                            }
                                            if let Err(e) = error_result {
                                                println!("❌ [DEBUG] 错误信号发送失败: {}", e);
                                            }
                                            
                                            // 🆕 [铁血熔断] 立即中断连接，不再发送后续数据
                                            return Err(anyhow::anyhow!("Budget limit exceeded"));
                                        }
                                        
                                        // 🆕 [节流阀] 只有满足以下条件之一才发送：
                                        // 1. 累计达到 10 个 token（减少前端负担）
                                        // 2. 金额变动超过 0.0001 元
                                        // 3. 距离上次发送超过 200ms（确保流畅性）
                                        if estimated_chunk_cost > 0.0 {
                                            let should_send_by_tokens = total_tokens >= 10;
                                            let cost_delta = current_total - *last_emitted_cost_clone.lock().unwrap();
                                            let should_send_by_cost = cost_delta.abs() >= 0.0001;
                                            
                                            let should_send_by_time = {
                                                let mut last = last_emit_clone.lock().unwrap();
                                                let elapsed = last.elapsed().as_millis();
                                                if elapsed >= 200 {
                                                    *last = std::time::Instant::now();
                                                    true
                                                } else {
                                                    false
                                                }
                                            };
                                            
                                            // 只有满足条件之一才发送
                                            if should_send_by_tokens || should_send_by_cost || should_send_by_time {
                                                let billing_msg = json!({
                                                    "type": "billing",
                                                    "model": model_for_cost.clone(),
                                                    "cost": current_total,
                                                    "currency": currency
                                                });
                                                
                                                match state_for_billing.ws_tx.send(billing_msg) {
                                                    Ok(_) => {},
                                                    Err(e) => println!("❌ [DEBUG] billing 消息发送失败: {}", e),
                                                }
                                                
                                                // 更新上次发送金额
                                                *last_emitted_cost_clone.lock().unwrap() = current_total;
                                                
                                                // 重置 token 计数器
                                                token_emit_counter_clone.store(0, std::sync::atomic::Ordering::Relaxed);
                                            }
                                        }
                                    }
                                }
                            }
                            
                            // 检查是否是最后一个 chunk（包含 usage 字段）
                            if let Some(usage) = json.get("usage") {
                                println!("🔍 [DEBUG] 检测到最后一个 chunk，包含 usage: {}", usage);
                                
                                let usage_struct: types::Usage = match serde_json::from_value(usage.clone()) {
                                    Ok(u) => u,
                                    Err(e) => {
                                        println!("⚠️ [DEBUG] 解析 usage 失败: {}", e);
                                        return Ok(chunk);
                                    }
                                };
                                
                                // 使用官方的 prompt_tokens 和实时计数的 completion_tokens
                                let prompt_tokens = usage_struct.prompt_tokens.unwrap_or(0) as f64;
                                let real_completion_tokens = completion_tokens.load(std::sync::atomic::Ordering::Relaxed) as f64;
                                
                                let (actual_cost, currency) = types::calculate_actual_cost_with_tokens(&model_for_cost, prompt_tokens, real_completion_tokens, &price_cache_for_cost);
                                
                                if actual_cost > 0.0 {
                                    let currency_symbol = if currency == "USD" { "$" } else { "￥" };
                                    
                                    let billing_msg = json!({
                                        "type": "billing",
                                        "model": model_for_cost,
                                        "cost": actual_cost,
                                        "currency": currency
                                    });
                                    
                                    println!("🔍 [DEBUG] 流式模式最终 billing 消息: {}", billing_msg);
                                    println!("💰 [WebSocket] 广播计费: {} = {}{:.9}", model_for_cost, currency_symbol, actual_cost);
                                    
                                    // 🆕 [单次计费模式 3] 同步更新：立即通过 WebSocket 发送给灵动岛
                                    match state_for_billing.ws_tx.send(billing_msg) {
                                        Ok(_) => println!("✅ [DEBUG] billing 消息发送成功"),
                                        Err(e) => println!("❌ [DEBUG] billing 消息发送失败: {}", e),
                                    }
                                    
                                    // 更新临时计数器（以分为单位）
                                    request_cost_for_ws.fetch_add((actual_cost * 100.0) as u64, std::sync::atomic::Ordering::Relaxed);
                                }
                                
                                // 🆕 [异步旁路] 使用 tokio::spawn 异步保存消息到 Redis（不阻塞主流）
                                let client_clone = client_for_redis.clone();
                                let sid = session_id_for_redis.clone();
                                let payload_clone = payload.clone();
                                let json_clone = json.clone(); // 🆕 克隆 json 以便在异步闭包中使用
                                tokio::spawn(async move {
                                    // 保存用户消息
                                    if let Some(messages) = payload_clone.get("messages").and_then(|m| m.as_array()) {
                                        if let Some(last_msg) = messages.last() {
                                            let _ = client_clone.save_messages_to_redis(&sid, last_msg).await;
                                        }
                                    }
                                    // 保存助手回复（从最后一个 chunk 中提取）
                                    if let Some(choices) = json_clone.get("choices").and_then(|c| c.as_array()) {
                                        if let Some(choice) = choices.first() {
                                            if let Some(message) = choice.get("message") {
                                                let _ = client_clone.save_messages_to_redis(&sid, message).await;
                                            }
                                        }
                                    }
                                });
                            }
                        }

                        Ok(chunk)
                    }
                    Err(e) => Err(anyhow::anyhow!("Stream error: {}", e))
                }
            });
            
            Ok(axum::response::Response::builder()
                .status(status)
                .header("Content-Type", "text/event-stream; charset=utf-8")
                .header("Cache-Control", "no-cache")
                .header("Connection", "keep-alive")
                .header("X-Content-Type-Options", "nosniff")
                .body(axum::body::Body::from_stream(mapped_stream))
                .unwrap()
                .into_response())
            } else {
                // 非流模式：提取 usage 并广播计费
                println!("🔍 [DEBUG] 非流模式，提取 usage");
                
                // 🆕 [异步旁路] 准备异步保存消息到 Redis
                let client_clone = state.client.clone();
                let sid = session_id.clone();
                let payload_clone = payload.clone();
                
                // 先保存状态码和 headers
                let status_code = resp.status().as_u16();
                let content_type = resp.headers().get("Content-Type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json")
                    .to_string();
                
                // 读取响应体
                let response_bytes = resp.bytes().await.map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
                let response_json: Value = serde_json::from_slice(&response_bytes).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
                
                println!("🔍 [DEBUG] 非流响应 JSON: {}", response_json);
                
                // 🆕 [异步旁路] 使用 tokio::spawn 异步保存消息到 Redis（不阻塞主流）
                let response_json_clone = response_json.clone(); // 🆕 克隆 response_json 以便在异步闭包中使用
                
                tokio::spawn(async move {
                    // 保存用户消息
                    if let Some(messages) = payload_clone.get("messages").and_then(|m| m.as_array()) {
                        if let Some(last_msg) = messages.last() {
                            let _ = client_clone.save_messages_to_redis(&sid, last_msg).await;
                        }
                    }
                    // 保存助手回复（从响应中提取）
                    if let Some(choices) = response_json_clone.get("choices").and_then(|c| c.as_array()) {
                        if let Some(choice) = choices.first() {
                            if let Some(message) = choice.get("message") {
                                let _ = client_clone.save_messages_to_redis(&sid, message).await;
                            }
                        }
                    }
                });
                
                // 检查是否有 usage 字段
                if let Some(usage) = response_json.get("usage") {
                    let simplified_model = model.to_lowercase().trim().to_string();
                    let usage_struct: types::Usage = serde_json::from_value(usage.clone()).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
                    let (actual_cost, currency) = types::calculate_actual_cost(&simplified_model, &usage_struct, &state.price_cache.lock().unwrap());
                    
                    if actual_cost > 0.0 {
                        let currency_symbol = if currency == "USD" { "$" } else { "￥" };
                        let current_total = state.total_cost.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1_000_000_000_000.0;
                        
                        let billing_msg = json!({
                            "type": "billing",
                            "model": model,
                            "cost": current_total,
                            "currency": currency
                        });
                        
                        println!("🔍 [DEBUG] 非流模式发送 billing 消息: {}", billing_msg);
                        println!("💰 [WebSocket] 广播计费: {} = {}{:.9}", model, currency_symbol, current_total);
                        
                        // 🆕 [单次计费模式 3] 同步更新：立即通过 WebSocket 发送给灵动岛
                        match state.ws_tx.send(billing_msg) {
                            Ok(_) => println!("✅ [DEBUG] billing 消息发送成功"),
                            Err(e) => println!("❌ [DEBUG] billing 消息发送失败: {}", e),
                        }
                        
                        // 更新临时计数器（以分为单位）
                        request_cost.fetch_add((actual_cost * 100.0) as u64, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        println!("⚠️ [DEBUG] 非流模式成本为 0，跳过计费广播");
                    }
                } else {
                    println!("⚠️ [DEBUG] 非流响应中未找到 usage 字段");
                }
                
                // 返回原始响应
                Ok(axum::response::Response::builder()
                    .status(status_code)
                    .header("Content-Type", content_type)
                    .body(axum::body::Body::from(response_bytes))
                    .unwrap())
            }
        }
        Err(e) => Ok((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()),
    }
}

#[axum::debug_handler]
async fn update_limit(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    if let Some(new_limit) = payload["limit"].as_f64() {
        let mut limit = state.budget_limit.lock().unwrap();
        *limit = new_limit;
        let currency_symbol = if state.client.currency_base == "USD" { "$" } else { "￥" };
        println!("🛡️ [哨兵] 熔断阈值已更新为: {}{}", currency_symbol, new_limit);
        return (axum::http::StatusCode::OK, "限额更新成功").into_response();
    }
    (axum::http::StatusCode::BAD_REQUEST, "无效的限额数值").into_response()
}

#[axum::debug_handler]
async fn reset_cost(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    state.total_cost.store(0, std::sync::atomic::Ordering::Relaxed);
    let currency_symbol = if state.client.currency_base == "USD" { "$" } else { "￥" };
    println!("💰 [哨兵] 累计费用已重置为: {}{}", currency_symbol, 0.0);
    Json(json!({
        "success": true,
        "message": "累计费用已重置为 0"
    }))
}

#[axum::debug_handler]
async fn refresh_prices(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    println!("🔄 [哨兵] 收到刷新价格缓存请求...");
    
    match state.client.get_all_prices_from_redis().await {
        Ok(prices) => {
            let mut guard = state.price_cache.lock().unwrap();
            *guard = prices;
            println!("✅ [哨兵] 价格缓存已刷新，当前支持 {} 个模型", guard.len());
            Json(json!({
                "success": true,
                "message": format!("成功刷新 {} 个模型价格", guard.len()),
                "count": guard.len()
            }))
        }
        Err(e) => {
            println!("❌ [哨兵] 刷新价格缓存失败: {}", e);
            Json(json!({
                "success": false,
                "message": format!("刷新失败: {}", e)
            }))
        }
    }
}

#[axum::debug_handler]
async fn get_chat_history(
    AxumPath(session_id): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let history = state.client.get_messages_from_redis(&session_id).await.unwrap_or_default();
    Json(json!({ "session_id": session_id, "history": history }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.ws_tx.subscribe();
    
    loop {
        tokio::select! {
            msg = rx.recv() => {
                if let Ok(msg) = msg {
                    if socket.send(Message::Text(msg.to_string())).await.is_err() {
                        break;
                    }
                }
            }
            Some(Ok(ws_msg)) = socket.next() => {
                match ws_msg {
                    Message::Ping(data) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => {
                        break;
                    }
                    _ => {}
                }
            }
            else => {
                break;
            }
        }
    }
}
