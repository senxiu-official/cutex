//! Unified line diff of two text versions (Management v2
//! `/v2/artifact-store/{id}/diff`). Myers' O(ND) algorithm on the lines
//! between the common prefix and suffix; past [`MAX_EDIT_DISTANCE`] the
//! middle is shown as one replaced block. Output stops at
//! [`MAX_OUTPUT_BYTES`] (`truncated`).

pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_EDIT_DISTANCE: usize = 2000;
const CONTEXT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// `(diff, truncated)`.
pub fn unified(old: &str, new: &str, old_label: &str, new_label: &str) -> (String, bool) {
    let a: Vec<&str> = split_lines(old);
    let b: Vec<&str> = split_lines(new);
    let ops = line_ops(&a, &b);
    render(&a, &b, &ops, old_label, new_label)
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive('\n').collect()
}

/// Edit script as `(op, old index, new index)` entries.
fn line_ops(a: &[&str], b: &[&str]) -> Vec<(Op, usize, usize)> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<(Op, usize, usize)> = (0..prefix).map(|i| (Op::Equal, i, i)).collect();
    match myers(a_mid, b_mid) {
        Some(middle) => ops.extend(middle.into_iter().map(|(op, i, j)| (op, i + prefix, j + prefix))),
        None => {
            ops.extend((0..a_mid.len()).map(|i| (Op::Delete, i + prefix, prefix)));
            ops.extend((0..b_mid.len()).map(|j| (Op::Insert, a.len() - suffix, j + prefix)));
        }
    }
    let (a_end, b_end) = (a.len() - suffix, b.len() - suffix);
    ops.extend((0..suffix).map(|k| (Op::Equal, a_end + k, b_end + k)));
    ops
}

/// Myers' greedy forward algorithm with a trace for backtracking; `None`
/// past [`MAX_EDIT_DISTANCE`].
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<(Op, usize, usize)>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 && m == 0 {
        return Some(Vec::new());
    }
    let limit = ((n + m) as usize).min(MAX_EDIT_DISTANCE);
    let offset = limit as isize + 1;
    let mut v = vec![0isize; 2 * limit + 3];
    // trace[d] = v[k] for k in -d-1..=d+1 before step d (what backtracking
    // reads), so memory grows with the edit distance only.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=limit as isize {
        trace.push(v[(offset - d - 1) as usize..=(offset + d + 1) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let index = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[index - 1] < v[index + 1]) { v[index + 1] } else { v[index - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[index] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let found = found?;
    // Backtrack.
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=found).rev() {
        let saved = &trace[d as usize];
        let at = |k: isize| saved[(k + d + 1) as usize];
        let k = x - y;
        let previous_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let previous_x = at(previous_k);
        let previous_y = previous_x - previous_k;
        while x > previous_x && y > previous_y {
            x -= 1;
            y -= 1;
            ops.push((Op::Equal, x as usize, y as usize));
        }
        if x == previous_x {
            y -= 1;
            ops.push((Op::Insert, x as usize, y as usize));
        } else {
            x -= 1;
            ops.push((Op::Delete, x as usize, y as usize));
        }
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        ops.push((Op::Equal, x as usize, y as usize));
    }
    ops.reverse();
    Some(ops)
}

fn render(a: &[&str], b: &[&str], ops: &[(Op, usize, usize)], old_label: &str, new_label: &str) -> (String, bool) {
    let mut out = String::new();
    if ops.iter().all(|(op, _, _)| *op == Op::Equal) {
        return (out, false);
    }
    out.push_str(&format!("--- {old_label}\n+++ {new_label}\n"));
    let changes: Vec<usize> = ops.iter().enumerate().filter(|(_, (op, _, _))| *op != Op::Equal).map(|(index, _)| index).collect();
    let mut index = 0;
    while index < changes.len() {
        let start = changes[index].saturating_sub(CONTEXT);
        let mut end = changes[index];
        while index + 1 < changes.len() && changes[index + 1] <= end + 2 * CONTEXT + 1 {
            index += 1;
            end = changes[index];
        }
        let end = (end + CONTEXT + 1).min(ops.len());
        let hunk = &ops[start..end];
        let old_start = hunk.iter().find(|(op, _, _)| *op != Op::Insert).map(|(_, i, _)| *i).unwrap_or(hunk[0].1);
        let new_start = hunk.iter().find(|(op, _, _)| *op != Op::Delete).map(|(_, _, j)| *j).unwrap_or(hunk[0].2);
        let old_count = hunk.iter().filter(|(op, _, _)| *op != Op::Insert).count();
        let new_count = hunk.iter().filter(|(op, _, _)| *op != Op::Delete).count();
        let line_start = |start: usize, count: usize| if count == 0 { start } else { start + 1 };
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            line_start(old_start, old_count),
            old_count,
            line_start(new_start, new_count),
            new_count
        ));
        for (op, i, j) in hunk {
            let (marker, line) = match op {
                Op::Equal => (' ', a[*i]),
                Op::Delete => ('-', a[*i]),
                Op::Insert => ('+', b[*j]),
            };
            out.push(marker);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
            if out.len() > MAX_OUTPUT_BYTES {
                let mut cut = MAX_OUTPUT_BYTES;
                while !out.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.truncate(cut);
                return (out, true);
            }
        }
        index += 1;
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_edit_has_one_hunk_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\nI\n";
        let (diff, truncated) = unified(old, new, "v1", "v2");
        assert!(!truncated);
        assert_eq!(
            diff,
            "--- v1\n+++ v2\n@@ -1,8 +1,9 @@\n a\n b\n c\n-d\n+D\n e\n f\n g\n h\n+I\n"
        );
    }

    #[test]
    fn far_apart_edits_make_two_hunks_and_identical_is_empty() {
        let old: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 2\n", "line two\n").replace("line 27\n", "");
        let (diff, _) = unified(&old, &new, "a", "b");
        assert_eq!(diff.matches("@@ -").count(), 2, "{diff}");
        assert!(diff.contains("-line 2\n+line two\n"));
        assert!(diff.contains("@@ -25,6 +25,5 @@"), "{diff}");
        assert_eq!(unified(&old, &old, "a", "b"), (String::new(), false));
        let (from_empty, _) = unified("", "x\n", "a", "b");
        assert_eq!(from_empty, "--- a\n+++ b\n@@ -0,0 +1,1 @@\n+x\n");
        let (no_newline, _) = unified("x", "y", "a", "b");
        assert!(no_newline.contains("-x\n\\ No newline at end of file\n+y\n"));
    }

    #[test]
    fn large_rewrites_fall_back_and_output_is_capped() {
        let old: String = (0..(MAX_EDIT_DISTANCE + 10)).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..(MAX_EDIT_DISTANCE + 10)).map(|i| format!("new {i}\n")).collect();
        let (diff, truncated) = unified(&old, &new, "a", "b");
        assert!(!truncated);
        assert!(diff.starts_with("--- a\n+++ b\n@@ -1,"));
        let huge_old: String = (0..60_000).map(|i| format!("{i:>30}\n")).collect();
        let huge_new: String = (0..60_000).map(|i| format!("{i:>31}\n")).collect();
        let (diff, truncated) = unified(&huge_old, &huge_new, "a", "b");
        assert!(truncated);
        assert!(diff.len() <= MAX_OUTPUT_BYTES);
    }
}
