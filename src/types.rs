use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// 🎯 配置项：是否强制国内模型显示人民币
// true：所有国内模型（qwen/glm/yi/deepseek）都显示人民币，数值会自动换算（乘7.2）
// false：按数据库原始数值显示
const FORCE_CNY_FOR_CHINESE_MODELS: bool = true;

#[allow(dead_code, unused_variables)]

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PriceInfo {
    pub input_price: f64,
    pub output_price: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Usage {
    #[serde(alias = "input_tokens")]
    pub prompt_tokens: Option<u64>,
    #[serde(alias = "output_tokens")]
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// ✅ 从流式响应中计算实时成本（基于 tiktoken 的实时精确计算）
/// 🆕 [性能优化] 接受外部传入的 bpe 编码器，避免重复加载
pub fn calculate_real_time_cost(chunk: &Value, model_id: &str, price_cache: &HashMap<String, PriceInfo>, bpe: &tiktoken_rs::CoreBPE) -> (f64, String) {
    // 尝试从 chunk 中提取内容并使用 tiktoken 进行精确计算
    if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
        if let Some(choice) = choices.first() {
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                    // 🆕 [性能优化] 直接使用外部传入的 bpe 编码器（全局复用）
                    let tokens = bpe.encode_with_special_tokens(content);
                    let token_count = tokens.len();
                    
                    let normalized_model = normalize_model_name(model_id);
                    
                    // 从价格缓存中获取价格信息
                    let price_info = price_cache.get(&normalized_model).cloned().or_else(|| {
                        let matching_key = price_cache.keys().find(|key| {
                            let key_lower = key.to_lowercase();
                            let model_lower = normalized_model.to_lowercase();
                            key_lower.contains(&model_lower) || model_lower.contains(&key_lower)
                        });
                        
                        if let Some(key) = matching_key {
                            price_cache.get(key).cloned()
                        } else {
                            None
                        }
                    });
                    
                    if let Some(ref price) = price_info {
                        // 计算成本：只计算输出token（completion tokens）
                        let cost_value = token_count as f64 * price.output_price;
                        
                        // 智能币种识别
                        let model_lower = model_id.to_lowercase();
                        
                        // 优化的币种识别逻辑
                        let is_cny = if model_lower.contains("qwen") || 
                                     model_lower.contains("glm") || 
                                     model_lower.contains("zhipu") || 
                                     model_lower.contains("yi-") {
                            // 1. 这些厂商在你的库里存的确实是"大数"，认定为人民币
                            true 
                        } else if model_lower.contains("deepseek") {
                            // 2. 特殊情况：你的数据库里 DeepSeek 是美金价
                            // 为了显示有意义的数值，DeepSeek应该显示为美金
                            false
                        } else if price.input_price > 0.01 {
                            // 3. 兜底逻辑：只要价格数值大，不管叫啥名，都是人民币
                            true
                        } else {
                            // 4. 其余全是美金
                            false
                        };
                        
                        let currency = if is_cny { "CNY".to_string() } else { "USD".to_string() };
                        
                        return (cost_value, currency);
                    }
                }
            }
        }
    }
    
    // 如果无法从 chunk 中提取内容，则尝试解析 usage 字段作为后备方案
    if let Some(usage_val) = chunk.get("usage") {
        // 如果 usage 字段本身就是 null，直接跳过
        if usage_val.is_null() { return (0.0, "USD".to_string()); }
        
        // 尝试自动解析。如果自动解析失败，我们手动抓取字段（这样最稳！）
        let (prompt, completion) = if let Ok(u) = serde_json::from_value::<Usage>(usage_val.clone()) {
            (u.prompt_tokens.unwrap_or(0), u.completion_tokens.unwrap_or(0))
        } else {
            // 如果自动解析失败了，尝试手动从 Value 里捞
            let p = usage_val.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let c = usage_val.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            (p, c)
        };
        
        // 只有当 tokens 不为 0 时才计算成本
        if prompt > 0 || completion > 0 {
            let usage = Usage {
                prompt_tokens: Some(prompt),
                completion_tokens: Some(completion),
                total_tokens: Some(prompt + completion),
            };
            return calculate_actual_cost(model_id, &usage, price_cache);
        }
    }
    
    // 中间过程的包（null 或没有 usage），直接返回 0.0，不要报错
    (0.0, "USD".to_string())
}

/// ✅ 解析 Usage 包
pub fn extract_usage_from_chunk(chunk: &Value) -> Option<(u64, u64)> {
    if let Some(usage) = chunk.get("usage") {
        let prompt_tokens = usage["prompt_tokens"].as_u64()?;
        let completion_tokens = usage["completion_tokens"].as_u64()?;
        Some((prompt_tokens, completion_tokens))
    } else {
        None
    }
}

pub fn estimate_cost(model: &str, payload: &Value) -> f64 {
    let model_lower = model.to_lowercase();
    
    let (text_tokens, image_count) = extract_tokens_and_images(payload);
    
    if model_lower.contains("vl") {
        let est_tokens = text_tokens + (image_count as f64 * 1000.0);
        (est_tokens / 1000.0) * 0.003
    } else {
        let est_tokens = text_tokens * 1.3;
        (est_tokens / 1000.0) * 0.8
    }
}

fn extract_tokens_and_images(payload: &Value) -> (f64, usize) {
    let mut text_tokens = 0.0;
    let mut image_count = 0;
    
    if let Some(msgs) = payload.get("messages").and_then(|v| v.as_array()) {
        if let Some(last_msg) = msgs.last() {
            let content = last_msg.get("content");
            
            if let Some(s) = content.and_then(|v| v.as_str()) {
                text_tokens = s.len() as f64;
            }
            
            if let Some(arr) = content.and_then(|v| v.as_array()) {
                for item in arr {
                    if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            text_tokens = text.len() as f64;
                        }
                    } else if item.get("type").and_then(|v| v.as_str()) == Some("image_url") {
                        image_count += 1;
                    }
                }
            }
        }
    }
    
    (text_tokens, image_count)
}

pub fn extract_prompt(json: &Value) -> String {
    if let Some(msgs) = json.get("messages").and_then(|v| v.as_array()) {
        if let Some(last_msg) = msgs.last() {
            let content = last_msg.get("content");
            
            if let Some(s) = content.and_then(|v| v.as_str()) {
                return s.to_string();
            }
            
            if let Some(arr) = content.and_then(|v| v.as_array()) {
                for item in arr {
                    if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                        return item.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    }
                }
            }
        }
    }
    if let Some(input) = json.get("input").and_then(|i| i.get("messages")).and_then(|v| v.as_array()) {
        if let Some(last_msg) = input.last() {
            let content = last_msg.get("content");
            
            if let Some(s) = content.and_then(|v| v.as_str()) {
                return s.to_string();
            }
            
            if let Some(arr) = content.and_then(|v| v.as_array()) {
                for item in arr {
                    if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                        return item.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    }
                }
            }
        }
    }
    String::new()
}

#[derive(Debug)]
pub struct ParsedRequest {
    pub model: String,
    pub prompt: String,
    pub original_request: serde_json::Value,
}

use std::fmt;

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ParseError {}

pub fn parse_request(request_body: &str) -> Result<ParsedRequest, ParseError> {
    let original_request: serde_json::Value = serde_json::from_str(request_body)
        .map_err(|e| ParseError { message: e.to_string() })?;
    
    let extract_model = |json: &serde_json::Value| {
        json.get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| ParseError { message: "Missing 'model' field".to_string() })
    };
    
    let extract_prompt = |json: &serde_json::Value| -> String {
        if let Some(messages) = json.get("messages").and_then(|v| v.as_array()) {
            if !messages.is_empty() {
                return messages
                    .iter()
                    .filter_map(|msg| {
                        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        Some(format!("{}: {}", role, content))
                    })
                    .collect::<Vec<String>>()
                    .join("\n");
            }
        }
        
        if let Some(input) = json.get("input") {
            if let Some(messages) = input.get("messages").and_then(|v| v.as_array()) {
                if !messages.is_empty() {
                    return messages
                        .iter()
                        .filter_map(|msg| {
                            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                            Some(format!("{}: {}", role, content))
                        })
                        .collect::<Vec<String>>()
                        .join("\n");
                }
            }
        }
        
        String::new()
    };
    
    let model = extract_model(&original_request)?;
    let prompt = extract_prompt(&original_request);
    
    Ok(ParsedRequest {
        model,
        prompt,
        original_request,
    })
}

pub fn calculate_actual_cost(model: &str, usage: &Usage, price_cache: &HashMap<String, PriceInfo>) -> (f64, String) {
    let input_tokens = usage.prompt_tokens.unwrap_or(0) as f64;
    let output_tokens = usage.completion_tokens.unwrap_or(0) as f64;
    
    let normalized_model = normalize_model_name(model);
    
    println!("🔍 [DEBUG] 计算成本 - 原始模型: '{}', 归一化后: '{}', 输入tokens: {}, 输出tokens: {}", model, normalized_model, input_tokens, output_tokens);
    println!("🔍 [DEBUG] 价格缓存中的模型列表: {:?}", price_cache.keys().collect::<Vec<_>>());
    
    // 🆕 [强化匹配] 先尝试精确匹配，再尝试包含匹配
    let price = price_cache.get(&normalized_model).cloned().or_else(|| {
        // 如果精确匹配失败，尝试查找包含该模型名的 key
        let matching_key = price_cache.keys().find(|key| {
            let key_lower = key.to_lowercase();
            let model_lower = normalized_model.to_lowercase();
            key_lower.contains(&model_lower) || model_lower.contains(&key_lower)
        });
        
        if let Some(key) = matching_key {
            println!("✅ [DEBUG] 通过包含匹配找到价格: {} -> {}", normalized_model, key);
            price_cache.get(key).cloned()
        } else {
            println!("⚠️ 哨兵提示：未找到模型 {} 的价格情报", normalized_model);
            Some(PriceInfo { input_price: 0.00001, output_price: 0.00001 })
        }
    });
    
    let (cost, currency) = if let Some(ref price_info) = price {
        // 🕵️‍♂️ 智能币种侦察兵
        let model_lower = model.to_lowercase();
        
        // 优化的币种识别逻辑
        let is_cny = if model_lower.contains("qwen") || 
                     model_lower.contains("glm") || 
                     model_lower.contains("zhipu") || 
                     model_lower.contains("yi-") ||
                     model_lower.contains("deepseek") {
            // 1. 这些厂商的模型都显示为人民币
            true 
        } else if price_info.input_price > 0.01 {
            // 3. 兜底逻辑：只要价格数值大，不管叫啥名，都是人民币
            true
        } else {
            // 4. 其余全是美金
            false
        };
        
        // ⚡️ 修正：直接使用每token价格（不再除以1,000,000）
        let cost_value = input_tokens * price_info.input_price
                          + output_tokens * price_info.output_price;
        
        if FORCE_CNY_FOR_CHINESE_MODELS && (model_lower.contains("qwen") || 
                                            model_lower.contains("glm") || 
                                            model_lower.contains("zhipu") || 
                                            model_lower.contains("yi-") || 
                                            model_lower.contains("deepseek")) {
            // 配置项：强制国内模型显示人民币
            // 如果是Qwen/GLM/Yi，直接显示CNY（数值已经是人民币）
            // 如果是DeepSeek，显示CNY但数值要乘7.2（因为库里是美金价）
            if model_lower.contains("deepseek") {
                (cost_value * 7.2, "CNY".to_string())
            } else {
                (cost_value, "CNY".to_string())
            }
        } else {
            // 使用新的识别逻辑
            if is_cny {
                (cost_value, "CNY".to_string())
            } else {
                (cost_value, "USD".to_string())
            }
        }
    } else {
        // 保底单价（每token）
        (0.0, "USD".to_string())
    };
    
    println!("🔍 [DEBUG] 实时计算出的成本: {:.9}, 币种: {}", cost, currency);
    
    (cost, currency)
}

pub fn normalize_model_name(model: &str) -> String {
    let model_lower = model.to_lowercase();
    
    // 先用 split('/') 取最后一部分，去掉所有前缀（包括 /）
    let base_name = model_lower.split('/').last().unwrap_or(&model_lower);
    
    let normalized = base_name.to_string()
        .replace("@", "-")
        .trim()
        .to_string();
    
    normalized
}

pub fn calculate_actual_cost_with_tokens(model: &str, prompt_tokens: f64, completion_tokens: f64, price_cache: &HashMap<String, PriceInfo>) -> (f64, String) {
    let normalized_model = normalize_model_name(model);
    
    println!("🔍 [DEBUG] 实时计费 - 原始模型: '{}', 归一化后: '{}', 输入tokens: {}, 输出tokens: {}", model, normalized_model, prompt_tokens, completion_tokens);
    println!("🔍 [DEBUG] 价格缓存中的模型列表: {:?}", price_cache.keys().collect::<Vec<_>>());
    
    // 🆕 [强化匹配] 先尝试精确匹配，再尝试包含匹配
    let price = price_cache.get(&normalized_model).cloned().or_else(|| {
        // 如果精确匹配失败，尝试查找包含该模型名的 key
        let matching_key = price_cache.keys().find(|key| {
            let key_lower = key.to_lowercase();
            let model_lower = normalized_model.to_lowercase();
            key_lower.contains(&model_lower) || model_lower.contains(&key_lower)
        });
        
        if let Some(key) = matching_key {
            println!("✅ [DEBUG] 通过包含匹配找到价格: {} -> {}", normalized_model, key);
            price_cache.get(key).cloned()
        } else {
            println!("⚠️ 哨兵提示：未找到模型 {} 的价格情报", normalized_model);
            Some(PriceInfo { input_price: 0.00001, output_price: 0.00001 })
        }
    });
    
    let (cost, currency) = if let Some(ref price_info) = price {
        // 🕵️‍♂️ 智能币种侦察兵
        let model_lower = model.to_lowercase();
        
        // 优化的币种识别逻辑
        let is_cny = if model_lower.contains("qwen") || 
                     model_lower.contains("glm") || 
                     model_lower.contains("zhipu") || 
                     model_lower.contains("yi-") ||
                     model_lower.contains("deepseek") {
            // 1. 这些厂商的模型都显示为人民币
            true 
        } else if price_info.input_price > 0.01 {
            // 3. 兜底逻辑：只要价格数值大，不管叫啥名，都是人民币
            true
        } else {
            // 4. 其余全是美金
            false
        };
        
        // ⚡️ 修正：直接使用每token价格（不再除以1,000,000）
        let cost_value = prompt_tokens * price_info.input_price
                          + completion_tokens * price_info.output_price;
        
        if FORCE_CNY_FOR_CHINESE_MODELS && (model_lower.contains("qwen") || 
                                            model_lower.contains("glm") || 
                                            model_lower.contains("zhipu") || 
                                            model_lower.contains("yi-") || 
                                            model_lower.contains("deepseek")) {
            // 配置项：强制国内模型显示人民币
            // 如果是Qwen/GLM/Yi，直接显示CNY（数值已经是人民币）
            // 如果是DeepSeek，显示CNY但数值要乘7.2（因为库里是美金价）
            if model_lower.contains("deepseek") {
                (cost_value * 7.2, "CNY".to_string())
            } else {
                (cost_value, "CNY".to_string())
            }
        } else {
            // 使用新的识别逻辑
            if is_cny {
                (cost_value, "CNY".to_string())
            } else {
                (cost_value, "USD".to_string())
            }
        }
    } else {
        // 保底单价（每token）
        (0.0, "USD".to_string())
    };
    
    println!("🔍 [DEBUG] 实时计算出的成本: {}, 币种: {}", cost, currency);
    
    (cost, currency)
}
