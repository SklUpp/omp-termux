pub const DEFAULT_MCP_TIMEOUT_MS: u64 = 30_000;
pub const MCP_TIMEOUT_ENV: &str = "OMP_MCP_TIMEOUT_MS";

pub fn resolve_mcp_timeout_ms(config_timeout: Option<u64>) -> u64 {
    if let Ok(val_str) = std::env::var(MCP_TIMEOUT_ENV) {
        if let Ok(parsed) = val_str.trim().parse::<u64>() {
            return parsed;
        }
    }
    config_timeout.unwrap_or(DEFAULT_MCP_TIMEOUT_MS)
}

pub fn is_mcp_timeout_enabled(timeout_ms: u64) -> bool {
    timeout_ms > 0
}

pub fn describe_mcp_timeout(timeout_ms: u64) -> String {
    if is_mcp_timeout_enabled(timeout_ms) {
        format!("{}ms", timeout_ms)
    } else {
        "disabled".to_string()
    }
}
