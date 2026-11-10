//! Compose-style `${VAR}` interpolation. Pure: values come from a lookup closure.
//!
//! Supported forms (same semantics as Docker Compose):
//! `$VAR`, `${VAR}`, `${VAR:-d}` / `${VAR-d}` (default), `${VAR:?msg}` / `${VAR?msg}`
//! (required), `${VAR:+r}` / `${VAR+r}` (replacement) and `$$` (a literal `$`).
//! The `:` variants treat an empty value like an unset one. Defaults, messages and
//! replacements may themselves contain interpolations. An unset variable with no
//! operator becomes the empty string.

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum InterpolationError {
    #[error("invalid interpolation format")]
    Invalid,
    #[error("unterminated '${{'")]
    Unterminated,
    #[error("required variable {var} is missing a value{}", message_suffix(.message))]
    Required { var: String, message: String },
}

fn message_suffix(message: &str) -> String {
    if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    }
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Expand every interpolation in `input`, resolving variables with `lookup`.
pub fn interpolate<F>(input: &str, lookup: &F) -> Result<String, InterpolationError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some((_, '$')) => {
                chars.next();
                out.push('$');
            }
            Some((_, '{')) => {
                chars.next();
                let start = i + 2;
                let end = matching_brace(input, start).ok_or(InterpolationError::Unterminated)?;
                out.push_str(&expand_braced(&input[start..end], lookup)?);
                // skip past the closing brace
                while let Some(&(j, _)) = chars.peek() {
                    if j > end {
                        break;
                    }
                    chars.next();
                }
            }
            Some((_, n)) if is_name_start(n) => {
                let mut name = String::new();
                while let Some(&(_, n)) = chars.peek() {
                    if !is_name_char(n) {
                        break;
                    }
                    name.push(n);
                    chars.next();
                }
                out.push_str(&lookup(&name).unwrap_or_default());
            }
            _ => return Err(InterpolationError::Invalid),
        }
    }
    Ok(out)
}

/// Byte index of the `}` closing the `${` whose body starts at `start`, honouring nesting.
fn matching_brace(input: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (off, c) in input[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + off);
                }
            }
            _ => {}
        }
    }
    None
}

fn expand_braced<F>(body: &str, lookup: &F) -> Result<String, InterpolationError>
where
    F: Fn(&str) -> Option<String>,
{
    let name_len = body
        .char_indices()
        .find(|&(i, c)| if i == 0 { !is_name_start(c) } else { !is_name_char(c) })
        .map_or(body.len(), |(i, _)| i);
    if name_len == 0 {
        return Err(InterpolationError::Invalid);
    }
    let (name, op) = body.split_at(name_len);
    let value = lookup(name);
    let unset_or_empty = value.as_deref().is_none_or(str::is_empty);
    let is_unset = value.is_none();

    let (colon, op) = match op.strip_prefix(':') {
        Some(rest) => (true, rest),
        None => (false, op),
    };
    let missing = if colon { unset_or_empty } else { is_unset };
    match op.chars().next() {
        None if !colon => Ok(value.unwrap_or_default()),
        Some('-') => {
            if missing {
                interpolate(&op[1..], lookup)
            } else {
                Ok(value.unwrap_or_default())
            }
        }
        Some('?') => {
            if missing {
                Err(InterpolationError::Required { var: name.to_string(), message: interpolate(&op[1..], lookup)? })
            } else {
                Ok(value.unwrap_or_default())
            }
        }
        Some('+') => {
            if missing {
                Ok(String::new())
            } else {
                interpolate(&op[1..], lookup)
            }
        }
        _ => Err(InterpolationError::Invalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str, vars: &[(&str, &str)]) -> Result<String, InterpolationError> {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        interpolate(input, &|n: &str| vars.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()))
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(run("hello world", &[]).unwrap(), "hello world");
        assert_eq!(run("", &[]).unwrap(), "");
        assert_eq!(run("ünï € text", &[]).unwrap(), "ünï € text");
    }

    #[test]
    fn simple_and_braced_variables() {
        let v = [("A", "1"), ("B_2", "two")];
        assert_eq!(run("$A", &v).unwrap(), "1");
        assert_eq!(run("${A}", &v).unwrap(), "1");
        assert_eq!(run("x-$A-y", &v).unwrap(), "x-1-y");
        assert_eq!(run("$B_2.", &v).unwrap(), "two.");
        assert_eq!(run("${A}${B_2}", &v).unwrap(), "1two");
    }

    #[test]
    fn unset_becomes_empty() {
        assert_eq!(run("a${NOPE}b$NOPE", &[]).unwrap(), "ab");
    }

    #[test]
    fn dollar_dollar_is_a_literal_dollar() {
        assert_eq!(run("$$HOME", &[("HOME", "x")]).unwrap(), "$HOME");
        assert_eq!(run("$$$A", &[("A", "1")]).unwrap(), "$1");
        assert_eq!(run("$$(date)", &[]).unwrap(), "$(date)");
    }

    #[test]
    fn default_colon_dash_covers_unset_and_empty() {
        assert_eq!(run("${A:-d}", &[]).unwrap(), "d");
        assert_eq!(run("${A:-d}", &[("A", "")]).unwrap(), "d");
        assert_eq!(run("${A:-d}", &[("A", "v")]).unwrap(), "v");
    }

    #[test]
    fn default_dash_covers_only_unset() {
        assert_eq!(run("${A-d}", &[]).unwrap(), "d");
        assert_eq!(run("${A-d}", &[("A", "")]).unwrap(), "");
        assert_eq!(run("${A-d}", &[("A", "v")]).unwrap(), "v");
    }

    #[test]
    fn required_forms() {
        assert_eq!(run("${A:?need A}", &[("A", "v")]).unwrap(), "v");
        assert_eq!(
            run("${A:?need A}", &[]).unwrap_err(),
            InterpolationError::Required { var: "A".into(), message: "need A".into() }
        );
        assert!(run("${A:?m}", &[("A", "")]).is_err());
        assert_eq!(run("${A?m}", &[("A", "")]).unwrap(), "");
        assert!(run("${A?m}", &[]).is_err());
        assert_eq!(run("${A:?}", &[]).unwrap_err().to_string(), "required variable A is missing a value");
        assert_eq!(run("${A:?set it}", &[]).unwrap_err().to_string(), "required variable A is missing a value: set it");
    }

    #[test]
    fn replacement_forms() {
        assert_eq!(run("${A:+yes}", &[("A", "v")]).unwrap(), "yes");
        assert_eq!(run("${A:+yes}", &[("A", "")]).unwrap(), "");
        assert_eq!(run("${A:+yes}", &[]).unwrap(), "");
        assert_eq!(run("${A+yes}", &[("A", "")]).unwrap(), "yes");
        assert_eq!(run("${A+yes}", &[]).unwrap(), "");
    }

    #[test]
    fn nested_interpolation_in_operands() {
        let v = [("B", "bee")];
        assert_eq!(run("${A:-${B}}", &v).unwrap(), "bee");
        assert_eq!(run("${A:-$B}", &v).unwrap(), "bee");
        assert_eq!(run("${A:-${C:-deep}}", &v).unwrap(), "deep");
        assert_eq!(run("${B:+[${B}]}", &v).unwrap(), "[bee]");
        assert_eq!(
            run("${A:?missing ${B}}", &v).unwrap_err().to_string(),
            "required variable A is missing a value: missing bee"
        );
    }

    #[test]
    fn default_may_contain_punctuation() {
        assert_eq!(run("${URL:-http://localhost:8080/x}", &[]).unwrap(), "http://localhost:8080/x");
        assert_eq!(run("127.0.0.1:${PORT:-18888}:18888", &[]).unwrap(), "127.0.0.1:18888:18888");
    }

    #[test]
    fn untouched_branch_is_not_evaluated() {
        // the required error in the unused default must not fire
        assert_eq!(run("${A:-${B:?boom}}", &[("A", "v")]).unwrap(), "v");
    }

    #[test]
    fn invalid_formats() {
        for bad in ["$", "a $ b", "$1", "${}", "${1A}", "${A:}", "${A:x}", "${A!}", "$-"] {
            assert!(matches!(run(bad, &[]), Err(InterpolationError::Invalid)), "{bad} should be invalid");
        }
        assert!(matches!(run("${A", &[]), Err(InterpolationError::Unterminated)));
        assert!(matches!(run("${A:-${B}", &[]), Err(InterpolationError::Unterminated)));
    }
}
