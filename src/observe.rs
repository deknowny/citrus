//! What a check's Python and Node programs actually read while it runs
//! locally. A cached check whose programs read a repository file outside its
//! inputs could be reused after that file changes, so its pass is not
//! reused and Citrus names the file. Programs other than Python and Node
//! (shell tools, compilers) are not observed: Cargo has `citrus deps`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Installed as `sitecustomize`: Python imports it at start-up from
/// `PYTHONPATH`, in every Python process the check starts.
const PYTHON: &str = r#"# Written by Citrus: records what this program reads (CITRUS_OBSERVE).
import os as _os
import sys as _sys

def _citrus_observe():
    target = _os.environ.get("CITRUS_OBSERVE")
    if not target:
        return
    fd = _os.open(target, _os.O_WRONLY | _os.O_APPEND | _os.O_CREAT, 0o600)
    seen = set()
    busy = [False]

    def note(kind, path):
        if busy[0] or not isinstance(path, (str, bytes, _os.PathLike)):
            return
        busy[0] = True
        try:
            path = _os.fsdecode(_os.fspath(path))
            line = kind + " " + _os.path.abspath(path) + "\n"
            if line not in seen:
                seen.add(line)
                _os.write(fd, line.encode("utf-8", "surrogateescape"))
        except Exception:
            pass
        finally:
            busy[0] = False

    def hook(event, args):
        if event == "open" and args and not isinstance(args[0], int):
            note("file", args[0])
        elif event in ("os.listdir", "os.scandir", "glob.glob") and args:
            note("dir" if event != "glob.glob" else "glob", args[0] if args[0] is not None else ".")
        elif event == "subprocess.Popen" and len(args) > 1:
            for arg in args[1] or []:
                if isinstance(arg, (str, bytes)) and _os.path.isfile(arg):
                    note("file", arg)

    _sys.addaudithook(hook)

_citrus_observe()
del _citrus_observe

# The project's own sitecustomize still runs.
_here = _os.path.dirname(_os.path.abspath(__file__))
for _entry in _sys.path:
    if _entry and _os.path.abspath(_entry) != _here and _os.path.isfile(_os.path.join(_entry, "sitecustomize.py")):
        import importlib.util as _util
        _spec = _util.spec_from_file_location("_project_sitecustomize", _os.path.join(_entry, "sitecustomize.py"))
        _spec.loader.exec_module(_util.module_from_spec(_spec))
        break
"#;

/// Loaded with `node --require`: the same for Node programs.
const NODE: &str = r#"// Written by Citrus: records what this program reads (CITRUS_OBSERVE).
'use strict';
const target = process.env.CITRUS_OBSERVE;
if (target) {
  const fs = require('fs');
  const path = require('path');
  const fd = fs.openSync(target, 'a', 0o600);
  const seen = new Set();
  let busy = false;
  const note = (kind, file) => {
    if (busy || (typeof file !== 'string' && !(file instanceof URL) && !Buffer.isBuffer(file))) return;
    busy = true;
    try {
      const name = file instanceof URL ? file.pathname : file.toString();
      const line = `${kind} ${path.resolve(name)}\n`;
      if (!seen.has(line)) {
        seen.add(line);
        fs.writeSync(fd, line);
      }
    } catch (_) {
    } finally {
      busy = false;
    }
  };
  const wrap = (object, name, kind) => {
    const original = object[name];
    if (typeof original !== 'function') return;
    object[name] = function (file, ...rest) {
      note(kind, file);
      return original.call(this, file, ...rest);
    };
  };
  for (const name of ['readFileSync', 'openSync', 'readFile', 'open', 'createReadStream']) wrap(fs, name, 'file');
  for (const name of ['readdirSync', 'readdir', 'opendirSync', 'opendir']) wrap(fs, name, 'dir');
  for (const name of ['readFile', 'open']) wrap(fs.promises, name, 'file');
  for (const name of ['readdir', 'opendir']) wrap(fs.promises, name, 'dir');
  const child = require('child_process');
  for (const name of ['spawn', 'spawnSync', 'execFile', 'execFileSync']) {
    const original = child[name];
    child[name] = function (command, args, ...rest) {
      for (const arg of [command, ...(Array.isArray(args) ? args : [])]) {
        try {
          if (typeof arg === 'string' && fs.statSync(arg, { throwIfNoEntry: false })?.isFile()) note('file', arg);
        } catch (_) {}
      }
      return original.call(this, command, args, ...rest);
    };
  }
}
"#;

/// The observers, written under `dir`; the environment that loads them.
pub fn environment(dir: &Path, log: &Path) -> Result<Vec<(String, String)>> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("sitecustomize.py"), PYTHON)?;
    std::fs::write(dir.join("observe.cjs"), NODE)?;
    let join = |name: &str, first: String, separator: &str| match std::env::var(name) {
        Ok(rest) if !rest.is_empty() => format!("{first}{separator}{rest}"),
        _ => first,
    };
    Ok(vec![
        ("CITRUS_OBSERVE".into(), log.display().to_string()),
        (
            "PYTHONPATH".into(),
            join("PYTHONPATH", dir.display().to_string(), ":"),
        ),
        (
            "NODE_OPTIONS".into(),
            join(
                "NODE_OPTIONS",
                format!("--require={}", dir.join("observe.cjs").display()),
                " ",
            ),
        ),
    ])
}

/// What was read inside `root`, relative to it: files as `path`, listed
/// directories as `path/` and glob patterns as written.
pub fn read(log: &Path, root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut found = BTreeSet::new();
    for line in text.lines() {
        let Some((kind, path)) = line.split_once(' ') else {
            continue;
        };
        let path = PathBuf::from(path);
        let path = path.canonicalize().unwrap_or(path);
        let Ok(relative) = path.strip_prefix(&root) else {
            continue;
        };
        let relative = relative.to_string_lossy().into_owned();
        if relative.is_empty() || relative.starts_with(".git") || relative.starts_with("target/") {
            continue;
        }
        found.insert(match kind {
            "dir" => format!("{relative}/"),
            _ => relative,
        });
    }
    found.into_iter().collect()
}
