//! Path globs for auto capture patterns and `.cutexignore` (a `.gitignore`
//! subset). Paths are relative, `/`-separated; matching ignores case on
//! Windows.
//!
//! Glob syntax: `*` (any characters within one segment), `?` (one
//! character), `**` (a whole segment: any number of segments, also none).

/// A compiled glob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob {
    segments: Vec<Segment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    AnyDepth,
    Pattern(Vec<char>),
}

fn fold(value: &str) -> String {
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value.to_string()
    }
}

impl Glob {
    pub fn new(pattern: &str) -> Self {
        let normalized = fold(&pattern.trim().replace('\\', "/"));
        let mut segments = Vec::new();
        for part in normalized.split('/').filter(|part| !part.is_empty() && *part != ".") {
            if part == "**" {
                if segments.last() != Some(&Segment::AnyDepth) {
                    segments.push(Segment::AnyDepth);
                }
            } else {
                segments.push(Segment::Pattern(part.chars().collect()));
            }
        }
        Self { segments }
    }

    /// The leading segments without wildcards (the directory a walk can
    /// start in), `/`-joined; empty = the root.
    pub fn literal_base(&self) -> String {
        let mut base = Vec::new();
        for segment in &self.segments {
            match segment {
                Segment::Pattern(chars) if !chars.iter().any(|ch| matches!(ch, '*' | '?')) => {
                    base.push(chars.iter().collect::<String>())
                }
                _ => break,
            }
        }
        // A fully literal pattern names a file; walk its parent.
        if base.len() == self.segments.len() {
            base.pop();
        }
        base.join("/")
    }

    pub fn matches(&self, path: &str) -> bool {
        let path = fold(&path.replace('\\', "/"));
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        match_segments(&self.segments, &parts)
    }
}

fn match_segments(segments: &[Segment], parts: &[&str]) -> bool {
    match segments.split_first() {
        None => parts.is_empty(),
        Some((Segment::AnyDepth, rest)) => (0..=parts.len()).any(|skip| match_segments(rest, &parts[skip..])),
        Some((Segment::Pattern(pattern), rest)) => match parts.split_first() {
            Some((part, tail)) => {
                let chars: Vec<char> = part.chars().collect();
                match_segment(pattern, &chars) && match_segments(rest, tail)
            }
            None => false,
        },
    }
}

/// `*` / `?` within one segment (iterative with backtracking to the last
/// star).
fn match_segment(pattern: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|ch| *ch == '*')
}

/// Any of several globs.
#[derive(Clone, Debug, Default)]
pub struct GlobSet {
    globs: Vec<Glob>,
}

impl GlobSet {
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Self {
        Self { globs: patterns.iter().map(|pattern| Glob::new(pattern.as_ref())).collect() }
    }

    pub fn matches(&self, path: &str) -> bool {
        self.globs.iter().any(|glob| glob.matches(path))
    }

    /// The directories a walk must visit (deduplicated, outermost first;
    /// a base inside another base is dropped).
    pub fn walk_bases(&self) -> Vec<String> {
        let mut bases: Vec<String> = self.globs.iter().map(Glob::literal_base).collect();
        bases.sort_by_key(|base| base.len());
        let mut kept: Vec<String> = Vec::new();
        for base in bases {
            let covered = kept.iter().any(|outer| outer.is_empty() || base == *outer || base.starts_with(&format!("{outer}/")));
            if !covered {
                kept.push(base);
            }
        }
        kept
    }
}

/// `.cutexignore` rules (the `.gitignore` subset: `#` comments, `!`
/// negation, a trailing `/` for directories only, a `/` inside the pattern
/// anchors it to the root; otherwise it matches a name at any depth).
#[derive(Clone, Debug, Default)]
pub struct IgnoreRules {
    rules: Vec<Rule>,
}

#[derive(Clone, Debug)]
struct Rule {
    glob: Glob,
    negated: bool,
    directory_only: bool,
}

impl IgnoreRules {
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            let line = line.trim_end();
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let (negated, body) = match line.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, line.strip_prefix('\\').unwrap_or(line)),
            };
            let body = body.trim_start();
            let (directory_only, body) = match body.strip_suffix('/') {
                Some(rest) => (true, rest),
                None => (false, body),
            };
            if body.is_empty() {
                continue;
            }
            let anchored = body.contains('/');
            let pattern = if anchored { body.trim_start_matches('/').to_string() } else { format!("**/{body}") };
            rules.push(Rule { glob: Glob::new(&pattern), negated, directory_only });
        }
        Self { rules }
    }

    /// Load `<root>/.cutexignore` (missing or unreadable = no rules).
    pub fn load(root: &std::path::Path) -> Self {
        match std::fs::read(root.join(".cutexignore")) {
            Ok(bytes) if bytes.len() <= 256 * 1024 => Self::parse(&String::from_utf8_lossy(&bytes)),
            _ => Self::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The last matching rule decides for this exact path.
    fn decide(&self, path: &str, is_dir: bool) -> Option<bool> {
        let mut decision = None;
        for rule in &self.rules {
            if rule.directory_only && !is_dir {
                continue;
            }
            if rule.glob.matches(path) {
                decision = Some(!rule.negated);
            }
        }
        decision
    }

    /// Whether a path is ignored; an ignored directory ignores everything
    /// beneath it (as in git, a file cannot be re-included below it).
    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let path = path.replace('\\', "/");
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        for end in 1..parts.len() {
            if self.decide(&parts[..end].join("/"), true) == Some(true) {
                return true;
            }
        }
        self.decide(&parts.join("/"), is_dir) == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_segments_and_depth() {
        let reports = Glob::new("reports/**");
        assert!(reports.matches("reports/a.md"));
        assert!(reports.matches("reports/x/y/z.png"));
        assert!(!reports.matches("reportsx/a.md"));
        assert!(!reports.matches("other/reports/a.md"));
        assert_eq!(reports.literal_base(), "reports");
        let md = Glob::new("**/*.md");
        assert!(md.matches("a.md"));
        assert!(md.matches("x/y/a.md"));
        assert!(!md.matches("x/y/a.mdx"));
        assert_eq!(md.literal_base(), "");
        let mid = Glob::new("results/*/summary-??.csv");
        assert!(mid.matches("results/run1/summary-01.csv"));
        assert!(!mid.matches("results/run1/deep/summary-01.csv"));
        assert!(!mid.matches("results/run1/summary-1.csv"));
        assert_eq!(mid.literal_base(), "results");
        let literal = Glob::new("handoff/notes.md");
        assert!(literal.matches("handoff/notes.md"));
        assert_eq!(literal.literal_base(), "handoff");
        assert!(Glob::new("a/**/b").matches("a/b"));
        assert!(Glob::new("a/**/b").matches("a/x/y/b"));
        assert!(Glob::new("*a*b*").matches("xxaxxbxx"));
        assert!(!Glob::new("*a*b").matches("xxaxxbxx"));
        assert!(Glob::new("out\\**").matches("out/a"));
        if cfg!(windows) {
            assert!(reports.matches("Reports/A.md"));
        }
        let set = GlobSet::new(&["reports/**", "reports/sub/**", "figures/**", "**/*.pdf"]);
        assert_eq!(set.walk_bases(), vec![String::new()]);
        let set = GlobSet::new(&["reports/**", "reports/sub/**", "figures/**"]);
        assert_eq!(set.walk_bases(), vec!["reports".to_string(), "figures".to_string()]);
        assert!(set.matches("figures/f.png"));
        assert!(!set.matches("src/main.rs"));
    }

    #[test]
    fn cutexignore_follows_the_gitignore_subset() {
        let rules = IgnoreRules::parse("# comment\n*.tmp\nscratch/\n/reports/private/**\n!keep.tmp\nlogs\n\\#hash\n");
        assert!(rules.is_ignored("reports/a.tmp", false));
        assert!(!rules.is_ignored("reports/keep.tmp", false), "negated");
        assert!(rules.is_ignored("reports/scratch/a.md", false), "directory rule ignores beneath");
        assert!(!rules.is_ignored("reports/scratch", false), "directory-only rule skips files");
        assert!(rules.is_ignored("reports/private/x.md", false), "anchored");
        assert!(!rules.is_ignored("other/reports/private/x.md", false), "anchored to the root");
        assert!(rules.is_ignored("results/logs", false));
        assert!(rules.is_ignored("results/logs/x.csv", false));
        assert!(rules.is_ignored("figures/#hash", false));
        assert!(!rules.is_ignored("reports/a.md", false));
        // A file cannot be re-included below an ignored directory.
        let rules = IgnoreRules::parse("out/\n!out/keep.md\n");
        assert!(rules.is_ignored("out/keep.md", false));
        assert!(IgnoreRules::parse("").is_empty());
    }
}
