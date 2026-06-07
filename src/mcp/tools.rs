//! MCP Tools - Tool registration and execution

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use async_trait::async_trait;

// ─── Lightweight sandbox policy (read from env at startup) ───────────────────

mod sandbox {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    pub struct Policy {
        pub denied_commands: Vec<String>,
        pub denied_paths: Vec<PathBuf>,
        pub allowed_paths: Vec<PathBuf>,  // empty = no whitelist (allow all)
    }

    fn expand_tilde(s: &str) -> PathBuf {
        if let Some(rest) = s.strip_prefix("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home).join(rest);
            }
        } else if s == "~" {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home);
            }
        }
        PathBuf::from(s)
    }

    fn parse_paths(raw: &str) -> Vec<PathBuf> {
        raw.split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| expand_tilde(s))
            .collect()
    }

    static POLICY: OnceLock<Policy> = OnceLock::new();

    fn get_policy() -> &'static Policy {
        POLICY.get_or_init(|| {
            let denied_commands_raw = std::env::var("LINGSHU_SANDBOX_DENIED_CMDS")
                .unwrap_or_else(|_| "sudo,su ,rm -rf /,shutdown,reboot,kill -9 1,launchctl bootout,launchctl remove,chmod 777,mkfs,dd if=".to_string());
            let denied_paths_raw = std::env::var("LINGSHU_SANDBOX_DENIED_PATHS")
                .unwrap_or_else(|_| "~/.ssh,~/.gnupg,/etc/passwd,/etc/shadow,~/.config/git/credentials".to_string());
            let allowed_paths_raw = std::env::var("LINGSHU_SANDBOX_ALLOWED_PATHS")
                .unwrap_or_default();

            Policy {
                denied_commands: denied_commands_raw.split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect(),
                denied_paths: parse_paths(&denied_paths_raw),
                allowed_paths: parse_paths(&allowed_paths_raw),
            }
        })
    }

    /// Check if a command is denied. Returns Some(reason) if blocked.
    pub fn check_command(command: &str) -> Option<String> {
        let policy = get_policy();
        let lower = command.to_lowercase();
        for denied in &policy.denied_commands {
            if lower.contains(denied.as_str()) {
                return Some(format!("command denied by sandbox policy: contains '{}'", denied));
            }
        }
        None
    }

    /// Check if a path is allowed. Returns Some(reason) if blocked.
    pub fn check_path(raw_path: &str) -> Option<String> {
        let policy = get_policy();
        let path = expand_tilde(raw_path);
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());

        // Check denied paths
        for denied in &policy.denied_paths {
            if canonical.starts_with(denied) || path.starts_with(denied) {
                return Some(format!("path denied by sandbox policy: '{}' is under protected path '{}'",
                    raw_path, denied.display()));
            }
        }

        // Check allowed paths whitelist (if configured)
        if !policy.allowed_paths.is_empty() {
            let allowed = policy.allowed_paths.iter().any(|a| {
                canonical.starts_with(a) || path.starts_with(a)
            });
            if !allowed {
                return Some(format!("path denied by sandbox policy: '{}' is not under any allowed path", raw_path));
            }
        }

        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub server_name: Option<String>,
}

impl McpTool {
    pub fn new(name: &str, description: &str, input_schema: serde_json::Value) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            input_schema,
            server_name: None,
        }
    }
    
    pub fn with_server(mut self, server_name: &str) -> Self {
        self.server_name = Some(server_name.to_string());
        self
    }
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value>;
}

pub type ToolExecutorFn = Box<dyn Fn(serde_json::Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<serde_json::Value>> + Send>> + Send + Sync>;

pub struct ToolRegistry {
    tools: Arc<RwLock<HashMap<String, McpTool>>>,
    executors: Arc<RwLock<HashMap<String, Arc<dyn ToolExecutor>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Arc::new(RwLock::new(HashMap::new())),
            executors: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    pub async fn register(&self, tool: McpTool, executor: Arc<dyn ToolExecutor>) {
        let name = tool.name.clone();
        let mut tools = self.tools.write().await;
        let mut executors = self.executors.write().await;
        tools.insert(name.clone(), tool);
        executors.insert(name, executor);
    }
    
    pub async fn unregister(&self, name: &str) {
        let mut tools = self.tools.write().await;
        let mut executors = self.executors.write().await;
        tools.remove(name);
        executors.remove(name);
    }
    
    pub async fn get(&self, name: &str) -> Option<McpTool> {
        let tools = self.tools.read().await;
        tools.get(name).cloned()
    }
    
    pub async fn list(&self) -> Vec<McpTool> {
        let tools = self.tools.read().await;
        tools.values().cloned().collect()
    }
    
    pub async fn execute(&self, name: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let executors = self.executors.read().await;
        let executor = executors.get(name)
            .ok_or_else(|| anyhow::anyhow!("Tool not found: {}", name))?;
        executor.execute(params).await
    }
    
    pub async fn register_builtin_tools(&self) {
        self.register(
            McpTool::new(
                "file_read",
                "Read file contents",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path to read"}
                    },
                    "required": ["path"]
                })
            ),
            Arc::new(BuiltinFileReadExecutor),
        ).await;
        
        self.register(
            McpTool::new(
                "file_write",
                "Write content to a file",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path to write"},
                        "content": {"type": "string", "description": "Content to write"}
                    },
                    "required": ["path", "content"]
                })
            ),
            Arc::new(BuiltinFileWriteExecutor),
        ).await;
        
        self.register(
            McpTool::new(
                "execute_command",
                "Execute a shell command",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Command to execute"},
                        "cwd": {"type": "string", "description": "Working directory"}
                    },
                    "required": ["command"]
                })
            ),
            Arc::new(BuiltinCommandExecutor),
        ).await;
        
        self.register(
            McpTool::new(
                "search",
                "Search for pattern in files",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Search pattern"},
                        "path": {"type": "string", "description": "Directory to search"}
                    },
                    "required": ["pattern"]
                })
            ),
            Arc::new(BuiltinSearchExecutor),
        ).await;
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

struct BuiltinFileReadExecutor;

#[async_trait]
impl ToolExecutor for BuiltinFileReadExecutor {
    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let path = params["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing path parameter"))?;

        // Sandbox: check path policy
        if let Some(reason) = sandbox::check_path(path) {
            return Ok(serde_json::json!({
                "success": false,
                "error": reason
            }));
        }
        
        let content = tokio::fs::read_to_string(path).await
            .map_err(|e| anyhow::anyhow!("Failed to read file: {}", e))?;
        
        Ok(serde_json::json!({
            "success": true,
            "content": content
        }))
    }
}

struct BuiltinFileWriteExecutor;

#[async_trait]
impl ToolExecutor for BuiltinFileWriteExecutor {
    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let path = params["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing path parameter"))?;
        let content = params["content"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing content parameter"))?;

        // Sandbox: check path policy
        if let Some(reason) = sandbox::check_path(path) {
            return Ok(serde_json::json!({
                "success": false,
                "error": reason
            }));
        }
        
        if let Some(parent) = std::path::Path::new(path).parent() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| anyhow::anyhow!("Failed to create directory: {}", e))?;
        }
        
        tokio::fs::write(path, content).await
            .map_err(|e| anyhow::anyhow!("Failed to write file: {}", e))?;
        
        Ok(serde_json::json!({
            "success": true,
            "path": path
        }))
    }
}

struct BuiltinCommandExecutor;

#[async_trait]
impl ToolExecutor for BuiltinCommandExecutor {
    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let command = params["command"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing command parameter"))?;
        let cwd = params["cwd"].as_str();

        // Sandbox: check command policy
        if let Some(reason) = sandbox::check_command(command) {
            return Ok(serde_json::json!({
                "success": false,
                "error": reason
            }));
        }

        // Windows 用 cmd /C，Unix 用 sh -c
        let output = if cfg!(target_os = "windows") {
            if let Some(dir) = cwd {
                tokio::process::Command::new("cmd")
                    .arg("/C")
                    .arg(command)
                    .current_dir(dir)
                    .output()
                    .await
            } else {
                tokio::process::Command::new("cmd")
                    .arg("/C")
                    .arg(command)
                    .output()
                    .await
            }
        } else {
            if let Some(dir) = cwd {
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .current_dir(dir)
                    .output()
                    .await
            } else {
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .output()
                    .await
            }
        };

        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                Ok(serde_json::json!({
                    "success": output.status.success(),
                    "stdout": stdout,
                    "stderr": stderr,
                    "exit_code": output.status.code()
                }))
            }
            Err(e) => Ok(serde_json::json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

struct BuiltinSearchExecutor;

#[async_trait]
impl ToolExecutor for BuiltinSearchExecutor {
    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let pattern = params["pattern"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing pattern parameter"))?;
        let path = params["path"].as_str().unwrap_or(".");

        // Windows 用 findstr，Unix 用 rg 或 grep
        let output = if cfg!(target_os = "windows") {
            tokio::process::Command::new("findstr")
                .arg("/s")
                .arg("/m")
                .arg(pattern)
                .arg(&format!("{}\\*", path))
                .output()
                .await
        } else {
            // 尝试 rg，失败则用 grep
            let rg_result = tokio::process::Command::new("rg")
                .arg("-l")
                .arg(pattern)
                .arg(path)
                .output()
                .await;

            match rg_result {
                Ok(output) => Ok(output),
                Err(_) => {
                    tokio::process::Command::new("grep")
                        .arg("-r")
                        .arg("-l")
                        .arg(pattern)
                        .arg(path)
                        .output()
                        .await
                }
            }
        };

        match output {
            Ok(output) => {
                let files = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({
                    "success": true,
                    "files": files
                }))
            }
            Err(e) => Ok(serde_json::json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sandbox;

    #[test]
    fn test_sandbox_denies_sudo() {
        let result = sandbox::check_command("sudo rm -rf /tmp/foo");
        assert!(result.is_some(), "sudo should be denied");
        assert!(result.unwrap().contains("sudo"));
    }

    #[test]
    fn test_sandbox_denies_reboot() {
        let result = sandbox::check_command("reboot now");
        assert!(result.is_some(), "reboot should be denied");
    }

    #[test]
    fn test_sandbox_allows_normal_commands() {
        let result = sandbox::check_command("ls -la /tmp");
        assert!(result.is_none(), "ls should be allowed");

        let result = sandbox::check_command("git status");
        assert!(result.is_none(), "git should be allowed");

        let result = sandbox::check_command("python3 script.py");
        assert!(result.is_none(), "python should be allowed");
    }

    #[test]
    fn test_sandbox_denies_ssh_path() {
        let result = sandbox::check_path("~/.ssh/id_rsa");
        assert!(result.is_some(), "~/.ssh should be denied");
    }

    #[test]
    fn test_sandbox_denies_etc_passwd() {
        let result = sandbox::check_path("/etc/passwd");
        assert!(result.is_some(), "/etc/passwd should be denied");
    }

    #[test]
    fn test_sandbox_allows_normal_path() {
        let result = sandbox::check_path("/tmp/test.txt");
        assert!(result.is_none(), "/tmp should be allowed");
    }
}
