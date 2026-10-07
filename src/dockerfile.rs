//! The part of a multi-stage Dockerfile one target is built from: the global
//! `ARG`s before the first `FROM`, the target stage and every stage it uses
//! through `FROM <stage>`, `COPY --from=<stage>` or `--mount=…,from=<stage>`.
//! An artifact keyed by this text is not rebuilt when another product's stage
//! in a shared Dockerfile changes.

use anyhow::{Result, bail};

struct Stage {
    name: Option<String>,
    text: String,
    uses: Vec<String>,
}

/// The text `target` depends on, in file order.
pub fn scope(text: &str, target: &str) -> Result<String> {
    let mut preamble = String::new();
    let mut stages: Vec<Stage> = Vec::new();
    let mut continued = false;
    for line in text.split_inclusive('\n') {
        let instruction = !continued && !line.trim_start().starts_with('#');
        continued = line.trim_end().ends_with('\\');
        let words: Vec<&str> = line.split_whitespace().collect();
        if instruction
            && words
                .first()
                .is_some_and(|word| word.eq_ignore_ascii_case("FROM"))
        {
            let base = words
                .iter()
                .skip(1)
                .find(|word| !word.starts_with("--"))
                .map(|word| word.to_string());
            let name = words
                .iter()
                .position(|word| word.eq_ignore_ascii_case("AS"))
                .and_then(|index| words.get(index + 1))
                .map(|name| name.to_lowercase());
            stages.push(Stage {
                name,
                text: String::new(),
                uses: base.into_iter().collect(),
            });
        }
        match stages.last_mut() {
            None => preamble.push_str(line),
            Some(stage) => {
                stage.text.push_str(line);
                for word in &words {
                    for part in word.split(',') {
                        let part = part.trim_start_matches("--");
                        if let Some(from) = part.strip_prefix("from=") {
                            stage.uses.push(from.to_owned());
                        }
                    }
                }
            }
        }
    }
    let find = |reference: &str| -> Option<usize> {
        let lower = reference.to_lowercase();
        stages
            .iter()
            .position(|stage| stage.name.as_deref() == Some(lower.as_str()))
            .or_else(|| {
                reference
                    .parse::<usize>()
                    .ok()
                    .filter(|index| *index < stages.len())
            })
    };
    let Some(start) = find(target) else {
        bail!("no stage `{target}` in the Dockerfile");
    };
    let mut needed = vec![false; stages.len()];
    let mut pending = vec![start];
    while let Some(index) = pending.pop() {
        if std::mem::replace(&mut needed[index], true) {
            continue;
        }
        for reference in &stages[index].uses {
            if let Some(used) = find(reference) {
                pending.push(used);
            }
        }
    }
    let mut out = preamble;
    for (stage, needed) in stages.iter().zip(needed) {
        if needed {
            out.push_str(&stage.text);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "ARG RUST=1.90\n\
        FROM rust:${RUST} AS base\nRUN cargo fetch\n\
        FROM base AS api_builder\nRUN --mount=type=cache,target=/c,from=cache cargo build \\\n  --bin api\n\
        FROM busybox AS cache\n\
        FROM prom/prometheus AS prometheus\nCOPY prometheus.yml /etc/\n\
        FROM debian AS api\nCOPY --from=api_builder /bin/api /bin/api\n";

    #[test]
    fn keeps_only_the_stages_a_target_uses() {
        let api = scope(FILE, "api").unwrap();
        for kept in [
            "ARG RUST",
            "cargo fetch",
            "--bin api",
            "FROM busybox",
            "COPY --from=api_builder",
        ] {
            assert!(api.contains(kept), "{kept} missing:\n{api}");
        }
        assert!(!api.contains("prometheus"), "{api}");
        assert_eq!(
            scope(&FILE.replace("prometheus.yml", "other.yml"), "api").unwrap(),
            api
        );
        assert!(
            scope(FILE, "prometheus")
                .unwrap()
                .contains("prometheus.yml")
        );
        assert!(scope(FILE, "nope").is_err());
    }
}
