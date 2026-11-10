//! `.env` file parser (Compose flavour).
//!
//! Lines are `KEY=value`, optionally prefixed with `export `. Blank lines and `#` comments
//! are skipped. A bare `KEY` takes its value from the environment (skipped when unset).
//! Unquoted values end at an inline ` #` comment; single quotes are literal; double quotes
//! understand `\n \r \t \\ \" \$`. Quoted values may span lines. Unquoted and double-quoted
//! values are interpolated, seeing earlier entries first and then the outer lookup.
//!
//! Error messages carry line numbers and key names only, never values: real `.env` files
//! hold secrets.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::interpolate::{interpolate, InterpolationError};

#[derive(Debug, Error, PartialEq)]
pub enum DotenvError {
    #[error("line {line}: invalid line (expected KEY=value)")]
    Syntax { line: usize },
    #[error("line {line}: invalid variable name")]
    BadName { line: usize },
    #[error("line {line}: unterminated quoted value")]
    Unterminated { line: usize },
    #[error("line {line}: unexpected text after the closing quote")]
    TrailingText { line: usize },
    #[error("line {line}: variable '{key}': {reason}")]
    Interpolation { line: usize, key: String, reason: String },
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Parse a `.env` document. `outer` resolves variables not defined earlier in the file
/// (normally the process environment).
pub fn parse_dotenv<F>(input: &str, outer: &F) -> Result<BTreeMap<String, String>, DotenvError>
where
    F: Fn(&str) -> Option<String>,
{
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let lines: Vec<&str> = input.lines().collect();
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line_no = i + 1;
        let mut line = lines[i].trim_start();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export") {
            if rest.starts_with([' ', '\t']) {
                line = rest.trim_start();
            }
        }
        let Some((key, value)) = line.split_once('=') else {
            let key = line.trim_end();
            if !valid_key(key) {
                return Err(DotenvError::Syntax { line: line_no });
            }
            if let Some(v) = outer(key) {
                vars.insert(key.to_string(), v);
            }
            continue;
        };
        let key = key.trim_end();
        if !valid_key(key) {
            return Err(DotenvError::BadName { line: line_no });
        }
        let value = value.trim_start();

        let lookup = |n: &str| vars.get(n).cloned().or_else(|| outer(n));
        let expand = |raw: &str| {
            interpolate(raw, &lookup).map_err(|e: InterpolationError| DotenvError::Interpolation {
                line: line_no,
                key: key.to_string(),
                reason: e.to_string(),
            })
        };

        let resolved = match value.chars().next() {
            Some(q @ ('\'' | '"')) => {
                let (body, rest, consumed) =
                    read_quoted(q, &value[1..], &lines[i..]).ok_or(DotenvError::Unterminated { line: line_no })?;
                i += consumed;
                let rest = rest.trim();
                if !rest.is_empty() && !rest.starts_with('#') {
                    return Err(DotenvError::TrailingText { line: line_no });
                }
                if q == '\'' { body } else { expand(&body)? }
            }
            _ => expand(strip_inline_comment(value).trim_end())?,
        };
        vars.insert(key.to_string(), resolved);
    }
    Ok(vars)
}

fn strip_inline_comment(value: &str) -> &str {
    match value.char_indices().find(|&(i, c)| {
        c == '#' && (i == 0 || value[..i].ends_with([' ', '\t']))
    }) {
        Some((i, _)) => &value[..i],
        None => value,
    }
}

/// Read a quoted value starting right after the opening quote `q` on the current line,
/// continuing on `more` lines when needed. Returns (body, text after the closing quote,
/// extra lines consumed). Double-quoted bodies have `\$` rewritten to `$$` so that the
/// interpolation step turns it back into a literal `$`.
fn read_quoted(q: char, first: &str, more: &[&str]) -> Option<(String, String, usize)> {
    let mut body = String::new();
    let mut consumed = 0;
    let mut current = first;
    loop {
        let mut chars = current.char_indices();
        while let Some((idx, c)) = chars.next() {
            if c == q {
                return Some((body, current[idx + 1..].to_string(), consumed));
            }
            if q == '"' && c == '\\' {
                match chars.next() {
                    Some((_, 'n')) => body.push('\n'),
                    Some((_, 'r')) => body.push('\r'),
                    Some((_, 't')) => body.push('\t'),
                    Some((_, '$')) => body.push_str("$$"),
                    Some((_, e @ ('\\' | '"'))) => body.push(e),
                    Some((_, other)) => {
                        body.push('\\');
                        body.push(other);
                    }
                    None => body.push('\\'),
                }
            } else {
                body.push(c);
            }
        }
        // no closing quote on this line: the value continues on the next one
        current = more.get(consumed)?;
        consumed += 1;
        body.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Result<BTreeMap<String, String>, DotenvError> {
        parse_dotenv(input, &|n: &str| (n == "HOST_VAR").then(|| "from-host".to_string()))
    }

    fn get(input: &str, key: &str) -> String {
        parse(input).unwrap()[key].clone()
    }

    #[test]
    fn basic_pairs_comments_and_blank_lines() {
        let m = parse("# comment\n\nA=1\n  B = 2 \nC=\n").unwrap();
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "2"); // spaces around '=' and the end are trimmed
        assert_eq!(m["C"], "");
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn export_prefix_and_bom_and_crlf() {
        let m = parse("\u{feff}export A=1\r\nexport   B=2\r\nexportC=3\r\n").unwrap();
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "2");
        assert_eq!(m["exportC"], "3");
    }

    #[test]
    fn inline_comments_only_after_whitespace() {
        assert_eq!(get("A=value # note", "A"), "value");
        assert_eq!(get("A=a#b", "A"), "a#b");
        assert_eq!(get("A=url#frag # c", "A"), "url#frag");
        assert_eq!(get("A=# all comment", "A"), "");
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(get("A='raw $X \\n # not comment'", "A"), "raw $X \\n # not comment");
        assert_eq!(get("A='x' # trailing", "A"), "x");
    }

    #[test]
    fn double_quotes_escape_and_interpolate() {
        assert_eq!(get("A=\"a\\nb\\t\\\"q\\\" \\\\\"", "A"), "a\nb\t\"q\" \\");
        assert_eq!(get("A=\"cost \\$5\"", "A"), "cost $5");
        assert_eq!(get("A=\"keep # hash\"", "A"), "keep # hash");
        assert_eq!(get("A=\"  padded  \"", "A"), "  padded  ");
        assert_eq!(get("A=\"\\q\"", "A"), "\\q");
    }

    #[test]
    fn multiline_quoted_values() {
        let m = parse("KEY=\"line1\nline2\"\nOTHER='a\nb'\nZ=1").unwrap();
        assert_eq!(m["KEY"], "line1\nline2");
        assert_eq!(m["OTHER"], "a\nb");
        assert_eq!(m["Z"], "1");
    }

    #[test]
    fn interpolation_sees_earlier_entries_then_outer() {
        let m = parse("A=1\nB=${A}-x\nC=$HOST_VAR\nD=${NOPE:-dflt}\nE=\"${A}\"\nF='${A}'").unwrap();
        assert_eq!(m["B"], "1-x");
        assert_eq!(m["C"], "from-host");
        assert_eq!(m["D"], "dflt");
        assert_eq!(m["E"], "1");
        assert_eq!(m["F"], "${A}");
    }

    #[test]
    fn later_entry_overrides_earlier() {
        assert_eq!(get("A=1\nA=2", "A"), "2");
    }

    #[test]
    fn bare_key_takes_the_outer_value_when_set() {
        let m = parse("HOST_VAR\nMISSING\n").unwrap();
        assert_eq!(m["HOST_VAR"], "from-host");
        assert!(!m.contains_key("MISSING"));
    }

    #[test]
    fn dotted_and_dashed_names_are_allowed() {
        assert_eq!(get("my.var-1=ok", "my.var-1"), "ok");
    }

    #[test]
    fn errors_carry_line_numbers() {
        assert_eq!(parse("A=1\nnot a pair\n").unwrap_err(), DotenvError::Syntax { line: 2 });
        assert_eq!(parse("=x").unwrap_err(), DotenvError::BadName { line: 1 });
        assert_eq!(parse("a b=x").unwrap_err(), DotenvError::BadName { line: 1 });
        assert_eq!(parse("A=\"open").unwrap_err(), DotenvError::Unterminated { line: 1 });
        assert_eq!(parse("A='x' junk").unwrap_err(), DotenvError::TrailingText { line: 1 });
    }

    #[test]
    fn errors_never_contain_values() {
        let e = parse("SECRET=\"hunter2${\"").unwrap_err().to_string();
        assert!(!e.contains("hunter2"), "{e}");
        let e = parse("SECRET=hunter2$").unwrap_err().to_string();
        assert!(!e.contains("hunter2"), "{e}");
        assert!(e.contains("SECRET") && e.contains("line 1"), "{e}");
    }
}
