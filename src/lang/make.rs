//! What `make <target>` reads: the Makefiles defining the rules it runs and
//! the repository files their recipes name, followed into the scripts those
//! files name in turn. Read from the files, never by running Make, so the
//! answer is the same for every checkout of the same sources.

use std::collections::{BTreeMap, BTreeSet};

use super::cargo::Files;

#[derive(Debug, Default)]
struct Rule {
    /// The Makefile defining it (a target may appear in several).
    files: BTreeSet<String>,
    prereqs: Vec<String>,
    recipe: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Makefiles {
    /// Files each script names, worked out once for every check.
    named: std::cell::RefCell<BTreeMap<String, Vec<String>>>,
    rules: BTreeMap<String, Rule>,
    /// Files assigning each variable.
    variables: BTreeMap<String, BTreeSet<String>>,
    /// Every Makefile read, in order.
    files: Vec<String>,
}

/// How deep scripts are followed into the files they name.
const SCRIPT_DEPTH: usize = 4;

impl Makefiles {
    /// The project's Makefile and everything it includes; None without one.
    pub fn load(files: &dyn Files) -> Option<Makefiles> {
        let entry = ["GNUmakefile", "makefile", "Makefile"]
            .into_iter()
            .find(|name| files.list().iter().any(|path| path == name))?;
        let mut makefiles = Makefiles::default();
        let mut queue = vec![entry.to_owned()];
        while let Some(file) = queue.pop() {
            if makefiles.files.contains(&file) {
                continue;
            }
            let Some(text) = files.read(&file) else {
                continue;
            };
            makefiles.files.push(file.clone());
            let mut included = makefiles.parse(&file, &text, files);
            included.reverse();
            queue.extend(included);
        }
        Some(makefiles)
    }

    /// Rules of one file; the files it includes.
    fn parse(&mut self, file: &str, text: &str, files: &dyn Files) -> Vec<String> {
        let mut includes = Vec::new();
        let mut current: Vec<String> = Vec::new();
        let mut in_define = false;
        for line in logical_lines(text) {
            if in_define {
                in_define = !line.trim_start().starts_with("endef");
                continue;
            }
            if let Some(recipe) = line.strip_prefix('\t') {
                for target in &current {
                    if let Some(rule) = self.rules.get_mut(target) {
                        rule.recipe.push(recipe.to_owned());
                    }
                }
                continue;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let first = trimmed.split_whitespace().next().unwrap_or_default();
            if matches!(first, "include" | "-include" | "sinclude") {
                for word in trimmed.split_whitespace().skip(1) {
                    if word.contains('$') {
                        continue;
                    }
                    if word.contains('*') {
                        let mut matched: Vec<String> = files
                            .list()
                            .iter()
                            .filter(|path| glob_word(word, path))
                            .cloned()
                            .collect();
                        matched.sort();
                        includes.extend(matched);
                    } else {
                        includes.push(word.to_owned());
                    }
                }
                current.clear();
                continue;
            }
            if first == "define" {
                in_define = true;
                current.clear();
                continue;
            }
            if matches!(
                first,
                "ifeq"
                    | "ifneq"
                    | "ifdef"
                    | "ifndef"
                    | "else"
                    | "endif"
                    | "export"
                    | "unexport"
                    | "override"
                    | "vpath"
            ) {
                continue;
            }
            if let Some(name) = assigned(trimmed) {
                self.variables
                    .entry(name)
                    .or_default()
                    .insert(file.to_owned());
                current.clear();
                continue;
            }
            let Some((targets, rest)) = split_rule(trimmed) else {
                current.clear();
                continue;
            };
            // `target: VAR = value` sets a variable for the target.
            if rest.contains('=') {
                continue;
            }
            let (prereqs, inline) = match rest.split_once(';') {
                Some((prereqs, inline)) => (prereqs, Some(inline.trim().to_owned())),
                None => (rest, None),
            };
            current = targets
                .split_whitespace()
                .filter(|target| {
                    !target.starts_with('.') && !target.contains('%') && !target.contains('$')
                })
                .map(str::to_owned)
                .collect();
            for target in &current {
                let rule = self.rules.entry(target.clone()).or_default();
                rule.files.insert(file.to_owned());
                rule.prereqs.extend(
                    prereqs
                        .split_whitespace()
                        .filter(|word| *word != "|" && !word.contains('$'))
                        .map(str::to_owned),
                );
                rule.recipe.extend(inline.clone());
            }
        }
        includes
    }

    /// Recipe lines of the rules `make <target>` runs, in no particular order.
    pub fn recipes(&self, target: &str) -> Vec<String> {
        let mut lines = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue = vec![target.to_owned()];
        while let Some(name) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(rule) = self.rules.get(&name) else {
                continue;
            };
            queue.extend(rule.prereqs.iter().cloned());
            for line in &rule.recipe {
                queue.extend(self.sub_makes(line));
                lines.push(line.clone());
            }
        }
        lines
    }

    /// Rules a recipe line runs through `$(MAKE) a b` or `make a b`. Quoted
    /// text is a message or data (`echo 'use make check'`), not a command.
    fn sub_makes(&self, line: &str) -> Vec<String> {
        let words = words(&unquoted(line));
        let mut found = Vec::new();
        for (index, word) in words.iter().enumerate() {
            if (word == "$(MAKE)" || word == "make")
                && let Some(rest) = words.get(index + 1..)
            {
                found.extend(
                    rest.iter()
                        .take_while(|word| !matches!(word.as_str(), "&&" | "||" | "|"))
                        .filter(|word| self.rules.contains_key(word.as_str()))
                        .cloned(),
                );
            }
        }
        found
    }

    /// Every rule's name.
    pub fn targets(&self) -> impl Iterator<Item = &str> {
        self.rules.keys().map(String::as_str)
    }

    /// Repository files `make <target>` reads: None when the target is not a
    /// rule here. Inputs are globs (`dir/**` for a directory a recipe names).
    /// First everything, then what certainly is read: the Makefiles and the
    /// files the recipes name (not the files scripts merely mention).
    pub fn inputs(&self, target: &str, files: &dyn Files) -> Option<(Vec<String>, Vec<String>)> {
        self.rules.get(target)?;
        let known: BTreeSet<&str> = files.list().iter().map(String::as_str).collect();
        let mut inputs: BTreeSet<String> = BTreeSet::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue = vec![target.to_owned()];
        let mut scripts: Vec<(String, usize)> = Vec::new();
        while let Some(name) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(rule) = self.rules.get(&name) else {
                // A file prerequisite is read like any input.
                if known.contains(name.as_str()) {
                    inputs.insert(name);
                }
                continue;
            };
            inputs.extend(rule.files.iter().cloned());
            queue.extend(rule.prereqs.iter().cloned());
            // Files assigning a variable the recipe uses.
            for line in &rule.recipe {
                for name in references(line) {
                    if let Some(defined) = self.variables.get(&name) {
                        inputs.extend(defined.iter().cloned());
                    }
                }
            }
            for line in &rule.recipe {
                queue.extend(self.sub_makes(line));
                for word in &words(line) {
                    if let Some(found) = repo_path(word, "", &known) {
                        if !found.ends_with("/**") {
                            scripts.push((found.clone(), 0));
                        }
                        inputs.insert(found);
                    }
                }
            }
        }
        let direct: Vec<String> = inputs.iter().cloned().collect();
        // Scripts read the files they name, and code follows into the code
        // it names; data and documents are read, not followed.
        while let Some((script, depth)) = scripts.pop() {
            if depth >= SCRIPT_DEPTH {
                continue;
            }
            for found in self.named(&script, files, &known) {
                if inputs.insert(found.clone()) && !found.ends_with("/**") {
                    scripts.push((found, depth + 1));
                }
            }
        }
        Some((inputs.into_iter().collect(), direct))
    }

    /// What a command line run directly reads, like a recipe line: the
    /// files its words name (Python modules too: `-m unittest scripts.x`),
    /// followed into the scripts they name. None when it names no file.
    pub fn command_inputs(
        &self,
        argv: &[String],
        files: &dyn Files,
    ) -> Option<(Vec<String>, Vec<String>)> {
        let known: BTreeSet<&str> = files.list().iter().map(String::as_str).collect();
        let mut direct: BTreeSet<String> = BTreeSet::new();
        for word in argv {
            let module = (!word.contains('/') && word.contains('.') && !word.ends_with(".py"))
                .then(|| format!("{}.py", word.replace('.', "/")))
                .filter(|path| known.contains(path.as_str()));
            if let Some(found) = module.or_else(|| repo_path(word, "", &known)) {
                direct.insert(found);
            }
        }
        if direct.is_empty() {
            return None;
        }
        let mut inputs = direct.clone();
        let mut scripts: Vec<(String, usize)> = direct
            .iter()
            .filter(|path| !path.ends_with("/**"))
            .map(|path| (path.clone(), 0))
            .collect();
        while let Some((script, depth)) = scripts.pop() {
            if depth >= SCRIPT_DEPTH {
                continue;
            }
            for found in self.named(&script, files, &known) {
                if inputs.insert(found.clone()) && !found.ends_with("/**") {
                    scripts.push((found, depth + 1));
                }
            }
        }
        Some((inputs.into_iter().collect(), direct.into_iter().collect()))
    }

    /// Files a script names (none for a file that is not code).
    fn named(&self, script: &str, files: &dyn Files, known: &BTreeSet<&str>) -> Vec<String> {
        if let Some(found) = self.named.borrow().get(script) {
            return found.clone();
        }
        let mut found = Vec::new();
        if let Some(text) = files.read(script).filter(|text| is_code(script, text)) {
            let dir = script.rsplit_once('/').map_or("", |(dir, _)| dir);
            for word in words(&text) {
                // Files only: a directory in a script is a pattern it matches
                // paths against far more often than a tree it reads.
                if let Some(path) = repo_path(&word, dir, known)
                    && !path.ends_with("/**")
                    && path != script
                    && !found.contains(&path)
                {
                    found.push(path);
                }
            }
            // Python modules beside it: `import ci_impact`, `from x import y`.
            if script.ends_with(".py") {
                for module in python_imports(&text) {
                    let path = if dir.is_empty() {
                        format!("{module}.py")
                    } else {
                        format!("{dir}/{module}.py")
                    };
                    if known.contains(path.as_str()) && !found.contains(&path) {
                        found.push(path);
                    }
                }
            }
        }
        self.named
            .borrow_mut()
            .insert(script.to_owned(), found.clone());
        found
    }
}

/// Programs, not data: shell, Python and JavaScript, or a file with a `#!`.
fn is_code(path: &str, text: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, ext)) => matches!(ext, "sh" | "bash" | "py" | "mjs" | "cjs" | "js" | "ts"),
        None => text.starts_with("#!"),
    }
}

/// The line without its quoted parts.
fn unquoted(line: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None => out.push(c),
        }
    }
    out
}

/// Lines with `\`-continuations joined; recipe lines keep their tab.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    for line in text.lines() {
        let joined = match pending.take() {
            Some(mut previous) => {
                previous.push(' ');
                previous.push_str(line.trim_start());
                previous
            }
            None => line.to_owned(),
        };
        match joined.strip_suffix('\\') {
            Some(open) => pending = Some(open.to_owned()),
            None => out.push(joined),
        }
    }
    out.extend(pending);
    out
}

/// The variable a line assigns: `NAME = …`, `:=`, `?=`, `+=`, `::=`.
fn assigned(line: &str) -> Option<String> {
    let line = line.strip_prefix("override ").unwrap_or(line);
    let line = line.strip_prefix("export ").unwrap_or(line);
    let end = line.find(['=', ':', '?', '+', ' ', '\t'])?;
    let (name, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let assigns = ["=", ":=", "::=", "?=", "+=", "!="]
        .iter()
        .any(|op| rest.starts_with(op));
    (assigns && !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| name.to_owned())
}

/// Variables a recipe line uses: `$(NAME)`, `${NAME}`.
fn references(line: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find(['$']) {
        rest = &rest[start + 1..];
        let close = match rest.chars().next() {
            Some('(') => ')',
            Some('{') => '}',
            _ => continue,
        };
        if let Some(end) = rest.find(close) {
            let name = &rest[1..end];
            if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.is_empty() {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// `targets: prereqs` — not an assignment (`:=`, `::=`) or a `VAR = a:b`.
fn split_rule(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let (targets, rest) = line.split_at(colon);
    if targets.contains('=') || rest.starts_with(":=") || rest.starts_with("::=") {
        return None;
    }
    let rest = rest.trim_start_matches(':');
    if rest.starts_with('=') {
        return None;
    }
    Some((targets, rest))
}

/// Shell-ish words of a recipe or a script: split on whitespace, quotes,
/// brackets and separators.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | ',' | ';' | '=' | '<' | '>' | '{' | '}'
            )
    })
    .filter(|word| !word.is_empty())
    .map(|word| {
        // `$(MAKE)` survives the split as `$` + `MAKE`; put it back.
        if word == "$" { "$" } else { word }
    })
    .map(str::to_owned)
    .fold(Vec::new(), |mut out: Vec<String>, word| {
        // Recipe prefixes (`@`, `-`, `+`) may stand before it.
        if word == "MAKE"
            && out
                .last()
                .is_some_and(|last| last.trim_start_matches(['@', '-', '+']) == "$")
        {
            out.pop();
            out.push("$(MAKE)".into());
        } else {
            out.push(word);
        }
        out
    })
}

/// A word naming a repository file or directory, relative to the root or to
/// `dir`: the file, or `dir/**`.
fn repo_path(word: &str, dir: &str, known: &BTreeSet<&str>) -> Option<String> {
    // Recipe prefixes: `@` (quiet), `+` (run under -n).
    let word = word.trim_start_matches(['@', '+']);
    let word = word
        .trim_start_matches("$$repo_root/")
        .trim_start_matches("$repo_root/")
        .trim_start_matches("$(CURDIR)/")
        .trim_start_matches("./")
        .trim_end_matches(['/', ':', '.']);
    if word.is_empty()
        || word.contains('$')
        || word.contains('*')
        || word.starts_with('-')
        || word.starts_with('/')
    {
        return None;
    }
    let candidates = [
        word.to_owned(),
        if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/{word}")
        },
    ];
    for candidate in candidates
        .iter()
        .filter(|candidate| !candidate.is_empty() && !candidate.contains(".."))
    {
        if known.contains(candidate.as_str()) {
            return Some(candidate.clone());
        }
        let prefix = format!("{candidate}/");
        if candidate.contains('/')
            && known
                .range(prefix.as_str()..)
                .next()
                .is_some_and(|path| path.starts_with(&prefix))
        {
            return Some(format!("{candidate}/**"));
        }
    }
    None
}

fn python_imports(text: &str) -> Vec<String> {
    let mut modules = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let names = if let Some(rest) = line.strip_prefix("import ") {
            rest.split(',')
                .map(|part| part.split_whitespace().next().unwrap_or_default())
                .collect::<Vec<_>>()
        } else if let Some(rest) = line.strip_prefix("from ") {
            rest.split_whitespace().next().into_iter().collect()
        } else {
            continue;
        };
        for name in names {
            let first = name.split('.').next().unwrap_or_default();
            if !first.is_empty() && first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                modules.push(first.to_owned());
            }
        }
    }
    modules
}

/// `make/*.mk`-style include patterns: `*` within one path segment.
fn glob_word(pattern: &str, path: &str) -> bool {
    crate::manifest::GlobList::new(&[pattern.to_owned()]).is_ok_and(|globs| globs.matches(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<String>, BTreeMap<String, String>);

    impl Files for Fake {
        fn read(&self, path: &str) -> Option<String> {
            self.1.get(path).cloned()
        }
        fn list(&self) -> &[String] {
            &self.0
        }
    }

    fn fake(files: &[(&str, &str)]) -> Fake {
        Fake(
            files.iter().map(|(path, _)| (*path).to_owned()).collect(),
            files
                .iter()
                .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn a_direct_command_reads_the_scripts_and_modules_it_names() {
        let files = fake(&[
            (
                "scripts/test_tool.py",
                "import tool\nopen('data/rows.json')\n",
            ),
            ("scripts/tool.py", "print(1)\n"),
            ("data/rows.json", "[]"),
            ("scripts/other.py", ""),
        ]);
        let makefiles = Makefiles::default();
        let argv = |words: &[&str]| {
            words
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>()
        };
        let (inputs, direct) = makefiles
            .command_inputs(
                &argv(&["python3", "-B", "-m", "unittest", "scripts.test_tool"]),
                &files,
            )
            .unwrap();
        assert_eq!(direct, ["scripts/test_tool.py"]);
        assert_eq!(
            inputs,
            ["data/rows.json", "scripts/test_tool.py", "scripts/tool.py"]
        );
        assert!(
            makefiles
                .command_inputs(&argv(&["python3", "-c", "1"]), &files)
                .is_none()
        );
    }

    #[test]
    fn a_target_reads_its_rules_scripts_and_what_they_name() {
        let files = fake(&[
            (
                "Makefile",
                "include make/*.mk\nTEST_ARGS := -q\nOTHER = x\n",
            ),
            (
                "make/tests.mk",
                ".PHONY: test-api\ntest-api: test-schema\n\t@SQLX_OFFLINE=true ./scripts/run-tests.sh \\\n\t\t-p api\n\t@$(MAKE) --no-print-directory test-docs $(TEST_ARGS)\n",
            ),
            (
                "make/schema.mk",
                "test-schema:\n\tpython3 -B scripts/test-schema.py\ntest-docs:\n\t@python3 -m unittest discover -s docs/tests\nunrelated:\n\t./scripts/other.sh\n",
            ),
            (
                "scripts/run-tests.sh",
                "#!/bin/sh\nexec cargo test \"$@\"\n",
            ),
            (
                "scripts/test-schema.py",
                "import schema_rules\nPath(__file__).with_name('fixtures.json')\n",
            ),
            ("scripts/schema_rules.py", "RULES = 'deploy/rules.toml'\n"),
            ("scripts/fixtures.json", "{}"),
            ("deploy/rules.toml", ""),
            ("docs/tests/test_a.py", ""),
            ("scripts/other.sh", ""),
        ]);
        let makefiles = Makefiles::load(&files).unwrap();
        let (inputs, direct) = makefiles.inputs("test-api", &files).unwrap();
        assert_eq!(
            direct,
            vec![
                "Makefile",
                "docs/tests/**",
                "make/schema.mk",
                "make/tests.mk",
                "scripts/run-tests.sh",
                "scripts/test-schema.py"
            ]
        );
        assert_eq!(
            inputs,
            vec![
                "Makefile",
                "deploy/rules.toml",
                "docs/tests/**",
                "make/schema.mk",
                "make/tests.mk",
                "scripts/fixtures.json",
                "scripts/run-tests.sh",
                "scripts/schema_rules.py",
                "scripts/test-schema.py",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        );
        assert!(makefiles.inputs("no-such-target", &files).is_none());
        // `make` in a message is not a command.
        let files = fake(&[
            (
                "Makefile",
                "test-a:\n\t@test -x a || { echo 'use make other' >&2; exit 2; }\nother:\n\t./scripts/other.sh\n",
            ),
            ("scripts/other.sh", ""),
        ]);
        let makefiles = Makefiles::load(&files).unwrap();
        assert_eq!(
            makefiles.inputs("test-a", &files).unwrap().0,
            vec!["Makefile"]
        );
    }
}
