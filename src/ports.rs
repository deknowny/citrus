//! `citrus ports FILE NAME…`: a block of free ports for a test stack.
//!
//! Pool agents share machines with other runs and production services, so a
//! project's test stack moves to its own block of ports. The run id picks
//! where the search for a free block starts; NAME[i] gets block + i. Values
//! that mention an old port (`--url KEY=NAME`) follow it. A name ending in `+`
//! is appended when the file does not have it.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::net::TcpListener;
use std::path::Path;

/// Below the usual ephemeral range.
const LOW: u32 = 21000;
const HIGH: u32 = 32000;
const STRIDE: u32 = 10;

pub struct Request<'a> {
    pub file: &'a Path,
    pub names: &'a [String],
    pub urls: &'a [String],
}

fn free(port: u32) -> bool {
    let Ok(port) = u16::try_from(port) else {
        return false;
    };
    if TcpListener::bind(("0.0.0.0", port)).is_err() {
        return false;
    }
    match TcpListener::bind(("::", port)) {
        Ok(_) => true,
        // No IPv6 on this host.
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
        ),
    }
}

/// A free block of `size` ports, searched from a start the run id picks.
fn block(run: &str, size: u32) -> Result<u32> {
    let stride = size.div_ceil(STRIDE).max(1) * STRIDE;
    let slots = (HIGH - LOW) / stride;
    let digest = Sha256::digest(run.as_bytes());
    let seed = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
    let start = (seed % u64::from(slots)) as u32;
    for step in 0..slots {
        let base = LOW + (start + step) % slots * stride;
        if (0..size).all(|offset| free(base + offset)) {
            return Ok(base);
        }
    }
    bail!("no free block of {size} ports")
}

/// `text` with the ports of `names` set to `base + index`, the ports in the
/// values of `urls` (`KEY=NAME`) moved along, and names marked `+` appended
/// when missing.
pub fn rewrite(text: &str, base: u32, names: &[String], urls: &[String]) -> Result<String> {
    let plain: Vec<&str> = names
        .iter()
        .map(|name| name.trim_end_matches('+'))
        .collect();
    let offset = |name: &str| plain.iter().position(|candidate| *candidate == name);
    let lines: Vec<&str> = text.lines().collect();
    let mut old: Vec<Option<String>> = vec![None; plain.len()];
    for line in &lines {
        if let Some((key, value)) = line.split_once('=')
            && let Some(index) = offset(key)
        {
            old[index] = Some(value.to_owned());
        }
    }
    let mut follow: Vec<(&str, usize)> = Vec::new();
    for url in urls {
        let (key, name) = url
            .split_once('=')
            .with_context(|| format!("--url wants KEY=NAME, got {url}"))?;
        let index =
            offset(name).with_context(|| format!("--url {url}: {name} is not a port name"))?;
        follow.push((key, index));
    }
    let mut seen = vec![false; plain.len()];
    let mut out = Vec::new();
    for line in &lines {
        let Some((key, value)) = line.split_once('=') else {
            out.push((*line).to_owned());
            continue;
        };
        if let Some(index) = offset(key) {
            seen[index] = true;
            out.push(format!("{key}={}", base + index as u32));
        } else if let Some((_, index)) = follow.iter().find(|(candidate, _)| *candidate == key)
            && let Some(previous) = &old[*index]
            && !previous.is_empty()
        {
            out.push(format!(
                "{key}={}",
                replace_port(value, previous, base + *index as u32)
            ));
        } else {
            out.push((*line).to_owned());
        }
    }
    for (index, name) in names.iter().enumerate() {
        if name.ends_with('+') && !seen[index] {
            out.push(format!("{}={}", plain[index], base + index as u32));
        }
    }
    Ok(out.join("\n") + "\n")
}

/// `value` with `:old` (as a whole number) replaced by `:new`.
fn replace_port(value: &str, old: &str, new: u32) -> String {
    let needle = format!(":{old}");
    let mut result = String::new();
    let mut rest = value;
    while let Some(at) = rest.find(&needle) {
        let after = &rest[at + needle.len()..];
        let whole = !after
            .chars()
            .next()
            .is_some_and(|next| next.is_alphanumeric() || next == '_');
        result.push_str(&rest[..at]);
        if whole {
            result.push_str(&format!(":{new}"));
        } else {
            result.push_str(&needle);
        }
        rest = after;
    }
    result.push_str(rest);
    result
}

pub fn run(request: &Request) -> Result<i32> {
    let Ok(run) = std::env::var("CITRUS_POOL_RUN") else {
        return Ok(0);
    };
    if run.is_empty() {
        return Ok(0);
    }
    if !request.file.is_file() {
        eprintln!(
            "citrus ports: {} is missing: this agent has no machine copy of it",
            request.file.display()
        );
        return Ok(0);
    }
    let base = block(&run, request.names.len() as u32)?;
    let text = std::fs::read_to_string(request.file)
        .with_context(|| format!("read {}", request.file.display()))?;
    let rewritten = rewrite(&text, base, request.names, request.urls)?;
    std::fs::write(request.file, rewritten)
        .with_context(|| format!("write {}", request.file.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(request.file, std::fs::Permissions::from_mode(0o600))?;
    }
    println!(
        "citrus ports: {}..{} in {}",
        base,
        base + request.names.len() as u32 - 1,
        request.file.display()
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        ["A_PORT", "B_PORT", "C_PORT+"].map(str::to_owned).to_vec()
    }

    #[test]
    fn ports_follow_the_block_and_urls_move_along() {
        let text = "A_PORT=5432\nB_PORT=8080\nDB_URL=postgres://u@127.0.0.1:5432/x\nOTHER=:5432abc\nNOTE\n";
        let out = rewrite(
            text,
            21010,
            &names(),
            &["DB_URL=A_PORT".into(), "OTHER=A_PORT".into()],
        )
        .unwrap();
        assert_eq!(
            out,
            "A_PORT=21010\nB_PORT=21011\nDB_URL=postgres://u@127.0.0.1:21010/x\nOTHER=:5432abc\nNOTE\nC_PORT=21012\n"
        );
    }

    #[test]
    fn a_port_already_in_the_file_is_not_appended() {
        let out = rewrite("C_PORT=1\n", 21020, &names(), &[]).unwrap();
        assert_eq!(out, "C_PORT=21022\n");
    }

    #[test]
    fn unknown_url_names_are_refused() {
        assert!(rewrite("", 21000, &names(), &["K=NOPE".into()]).is_err());
    }

    #[test]
    fn a_block_is_free_and_stays_in_range() {
        let base = block("r-test", 8).unwrap();
        assert!((LOW..HIGH).contains(&base));
        assert!((0..8).all(|offset| free(base + offset)));
    }
}
