//! `citrus fmt`: the canonical layout of `.ci` files. Line-based, so comments
//! and line breaks stay where people put them; what changes is whitespace:
//! two spaces per nesting level, one space around `=` and after `,` and `:`
//! outside strings (no aligned columns), no trailing spaces, at most one blank
//! line in a row, one newline at the end. Text inside strings is never touched.

/// The formatted text; equal to the input when it is already canonical.
pub fn format(source: &str) -> String {
    let mut out = String::new();
    // One entry per line that left brackets open: how many it still holds.
    // A line opening `flatten([` indents what follows by one level, not two.
    let mut open: Vec<usize> = Vec::new();
    let mut blank = 0;
    let mut in_triple = false;
    for raw in source.lines() {
        if in_triple {
            // Multi-line strings keep their lines exactly.
            out.push_str(raw);
            out.push('\n');
            if raw.matches("\"\"\"").count() % 2 == 1 {
                in_triple = false;
            }
            continue;
        }
        let line = raw.trim();
        if line.is_empty() {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        blank = 0;
        let (code, comment) = split_comment(line);
        let code = normalize(code.trim_end());
        let closes = code
            .chars()
            .take_while(|c| matches!(c, '}' | ']' | ')'))
            .count();
        let indent = {
            let mut levels = open.clone();
            let mut remaining = closes;
            while remaining > 0 {
                let Some(top) = levels.last_mut() else { break };
                let used = remaining.min(*top);
                *top -= used;
                remaining -= used;
                if *top == 0 {
                    levels.pop();
                }
            }
            levels.len()
        };
        out.push_str(&"  ".repeat(indent));
        out.push_str(&code);
        if let Some(comment) = comment {
            if !code.is_empty() {
                out.push_str("  ");
            }
            out.push_str(comment.trim_end());
        }
        out.push('\n');
        track(&code, &mut open);
        if code.matches("\"\"\"").count() % 2 == 1 {
            in_triple = true;
        }
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// (code, comment) with the comment starting at a `#` outside strings.
fn split_comment(line: &str) -> (&str, Option<&str>) {
    let mut in_string = false;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return (&line[..index], Some(&line[index..])),
            _ => {}
        }
    }
    (line, None)
}

/// Applies the brackets of `code` (outside strings) to the open-line stack.
fn track(code: &str, open: &mut Vec<usize>) {
    let mut opened = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in code.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' | '[' | '(' if !in_string => opened += 1,
            '}' | ']' | ')' if !in_string => {
                if opened > 0 {
                    opened -= 1;
                } else if let Some(top) = open.last_mut() {
                    *top -= 1;
                    if *top == 0 {
                        open.pop();
                    }
                }
            }
            _ => {}
        }
    }
    if opened > 0 {
        open.push(opened);
    }
}

/// Spacing outside strings: runs of spaces become one; one space around `=`
/// (not `==`, `!=`, `<=`, `>=`), after `,` and after a map key's `:`.
fn normalize(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let mut out = String::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ' ' | '\t' => {
                if !out.ends_with(' ') && !out.is_empty() {
                    out.push(' ');
                }
            }
            '=' => {
                let previous = out.trim_end().chars().last();
                let next = chars.get(index + 1).copied();
                if next == Some('>') {
                    // `=>` of a `match` arm: one space on each side.
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push_str(" => ");
                    index += 1;
                    while chars.get(index + 1).is_some_and(|c| *c == ' ') {
                        index += 1;
                    }
                } else if matches!(previous, Some('=' | '!' | '<' | '>')) || next == Some('=') {
                    out.push('=');
                } else {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push_str(" = ");
                    while chars.get(index + 1).is_some_and(|c| *c == ' ') {
                        index += 1;
                    }
                }
            }
            ',' | ':' => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(c);
                if chars.get(index + 1).is_some_and(|next| *next != ' ') {
                    out.push(' ');
                }
            }
            _ => out.push(c),
        }
        index += 1;
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_alignment_and_reindents_but_keeps_strings_and_comments() {
        let source = "citrus 1\n\n\n\nproject {\n    base      = \"origin/main\"   # where   changes start\n  toolchain=[\"a\",\"b\"]\n}\ncheck \"x\" {\n owns  = [\"src/**\"]\n run   = run(\"sh\", \"-c\", \"a  =  b\")\n env = { A:\"1\" }\n if  a == b { }\n}\n\n";
        let expected = "citrus 1\n\nproject {\n  base = \"origin/main\"  # where   changes start\n  toolchain = [\"a\", \"b\"]\n}\ncheck \"x\" {\n  owns = [\"src/**\"]\n  run = run(\"sh\", \"-c\", \"a  =  b\")\n  env = { A: \"1\" }\n  if a == b { }\n}\n";
        assert_eq!(format(source), expected);
        assert_eq!(format(expected), expected);
    }

    #[test]
    fn a_line_opening_two_brackets_indents_once() {
        let source = "owns = flatten([\n[\"a\"],\n[\"b\"],\n])\nx = 1\n";
        assert_eq!(
            format(source),
            "owns = flatten([\n  [\"a\"],\n  [\"b\"],\n])\nx = 1\n"
        );
    }

    #[test]
    fn keeps_multi_line_strings_and_closing_lines() {
        let source = "let x = \"\"\"\n   kept   as is\n\"\"\"\nlet y = [\n\"a\",\n]\n";
        assert_eq!(
            format(source),
            "let x = \"\"\"\n   kept   as is\n\"\"\"\nlet y = [\n  \"a\",\n]\n"
        );
    }
}
