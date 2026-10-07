//! `ci/targets.toml`: declared targets, their input globs and input fingerprints.
//!
//! The fingerprint and receipt formats are specified in `docs/manifest.md`, so
//! other tools of a project can produce and reuse the same receipts.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const KNOWN_KEYS: [&str; 5] = [
    "inputs",
    "extra_inputs",
    "cache",
    "description",
    "resources",
];

#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub description: Option<String>,
    pub inputs: Vec<String>,
    pub extra_inputs: Vec<String>,
    pub cache: bool,
    /// Resource classes the project's scheduler gives this target (free-form for Citrus).
    pub resources: Vec<String>,
    /// Project-specific keys, kept as written.
    pub extensions: BTreeMap<String, toml::Value>,
    owned: Vec<Glob>,
    all: Vec<Glob>,
}

impl Target {
    /// Same inputs, cache and resources: a reformatted entry is not a new check.
    pub fn same_declaration(&self, other: &Target) -> bool {
        self.inputs == other.inputs
            && self.extra_inputs == other.extra_inputs
            && self.cache == other.cache
            && self.resources == other.resources
            && self.extensions == other.extensions
    }

    pub fn owns(&self, path: &str) -> bool {
        self.owned.iter().any(|glob| glob.matches(path))
    }

    pub fn reads(&self, path: &str) -> bool {
        self.all.iter().any(|glob| glob.matches(path))
    }
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub targets: BTreeMap<String, Target>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Manifest> {
        if !path.exists() {
            return Ok(Manifest::default());
        }
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Manifest> {
        let table: toml::Table = toml::from_str(text)?;
        let Some(toml::Value::Table(targets)) = table.get("targets") else {
            bail!("manifest has no [targets.*] entries");
        };
        let mut result = BTreeMap::new();
        for (name, value) in targets {
            if !valid_name(name) {
                bail!("invalid target name: {name}");
            }
            let toml::Value::Table(entry) = value else {
                bail!("{name}: entry must be a table")
            };
            // Keys Citrus does not know belong to the project's own planner
            // (for example scheduling hints); they are kept, not rejected.
            let extensions: BTreeMap<String, toml::Value> = entry
                .iter()
                .filter(|(key, _)| !KNOWN_KEYS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let inputs = strings(entry.get("inputs"))
                .with_context(|| format!("{name}: inputs must be a non-empty list of globs"))?;
            if inputs.is_empty() {
                bail!("{name}: inputs must be a non-empty list of globs");
            }
            let extra_inputs = match entry.get("extra_inputs") {
                None => Vec::new(),
                some => strings(some)
                    .with_context(|| format!("{name}: extra_inputs must be a list of globs"))?,
            };
            let cache = match entry.get("cache") {
                None => false,
                Some(toml::Value::Boolean(value)) => *value,
                Some(_) => bail!("{name}: cache must be true or false"),
            };
            let resources = match entry.get("resources") {
                None => Vec::new(),
                some => strings(some)
                    .with_context(|| format!("{name}: resources must be a list of names"))?,
            };
            let description = entry
                .get("description")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            let owned = inputs
                .iter()
                .map(|pattern| Glob::new(pattern))
                .collect::<Result<Vec<_>>>()?;
            let all = inputs
                .iter()
                .chain(&extra_inputs)
                .map(|pattern| Glob::new(pattern))
                .collect::<Result<Vec<_>>>()?;
            result.insert(
                name.clone(),
                Target {
                    name: name.clone(),
                    description,
                    inputs,
                    extra_inputs,
                    cache,
                    resources,
                    extensions,
                    owned,
                    all,
                },
            );
        }
        Ok(Manifest { targets: result })
    }

    pub fn owners(&self, path: &str) -> Vec<&str> {
        self.targets
            .values()
            .filter(|target| target.owns(path))
            .map(|target| target.name.as_str())
            .collect()
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn strings(value: Option<&toml::Value>) -> Result<Vec<String>> {
    let Some(toml::Value::Array(items)) = value else {
        bail!("expected a list")
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .context("expected a string")
        })
        .collect()
}

/// Whether `pattern` (a manifest glob) matches at least one of `files`.
pub fn pattern_matches_any(pattern: &str, files: &[String]) -> Result<bool> {
    let glob = Glob::new(pattern)?;
    Ok(files.iter().any(|path| glob.matches(path)))
}

/// Path glob: `**/` spans whole directories, `**` anything, `*` and `?` stay in one segment.
#[derive(Debug, Clone)]
struct Glob(Vec<Token>);

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Dirs,
    Any,
    Star,
    One,
    Char(char),
}

impl Glob {
    fn new(pattern: &str) -> Result<Glob> {
        if pattern.starts_with('/') || pattern.split('/').any(|part| part == "..") {
            bail!("input glob must be repository-relative: {pattern}");
        }
        let chars: Vec<char> = pattern.chars().collect();
        let mut tokens = Vec::new();
        let mut index = 0;
        while index < chars.len() {
            let rest = &chars[index..];
            if rest.starts_with(&['*', '*', '/']) {
                tokens.push(Token::Dirs);
                index += 3;
            } else if rest.starts_with(&['*', '*']) {
                tokens.push(Token::Any);
                index += 2;
            } else {
                tokens.push(match rest[0] {
                    '*' => Token::Star,
                    '?' => Token::One,
                    other => Token::Char(other),
                });
                index += 1;
            }
        }
        Ok(Glob(tokens))
    }

    fn matches(&self, path: &str) -> bool {
        let chars: Vec<char> = path.chars().collect();
        matches_at(&self.0, &chars)
    }
}

fn matches_at(tokens: &[Token], text: &[char]) -> bool {
    let Some((token, rest)) = tokens.split_first() else {
        return text.is_empty();
    };
    match token {
        Token::Char(c) => text.first() == Some(c) && matches_at(rest, &text[1..]),
        Token::One => text.first().is_some_and(|c| *c != '/') && matches_at(rest, &text[1..]),
        Token::Star => {
            for split in 0..=text.len() {
                if matches_at(rest, &text[split..]) {
                    return true;
                }
                if text.get(split) == Some(&'/') {
                    break;
                }
            }
            false
        }
        Token::Any => (0..=text.len()).any(|split| matches_at(rest, &text[split..])),
        Token::Dirs => {
            // Zero or more complete "segment/" prefixes.
            if matches_at(rest, text) {
                return true;
            }
            let mut start = 0;
            while let Some(offset) = text[start..].iter().position(|c| *c == '/') {
                if offset == 0 {
                    return false;
                }
                start += offset + 1;
                if matches_at(rest, &text[start..]) {
                    return true;
                }
            }
            false
        }
    }
}

/// Input fingerprint of a declared target plus the per-file digests it covers.
#[derive(Debug, Clone)]
pub struct Fingerprint {
    pub value: String,
    pub files: Vec<(String, String)>,
}

/// `toolchain` lists files every receipt depends on in addition to declared inputs.
pub fn fingerprint(
    root: &Path,
    files: &[String],
    target: &Target,
    toolchain: &[String],
) -> Result<Fingerprint> {
    let mut digest = Sha256::new();
    let header = format!(
        "{{\"extra_inputs\": {}, \"inputs\": {}, \"target\": {}}}",
        py_json_list(&target.extra_inputs),
        py_json_list(&target.inputs),
        py_json_str(&target.name)
    );
    digest.update(header.as_bytes());
    let mut covered = Vec::new();
    let paths = files
        .iter()
        .filter(|path| target.reads(path))
        .chain(toolchain)
        .map(String::as_str);
    for path in paths {
        let file = root.join(path);
        if !file.is_file() {
            digest.update(format!("missing:{path}\0").as_bytes());
            covered.push((path.to_owned(), "missing".to_owned()));
            continue;
        }
        let executable = if is_executable(&file) { "x" } else { "-" };
        digest.update(format!("{path}\0{executable}\0").as_bytes());
        let content = Sha256::digest(fs::read(&file).with_context(|| format!("read {path}"))?);
        digest.update(content);
        covered.push((
            path.to_owned(),
            format!("{executable}{}", hex::encode(content)),
        ));
    }
    Ok(Fingerprint {
        value: hex::encode(digest.finalize()),
        files: covered,
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: c_path is a valid NUL-terminated string for the duration of the call.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

/// `json.dumps` with default separators and `ensure_ascii=True`.
fn py_json_str(value: &str) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_default();
    let mut out = String::with_capacity(encoded.len());
    for c in encoded.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

fn py_json_list(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| py_json_str(value))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// PASS receipts: one file per target and input fingerprint (`docs/manifest.md`).
#[derive(Debug, Clone)]
pub struct Receipts {
    pub dir: PathBuf,
    pub max_age: u64,
}

impl Receipts {
    fn path(&self, target: &str, fingerprint: &str) -> PathBuf {
        self.dir.join(format!("{target}-{fingerprint}.pass"))
    }

    pub fn valid(&self, target: &str, fingerprint: &str) -> bool {
        let path = self.path(target, fingerprint);
        let Ok(info) = fs::symlink_metadata(&path) else {
            return false;
        };
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        if !info.file_type().is_file()
            || info.uid() != uid
            || info.permissions().mode() & 0o022 != 0
        {
            return false;
        }
        now().saturating_sub(info.mtime().max(0) as u64) < self.max_age
    }

    pub fn write(&self, target: &str, fingerprint: &str) -> Result<()> {
        crate::repo::private_dir(&self.dir)?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        let path = self.path(target, fingerprint);
        let temporary = self
            .dir
            .join(format!(".receipt-citrus-{}", std::process::id()));
        fs::write(
            &temporary,
            format!(
                "target={target}\nfingerprint={fingerprint}\nresult=PASS\ncompleted_epoch={}\n",
                now()
            ),
        )?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        fs::rename(&temporary, &path)?;
        Ok(())
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(pattern: &str, path: &str) -> bool {
        Glob::new(pattern).unwrap().matches(path)
    }

    #[test]
    fn globs_follow_ci_targets_semantics() {
        assert!(glob("scripts/*.py", "scripts/a.py"));
        assert!(!glob("scripts/*.py", "scripts/x/a.py"));
        assert!(glob("deploy/**", "deploy/a/b/c.yaml"));
        assert!(glob("deploy/**/c.yaml", "deploy/c.yaml"));
        assert!(glob("deploy/**/c.yaml", "deploy/a/b/c.yaml"));
        assert!(!glob("deploy/**/c.yaml", "deploy/a/b/d.yaml"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "a/c"));
        assert!(Glob::new("/abs").is_err());
        assert!(Glob::new("a/../b").is_err());
    }

    #[test]
    fn manifest_keeps_project_keys_and_rejects_bad_entries() {
        let extended =
            Manifest::parse("[targets.ok]\ninputs=[\"a\"]\nlinux=true\nsnapshot=[\"assets\"]\n")
                .unwrap();
        assert_eq!(
            extended.targets["ok"].extensions["linux"],
            toml::Value::Boolean(true)
        );
        assert!(Manifest::parse("[targets.ok]\ninputs=[\"a\"]\ncache=\"yes\"\n").is_err());
        assert!(Manifest::parse("[targets.Bad]\ninputs=[\"a\"]\n").is_err());
        assert!(Manifest::parse("[targets.ok]\ninputs=[]\n").is_err());
        let manifest = Manifest::parse("[targets.ok]\ninputs=[\"a/*\"]\ncache=true\n").unwrap();
        assert_eq!(manifest.owners("a/b"), vec!["ok"]);
    }

    #[test]
    fn python_json_escaping() {
        assert_eq!(
            py_json_list(&["a\"b".into(), "\u{e9}".into()]),
            "[\"a\\\"b\", \"\\u00e9\"]"
        );
        assert_eq!(py_json_list(&[]), "[]");
    }
}
