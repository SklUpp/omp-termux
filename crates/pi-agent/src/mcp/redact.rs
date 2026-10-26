use regex::Regex;
use std::collections::HashMap;

pub fn redact_url_for_log(url_str: &str) -> String {
    if let Some((base, query)) = url_str.split_once('?') {
        let param_re = Regex::new(r"(?i)(key|token|secret|auth)").unwrap();
        let mut new_query_parts = Vec::new();
        for pair in query.split('&') {
            if let Some((k, _)) = pair.split_once('=') {
                if param_re.is_match(k) {
                    new_query_parts.push(format!("{}=[redacted]", k));
                } else {
                    new_query_parts.push(pair.to_string());
                }
            } else if param_re.is_match(pair) {
                new_query_parts.push(format!("{}=[redacted]", pair));
            } else {
                new_query_parts.push(pair.to_string());
            }
        }
        format!("{}?{}", base, new_query_parts.join("&"))
    } else {
        url_str.to_string()
    }
}

pub fn redact_headers_for_log(headers: &HashMap<String, String>) -> HashMap<String, String> {
    let mut redacted = HashMap::new();
    let re = Regex::new(r"(?i)auth|key|token|secret|cookie").unwrap();
    for (k, v) in headers {
        if re.is_match(k) {
            redacted.insert(k.clone(), "[redacted]".to_string());
        } else {
            redacted.insert(k.clone(), v.clone());
        }
    }
    redacted
}
