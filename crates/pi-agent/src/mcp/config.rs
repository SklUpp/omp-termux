use crate::mcp::types::MCPServerConfig;

pub fn validate_server_config(name: &str, config: &MCPServerConfig) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    match config {
        MCPServerConfig::Stdio(s) => {
            if s.command.trim().is_empty() {
                errors.push(format!("Server \"{}\": stdio server requires \"command\" field", name));
            }
        }
        MCPServerConfig::Http(h) => {
            if h.url.trim().is_empty() {
                errors.push(format!("Server \"{}\": http server requires \"url\" field", name));
            }
            if let Some(auth) = &h.auth {
                if auth.get("type").and_then(|v| v.as_str()) == Some("oauth") {
                    errors.push(format!(
                        "Server \"{}\": OAuth authentication requires credential storage (step 5)",
                        name
                    ));
                }
            }
            if h.oauth.is_some()
                && !h.headers.contains_key("Authorization")
                && !h.headers.contains_key("authorization")
            {
                errors.push(format!(
                    "Server \"{}\": OAuth authentication requires credential storage (step 5)",
                    name
                ));
            }
        }
        MCPServerConfig::Sse(s) => {
            if s.url.trim().is_empty() {
                errors.push(format!("Server \"{}\": sse server requires \"url\" field", name));
            }
            if let Some(auth) = &s.auth {
                if auth.get("type").and_then(|v| v.as_str()) == Some("oauth") {
                    errors.push(format!(
                        "Server \"{}\": OAuth authentication requires credential storage (step 5)",
                        name
                    ));
                }
            }
            if s.oauth.is_some()
                && !s.headers.contains_key("Authorization")
                && !s.headers.contains_key("authorization")
            {
                errors.push(format!(
                    "Server \"{}\": OAuth authentication requires credential storage (step 5)",
                    name
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
