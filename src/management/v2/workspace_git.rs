//! Workspace git routes (shelling out to `git`): status and unified diff,
//! plus the ignore/file-list helpers used by the file routes. Paths in
//! responses are relative to the workspace root, which may be a
//! subdirectory of the repository.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;

use serde_json::json;
use serde_json::Value;

use super::workspace::ResolvedRoot;
use super::workspace::WorkspaceError;
use super::workspace::WorkspaceResult;

const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;
const UNTRACKED_COUNT_LIMIT: u64 = 1024 * 1024;

fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    command
}

/// Run git in `dir`; `input` is written to stdin when present.
pub(super) fn run_git(dir: &Path, args: &[&str], input: Option<&[u8]>) -> std::io::Result<Output> {
    let mut command = git_command(dir);
    command.args(args);
    if input.is_none() {
        return command.output();
    }
    command.stdin(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = input.unwrap_or_default().to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output();
    let _ = writer.join();
    output
}

/// Names (entries of `dir`) that `.gitignore` rules ignore. Empty when `dir`
/// is not inside a repository or git is unavailable.
pub fn check_ignored(dir: &Path, names: &[&str]) -> HashSet<String> {
    if names.is_empty() {
        return HashSet::new();
    }
    let mut input = Vec::new();
    for name in names {
        input.extend_from_slice(name.as_bytes());
        input.push(0);
    }
    match run_git(dir, &["check-ignore", "-z", "--stdin"], Some(&input)) {
        Ok(output) if output.status.code() == Some(0) => split_nul(&output.stdout)
            .into_iter()
            .map(|path| path.trim_end_matches('/').to_string())
            .collect(),
        _ => HashSet::new(),
    }
}

/// Tracked + untracked-not-ignored files under `root` (root-relative), or
/// `None` when `root` is not inside a git work tree.
pub fn list_files(root: &Path) -> Option<Vec<String>> {
    let output = run_git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--deduplicate",
        ],
        None,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        split_nul(&output.stdout)
            .into_iter()
            .filter(|path| {
                !path
                    .split('/')
                    .any(|part| part.eq_ignore_ascii_case(".git"))
            })
            .collect(),
    )
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// Repository prefix of `root` (`""` at the top level, else `"sub/dir/"`),
/// or `None` when not in a work tree.
fn repo_prefix(root: &Path) -> Option<String> {
    let output = run_git(
        root,
        &["rev-parse", "--is-inside-work-tree", "--show-prefix"],
        None,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    if lines.next()? != "true" {
        return None;
    }
    Some(lines.next().unwrap_or("").to_string())
}

fn status_label(code: char) -> &'static str {
    match code {
        'M' => "modified",
        'A' => "added",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'T' => "typechange",
        'U' => "conflicted",
        _ => "modified",
    }
}

fn numstat(root: &Path, cached: bool) -> HashMap<String, (Option<u64>, Option<u64>)> {
    let mut args = vec!["diff", "--numstat", "-z", "--relative", "--no-renames"];
    if cached {
        args.push("--cached");
    }
    let mut map = HashMap::new();
    let Ok(output) = run_git(root, &args, None) else {
        return map;
    };
    for record in split_nul(&output.stdout) {
        let mut fields = record.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        map.insert(path.to_string(), (added.parse().ok(), deleted.parse().ok()));
    }
    map
}

fn count_lines(path: &Path) -> Option<u64> {
    let meta = path.metadata().ok()?;
    if !meta.is_file() || meta.len() > UNTRACKED_COUNT_LIMIT {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    Some(newlines + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n")))
}

/// `GET {scope}/git/status`.
pub fn status_resource(root: &ResolvedRoot) -> WorkspaceResult<Value> {
    let dir = &root.canonical;
    let Some(prefix) = repo_prefix(dir) else {
        return Ok(json!({
            "isRepo": false,
            "branch": null,
            "upstream": null,
            "ahead": null,
            "behind": null,
            "head": null,
            "files": [],
        }));
    };
    let output = run_git(
        dir,
        &[
            "status",
            "--porcelain=v2",
            "-b",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
        None,
    )
    .map_err(|error| WorkspaceError::new(503, "git_unavailable", error.to_string()))?;
    if !output.status.success() {
        return Err(WorkspaceError::new(
            500,
            "git_failed",
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let parsed = parse_porcelain_v2(&output.stdout, &prefix);
    let unstaged = numstat(dir, false);
    let staged = numstat(dir, true);
    let files: Vec<Value> = parsed
        .files
        .iter()
        .map(|file| {
            let counts = if file.status == "untracked" {
                (count_lines(&dir.join(&file.path)), Some(0))
            } else if file.staged {
                staged.get(&file.path).copied().unwrap_or((None, None))
            } else {
                unstaged.get(&file.path).copied().unwrap_or((None, None))
            };
            json!({
                "path": file.path,
                "origPath": file.orig_path,
                "status": file.status,
                "staged": file.staged,
                "added": counts.0,
                "deleted": counts.1,
            })
        })
        .collect();
    let head = match &parsed.oid {
        Some(_) => run_git(dir, &["log", "-1", "--format=%H%x00%s"], None)
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                let text = String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_string();
                let (sha, subject) = text.split_once('\0')?;
                Some(json!({"sha": sha, "subject": subject}))
            })
            .unwrap_or(Value::Null),
        None => Value::Null,
    };
    Ok(json!({
        "isRepo": true,
        "branch": parsed.branch,
        "upstream": parsed.upstream,
        "ahead": parsed.ahead,
        "behind": parsed.behind,
        "head": head,
        "files": files,
    }))
}

#[derive(Debug, Default)]
struct PorcelainStatus {
    oid: Option<String>,
    branch: Option<String>,
    upstream: Option<String>,
    ahead: Option<u64>,
    behind: Option<u64>,
    files: Vec<StatusFile>,
}

#[derive(Debug, PartialEq, Eq)]
struct StatusFile {
    path: String,
    orig_path: Option<String>,
    status: &'static str,
    staged: bool,
}

fn strip_prefix_path(path: &str, prefix: &str) -> Option<String> {
    path.strip_prefix(prefix).map(str::to_string)
}

/// Parse `git status --porcelain=v2 -b -z`. Repo-root paths are made
/// relative to `prefix` (entries outside it are dropped). One entry per
/// (path, index/worktree) side that changed.
fn parse_porcelain_v2(bytes: &[u8], prefix: &str) -> PorcelainStatus {
    let mut status = PorcelainStatus::default();
    let records = split_nul(bytes);
    let mut index = 0;
    while index < records.len() {
        let record = &records[index];
        index += 1;
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => status.oid = Some(value.to_string()),
                "branch.head" if value != "(detached)" => status.branch = Some(value.to_string()),
                "branch.upstream" => status.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(ahead) = part.strip_prefix('+') {
                            status.ahead = ahead.parse().ok();
                        } else if let Some(behind) = part.strip_prefix('-') {
                            status.behind = behind.parse().ok();
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let kind = record.chars().next().unwrap_or(' ');
        let (path, orig_path, xy) = match kind {
            '1' => {
                let fields: Vec<&str> = record.splitn(9, ' ').collect();
                if fields.len() < 9 {
                    continue;
                }
                (fields[8].to_string(), None, fields[1].to_string())
            }
            '2' => {
                let fields: Vec<&str> = record.splitn(10, ' ').collect();
                let orig = records.get(index).cloned();
                index += 1;
                if fields.len() < 10 {
                    continue;
                }
                (fields[9].to_string(), orig, fields[1].to_string())
            }
            'u' => {
                let fields: Vec<&str> = record.splitn(11, ' ').collect();
                if fields.len() < 11 {
                    continue;
                }
                if let Some(path) = strip_prefix_path(fields[10], prefix) {
                    status.files.push(StatusFile {
                        path,
                        orig_path: None,
                        status: "conflicted",
                        staged: false,
                    });
                }
                continue;
            }
            '?' => {
                if let Some(path) = record
                    .get(2..)
                    .and_then(|path| strip_prefix_path(path, prefix))
                {
                    status.files.push(StatusFile {
                        path,
                        orig_path: None,
                        status: "untracked",
                        staged: false,
                    });
                }
                continue;
            }
            _ => continue,
        };
        let Some(path) = strip_prefix_path(&path, prefix) else {
            continue;
        };
        let orig_path =
            orig_path.map(|orig| orig.strip_prefix(prefix).unwrap_or(&orig).to_string());
        let mut chars = xy.chars();
        let (x, y) = (chars.next().unwrap_or('.'), chars.next().unwrap_or('.'));
        if x != '.' {
            status.files.push(StatusFile {
                path: path.clone(),
                orig_path: orig_path.clone(),
                status: status_label(x),
                staged: true,
            });
        }
        if y != '.' {
            status.files.push(StatusFile {
                path,
                orig_path: None,
                status: status_label(y),
                staged: false,
            });
        }
    }
    status
}

/// `GET {scope}/git/diff?path=&staged=`. Without `path`, the whole root.
/// An untracked `path` is diffed against /dev/null.
pub fn diff_resource(
    root: &ResolvedRoot,
    params: &HashMap<String, String>,
) -> WorkspaceResult<Value> {
    let staged = match params.get("staged").map(String::as_str) {
        None | Some("false") | Some("0") => false,
        Some("true") | Some("1") => true,
        Some(_) => return Err(WorkspaceError::bad_request("staged must be true or false")),
    };
    let dir = &root.canonical;
    if repo_prefix(dir).is_none() {
        return Err(WorkspaceError::new(
            409,
            "not_a_git_repository",
            "the workspace root is not in a git work tree",
        ));
    }
    let rel = match params.get("path").filter(|path| !path.is_empty()) {
        // Deleted files no longer exist, so only validate the lexical form.
        Some(raw) => Some(lexical_rel(raw)?),
        None => None,
    };
    let mut args = vec!["diff", "--relative", "--no-ext-diff", "--no-color"];
    if staged {
        args.push("--cached");
    }
    args.push("--");
    args.push(rel.as_deref().unwrap_or("."));
    let output = run_git(dir, &args, None)
        .map_err(|error| WorkspaceError::new(503, "git_unavailable", error.to_string()))?;
    if !output.status.success() {
        return Err(WorkspaceError::new(
            400,
            "git_failed",
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let mut diff = output.stdout;
    if diff.is_empty() && !staged {
        if let Some(rel) = rel.as_deref() {
            let untracked = run_git(
                dir,
                &[
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--",
                    rel,
                ],
                None,
            )
            .map(|output| !output.stdout.is_empty())
            .unwrap_or(false);
            // The file itself must stay inside the root (no symlink escape).
            let contained = std::fs::canonicalize(dir.join(rel))
                .is_ok_and(|path| path.starts_with(dir) && path.is_file());
            if untracked && contained {
                // Exit status 1 means "differences found" for --no-index.
                if let Ok(output) = run_git(
                    dir,
                    &["diff", "--no-index", "--no-color", "--", "/dev/null", rel],
                    None,
                ) {
                    diff = output.stdout;
                }
            }
        }
    }
    let truncated = diff.len() > MAX_DIFF_BYTES;
    diff.truncate(MAX_DIFF_BYTES);
    Ok(json!({
        "path": rel.unwrap_or_default(),
        "staged": staged,
        "diff": String::from_utf8_lossy(&diff),
        "truncated": truncated,
    }))
}

fn lexical_rel(raw: &str) -> WorkspaceResult<String> {
    if raw.contains('\0')
        || (cfg!(windows) && raw.contains(':'))
        || raw.starts_with('/')
        || raw.starts_with('\\')
    {
        return Err(WorkspaceError::bad_request("path must be root-relative"));
    }
    let mut parts = Vec::new();
    let normalized = raw.replace('\\', "/");
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(WorkspaceError::new(
                    403,
                    "path_outside_root",
                    "path must not contain '..'",
                ))
            }
            name if name.eq_ignore_ascii_case(".git") => {
                return Err(WorkspaceError::new(
                    403,
                    "path_forbidden",
                    ".git internals are not exposed",
                ))
            }
            name => parts.push(name),
        }
    }
    if parts.is_empty() {
        return Err(WorkspaceError::bad_request("path must name a file"));
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::super::workspace::Scope;
    use super::super::workspace::ScopeKind;
    use super::*;
    use std::path::PathBuf;

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = run_git(dir, args, None).unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn temp_repo(name: &str) -> (PathBuf, ResolvedRoot) {
        let dir = std::env::temp_dir().join(format!(
            "cutex-git-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@example.invalid"]);
        git(&dir, &["config", "user.name", "T"]);
        git(&dir, &["config", "core.autocrlf", "false"]);
        let root = ResolvedRoot::from_dir(
            Scope {
                kind: ScopeKind::Session,
                id: "s".into(),
            },
            &dir,
        )
        .unwrap();
        (dir, root)
    }

    #[test]
    fn parses_porcelain_v2_with_prefix() {
        let raw = b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 h1 h2 sub/a.txt\0\
1 M. N... 100644 100644 100644 h1 h2 other/b.txt\0\
2 R. N... 100644 100644 100644 h1 h2 R100 sub/new.txt\0sub/old.txt\0\
? sub/u.txt\0u UU N... 100644 100644 100644 100644 h1 h2 h3 sub/c.txt\0";
        let parsed = parse_porcelain_v2(raw, "sub/");
        assert_eq!(parsed.branch.as_deref(), Some("main"));
        assert_eq!(parsed.upstream.as_deref(), Some("origin/main"));
        assert_eq!((parsed.ahead, parsed.behind), (Some(2), Some(1)));
        assert_eq!(
            parsed.files,
            vec![
                StatusFile {
                    path: "a.txt".into(),
                    orig_path: None,
                    status: "modified",
                    staged: false
                },
                StatusFile {
                    path: "new.txt".into(),
                    orig_path: Some("old.txt".into()),
                    status: "renamed",
                    staged: true
                },
                StatusFile {
                    path: "u.txt".into(),
                    orig_path: None,
                    status: "untracked",
                    staged: false
                },
                StatusFile {
                    path: "c.txt".into(),
                    orig_path: None,
                    status: "conflicted",
                    staged: false
                },
            ]
        );
    }

    #[test]
    fn status_and_diff_on_real_repo() {
        if !git_available() {
            return;
        }
        let (dir, root) = temp_repo("status");
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "first commit"]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(dir.join("new.txt"), "x\ny\n").unwrap();
        std::fs::write(dir.join("staged.txt"), "s\n").unwrap();
        git(&dir, &["add", "staged.txt"]);

        let status = status_resource(&root).unwrap();
        assert_eq!(status["isRepo"], true);
        assert_eq!(status["branch"], "main");
        assert_eq!(status["head"]["subject"], "first commit");
        let files = status["files"].as_array().unwrap();
        let find = |path: &str| {
            files
                .iter()
                .find(|file| file["path"] == path)
                .unwrap()
                .clone()
        };
        assert_eq!(find("a.txt")["status"], "modified");
        assert_eq!(find("a.txt")["staged"], false);
        assert_eq!(find("a.txt")["added"], 2);
        assert_eq!(find("a.txt")["deleted"], 1);
        assert_eq!(find("new.txt")["status"], "untracked");
        assert_eq!(find("new.txt")["added"], 2);
        assert_eq!(find("staged.txt")["staged"], true);
        assert_eq!(find("staged.txt")["status"], "added");
        assert_eq!(find("staged.txt")["added"], 1);

        let params = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let diff = diff_resource(&root, &params(&[("path", "a.txt")])).unwrap();
        assert!(diff["diff"].as_str().unwrap().contains("+TWO"));
        let untracked = diff_resource(&root, &params(&[("path", "new.txt")])).unwrap();
        let text = untracked["diff"].as_str().unwrap();
        assert!(text.contains("/dev/null") && text.contains("+x"), "{text}");
        let staged = diff_resource(
            &root,
            &params(&[("path", "staged.txt"), ("staged", "true")]),
        )
        .unwrap();
        assert!(staged["diff"].as_str().unwrap().contains("+s"));
        let all = diff_resource(&root, &params(&[])).unwrap();
        assert!(all["diff"].as_str().unwrap().contains("a.txt"));
        assert_eq!(
            diff_resource(&root, &params(&[("path", "../x")]))
                .unwrap_err()
                .code,
            "path_outside_root"
        );

        // Subdirectory root: paths are relative to it.
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/inner.txt"), "i\n").unwrap();
        let sub_root = ResolvedRoot::from_dir(root.scope.clone(), &dir.join("sub")).unwrap();
        let sub_status = status_resource(&sub_root).unwrap();
        let sub_files = sub_status["files"].as_array().unwrap();
        assert_eq!(sub_files.len(), 1);
        assert_eq!(sub_files[0]["path"], "inner.txt");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn non_repo_status() {
        let dir =
            std::env::temp_dir().join(format!("cutex-nogit-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        // Guard against a parent repository on the temp path.
        if repo_prefix(&dir).is_none() {
            let root = ResolvedRoot::from_dir(
                Scope {
                    kind: ScopeKind::Project,
                    id: "p".into(),
                },
                &dir,
            )
            .unwrap();
            let status = status_resource(&root).unwrap();
            assert_eq!(status["isRepo"], false);
            assert_eq!(status["files"], json!([]));
            assert!(list_files(&dir).is_none());
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
