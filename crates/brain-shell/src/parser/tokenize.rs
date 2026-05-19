//! REPL line → argv-style token list.
//!
//! Supports double- and single-quoted strings with `\\`, `\"`,
//! `\'`, `\n`, `\t`, `\r`, `\\` escape sequences inside double
//! quotes. Single-quoted strings are literal (no escapes), like
//! POSIX `sh`.

/// Failure modes for [`tokenize_line`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenizeError {
    /// A quoted string was opened but never closed.
    #[error("unterminated string starting at byte offset {0}")]
    UnterminatedString(usize),
    /// A trailing `\\` with nothing after it.
    #[error("trailing backslash at byte offset {0}")]
    TrailingBackslash(usize),
}

/// Split a REPL line into argv-style tokens.
///
/// # Examples
///
/// ```
/// use brain_shell::parser::tokenize::tokenize_line;
/// let toks = tokenize_line(r#"encode "hello world" --context 7"#).unwrap();
/// assert_eq!(toks, vec!["encode", "hello world", "--context", "7"]);
/// ```
pub fn tokenize_line(line: &str) -> Result<Vec<String>, TokenizeError> {
    let bytes = line.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        // Whitespace outside quotes ends the current token.
        if b == b' ' || b == b'\t' {
            if in_token {
                out.push(std::mem::take(&mut cur));
                in_token = false;
            }
            i += 1;
            continue;
        }

        if b == b'"' {
            // Double-quoted: process escape sequences.
            let start = i;
            i += 1;
            in_token = true;
            loop {
                if i >= bytes.len() {
                    return Err(TokenizeError::UnterminatedString(start));
                }
                let c = bytes[i];
                if c == b'"' {
                    i += 1;
                    break;
                }
                if c == b'\\' {
                    if i + 1 >= bytes.len() {
                        return Err(TokenizeError::TrailingBackslash(i));
                    }
                    let esc = bytes[i + 1];
                    match esc {
                        b'n' => cur.push('\n'),
                        b't' => cur.push('\t'),
                        b'r' => cur.push('\r'),
                        b'"' => cur.push('"'),
                        b'\'' => cur.push('\''),
                        b'\\' => cur.push('\\'),
                        other => {
                            // Unknown escape: keep literal `\X` so users
                            // can paste regexes without quoting hell.
                            cur.push('\\');
                            cur.push(other as char);
                        }
                    }
                    i += 2;
                    continue;
                }
                // Decode the next UTF-8 codepoint from the slice.
                let s = &line[i..];
                let ch = s.chars().next().expect("invariant: byte already inspected");
                cur.push(ch);
                i += ch.len_utf8();
            }
            continue;
        }

        if b == b'\'' {
            // Single-quoted: literal until the closing quote.
            let start = i;
            i += 1;
            in_token = true;
            loop {
                if i >= bytes.len() {
                    return Err(TokenizeError::UnterminatedString(start));
                }
                let c = bytes[i];
                if c == b'\'' {
                    i += 1;
                    break;
                }
                let s = &line[i..];
                let ch = s.chars().next().expect("invariant: byte already inspected");
                cur.push(ch);
                i += ch.len_utf8();
            }
            continue;
        }

        if b == b'\\' {
            // Bare-word backslash: escape the next char.
            if i + 1 >= bytes.len() {
                return Err(TokenizeError::TrailingBackslash(i));
            }
            in_token = true;
            let s = &line[i + 1..];
            let ch = s.chars().next().expect("invariant: byte already inspected");
            cur.push(ch);
            i += 1 + ch.len_utf8();
            continue;
        }

        // Bare word.
        in_token = true;
        let s = &line[i..];
        let ch = s.chars().next().expect("invariant: byte already inspected");
        cur.push(ch);
        i += ch.len_utf8();
    }

    if in_token {
        out.push(cur);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_words() {
        let t = tokenize_line("encode hello world").unwrap();
        assert_eq!(t, vec!["encode", "hello", "world"]);
    }

    #[test]
    fn double_quoted_preserves_spaces() {
        let t = tokenize_line(r#"encode "hello world" --context 7"#).unwrap();
        assert_eq!(t, vec!["encode", "hello world", "--context", "7"]);
    }

    #[test]
    fn single_quoted_is_literal() {
        let t = tokenize_line(r#"encode 'a \n b'"#).unwrap();
        assert_eq!(t, vec!["encode", r"a \n b"]);
    }

    #[test]
    fn double_quoted_escapes() {
        let t = tokenize_line(r#"encode "line1\nline2\t\"quoted\"""#).unwrap();
        assert_eq!(t, vec!["encode", "line1\nline2\t\"quoted\""]);
    }

    #[test]
    fn unterminated_double_quote() {
        let e = tokenize_line(r#"encode "abc"#).unwrap_err();
        assert!(matches!(e, TokenizeError::UnterminatedString(_)));
    }

    #[test]
    fn unterminated_single_quote() {
        let e = tokenize_line("encode 'abc").unwrap_err();
        assert!(matches!(e, TokenizeError::UnterminatedString(_)));
    }

    #[test]
    fn trailing_backslash() {
        let e = tokenize_line("encode abc\\").unwrap_err();
        assert!(matches!(e, TokenizeError::TrailingBackslash(_)));
    }

    #[test]
    fn empty_input_yields_empty_vec() {
        assert_eq!(tokenize_line("").unwrap(), Vec::<String>::new());
        assert_eq!(tokenize_line("   \t  ").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn bare_backslash_escapes_next_char() {
        let t = tokenize_line(r#"encode hello\ world"#).unwrap();
        assert_eq!(t, vec!["encode", "hello world"]);
    }

    #[test]
    fn utf8_bare_word() {
        let t = tokenize_line("encode café résumé").unwrap();
        assert_eq!(t, vec!["encode", "café", "résumé"]);
    }

    #[test]
    fn utf8_inside_quotes() {
        let t = tokenize_line(r#"encode "café — résumé""#).unwrap();
        assert_eq!(t, vec!["encode", "café — résumé"]);
    }
}
