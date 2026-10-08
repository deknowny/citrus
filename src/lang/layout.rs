//! `citrus fmt`: the layout of a `.ci` file. Indentation follows the
//! brackets (four spaces a level), lines lose trailing spaces, runs of blank
//! lines become one and the file ends with one newline. Words, strings and
//! comments stay as written, so formatting never changes what a file means.

/// The formatted text of a `.ci` file.
pub fn format(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    // Open brackets by the line that opened them: a line opening several
    // (`#[paths(`) indents what follows by one level, not by each.
    let mut levels: Vec<usize> = Vec::new();
    // Inside a string that continues on the next line: copy lines as they are.
    let mut open_string = false;
    for raw in text.lines() {
        if open_string {
            let scan = scan(raw, true);
            open_string = scan.open_string;
            apply(&mut levels, &scan);
            out.push(raw.trim_end().to_owned());
            continue;
        }
        let line = raw.trim();
        if line.is_empty() {
            if out.last().is_some_and(|last| !last.is_empty()) {
                out.push(String::new());
            }
            continue;
        }
        let scan = scan(line, false);
        // Levels the line's leading closers end.
        let mut ended = 0;
        let mut closers = scan.leading_closers;
        for open in levels.iter().rev() {
            if closers >= *open {
                closers -= open;
                ended += 1;
            } else {
                // `] - [` closes part of a level: it lines up with its opener.
                if closers > 0 {
                    ended += 1;
                }
                break;
            }
        }
        let mut level = levels.len() - ended;
        // A continued expression (`.context(…)`, `&& …`) sits one level in.
        if [".", "&&", "||", "?"]
            .iter()
            .any(|start| line.starts_with(start))
        {
            level += 1;
        }
        out.push(format!("{}{}", "    ".repeat(level), line));
        open_string = scan.open_string;
        apply(&mut levels, &scan);
    }
    while out.first().is_some_and(String::is_empty) {
        out.remove(0);
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

struct Scan {
    /// Closing brackets before anything else on the line.
    leading_closers: usize,
    /// Brackets in order: `true` opens, `false` closes.
    brackets: Vec<bool>,
    open_string: bool,
}

/// Closers pop from the newest level; what the line still has open is one level.
/// A line that closed part of the newest level and opens again (`] - [`)
/// continues that level.
fn apply(levels: &mut Vec<usize>, scan: &Scan) {
    let mut opened = 0;
    let mut reopens = false;
    for &open in &scan.brackets {
        if open {
            opened += 1;
        } else if opened > 0 {
            opened -= 1;
        } else if let Some(last) = levels.last_mut() {
            *last -= 1;
            reopens = *last > 0;
            if *last == 0 {
                levels.pop();
            }
        }
    }
    if opened > 0 {
        match levels.last_mut() {
            Some(last) if reopens => *last += opened,
            _ => levels.push(opened),
        }
    }
}

/// Brackets of one line outside strings and comments.
fn scan(line: &str, in_string: bool) -> Scan {
    let mut scan = Scan {
        leading_closers: 0,
        brackets: Vec::new(),
        open_string: in_string,
    };
    let mut leading = !in_string;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if scan.open_string {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => scan.open_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                scan.open_string = true;
                leading = false;
            }
            '/' if chars.peek() == Some(&'/') => break,
            '{' | '[' | '(' => {
                scan.brackets.push(true);
                leading = false;
            }
            '}' | ']' | ')' => {
                scan.brackets.push(false);
                if leading {
                    scan.leading_closers += 1;
                }
            }
            c if c.is_whitespace() => {}
            _ => leading = false,
        }
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn indentation_follows_brackets_and_keeps_words() {
        let text = "#![citrus(2)]\n\n\n/// A group.\n#[paths(\n\"a/**\", // {not a bracket\n  \"b/**\",\n)]\ngroup g {\n      check c {\n  run!(\"echo '{x}' }\")?;   \n}\n}\n\n";
        assert_eq!(
            format(text),
            "#![citrus(2)]\n\n/// A group.\n#[paths(\n    \"a/**\", // {not a bracket\n    \"b/**\",\n)]\ngroup g {\n    check c {\n        run!(\"echo '{x}' }\")?;\n    }\n}\n"
        );
    }

    #[test]
    fn a_line_closing_part_of_a_level_lines_up_with_its_opener() {
        let text = "#[paths([\n\"a\",\n] - [\n\"b\",\n])]\ngroup g {}\n";
        assert_eq!(
            format(text),
            "#[paths([\n    \"a\",\n] - [\n    \"b\",\n])]\ngroup g {}\n"
        );
    }

    #[test]
    fn formatting_twice_changes_nothing() {
        let once = format("group g {\ncheck c { run!(\"x\")?; }\n  }\n");
        assert_eq!(format(&once), once);
    }
}
