//! Declared checks (from `citrus.ci`), their input globs and input fingerprints.
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

#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub description: Option<String>,
    pub inputs: Vec<String>,
    pub extra_inputs: Vec<String>,
    pub cache: bool,
    /// Resource classes the project's scheduler gives this target (free-form for Citrus).
    pub resources: Vec<String>,
    /// Project-specific data (`meta = { … }`), kept as written.
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Extra environment of its steps.
    pub env: BTreeMap<String, String>,
    /// Steps declared in `citrus.ci`; empty means `run.local` (`make <target>`).
    pub steps: Vec<crate::lang::compile::Step>,
    /// `file:line` of the declaration in `citrus.ci`.
    pub source: Option<String>,
    /// Profiles it belongs to; empty: all.
    pub profiles: Vec<String>,
    /// Checks that already run this one.
    pub covered_by: Vec<String>,
    /// Plan-time condition.
    pub when: Option<crate::lang::compile::Cond>,
    /// Order of declaration in `citrus.ci`: plans list checks in this order.
    pub position: usize,
    owned: GlobList,
    extra: GlobList,
}

/// Globs in order; `!glob` removes matches of the globs before it, and the
/// last glob that matches a path decides (like `.gitignore`).
#[derive(Debug, Clone, Default)]
pub struct GlobList(Vec<(bool, Glob)>);

impl GlobList {
    pub fn new(patterns: &[String]) -> Result<GlobList> {
        patterns
            .iter()
            .map(|pattern| match pattern.strip_prefix('!') {
                Some(rest) => Ok((false, Glob::new(rest)?)),
                None => Ok((true, Glob::new(pattern)?)),
            })
            .collect::<Result<Vec<_>>>()
            .map(GlobList)
    }

    pub fn matches(&self, path: &str) -> bool {
        self.0
            .iter()
            .rev()
            .find(|(_, glob)| glob.matches(path))
            .is_some_and(|(include, _)| *include)
    }
}

impl Target {
    /// A check declared in `citrus.ci`.
    pub fn declared(check: &crate::lang::compile::Check, source: String) -> Result<Target> {
        let owned = GlobList::new(&check.owns)?;
        let extra = GlobList::new(&check.reads)?;
        Ok(Target {
            name: check.name.clone(),
            description: check.description.clone(),
            inputs: check.owns.clone(),
            extra_inputs: check.reads.clone(),
            cache: check.cache,
            resources: check.resources.clone(),
            extensions: check.meta.clone(),
            env: check.env.iter().cloned().collect(),
            steps: check.steps.clone(),
            source: Some(source),
            profiles: check.profiles.clone(),
            covered_by: check.covered_by.clone(),
            when: check.when.clone(),
            position: 0,
            owned,
            extra,
        })
    }

    /// Same inputs, cache and resources: a reformatted entry is not a new check.
    pub fn same_declaration(&self, other: &Target) -> bool {
        self.declaration() == other.declaration()
            && self.cache == other.cache
            && self.resources == other.resources
            && self.extensions == other.extensions
    }

    /// What a PASS of this check proves, as hashed first in its fingerprint
    /// (docs/manifest.md): its globs, what it runs and with which environment.
    pub fn declaration(&self) -> serde_json::Value {
        serde_json::json!({
            "env": self.env,
            "extra_inputs": self.extra_inputs,
            "inputs": self.inputs,
            "run": self.steps.iter().map(|step| step.work.canonical()).collect::<Vec<_>>(),
            "target": self.name,
        })
    }

    pub fn owns(&self, path: &str) -> bool {
        self.owned.matches(path)
    }

    pub fn reads(&self, path: &str) -> bool {
        self.owned.matches(path) || self.extra.matches(path)
    }
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub targets: BTreeMap<String, Target>,
    /// The `.ci` files the checks were declared in.
    pub files: Vec<String>,
    /// Named path sets for conditions and plan notes.
    pub groups: Vec<PathGroup>,
    /// Prints `SIGNAL <name>` lines for changed paths.
    pub signals: Vec<String>,
    /// Named conditions reported with a plan.
    pub labels: Vec<(String, crate::lang::compile::Cond)>,
}

/// A named path set; declaring a group again adds paths (each declaration's
/// globs are matched on their own).
#[derive(Debug)]
pub struct PathGroup {
    pub name: String,
    pub note: Option<String>,
    /// False: only for conditions; a path in it alone stays unmapped.
    pub claims: bool,
    /// Each declaration's globs, and whether its paths touch no other
    /// claiming group (`exclusive = true`).
    globs: Vec<(GlobList, bool)>,
}

impl PathGroup {
    pub fn owns(&self, path: &str) -> bool {
        self.globs.iter().any(|(globs, _)| globs.matches(path))
    }

    pub fn owns_exclusively(&self, path: &str) -> bool {
        self.globs
            .iter()
            .any(|(globs, exclusive)| *exclusive && globs.matches(path))
    }
}

impl Manifest {
    /// The checks of a compiled `citrus.ci`.
    pub fn from_project(
        project: &crate::lang::compile::Project,
        sources: &crate::lang::Sources,
    ) -> Result<Manifest> {
        let mut targets = BTreeMap::new();
        for (position, check) in project.checks.iter().enumerate() {
            let (file, line, _) = sources.locate(check.span);
            let mut target = Target::declared(check, format!("{file}:{line}"))?;
            target.position = position;
            targets.insert(check.name.clone(), target);
        }
        let mut groups: Vec<PathGroup> = Vec::new();
        for group in &project.groups {
            let globs = GlobList::new(&group.owns)?;
            match groups.iter_mut().find(|known| known.name == group.name) {
                Some(known) => {
                    known.globs.push((globs, group.exclusive));
                    if known.note.is_none() {
                        known.note = group.note.clone();
                    }
                }
                None => groups.push(PathGroup {
                    name: group.name.clone(),
                    note: group.note.clone(),
                    claims: group.claims,
                    globs: vec![(globs, group.exclusive)],
                }),
            }
        }
        Ok(Manifest {
            targets,
            files: project.files.clone(),
            groups,
            signals: project.signals.clone(),
            labels: project.labels.clone(),
        })
    }

    /// The declared checks as other tools of the project read them
    /// (`CITRUS_CHECKS`, `citrus targets --json`; docs/manifest.md).
    pub fn export(&self, toolchain: &[String]) -> serde_json::Value {
        let checks: Vec<serde_json::Value> = self
            .targets
            .values()
            .map(|target| {
                serde_json::json!({
                    "target": target.name,
                    "description": target.description,
                    "cache": target.cache,
                    "inputs": target.inputs,
                    "extra_inputs": target.extra_inputs,
                    "resources": target.resources,
                    "meta": target.extensions,
                    "profiles": target.profiles,
                    "covered_by": target.covered_by,
                    "when": target.when,
                    "source": target.source,
                    "declaration": target.declaration(),
                })
            })
            .collect();
        serde_json::json!({"toolchain": toolchain, "files": self.files, "checks": checks})
    }

    /// `export` in a file under the state directory, named by its content.
    pub fn export_file(&self, repo: &crate::repo::Repo) -> Result<PathBuf> {
        let text = serde_json::to_string_pretty(&self.export(&repo.config.toolchain_files))?;
        let dir = repo.state_dir().join("tmp");
        crate::repo::private_dir(&dir)?;
        let path = dir.join(format!(
            "checks-{}.json",
            &hex::encode(Sha256::digest(&text))[..16]
        ));
        if !path.exists() {
            let partial = path.with_extension(format!("{}.part", std::process::id()));
            fs::write(&partial, &text)?;
            fs::rename(&partial, &path)?;
        }
        Ok(path)
    }

    pub fn owners(&self, path: &str) -> Vec<&str> {
        self.targets
            .values()
            .filter(|target| target.owns(path))
            .map(|target| target.name.as_str())
            .collect()
    }
}

pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Whether `pattern` (a manifest glob) matches at least one of `files`.
/// An exclusion (`!glob`) is checked for what it excludes.
pub fn pattern_matches_any(pattern: &str, files: &[String]) -> Result<bool> {
    let glob = Glob::new(pattern.strip_prefix('!').unwrap_or(pattern))?;
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
    digest.update(py_json(&target.declaration()).as_bytes());
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

/// `json.dumps(value, sort_keys=True)` of Python: `", "` and `": "`
/// separators, non-ASCII escaped.
fn py_json(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::String(text) => py_json_str(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_json).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.iter()
                    .map(|key| format!("{}: {}", py_json_str(key), py_json(&map[*key])))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        other => other.to_string(),
    }
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
    fn exclusions_follow_the_last_matching_glob() {
        let list = GlobList::new(&[
            "crates/backend/**".into(),
            "!crates/backend/src/bots/clyer/**".into(),
            "crates/backend/src/bots/clyer/shared.rs".into(),
        ])
        .unwrap();
        assert!(list.matches("crates/backend/src/main.rs"));
        assert!(!list.matches("crates/backend/src/bots/clyer/mod.rs"));
        assert!(list.matches("crates/backend/src/bots/clyer/shared.rs"));
        assert!(!list.matches("crates/other/x.rs"));
    }

    #[test]
    fn python_json_escaping() {
        assert_eq!(
            py_json(&serde_json::json!(["a\"b", "\u{e9}"])),
            "[\"a\\\"b\", \"\\u00e9\"]"
        );
        assert_eq!(
            py_json(&serde_json::json!({"b": [], "a": {"x": true}})),
            "{\"a\": {\"x\": true}, \"b\": []}"
        );
    }
}
