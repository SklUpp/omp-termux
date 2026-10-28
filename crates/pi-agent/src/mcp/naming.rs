use std::collections::HashMap;

pub const MAX_MCP_TOOL_NAME_LENGTH: usize = 64;
pub const MCP_TOOL_NAME_HASH_LENGTH: usize = 8;

pub fn sanitize_mcp_tool_name_part(value: &str, fallback: &str) -> String {
    let lower = value.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_was_underscore = false;

    for c in lower.chars() {
        if c.is_ascii_lowercase() {
            out.push(c);
            last_was_underscore = false;
        } else {
            if !last_was_underscore && !out.is_empty() {
                out.push('_');
                last_was_underscore = true;
            }
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

// Exact Wyhash v3 matching Bun.hash(str) with seed = 0
const WY_SECRET: [u64; 4] = [
    0xa0761d6478bd642f,
    0xe7037ed1a0b428db,
    0x8ebc6af09c88c6e3,
    0x589965cc75374cc3,
];

#[inline(always)]
fn wymum(a: u64, b: u64) -> u64 {
    let p = (a as u128) * (b as u128);
    ((p & 0xffffffffffffffff) ^ (p >> 64)) as u64
}

#[inline(always)]
fn wyr4(data: &[u8]) -> u64 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&data[..4]);
    u32::from_le_bytes(b) as u64
}

#[inline(always)]
fn wyr8(data: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[..8]);
    u64::from_le_bytes(b)
}

pub fn bun_wyhash(seed: u64, data: &[u8]) -> u64 {
    let mut s0 = seed ^ wymum(seed ^ WY_SECRET[0], WY_SECRET[1]);
    let mut s1 = s0;
    let mut s2 = s0;
    let length = data.len();

    let (mut a, mut b);
    if length <= 16 {
        if length >= 4 {
            let end = length - 4;
            let quarter = (length >> 3) << 2;
            a = (wyr4(&data[0..]) << 32) | wyr4(&data[quarter..]);
            b = (wyr4(&data[end..]) << 32) | wyr4(&data[end - quarter..]);
        } else if length > 0 {
            a = ((data[0] as u64) << 16) | ((data[length >> 1] as u64) << 8) | (data[length - 1] as u64);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut i = 0;
        if length >= 48 {
            while i + 48 < length {
                s0 = wymum(wyr8(&data[i..]) ^ WY_SECRET[1], wyr8(&data[i + 8..]) ^ s0);
                s1 = wymum(wyr8(&data[i + 16..]) ^ WY_SECRET[2], wyr8(&data[i + 24..]) ^ s1);
                s2 = wymum(wyr8(&data[i + 32..]) ^ WY_SECRET[3], wyr8(&data[i + 40..]) ^ s2);
                i += 48;
            }
            s0 ^= s1 ^ s2;
        }
        while i + 16 < length {
            s0 = wymum(wyr8(&data[i..]) ^ WY_SECRET[1], wyr8(&data[i + 8..]) ^ s0);
            i += 16;
        }
        a = wyr8(&data[length - 16..]);
        b = wyr8(&data[length - 8..]);
    }

    a ^= WY_SECRET[1];
    b ^= s0;
    let p = (a as u128) * (b as u128);
    let a2 = (p & 0xffffffffffffffff) as u64;
    let b2 = (p >> 64) as u64;
    wymum(a2 ^ WY_SECRET[0] ^ (length as u64), b2 ^ WY_SECRET[1])
}

/// `Bun.hash(data).toString(36)` for stable-id normalization shared with TS.
pub fn bun_hash_base36(data: &[u8]) -> String {
    to_base36(bun_wyhash(0, data))
}

fn to_base36(mut val: u64) -> String {
    if val == 0 {
        return "0".to_string();
    }
    const RADIX: u64 = 36;
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while val > 0 {
        buf.push(DIGITS[(val % RADIX) as usize]);
        val /= RADIX;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

pub fn cap_mcp_tool_name_length(name: &str) -> String {
    if name.len() <= MAX_MCP_TOOL_NAME_LENGTH {
        return name.to_string();
    }

    // Matches Bun.hash(name).toString(36).slice(0, 8)
    let full_hash = to_base36(bun_wyhash(0, name.as_bytes()));
    let hash = if full_hash.len() > MCP_TOOL_NAME_HASH_LENGTH {
        &full_hash[..MCP_TOOL_NAME_HASH_LENGTH]
    } else {
        &full_hash
    };

    let keep = MAX_MCP_TOOL_NAME_LENGTH - hash.len() - 1;
    format!("{}_{}", &name[..keep], hash)
}

pub fn create_mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    let sanitized_server = sanitize_mcp_tool_name_part(server_name, "server");
    let sanitized_tool = sanitize_mcp_tool_name_part(tool_name, "tool");

    let prefix_with_underscore = format!("{}_", sanitized_server);
    let normalized_tool = if sanitized_tool.starts_with(&prefix_with_underscore) {
        &sanitized_tool[prefix_with_underscore.len()..]
    } else {
        &sanitized_tool
    };

    let minted = format!("mcp__{}_{}", sanitized_server, normalized_tool);
    cap_mcp_tool_name_length(&minted)
}

pub fn parse_mcp_tool_name(name: &str) -> Option<(String, String)> {
    if !name.starts_with("mcp__") {
        return None;
    }
    let rest = &name[5..];
    let underscore_idx = rest.find('_')?;
    Some((rest[..underscore_idx].to_string(), rest[underscore_idx + 1..].to_string()))
}

pub fn get_mcp_tool_origin_key(server_name: &str, tool_name: &str) -> String {
    format!("{}\0{}", server_name, tool_name)
}

pub trait McpToolOriginSource {
    fn name(&self) -> &str;
    fn origin_key(&self) -> Option<String>;
}

pub fn deduplicate_mcp_tools_by_name<T: McpToolOriginSource + Clone>(tools: &[T]) -> Vec<T> {
    let mut deduplicated: Vec<T> = Vec::new();
    let mut registered: HashMap<String, (T, String, usize)> = HashMap::new();

    for tool in tools {
        let origin_key = match tool.origin_key() {
            Some(k) => k,
            None => {
                deduplicated.push(tool.clone());
                continue;
            }
        };

        if let Some((existing_tool, existing_origin_key, idx)) = registered.get_mut(tool.name()) {
            if *existing_origin_key == origin_key {
                continue;
            }
            let keep_existing = *existing_origin_key < origin_key;
            if !keep_existing {
                deduplicated[*idx] = tool.clone();
                *existing_tool = tool.clone();
                *existing_origin_key = origin_key;
            }
        } else {
            let idx = deduplicated.len();
            registered.insert(tool.name().to_string(), (tool.clone(), origin_key, idx));
            deduplicated.push(tool.clone());
        }
    }

    deduplicated
}

pub fn mcp_tool_match_score(server_name: &str, tool_name: &str) -> usize {
    let sanitized = sanitize_mcp_tool_name_part(server_name, "server");
    let exact_prefix = format!("mcp__{sanitized}_");
    if tool_name.starts_with(&exact_prefix) {
        return exact_prefix.len();
    }

    if tool_name.len() == MAX_MCP_TOOL_NAME_LENGTH && tool_name.starts_with("mcp__") {
        if let Some(last_underscore) = tool_name.rfind('_') {
            if last_underscore >= 5 {
                let prefix_in_tool = &tool_name[..last_underscore];
                let full_prefix = format!("mcp__{sanitized}");
                if full_prefix.starts_with(prefix_in_tool) {
                    return prefix_in_tool.len();
                }
            }
        }
    }

    0
}
