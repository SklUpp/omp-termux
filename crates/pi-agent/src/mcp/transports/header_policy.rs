use std::collections::HashMap;

pub fn without_header(
    headers: &HashMap<String, String>,
    name: &str,
) -> HashMap<String, String> {
    let lower = name.to_lowercase();
    headers
        .iter()
        .filter(|(k, _)| k.to_lowercase() != lower)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn merge_mcp_headers(
    generated: &HashMap<String, String>,
    configured: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged = HashMap::new();
    let generated_lower: std::collections::HashSet<String> =
        generated.keys().map(|k| k.to_lowercase()).collect();

    for (k, v) in configured {
        if !generated_lower.contains(&k.to_lowercase()) {
            merged.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in generated {
        merged.insert(k.clone(), v.clone());
    }

    merged
}
