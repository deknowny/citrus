//! `citrus fmt`: the canonical layout of `.ci` files. Line-based, so comments
//! and line breaks stay where people put them; what changes is whitespace:
//! two spaces per nesting level, one space around `=` and after `,` and `:`
//! outside strings (no aligned columns), no trailing spaces, at most one blank
//! line in a row, one newline at the end. Text inside strings is never touched.

/// The formatted text; equal to the input when it is already canonical.
pub fn format(source: &str) -> String {
    let mut out = String::new();
    let mut depth: i64 = 0;
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
            .count() as i64;
        let indent = (depth - closes).max(0) as usize;
        out.push_str(&"  ".repeat(indent));
        out.push_str(&code);
        if let Some(comment) = comment {
            if !code.is_empty() {
                out.push_str("  ");
            }
            out.push_str(comment.trim_end());
        }
        out.push('\n');
        depth += nesting(&code);
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

/// Brackets opened minus closed outside strings.
fn nesting(code: &str) -> i64 {
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in code.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' | '[' | '(' if !in_string => depth += 1,
            '}' | ']' | ')' if !in_string => depth -= 1,
            _ => {}
        }
    }
    depth
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
                if matches!(previous, Some('=' | '!' | '<' | '>')) || next == Some('=') {
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
    fn keeps_multi_line_strings_and_closing_lines() {
        let source = "let x = \"\"\"\n   kept   as is\n\"\"\"\nlet y = [\n\"a\",\n]\n";
        assert_eq!(
            format(source),
            "let x = \"\"\"\n   kept   as is\n\"\"\"\nlet y = [\n  \"a\",\n]\n"
        );
    }
}
