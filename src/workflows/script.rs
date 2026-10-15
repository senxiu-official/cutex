//! Static check of a workflow script before approval (design §2.2).
//!
//! A script is plain JavaScript in the Claude Code dialect: it begins with
//! `export const meta = {...}` (a pure literal with `name` and `description`),
//! then a body that may use top-level `await` and `return`. The check:
//! - finds the `meta` literal, refuses anything that is not a pure literal
//!   (calls, spreads, template interpolation, variables) and reads it as JSON;
//! - compiles the body once inside QuickJS without running it (syntax errors);
//! - warns when a `meta.phases` title never appears as a literal in the body;
//! - collects literal `profile:` / `model:` options for the approval card.
//!
//! Diagnostics read `{path}:L{line}:C{col} {message}` so an agent can edit its
//! draft and resubmit.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Upper bound for a script (inline or draft).
pub const MAX_SCRIPT_BYTES: usize = 256 * 1024;
/// The body is wrapped in an async function so `await` / `return` work at top
/// level. The prefix shares line 1 with the script so line numbers stay exact.
pub const BODY_PREFIX: &str = "(async function(){";
pub const BODY_SUFFIX: &str = "\n})";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseMeta {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptMeta {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub phases: Vec<PhaseMeta>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub line: u32,
    pub col: u32,
    pub message: String,
}

impl Diagnostic {
    pub fn render(&self, path: &str) -> String {
        format!("{path}:L{}:C{} {}", self.line, self.col, self.message)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheckedScript {
    pub meta: ScriptMeta,
    /// The wrapped body handed to the exec child (`export` blanked out).
    pub body: String,
    /// Literal `profile: "…"` values in agent() options.
    pub profiles: Vec<String>,
    /// Literal `model: "…"` values in agent() options and meta phases.
    pub models: Vec<String>,
    pub warnings: Vec<Diagnostic>,
}

/// Byte offset → 1-based (line, column in chars).
fn position(source: &str, offset: usize) -> (u32, u32) {
    let before = &source[..offset.min(source.len())];
    let line = before.matches('\n').count() as u32 + 1;
    let col = before.rsplit('\n').next().map(|s| s.chars().count()).unwrap_or(0) as u32 + 1;
    (line, col)
}

fn diag(source: &str, offset: usize, message: impl Into<String>) -> Diagnostic {
    let (line, col) = position(source, offset);
    Diagnostic { line, col, message: message.into() }
}

#[derive(Debug, PartialEq)]
enum Tok<'a> {
    Punct(char),
    Ident(&'a str),
    Str(String),
    Template { interpolated: bool, text: String },
    Number(&'a str),
    Spread,
    Other(char),
}

/// A tiny JS tokenizer, good enough for a literal and for skipping comments.
struct Lexer<'a> {
    src: &'a str,
    at: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, at: usize) -> Self {
        Self { src, at }
    }

    fn skip_trivia(&mut self) -> Result<(), usize> {
        let bytes = self.src.as_bytes();
        loop {
            while self.at < bytes.len() && (bytes[self.at] as char).is_whitespace() {
                self.at += 1;
            }
            if self.src[self.at..].starts_with("//") {
                match self.src[self.at..].find('\n') {
                    Some(n) => self.at += n + 1,
                    None => self.at = bytes.len(),
                }
            } else if self.src[self.at..].starts_with("/*") {
                match self.src[self.at + 2..].find("*/") {
                    Some(n) => self.at += n + 4,
                    None => return Err(self.at),
                }
            } else if self.src[self.at..].starts_with('\u{feff}') {
                self.at += '\u{feff}'.len_utf8();
            } else {
                return Ok(());
            }
        }
    }

    /// Next token and its start offset; `Ok(None)` at the end.
    fn next(&mut self) -> Result<Option<(usize, Tok<'a>)>, (usize, &'static str)> {
        self.skip_trivia().map_err(|at| (at, "unterminated comment"))?;
        let start = self.at;
        let rest = &self.src[self.at..];
        let Some(c) = rest.chars().next() else { return Ok(None) };
        if rest.starts_with("...") {
            self.at += 3;
            return Ok(Some((start, Tok::Spread)));
        }
        if c == '\'' || c == '"' {
            let mut out = String::new();
            let mut chars = rest.char_indices().skip(1);
            while let Some((i, ch)) = chars.next() {
                if ch == c {
                    self.at += i + 1;
                    return Ok(Some((start, Tok::Str(out))));
                }
                if ch == '\n' {
                    break;
                }
                if ch == '\\' {
                    match chars.next() {
                        Some((_, 'n')) => out.push('\n'),
                        Some((_, 't')) => out.push('\t'),
                        Some((_, other)) => out.push(other),
                        None => break,
                    }
                } else {
                    out.push(ch);
                }
            }
            return Err((start, "unterminated string"));
        }
        if c == '`' {
            let mut out = String::new();
            let mut interpolated = false;
            let mut chars = rest.char_indices().skip(1).peekable();
            while let Some((i, ch)) = chars.next() {
                if ch == '`' {
                    self.at += i + 1;
                    return Ok(Some((start, Tok::Template { interpolated, text: out })));
                }
                if ch == '\\' {
                    if let Some((_, other)) = chars.next() {
                        out.push(other);
                    }
                    continue;
                }
                if ch == '$' && chars.peek().map(|(_, n)| *n) == Some('{') {
                    interpolated = true;
                }
                out.push(ch);
            }
            return Err((start, "unterminated template"));
        }
        if c.is_ascii_digit() || (c == '.' && rest[1..].starts_with(|d: char| d.is_ascii_digit())) {
            let len = rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_'))
                .unwrap_or(rest.len());
            self.at += len;
            return Ok(Some((start, Tok::Number(&rest[..len]))));
        }
        if c.is_alphabetic() || c == '_' || c == '$' {
            let len = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '$'))
                .unwrap_or(rest.len());
            self.at += len;
            return Ok(Some((start, Tok::Ident(&rest[..len]))));
        }
        self.at += c.len_utf8();
        if "{}[],:".contains(c) {
            Ok(Some((start, Tok::Punct(c))))
        } else {
            Ok(Some((start, Tok::Other(c))))
        }
    }
}

/// Where `export const meta = {` … `}` is: (export start, literal start, literal end).
fn locate_meta(source: &str) -> Result<(usize, usize, usize), Diagnostic> {
    let mut lexer = Lexer::new(source, 0);
    let expect = |lexer: &mut Lexer, want: &str| -> Result<usize, Diagnostic> {
        match lexer.next() {
            Ok(Some((at, Tok::Ident(word)))) if word == want => Ok(at),
            Ok(Some((at, Tok::Other('=')))) if want == "=" => Ok(at),
            Ok(Some((at, _))) => Err(diag(source, at, "a workflow script must begin with `export const meta = {...}`")),
            Ok(None) => Err(diag(source, source.len(), "a workflow script must begin with `export const meta = {...}`")),
            Err((at, message)) => Err(diag(source, at, message)),
        }
    };
    let export_at = expect(&mut lexer, "export")?;
    expect(&mut lexer, "const")?;
    expect(&mut lexer, "meta")?;
    expect(&mut lexer, "=")?;
    let open = match lexer.next() {
        Ok(Some((at, Tok::Punct('{')))) => at,
        Ok(Some((at, _))) => return Err(diag(source, at, "meta must be an object literal")),
        Ok(None) => return Err(diag(source, source.len(), "meta must be an object literal")),
        Err((at, message)) => return Err(diag(source, at, message)),
    };
    // Walk the literal: only keys, strings, numbers, true/false/null, nesting.
    let mut depth = 1usize;
    let mut previous: Option<Tok> = Some(Tok::Punct('{'));
    loop {
        let (at, tok) = match lexer.next() {
            Ok(Some(found)) => found,
            Ok(None) => return Err(diag(source, open, "meta literal is not closed")),
            Err((at, message)) => return Err(diag(source, at, message)),
        };
        let impure = |what: &str| diag(source, at, format!("meta must be a pure literal ({what} is not allowed)"));
        match &tok {
            Tok::Punct('{') | Tok::Punct('[') => depth += 1,
            Tok::Punct('}') | Tok::Punct(']') => {
                depth -= 1;
                if depth == 0 {
                    return Ok((export_at, open, at + 1));
                }
            }
            Tok::Punct(_) | Tok::Str(_) | Tok::Number(_) => {}
            Tok::Template { interpolated: true, .. } => return Err(impure("template interpolation")),
            Tok::Template { .. } => {}
            Tok::Spread => return Err(impure("a spread")),
            Tok::Other('-') | Tok::Other('+') => {}
            Tok::Other('(') => return Err(impure("a call or expression")),
            Tok::Other(c) => return Err(impure(&format!("`{c}`"))),
            Tok::Ident(word) => {
                let key_position = matches!(previous, Some(Tok::Punct('{')) | Some(Tok::Punct(',')));
                let value_word = matches!(*word, "true" | "false" | "null");
                if !key_position && !value_word {
                    return Err(impure(&format!("the name `{word}`")));
                }
                if key_position {
                    // A shorthand `{name}` is a variable reference.
                    let mut peek = Lexer::new(source, lexer.at);
                    if !matches!(peek.next(), Ok(Some((_, Tok::Punct(':'))))) {
                        return Err(impure(&format!("the shorthand `{word}`")));
                    }
                }
            }
        }
        previous = Some(tok);
    }
}

fn string_field(meta: &Value, key: &str) -> Option<String> {
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

fn parse_meta(source: &str, open: usize, value: Value) -> Result<ScriptMeta, Diagnostic> {
    let bad = |message: &str| diag(source, open, message.to_string());
    if !value.is_object() {
        return Err(bad("meta must be an object"));
    }
    let name = string_field(&value, "name").filter(|s| !s.trim().is_empty()).ok_or_else(|| bad("meta.name is required (a non-empty string)"))?;
    if name.chars().count() > 120 {
        return Err(bad("meta.name is longer than 120 characters"));
    }
    let description = string_field(&value, "description")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| bad("meta.description is required (a non-empty string)"))?;
    let mut phases = Vec::new();
    match value.get("phases") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items {
                let title = string_field(item, "title").filter(|s| !s.trim().is_empty()).ok_or_else(|| bad("every meta.phases entry needs a title"))?;
                phases.push(PhaseMeta { title, detail: string_field(item, "detail"), model: string_field(item, "model") });
            }
        }
        Some(_) => return Err(bad("meta.phases must be an array")),
    }
    Ok(ScriptMeta { name, description, when_to_use: string_field(&value, "whenToUse"), phases })
}

/// Literal `key: "value"` occurrences in the body (approval card facts).
fn literal_options(body: &str, key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut lexer = Lexer::new(body, 0);
    let mut window: Vec<Tok> = Vec::new();
    while let Ok(Some((_, tok))) = lexer.next() {
        window.push(tok);
        if window.len() > 3 {
            window.remove(0);
        }
        if let [Tok::Ident(k), Tok::Punct(':'), Tok::Str(value)] = window.as_slice() {
            if *k == key && !out.contains(value) {
                out.push(value.clone());
            }
        }
    }
    out
}

fn body_mentions(body: &str, title: &str) -> bool {
    let mut lexer = Lexer::new(body, 0);
    while let Ok(Some((_, tok))) = lexer.next() {
        match tok {
            Tok::Str(text) | Tok::Template { text, .. } if text == title => return true,
            _ => {}
        }
    }
    false
}

/// Parse `file:line:col` out of a QuickJS stack / message.
fn quickjs_position(text: &str, file: &str) -> Option<(u32, u32)> {
    let at = text.find(&format!("{file}:"))?;
    let mut parts = text[at + file.len() + 1..].split(|c: char| !c.is_ascii_digit());
    let line = parts.next()?.parse().ok()?;
    let col = parts.next()?.parse().ok()?;
    Some((line, col))
}

/// Run the static check. `path` labels diagnostics (draft path or `<inline>`).
pub fn check(source: &str) -> Result<CheckedScript, Vec<Diagnostic>> {
    if source.len() > MAX_SCRIPT_BYTES {
        return Err(vec![Diagnostic { line: 1, col: 1, message: format!("script is larger than {} KiB", MAX_SCRIPT_BYTES / 1024) }]);
    }
    let (export_at, open, close) = locate_meta(source).map_err(|d| vec![d])?;
    let literal = &source[open..close];
    let meta_value = super::exec::eval_literal(literal).map_err(|message| vec![diag(source, open, format!("meta: {message}"))])?;
    let meta = parse_meta(source, open, meta_value).map_err(|d| vec![d])?;
    // Blank `export` (same length keeps columns); the body keeps `const meta`.
    let mut body_source = String::with_capacity(source.len());
    body_source.push_str(&source[..export_at]);
    body_source.push_str("      ");
    body_source.push_str(&source[export_at + "export".len()..]);
    let body = format!("{BODY_PREFIX}{body_source}{BODY_SUFFIX}");
    if let Err(message) = super::exec::compile_only(&body, super::exec::SCRIPT_FILE) {
        let (line, mut col) = quickjs_position(&message, super::exec::SCRIPT_FILE).unwrap_or((1, 1));
        if line == 1 {
            col = col.saturating_sub(BODY_PREFIX.chars().count() as u32).max(1);
        }
        let first = message.lines().next().unwrap_or("syntax error").trim_start_matches("SyntaxError: ").to_string();
        return Err(vec![Diagnostic { line, col, message: first }]);
    }
    let after_meta = &source[close..];
    let mut warnings = Vec::new();
    for phase in &meta.phases {
        if !body_mentions(after_meta, &phase.title) {
            warnings.push(diag(source, open, format!("meta phase \"{}\" never appears in a phase() call or {{phase:}} option", phase.title)));
        }
    }
    let profiles = literal_options(after_meta, "profile");
    let mut models = literal_options(after_meta, "model");
    for phase in &meta.phases {
        if let Some(model) = &phase.model {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
    }
    Ok(CheckedScript { meta, body, profiles, models, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "// leading comment\nexport const meta = {\n  name: 'review',\n  description: \"Review things\",\n  phases: [{ title: 'Find', detail: 'look' }, { title: 'Judge' }],\n}\nphase('Find')\nconst r = await agent('find bugs', { profile: 'kimi', model: 'k2' })\nreturn r\n";

    #[test]
    fn good_script_passes_with_meta_and_literals() {
        let checked = check(GOOD).unwrap();
        assert_eq!(checked.meta.name, "review");
        assert_eq!(checked.meta.phases.len(), 2);
        assert_eq!(checked.profiles, vec!["kimi".to_string()]);
        assert_eq!(checked.models, vec!["k2".to_string()]);
        assert_eq!(checked.warnings.len(), 1, "{:?}", checked.warnings);
        assert!(checked.warnings[0].message.contains("Judge"));
        assert!(checked.body.starts_with(BODY_PREFIX));
        assert!(!checked.body.contains("export const"));
    }

    #[test]
    fn meta_must_come_first_and_be_pure() {
        let missing = check("const x = 1\n").unwrap_err();
        assert!(missing[0].message.contains("must begin with"), "{missing:?}");
        let call = check("export const meta = { name: String(1), description: 'd' }\n").unwrap_err();
        assert!(call[0].message.contains("pure literal"), "{call:?}");
        let spread = check("export const meta = { ...base, description: 'd' }\n").unwrap_err();
        assert!(spread[0].message.contains("spread"), "{spread:?}");
        let template = check("export const meta = { name: `a${b}`, description: 'd' }\n").unwrap_err();
        assert!(template[0].message.contains("template"), "{template:?}");
        let shorthand = check("export const meta = { name, description: 'd' }\n").unwrap_err();
        assert!(shorthand[0].message.contains("shorthand"), "{shorthand:?}");
        let no_desc = check("export const meta = { name: 'a' }\n").unwrap_err();
        assert!(no_desc[0].message.contains("description"), "{no_desc:?}");
    }

    #[test]
    fn syntax_errors_carry_line_and_column() {
        let errors = check("export const meta = { name: 'a', description: 'd' }\nconst x = 1\nlet y = ;\n").unwrap_err();
        assert_eq!(errors[0].line, 3, "{errors:?}");
        assert!(errors[0].render("draft.js").starts_with("draft.js:L3:C"), "{}", errors[0].render("draft.js"));
        // TypeScript annotations are not JavaScript.
        let ts = check("export const meta = { name: 'a', description: 'd' }\nconst xs: string[] = []\n").unwrap_err();
        assert_eq!(ts[0].line, 2, "{ts:?}");
    }
}
