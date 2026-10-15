//! Bounded, conservative reverse unified patches. Never runs a patch program.

#[derive(Debug, PartialEq, Eq)]
pub struct Reversed {
    pub text: String,
    pub already: bool,
    pub added: usize,
    pub deleted: usize,
}

#[derive(Default, Debug)]
struct Hunk {
    line: Option<usize>,
    old: Vec<String>,
    new: Vec<String>,
    old_no_newline: bool,
    new_no_newline: bool,
    expected: Option<(usize, usize)>,
    old_line: Option<usize>,
    added: usize,
    deleted: usize,
}

fn lines(text: &str) -> Vec<String> {
    let mut result: Vec<_> = text
        .split('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s).to_owned())
        .collect();
    if text.ends_with('\n') || text.is_empty() {
        result.pop();
    }
    result
}

pub fn equivalent(a: &str, b: &str) -> bool {
    lines(a) == lines(b) && a.ends_with('\n') == b.ends_with('\n')
}

fn range(text: &str, prefix: char) -> Result<(usize, usize), String> {
    let text = text.strip_prefix(prefix).ok_or("invalid hunk range")?;
    let (line, count) = text.split_once(',').unwrap_or((text, "1"));
    Ok((
        line.parse().map_err(|_| "invalid hunk line")?,
        count.parse().map_err(|_| "invalid hunk count")?,
    ))
}

fn parse(diff: &str) -> Result<Vec<Hunk>, String> {
    let mut hunks = Vec::new();
    let mut hunk: Option<Hunk> = None;
    let mut last = ' ';
    let has_hunks = diff.lines().any(|line| line.starts_with("@@ "));
    for raw in diff.split_terminator('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.starts_with("@@ ") {
            if let Some(h) = hunk.take() {
                hunks.push(h);
            }
            let mut fields = raw.split_whitespace();
            fields.next();
            let (old_line, old_count) = range(fields.next().ok_or("invalid hunk")?, '-')?;
            let (new_line, new_count) = range(fields.next().ok_or("invalid hunk")?, '+')?;
            if fields.next() != Some("@@") {
                return Err("invalid hunk header".into());
            }
            hunk = Some(Hunk {
                line: Some(if new_count == 0 {
                    new_line
                } else {
                    new_line.saturating_sub(1)
                }),
                old_line: Some(if old_count == 0 {
                    old_line
                } else {
                    old_line.saturating_sub(1)
                }),
                expected: Some((old_count, new_count)),
                ..Default::default()
            });
            continue;
        }
        if has_hunks
            && hunk.is_none()
            && (raw.starts_with("--- ")
                || raw.starts_with("+++ ")
                || raw.starts_with("diff ")
                || raw.starts_with("index "))
        {
            continue;
        }
        if raw == "\\ No newline at end of file" {
            let h = hunk.as_mut().ok_or("newline marker without hunk")?;
            if last != '+' {
                h.old_no_newline = true;
            }
            if last != '-' {
                h.new_no_newline = true;
            }
            continue;
        }
        let prefix = raw.chars().next().ok_or("unprefixed empty diff line")?;
        if !matches!(prefix, ' ' | '+' | '-') {
            return Err("unsupported diff line".into());
        }
        let h = hunk.get_or_insert_with(Hunk::default);
        let text = raw[1..].to_owned();
        if prefix != '+' {
            h.old.push(text.clone());
        }
        if prefix != '-' {
            h.new.push(text);
        }
        if prefix == '-' {
            h.added += 1;
        }
        if prefix == '+' {
            h.deleted += 1;
        }
        last = prefix;
    }
    if let Some(h) = hunk {
        hunks.push(h);
    }
    if hunks.is_empty() {
        return Err("diff contains no hunks".into());
    }
    for h in &hunks {
        if let Some((old, new)) = h.expected {
            if (h.old.len(), h.new.len()) != (old, new) {
                return Err("incomplete hunk".into());
            }
        }
    }
    Ok(hunks)
}

fn locate(
    content: &[String],
    needle: &[String],
    expected: Option<usize>,
) -> Result<Option<usize>, String> {
    if let Some(pos) = expected {
        if pos <= content.len()
            && content.get(pos..pos.saturating_add(needle.len())) == Some(needle)
        {
            return Ok(Some(pos));
        }
    }
    if needle.is_empty() {
        return Err("empty patch side requires an exact hunk position".into());
    }
    let mut matches = content
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle);
    let first = matches.next().map(|(i, _)| i);
    if matches.next().is_some() {
        return Err("patch context is ambiguous".into());
    }
    Ok(first)
}

pub fn reverse(current: &str, diff: &str) -> Result<Reversed, String> {
    if diff.is_empty() {
        return Ok(Reversed {
            text: current.into(),
            already: true,
            added: 0,
            deleted: 0,
        });
    }
    apply(current, parse(diff)?)
}

/// Used only to prove a whole change chain is already in its pre-turn state.
pub fn forward(current: &str, diff: &str) -> Result<Reversed, String> {
    if diff.is_empty() {
        return reverse(current, diff);
    }
    let mut hunks = parse(diff)?;
    for h in &mut hunks {
        std::mem::swap(&mut h.old, &mut h.new);
        std::mem::swap(&mut h.old_no_newline, &mut h.new_no_newline);
        std::mem::swap(&mut h.added, &mut h.deleted);
        h.line = h.old_line;
    }
    apply(current, hunks)
}

fn apply(current: &str, hunks: Vec<Hunk>) -> Result<Reversed, String> {
    let mut content = lines(current);
    let mut trailing = current.ends_with('\n');
    let crlf = current
        .as_bytes()
        .windows(2)
        .filter(|w| *w == b"\r\n")
        .count();
    let lf = current.bytes().filter(|b| *b == b'\n').count();
    let eol = if crlf > lf.saturating_sub(crlf) {
        "\r\n"
    } else {
        "\n"
    };
    let mut changed = false;
    let mut added = 0;
    let mut deleted = 0;
    for h in hunks.iter().rev() {
        if h.new.is_empty() && !h.old.is_empty() && locate(&content, &h.old, h.line)?.is_some() {
            continue;
        }
        let found = locate(&content, &h.new, h.line)?;
        let Some(pos) = found else {
            if h.old.is_empty() && content.is_empty() {
                continue;
            }
            if !h.old.is_empty() && locate(&content, &h.old, None)?.is_some() {
                continue;
            }
            return Err("current content does not match either patch side".into());
        };
        if pos + h.new.len() == content.len() && (h.old_no_newline || h.new_no_newline) {
            trailing = !h.old_no_newline;
        }
        added += h.added;
        deleted += h.deleted;
        content.splice(pos..pos + h.new.len(), h.old.clone());
        changed |= h.old != h.new || h.old_no_newline != h.new_no_newline;
    }
    let mut text = content.join(eol);
    if trailing && !content.is_empty() {
        text.push_str(eol);
    }
    Ok(Reversed {
        text: if changed { text } else { current.into() },
        already: !changed,
        added,
        deleted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_hunks_offsets_crlf_and_fallback() {
        for diff in [
            "@@ -1 +1 @@\n-old\n+new\n",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n",
            "-old\n+new",
        ] {
            assert_eq!(
                reverse("prefix\r\nnew\r\n", diff).unwrap().text,
                "prefix\r\nold\r\n"
            );
            assert!(reverse("old\n", diff).unwrap().already);
        }
        assert!(reverse("new\nnew\n", "-old\n+new").is_err());
        assert!(reverse("new", "@@ -1,2 +1 @@\n-old\n+new").is_err());
        assert_eq!(
            reverse("++ new\n", "--- old\n+++ new\n").unwrap().text,
            "-- old\n"
        );
    }

    #[test]
    fn newline_marker_and_empty_side() {
        assert_eq!(
            reverse(
                "new\n",
                "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n"
            )
            .unwrap()
            .text,
            "old"
        );
        assert_eq!(
            reverse(
                "new",
                "@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n"
            )
            .unwrap()
            .text,
            "old\n"
        );
        assert_eq!(
            reverse("one\n", "@@ -1,2 +1 @@\n one\n-two\n")
                .unwrap()
                .text,
            "one\ntwo\n"
        );
        assert!(reverse("", "-old").is_err());
    }
}
