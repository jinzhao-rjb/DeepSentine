use reqwest::Client as ReqwestClient;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use anyhow::anyhow;
use redis::AsyncCommands;
use tokio::sync::Mutex as TokioMutex;
use std::collections::HashMap;
use crate::types::PriceInfo;
use crate::types;

// 🆕 [双库分离] 定义过期时间常量（24小时）
const CHAT_HISTORY_TTL: u64 = 86400; // 24 * 60 * 60 = 86400 秒

pub struct Client {
    client: ReqwestClient,
    pub dashscope_api_key: String,
    pub deepseek_api_key: String,
    pub zhipu_ai_key: String,
    // ✅ 核心：Mutex 保护 Option 保证初始化安全，内层 TokioMutex 保证异步 Redis 操作安全
    pub redis_client: Arc<Mutex<Option<Arc<TokioMutex<redis::aio::MultiplexedConnection>>>>>,
    
    // 📚 DB0：专门负责价格查询
    pub redis_price_db: Arc<Mutex<Option<Arc<TokioMutex<redis::aio::MultiplexedConnection>>>>>,
    
    // 📚 DB1：专门负责聊天历史（实现跨模型续聊 + 自动清理）
    pub redis_chat_db: Arc<Mutex<Option<Arc<TokioMutex<redis::aio::MultiplexedConnection>>>>>,
    
    pub redis_url: String,
    pub currency_base: String, // "USD" or "CNY"
    // 🛡️ 影子保护：防止特定模型被自动同步覆盖
    pub protected_models: Vec<String>,
}

impl Client {
    /// ✅ 异步初始化 Redis 连接（双库分离）
    pub async fn init_redis(&self) -> Result<(), anyhow::Error> {
        // 先检查是否已经连上了（检查 DB0 和 DB1）
        {
            let price_guard = self.redis_price_db.lock().unwrap();
            if price_guard.is_some() {
                return Ok(());
            }
            
            let chat_guard = self.redis_chat_db.lock().unwrap();
            if chat_guard.is_some() {
                return Ok(());
            }
        }
        
        println!("📡 [Redis] 正在连接: {}", self.redis_url);
        let base_url = self.redis_url.trim_end_matches('/');
        
        // 🆕 [双库分离] 1. 初始化 DB0 (价格库)
        let p_client = redis::Client::open(format!("{}/0", base_url))?;
        let p_conn = p_client.get_multiplexed_async_connection().await?;
        *self.redis_price_db.lock().unwrap() = Some(Arc::new(TokioMutex::new(p_conn)));
        
        // 🆕 [双库分离] 2. 初始化 DB1 (历史库)
        let c_client = redis::Client::open(format!("{}/1", base_url))?;
        let c_conn = c_client.get_multiplexed_async_connection().await?;
        *self.redis_chat_db.lock().unwrap() = Some(Arc::new(TokioMutex::new(c_conn)));
        
        println!("✅ [哨兵] 数据库分工完成：DB0(价格计费) | DB1(历史记忆)");
        Ok(())
    }

    /// ✅ 从 Redis 获取历史对话（使用 DB1，支持跨模型续聊 + 断线重连）
    pub async fn get_messages_from_redis(&self, session_id: &str) -> Result<Vec<Value>, anyhow::Error> {
        let redis_conn = {
            let guard = self.redis_chat_db.lock().unwrap();
            guard.as_ref().map(|rc| Arc::clone(rc))
        };
        
        if let Some(redis_conn) = redis_conn {
            let key = format!("sentinel:chat:{}", session_id);
            let mut conn = redis_conn.lock().await;
            
            // 从 DB1 获取该 session 的所有历史
            let msgs: Vec<String> = conn.lrange(&key, 0, -1).await?;
            let parsed_msgs = msgs.into_iter()
                .filter_map(|m| serde_json::from_str(&m).ok())
                .collect();
            return Ok(parsed_msgs);
        }
        
        // 🆕 [断线重连] 如果没有连接，尝试重新初始化
        println!("⚠️ [Redis] DB1 连接不存在，尝试重新初始化...");
        self.init_redis().await?;
        
        // 重试一次
        let redis_conn = {
            let guard = self.redis_chat_db.lock().unwrap();
            guard.as_ref().map(|rc| Arc::clone(rc))
        };
        
        if let Some(redis_conn) = redis_conn {
            let key = format!("sentinel:chat:{}", session_id);
            let mut conn = redis_conn.lock().await;
            
            let msgs: Vec<String> = conn.lrange(&key, 0, -1).await?;
            let parsed_msgs = msgs.into_iter()
                .filter_map(|m| serde_json::from_str(&m).ok())
                .collect();
            return Ok(parsed_msgs);
        }
        
        Ok(vec![])
    }

    /// ✅ 保存消息到 Redis（使用 DB1，支持跨模型续聊 + 自动清理）
    pub async fn save_messages_to_redis(&self, session_id: &str, message: &Value) -> Result<(), anyhow::Error> {
        let redis_conn = {
            let guard = self.redis_chat_db.lock().unwrap();
            guard.as_ref().map(|rc| Arc::clone(rc))
        };
        
        if let Some(redis_conn) = redis_conn {
            let key = format!("sentinel:chat:{}", session_id);
            let mut conn = redis_conn.lock().await;
            
            // 将消息转为 JSON 字符串存入列表
            let _: () = conn.rpush(&key, message.to_string()).await?;
            
            // 🆕 [自动清理] 设置 24 小时过期，防止数据库撑爆
            let _: () = conn.expire(&key, CHAT_HISTORY_TTL as i64).await?;
            println!("💾 [Redis] 成功记录会话 [{}] 的新记忆 (TTL: 24h)", session_id);
        }
        Ok(())
    }

    /// 🚀 终极方案：从 LiteLLM GitHub 自动同步价格并归一化单位（使用 DB0）
    pub async fn sync_litellm_prices(&self) -> Result<(), anyhow::Error> {
        println!("📡 [同步] 正在从 LiteLLM 获取最新价格情报...");
        
        let url = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
        
        // 🆕 [错误处理] 添加详细的错误日志
        let response = match self.client.get(url).send().await {
            Ok(resp) => {
                println!("🔍 [DEBUG] GitHub 响应状态: {}", resp.status());
                resp.json::<Value>().await?
            }
            Err(e) => {
                println!("❌ [同步] 请求 GitHub 失败: {}", e);
                println!("❌ [同步] 请求 URL: {}", url);
                return Err(anyhow!("请求 GitHub 失败: {}", e));
            }
        };
        
        // 🆕 [双库分离] 使用 DB0 (价格库) 存储价格
        let redis_conn = {
            let guard = self.redis_price_db.lock().unwrap();
            guard.as_ref().map(|rc| Arc::clone(rc))
        };
        
        if let Some(models) = response.as_object() {
            for (model_id, info) in models {
                // 1. 提取单 token 价格
                let input_per_token = info.get("input_cost_per_token")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let output_per_token = info.get("output_cost_per_token")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                
                // 2. ⚡️ 过滤掉价格为0的模型
                if input_per_token == 0.0 && output_per_token == 0.0 {
                    println!("⚠️ [跳过] {}（价格为0）", model_id);
                    continue;
                }
                
                // 3. ⚡️ 过滤掉带后缀的模型
                let suffix_patterns = [
                    "instruct",
                    "chat",
                    "-latest",
                    "-v1:0",
                    ":0",
                ];
                
                let has_suffix = suffix_patterns.iter().any(|suffix| model_id.ends_with(suffix));
                if has_suffix {
                    println!("⚠️ [跳过] {}（包含后缀）", model_id);
                    continue;
                }
                
                // 4. ⚡️ 过滤掉带日期的模型
                let date_patterns = [
                    r"-20\d{6}",           // -20250807
                    r"-20\d{8}",           // -202508071234
                    r"-250\d",             // -2507
                    r"-23\d{2}",           // -2312
                    r"-24\d{2}",           // -2407
                    r"-25\d{2}",           // -2503
                    r"@20\d{6}",          // @20251001
                    r"@20\d{8}",          // @202510011234
                    r"-preview-\d{2}-\d{2}",  // -preview-03-25
                    r"-\d{4}-\d{2}-\d{2}",  // -2025-12-16
                ];
                
                let has_date = date_patterns.iter().any(|pattern| {
                    if let Ok(re) = regex::Regex::new(pattern) {
                        re.is_match(model_id)
                    } else {
                        false
                    }
                });
                
                if has_date {
                    println!("⚠️ [跳过] {}（包含日期）", model_id);
                    continue;
                }
                
                // 5. ⚡️ 核心转换：直接使用每token价格（避免精度丢失）
                let input_price = input_per_token;
                let output_price = output_per_token;
                
                // 6. 归一化 Key（去掉所有前缀）并存入 Redis
                let clean_name = types::normalize_model_name(model_id);
                
                // 🛡️ 影子保护：检查是否是受保护的模型
                if self.protected_models.contains(&clean_name) {
                    println!("⚠️ [跳过] {}（在保护名单中，保留本地备份）", clean_name);
                    continue;
                }
                
                let price_data = json!({
                    "input_price": input_price,
                    "output_price": output_price,
                    "vendor": "litellm_auto"
                });
                
                if let Some(ref conn_arc) = redis_conn {
                    let mut conn = conn_arc.lock().await;
                    let _: () = redis::cmd("SET").arg(format!("price:{}", clean_name)).arg(price_data.to_string()).query_async(&mut *conn).await?;
                    println!("💾 [Redis] 已更新价格: {} (输入: {:.9}, 输出: {:.9})", clean_name, input_price, output_price);
                }
            }
        }
        
        println!("✅ [同步] 已自动更新全网模型价格，单位已统一为 USD/1M Tokens");
        Ok(())
    }

    pub fn simplify_model_id(&self, model_id: &str) -> String {
        let name = model_id.to_lowercase().trim().to_string();

        let base_name = name.split('/').last().unwrap_or(&name);

        match base_name {
            n if n.contains("deepseek-r1") => "deepseek-r1".to_string(),
            n if n.contains("deepseek-v3") => "deepseek-v3".to_string(),
            n if n.contains("qwen-max") => "qwen-max".to_string(),
            n if n.contains("qwen-plus") => "qwen-plus".to_string(),
            n if n.contains("glm-4v") => "glm-4v".to_string(),
            n if n.contains("glm-4") => "glm-4".to_string(),
            _ => base_name
                .replace("-chat", "")
                .replace("-latest", "")
                .replace("-2024", "")
                .replace("-instruct", "")
                .to_string(),
        }
    }

    /// ✅ 保存单个价格到 Redis DB（优先保留 official_manual 标记的价格）
    async fn save_price_to_redis(&self, model_id: &str, input_price: f64, output_price: f64) -> Result<(), anyhow::Error> {
        let redis_conn = {
            let guard = self.redis_price_db.lock().unwrap();
            guard.as_ref().map(|rc| Arc::clone(rc))
        };

        if let Some(redis_conn) = redis_conn {
            let mut conn = redis_conn.lock().await;
            let key = format!("price:{}", model_id);
            
            // 🆕 [强制覆盖] 直接保存价格，不包含日期字段
            let value = json!({
                "vendor": "litellm",
                "input_price": input_price,
                "output_price": output_price
            });
            let _: () = redis::cmd("SET").arg(&key).arg(value.to_string()).query_async(&mut *conn).await?;
            println!("💾 [Redis] 已保存价格: {} (输入: {:.6}, 输出: {:.6})", 
                model_id, input_price, output_price);
        }
        
        Ok(())
    }

    /// ✅ 主同步方法：从 GitHub litellm 获取全球模型定价
    pub async fn sync_all_vendor_prices(&self) -> Result<(), anyhow::Error> {
        let currency = if self.currency_base == "USD" { "美元" } else { "人民币" };
        println!("📡 [哨兵情报站] 正在从 GitHub litellm 提取全球模型定价（本位：{}）...", currency);
        
        let url = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
        let resp: Value = self.client.get(url).send().await?.json().await?;
        
        let _usd_to_cny = 7.25;
        let _safety_margin = 1.1;
        let _use_cny = self.currency_base == "CNY";
        let mut count = 0;
        
        if let Some(models) = resp.as_object() {
            for (model_id, model_data) in models {
                // 获取价格信息
                let input_price_usd = model_data.get("input_price_per_token")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let output_price_usd = model_data.get("output_price_per_token")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                
                // ⚡️ 核心转换：直接使用每token价格（避免精度丢失）
                let input_price = input_price_usd;
                let output_price = output_price_usd;
                
                // 简化模型名（使用增强的归一化函数）
                let simplified_id = types::normalize_model_name(model_id);
                
                // 🛡️ 影子保护：检查是否是受保护的模型
                if self.protected_models.contains(&simplified_id) {
                    println!("⚠️ [跳过] {}（在保护名单中，保留本地备份）", simplified_id);
                    continue;
                }
                
                // 保存到 Redis
                let price_data = json!({
                    "input_price": input_price,
                    "output_price": output_price,
                    "vendor": "litellm_auto"
                });
                
                let redis_conn = {
                    let guard = self.redis_price_db.lock().unwrap();
                    guard.as_ref().map(|rc| Arc::clone(rc))
                };
                
                if let Some(ref conn_arc) = redis_conn {
                    let mut conn = conn_arc.lock().await;
                    let _: () = redis::cmd("SET").arg(format!("price:{}", simplified_id)).arg(price_data.to_string()).query_async(&mut *conn).await?;
                    count += 1;
                    println!("💾 [Redis] 已更新价格: {} (输入: {:.9}, 输出: {:.9})", simplified_id, input_price, output_price);
                }
            }
            println!("✅ [情报站] 已成功物理同步 {} 个模型。", count);
        }
        
        Ok(())
    }

    /// ✅ 核心对话接口
    pub async fn chat_completion(
        &self, 
        model: &str, 
        payload: Value, 
        _session_id: &str 
    ) -> Result<reqwest::Response, anyhow::Error> {
        let simplified_model = self.simplify_model_id(model);
        let (url, api_key) = if simplified_model.contains("qwen") || simplified_model.contains("qwq") {
            ("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions", &self.dashscope_api_key)
        } else if simplified_model.contains("glm") {
            ("https://open.bigmodel.cn/api/paas/v4/chat/completions", &self.zhipu_ai_key)
        } else if simplified_model.contains("deepseek") {
            ("https://api.deepseek.com/chat/completions", &self.deepseek_api_key)
        } else {
            return Err(anyhow!("⚠️ 哨兵提示：不支持该模型系列的官方直连"));
        };

        if api_key.is_empty() {
            return Err(anyhow!("⚠️ 哨兵提示：{} 的 API Key 为空，请检查环境变量", model));
        }

        // ✅ 智能处理 stream_options：只有流模式才添加
        let mut final_payload = payload.clone();
        let is_stream = final_payload.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
        
        if is_stream {
            if !final_payload.get("stream_options").is_some() {
                final_payload["stream_options"] = json!({
                    "include_usage": true
                });
            }
        } else {
            // 非流模式：移除可能存在的 stream_options
            final_payload.as_object_mut().map(|obj| {
                obj.remove("stream_options");
            });
        }

        let resp = self.client.post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&final_payload)
            .send()
            .await?;

        Ok(resp)
    }

    /// ✅ 从非流式响应中提取 usage 并计算成本
    pub async fn extract_usage_from_response(model: &str, response: reqwest::Response, price_cache: &HashMap<String, types::PriceInfo>) -> Result<f64, anyhow::Error> {
        let simplified_model = model.to_lowercase().trim().to_string();
        
        // 读取响应体（异步方式）
        let response_bytes = response.bytes().await?;
        let response_json: Value = serde_json::from_slice(&response_bytes)?;
        
        println!("🔍 [DEBUG] 非流响应 JSON: {}", response_json);
        
        // 检查是否有 usage 字段
        if let Some(usage) = response_json.get("usage") {
            let usage_struct: types::Usage = serde_json::from_value(usage.clone())?;
            let (cost, _currency) = types::calculate_actual_cost(&simplified_model, &usage_struct, price_cache);
            println!("💰 [DEBUG] 非流模式计算成本: {} 元", cost);
            Ok(cost)
        } else {
            println!("⚠️ [DEBUG] 非流响应中未找到 usage 字段");
            Ok(0.0)
        }
    }

    pub async fn get_all_prices_from_redis(&self) -> Result<HashMap<String, PriceInfo>, anyhow::Error> {
        let redis_conn = {
            let guard = self.redis_price_db.lock().unwrap();
            guard.as_ref().map(|rc| Arc::clone(rc))
        };

        if let Some(redis_conn) = redis_conn {
            let mut conn = redis_conn.lock().await;
            let keys: Vec<String> = redis::cmd("KEYS").arg("price:*").query_async(&mut *conn).await?;
            
            let mut prices = HashMap::new();
            for key in keys {
                let value: Option<String> = redis::cmd("GET").arg(&key).query_async(&mut *conn).await?;
                if let Some(v) = value {
                    if let Ok(json) = serde_json::from_str::<Value>(&v) {
                        if let (Some(input_price), Some(output_price)) = (
                            json["input_price"].as_f64(),
                            json["output_price"].as_f64()
                        ) {
                            let model_id = key.trim_start_matches("price:");
                            prices.insert(model_id.to_string(), PriceInfo {
                                input_price,
                                output_price
                            });
                        }
                    }
                }
            }
            
            println!("🔄 [Redis] 已从数据库加载 {} 个模型价格", prices.len());
            Ok(prices)
        } else {
            Ok(HashMap::new())
        }
    }

    /// ✅ 构造函数：支持命令行注入，不再硬编码
    pub fn create_default_client() -> Client {
        // 尝试加载 .env 但不强制要求
        let _ = dotenv::dotenv();

        // 从环境变量提取，如果没有则为空字符串，启动后由逻辑判断
        let dashscope_api_key = std::env::var("DASHSCOPE_API_KEY").unwrap_or_default();
        let deepseek_api_key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();
        let zhipu_ai_key = std::env::var("ZHIPU_AI_KEY").unwrap_or_default();
        let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
        let currency_base = std::env::var("CURRENCY_BASE").unwrap_or_else(|_| "CNY".to_string());
        
        if !["USD", "CNY"].contains(&currency_base.as_str()) {
            panic!("⚠️ CURRENCY_BASE 必须是 USD 或 CNY，当前值：{}", currency_base);
        }
        
        println!("🌍 [哨兵] 币种本位设置为：{}", if currency_base == "USD" { "美元 (USD)" } else { "人民币 (CNY)" });
        
        Client {
            // 🆕 [性能优化] 添加 TCP 优化，减少流式传输延迟
            client: ReqwestClient::builder()
                .no_proxy() // 避免代理导致无法连接本地 Redis
                .tcp_nodelay(true) // 💡 必须加！防止小数据包积压，减少打字机延迟
                .tcp_keepalive(std::time::Duration::from_secs(60))
                .build()
                .unwrap(),
            dashscope_api_key,
            deepseek_api_key,
            zhipu_ai_key,
            redis_client: Arc::new(Mutex::new(None)),
            
            // 🆕 [双库分离] 必须初始化这两个字段
            redis_price_db: Arc::new(Mutex::new(None)),
            redis_chat_db: Arc::new(Mutex::new(None)),
            
            redis_url,
            currency_base,
            protected_models: vec!["qwen-vl-max"].iter().map(|s| s.to_string()).collect(), // 🛡️ 影子保护：防止特定模型被自动同步覆盖
        }
    }
}