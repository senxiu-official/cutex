use std::fmt::Write as _;

pub(super) struct Draft<'a> {
    pub masked: String,
    prefix: String,
    fragments: Vec<&'a str>,
}

impl<'a> Draft<'a> {
    pub fn new(text: &'a str) -> Self {
        let prefix = format!("CUTEX_KEEP_{}_", uuid::Uuid::new_v4().simple());
        let mut draft = Self {
            masked: String::new(),
            prefix,
            fragments: Vec::new(),
        };
        let mut rest = text;
        while !rest.is_empty() {
            let length = protected_len(rest);
            if length > 0 {
                let index = draft.fragments.len();
                draft.fragments.push(&rest[..length]);
                write!(&mut draft.masked, "{}{}__", draft.prefix, index).unwrap();
                rest = &rest[length..];
            } else {
                let c = rest.chars().next().unwrap();
                let run = rest
                    .as_bytes()
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .count();
                let width = run.max(c.len_utf8());
                draft.masked.push_str(&rest[..width]);
                rest = &rest[width..];
            }
        }
        draft
    }

    pub fn contains_marker(&self, text: &str) -> bool {
        text.contains(&self.prefix)
    }

    pub fn restore(&self, text: &str) -> Option<String> {
        let mut value = String::new();
        let mut rest = text;
        for (index, literal) in self.fragments.iter().enumerate() {
            let (left, right) = rest.split_once(&self.prefix)?;
            let marker = format!("{index}__");
            rest = right.strip_prefix(&marker)?;
            value.push_str(left);
            value.push_str(literal);
        }
        if self.contains_marker(rest) {
            return None;
        }
        value.push_str(rest);
        Some(value)
    }
}

fn protected_len(text: &str) -> usize {
    if text.starts_with('`') || text.starts_with("~~~") {
        let delimiter = if text.starts_with('~') { '~' } else { '`' };
        let width = text.bytes().take_while(|c| *c == delimiter as u8).count();
        let mut offset = width;
        while let Some(next) = text[offset..].find(delimiter) {
            offset += next;
            let n = text[offset..]
                .bytes()
                .take_while(|c| *c == delimiter as u8)
                .count();
            offset += n;
            if n == width || (width >= 3 && n >= width) {
                return offset;
            }
        }
        return text.len();
    }
    if text.starts_with("<@") {
        return text.find('>').map_or(0, |end| end + 1);
    }
    if text.starts_with("@[") {
        return text.find(']').map_or(0, |end| end + 1);
    }
    if let Some(link) = markdown_link_len(text) {
        return link;
    }
    if let Some(quote) = text.chars().next().filter(|c| matches!(c, '\'' | '"')) {
        if let Some(end) = text[1..].find(quote) {
            let reference = &text[1..end + 1];
            if reference.contains('/') || reference.contains('\\') {
                return end + 2;
            }
        }
    }
    if ["https://", "http://", "file://", "mailto:", "cutex:"]
        .iter()
        .any(|scheme| text.starts_with(scheme))
    {
        return text
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\''))
            .unwrap_or(text.len());
    }
    let first = text.as_bytes()[0];
    if !text.starts_with(|c: char| c.is_alphanumeric())
        && !matches!(first, b'@' | b'/' | b'~' | b'.' | b'\\')
    {
        return 0;
    }
    let end = text
        .find(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '<' | '>' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '。' | '，' | '；'
                )
        })
        .unwrap_or(text.len());
    let token = &text[..end];
    let url = token.starts_with("https://")
        || token.starts_with("http://")
        || token.starts_with("file://")
        || token.starts_with("mailto:");
    let path = token.starts_with('/')
        || token.starts_with("~/")
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("\\\\")
        || token.contains('/')
        || token.contains('\\');
    let mention = token
        .strip_prefix('@')
        .is_some_and(|value| value.starts_with(|c: char| c.is_alphanumeric() || c == '_'));
    if url || path || mention {
        end
    } else {
        0
    }
}

fn markdown_link_len(text: &str) -> Option<usize> {
    let label = text.strip_prefix('[')?;
    let mut escaped = false;
    for (i, c) in label.char_indices() {
        match c {
            '\n' | '\r' => return None,
            ']' if !escaped => {
                let destination = label[i + 1..].strip_prefix('(')?;
                let mut depth = 1;
                for (j, c) in destination.char_indices() {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(i + j + 4);
                            }
                        }
                        '\n' | '\r' => return None,
                        _ => {}
                    }
                }
                return None;
            }
            '\\' if !escaped => {
                escaped = true;
                continue;
            }
            _ => {}
        }
        escaped = false;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_code_links_paths_and_mentions_exactly() {
        let original = "用 `let x = 1;` 查看 D:\\repo\\x.rs 和 src/y.rs，链接 https://example.test/p 提醒 <@id>。\n~~~rs\nlet x = 2;\n~~~";
        let d = Draft::new(original);
        assert_eq!(d.fragments.len(), 6);
        assert!(d.masked.contains("用"));
        assert!(!d.masked.contains("example.test"));
        assert_eq!(d.restore(&d.masked).as_deref(), Some(original));
    }

    #[test]
    fn rejects_missing_duplicate_or_reordered_markers() {
        let d = Draft::new("use `x` and @agent");
        assert!(d.restore("missing").is_none());
        assert!(d
            .restore(&format!("{}1__{}0__", d.prefix, d.prefix))
            .is_none());
        assert!(d
            .restore(&format!("{} {}0__", d.masked, d.prefix))
            .is_none());
    }

    #[test]
    fn preserves_composer_mentions_by_their_complete_wire_form() {
        let mention = "[@A\\] B@host](cutex:agent/cutex.ag-example@host)";
        let input = format!("Ask {mention} and read [one](https://example.test/Page_(part)).");
        let draft = Draft::new(&input);
        assert_eq!(draft.fragments.len(), 2);
        assert_eq!(draft.fragments[0], mention);
        assert_eq!(
            draft.restore(&draft.masked).as_deref(),
            Some(input.as_str())
        );
    }
}
