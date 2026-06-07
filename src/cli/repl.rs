//! REPL Module - Interactive Read-Eval-Print Loop
//!
//! Beautiful REPL interface matching the original Claude Code aesthetic

use crate::api::{ApiClient, ChatMessage, ToolDefinition, ToolCall};
use crate::cli::ui;
use crate::state::AppState;
use crate::mcp::ToolRegistry;
use colored::Colorize;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

pub struct Repl {
    state: AppState,
    conversation_history: Vec<ChatMessage>,
    tool_registry: Arc<ToolRegistry>,
}

impl Repl {
    pub fn new(state: AppState) -> Self {
        ui::init_terminal();
        let tool_registry = Arc::new(ToolRegistry::new());

        // 注册内置工具（使用 tokio::task::block_in_place）
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tool_registry.register_builtin_tools().await;
            });
        });

        Self {
            state,
            conversation_history: Vec::new(),
            tool_registry,
        }
    }

    pub fn start(&mut self, initial_prompt: Option<String>) -> anyhow::Result<()> {
        ui::print_welcome();

        if let Some(prompt) = initial_prompt {
            self.process_input(&prompt)?;
        }

        let stdin = io::stdin();
        let mut stdout = io::stdout();

        loop {
            ui::print_prompt();
            stdout.flush()?;

            let mut input = String::new();
            stdin.lock().read_line(&mut input)?;
            let input = input.trim();

            if input.is_empty() {
                continue;
            }

            match input {
                "exit" | "quit" | ".exit" | ":q" => {
                    println!("\n  {} {}\n",
                        "👋".yellow(),
                        "Goodbye!".truecolor(255, 140, 66).bold()
                    );
                    break;
                }
                "help" | ".help" | ":h" => ui::print_help(),
                "status" | ".status" => self.print_status(),
                "clear" | ".clear" | ":c" => ui::clear_screen(),
                "history" | ".history" => self.print_history(),
                "reset" | ".reset" => self.reset_conversation(),
                "config" | ".config" => self.print_config(),
                _ => self.process_input(input)?,
            }
        }

        Ok(())
    }

    fn process_input(&mut self, input: &str) -> anyhow::Result<()> {
        // Show user message with styling
        ui::print_user_message(input);

        let client = ApiClient::new(self.state.settings.clone());

        let api_key = match client.get_api_key() {
            Some(key) => key,
            None => {
                ui::print_error("API key not configured\n\nSet it with:\n  claude-code config set api_key \"your-api-key\"");
                return Ok(());
            }
        };

        self.conversation_history.push(ChatMessage::user(input));

        // 获取工具定义
        let tools = self.get_tool_definitions();

        let temperature: f32 = std::env::var("TEMPERATURE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.6);

        // 工具调用循环
        loop {
            // Show typing indicator
            ui::print_typing_indicator();

            let messages = self.conversation_history.clone();
            let base_url = client.get_base_url();
            let model = client.get_model().to_string();
            let max_tokens = self.state.settings.api.max_tokens;

            let mut request_body = serde_json::json!({
                "model": model,
                "messages": messages,
                "max_tokens": max_tokens,
                "stream": true,
                "temperature": temperature
            });

            // 注入工具定义
            if !tools.is_empty() {
                request_body["tools"] = serde_json::to_value(&tools)?;
            }

            let timeout_secs = self.state.settings.api.timeout;
            let http_client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .build()
                .unwrap_or_default();
            let url = format!("{}/v1/chat/completions", base_url);

            let resp = match http_client
                .post(&url)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&request_body)
                .send()
            {
                Ok(r) => r,
                Err(e) => {
                    ui::print_error(&format!("Request failed: {}", e));
                    return Ok(());
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().unwrap_or_default();
                ui::print_error(&format!("API error ({}): {}", status, body));
                return Ok(());
            }

            // Parse SSE stream and accumulate response
            let body_text = resp.text().unwrap_or_default();
            let mut full_content = String::new();
            let mut tool_calls_map: std::collections::HashMap<usize, (String, String, String, String)> = std::collections::HashMap::new();
            let mut usage_prompt: u64 = 0;
            let mut usage_completion: u64 = 0;

            for line in body_text.lines() {
                let line = line.trim();
                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    break;
                }
                let chunk: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
                    for choice in choices {
                        let delta = match choice.get("delta") {
                            Some(d) => d,
                            None => continue,
                        };

                        // Accumulate content
                        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                            full_content.push_str(content);
                        }

                        // Accumulate tool_calls
                        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                            for tc in tcs {
                                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                                let entry = tool_calls_map.entry(idx).or_insert_with(|| {
                                    (String::new(), String::new(), String::new(), String::new())
                                });
                                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                    entry.0 = id.to_string();
                                }
                                if let Some(tp) = tc.get("type").and_then(|t| t.as_str()) {
                                    entry.1 = tp.to_string();
                                }
                                if let Some(func) = tc.get("function") {
                                    if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                        entry.2 = name.to_string();
                                    }
                                    if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                                        entry.3.push_str(args);
                                    }
                                }
                            }
                        }
                    }
                }

                // Capture usage from final chunk
                if let Some(usage) = chunk.get("usage") {
                    if let Some(p) = usage.get("prompt_tokens").and_then(|t| t.as_u64()) {
                        usage_prompt = p;
                    }
                    if let Some(c) = usage.get("completion_tokens").and_then(|t| t.as_u64()) {
                        usage_completion = c;
                    }
                }
            }

            // Process accumulated tool_calls
            if !tool_calls_map.is_empty() {
                let mut sorted_calls: Vec<_> = tool_calls_map.into_iter().collect();
                sorted_calls.sort_by_key(|(idx, _)| *idx);

                // Print tool call info
                println!();
                for (_, (_, _, ref name, _)) in &sorted_calls {
                    if !name.is_empty() {
                        println!("  {} Executing tool: {}",
                            "🔧".truecolor(255, 200, 100),
                            name.cyan().bold()
                        );
                    }
                }
                println!();

                // Build tool_calls for conversation history
                let tool_calls_parsed: Vec<ToolCall> = sorted_calls.iter().filter_map(|(_, (id, tp, name, args))| {
                    if id.is_empty() || name.is_empty() { return None; }
                    Some(ToolCall {
                        id: id.clone(),
                        r#type: if tp.is_empty() { "function".to_string() } else { tp.clone() },
                        function: crate::api::ToolCallFunction {
                            name: name.clone(),
                            arguments: args.clone(),
                        },
                    })
                }).collect();

                let content_for_history = if full_content.is_empty() { None } else { Some(full_content.clone()) };
                let assistant_msg = ChatMessage {
                    role: "assistant".to_string(),
                    content: content_for_history,
                    tool_calls: Some(tool_calls_parsed),
                    tool_call_id: None,
                };
                self.conversation_history.push(assistant_msg);

                // Execute each tool call
                for (_, (id, _, name, args_str)) in &sorted_calls {
                    let args: serde_json::Value = serde_json::from_str(args_str)
                        .unwrap_or(serde_json::json!({}));
                    let result = self.execute_tool(name, args);
                    let tool_result_msg = ChatMessage::tool(id, result);
                    self.conversation_history.push(tool_result_msg);
                }

                // Continue loop to let AI process tool results
                continue;
            }

            // No tool calls — handle normal response
            if !full_content.is_empty() {
                ui::print_claude_message(&full_content);
                self.conversation_history.push(ChatMessage::assistant(full_content));

                // Print token usage
                if usage_prompt > 0 || usage_completion > 0 {
                    let total = usage_prompt + usage_completion;
                    println!("  {} {} prompt · {} generated · {} total",
                        "◦".truecolor(100, 100, 100),
                        usage_prompt.to_string().truecolor(150, 150, 150),
                        usage_completion.to_string().truecolor(150, 150, 150),
                        total.to_string().truecolor(180, 180, 180)
                    );
                }
            }

            // Exit loop
            break;
        }

        Ok(())
    }

    /// 获取 MCP 工具定义（转换为 API 格式）
    fn get_tool_definitions(&self) -> Vec<ToolDefinition> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let tools = self.tool_registry.list().await;
                tools.into_iter().map(|t| {
                    ToolDefinition::new(
                        t.name,
                        t.description,
                        t.input_schema
                    )
                }).collect()
            })
        })
    }

    /// 执行工具调用
    fn execute_tool(&self, name: &str, args: serde_json::Value) -> String {
        let name = name.to_string();
        let registry = self.tool_registry.clone();

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                match registry.execute(&name, args).await {
                    Ok(result) => {
                        // 打印工具结果摘要
                        if let Some(success) = result.get("success").and_then(|s| s.as_bool()) {
                            if success {
                                println!("  {} Tool succeeded", "✓".green());
                            } else {
                                println!("  {} Tool failed", "✗".red());
                            }
                        }
                        result.to_string()
                    }
                    Err(e) => {
                        println!("  {} Tool error: {}", "✗".red(), e);
                        serde_json::json!({"error": e.to_string()}).to_string()
                    }
                }
            })
        })
    }

    fn print_status(&self) {
        let status = ui::StatusInfo {
            model: self.state.settings.model.clone(),
            api_base: self.state.settings.api.base_url.clone(),
            max_tokens: self.state.settings.api.max_tokens.to_string(),
            timeout: self.state.settings.api.timeout,
            streaming: self.state.settings.api.streaming,
            message_count: self.conversation_history.len(),
            api_key_set: self.state.settings.api.get_api_key().is_some(),
        };
        ui::print_status(&status);
    }

    fn print_history(&self) {
        println!();
        if self.conversation_history.is_empty() {
            println!("  {} {}",
                "◦".truecolor(100, 100, 100),
                "No conversation history".bright_black()
            );
        } else {
            println!("  {} {}",
                "◦".truecolor(147, 112, 219),
                format!("Conversation history ({} messages)", self.conversation_history.len())
                    .truecolor(147, 112, 219).bold()
            );
            println!();

            for (i, msg) in self.conversation_history.iter().enumerate() {
                let (_icon, _color) = match msg.role.as_str() {
                    "user" => ("●", "truecolor(255, 140, 66)"),
                    "assistant" => ("●", "truecolor(147, 112, 219)"),
                    _ => ("●", "bright_black"),
                };

                let role_label = match msg.role.as_str() {
                    "user" => "You".truecolor(255, 180, 100),
                    "assistant" => "Claude".truecolor(200, 150, 255),
                    _ => "Unknown".bright_black(),
                };

                let content = msg.content.as_deref().unwrap_or("");
                let preview: String = content.chars().take(50).collect();
                let suffix = if content.len() > 50 { "..." } else { "" };

                println!("  {}. {}  {}{}",
                    (i + 1).to_string().truecolor(100, 100, 100),
                    role_label,
                    preview.bright_white(),
                    suffix.bright_black()
                );
            }
        }
        println!();
    }

    fn print_config(&self) {
        println!();
        println!("  {} {}",
            "⚙".truecolor(147, 112, 219),
            "Configuration".truecolor(147, 112, 219).bold()
        );
        println!();

        match serde_json::to_string_pretty(&self.state.settings) {
            Ok(json) => {
                for line in json.lines() {
                    println!("  {}", line.bright_white());
                }
            }
            Err(_) => {
                ui::print_error("Failed to serialize configuration");
            }
        }
        println!();
    }

    fn reset_conversation(&mut self) {
        self.conversation_history.clear();
        ui::print_success("Conversation reset");
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repl_creation() {
        let state = AppState::default();
        let repl = Repl::new(state);
        assert!(repl.conversation_history.is_empty());
    }
}
