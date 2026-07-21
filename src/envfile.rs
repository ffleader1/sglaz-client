use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A single KEY=VALUE pair. Matches the server's JSON shape.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// Read and parse a .env file. Returns an empty list if the file is absent.
pub fn read(path: &str) -> Vec<EnvVar> {
    match std::fs::read_to_string(path) {
        Ok(content) => parse(&content),
        Err(_) => Vec::new(),
    }
}

/// Parse .env contents into key/value pairs. Comments and blank lines are
/// ignored; surrounding quotes are stripped.
pub fn parse(content: &str) -> Vec<EnvVar> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some(idx) = line.find('=') {
            let key = line[..idx].trim().to_string();
            if key.is_empty() {
                continue;
            }
            let value = unquote(line[idx + 1..].trim());
            out.push(EnvVar { key, value });
        }
    }
    out
}

/// Write key/value pairs to a .env file, creating parent dirs as needed.
pub fn write(path: &str, vars: &[EnvVar]) -> Result<()> {
    let mut s = String::new();
    for v in vars {
        s.push_str(&v.key);
        s.push('=');
        s.push_str(&format_value(&v.value));
        s.push('\n');
    }
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    std::fs::write(path, s).with_context(|| format!("writing {}", path))?;
    Ok(())
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].to_string();
    }
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return s[1..s.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    s.to_string()
}

fn format_value(v: &str) -> String {
    let needs_quote = v.is_empty()
        || v.chars()
            .any(|c| c.is_whitespace() || matches!(c, '#' | '"' | '\'' | '=' | '$'));
    if !needs_quote {
        return v.to_string();
    }
    // Prefer single quotes (no escaping); fall back to escaped double quotes.
    if !v.contains('\'') {
        return format!("'{}'", v);
    }
    let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}
