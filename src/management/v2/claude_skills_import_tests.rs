use super::*;

struct Dirs {
    root: PathBuf,
    source: PathBuf,
    target: PathBuf,
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn dirs(label: &str) -> Dirs {
    let root = std::env::temp_dir().join(format!("cutex-skills-import-{label}-{}", uuid::Uuid::new_v4().simple()));
    let source = root.join("claude/skills");
    let target = root.join("cutex/claude-skills");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(root.join("cutex")).unwrap();
    let root = root.canonicalize().unwrap();
    Dirs { source: root.join("claude/skills"), target: root.join("cutex/claude-skills"), root }
}

fn skill(source: &Path, name: &str, description: &str) {
    let dir = source.join(name);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("SKILL.md"), format!("---\nname: {name}\ndescription: \"{description}\"\n---\nBody\n")).unwrap();
    std::fs::write(dir.join("scripts/run.sh"), "echo hi\n").unwrap();
}

fn names(listing: &Value) -> Vec<String> {
    listing["skills"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect()
}

#[test]
fn paths() {
    assert!(is_path("/v2/claude/skills/import"));
    assert!(is_path("/v2/claude/skills/import?source=%2Ftmp"));
    assert!(!is_path("/v2/claude/skills"));
    assert!(!is_path("/v2/claude/accounts"));
}

#[test]
fn lists_skill_directories_with_front_matter_and_existing_state() {
    let d = dirs("list");
    skill(&d.source, "pdf", "Read PDFs");
    skill(&d.source, "review", "Review code");
    std::fs::create_dir_all(d.source.join("notes")).unwrap(); // no SKILL.md: not a skill
    std::fs::create_dir_all(d.source.join(".hidden")).unwrap();
    std::fs::write(d.source.join("README.md"), "x").unwrap();
    std::fs::create_dir_all(d.target.join("review")).unwrap();
    let listing = list(&d.source, &d.target).unwrap();
    assert_eq!(names(&listing), ["pdf", "review"]);
    let pdf = &listing["skills"][0];
    assert_eq!(pdf["title"], "pdf");
    assert_eq!(pdf["description"], "Read PDFs");
    assert_eq!(pdf["files"], 2);
    assert_eq!(pdf["importable"], true);
    assert_eq!(pdf["exists"], false);
    assert_eq!(listing["skills"][1]["exists"], true);
    assert_eq!(listing["sourceExists"], true);
}

#[test]
fn missing_source_lists_nothing() {
    let d = dirs("missing");
    let listing = list(&d.root.join("nope"), &d.target).unwrap();
    assert_eq!(listing["sourceExists"], false);
    assert!(listing["skills"].as_array().unwrap().is_empty());
    assert!(matches!(import(&d.root.join("nope"), &d.target, &["x".into()], false), Err(ImportError::Invalid(_))));
}

#[test]
fn overlapping_directories_are_refused() {
    let d = dirs("overlap");
    std::fs::create_dir_all(&d.target).unwrap();
    assert!(matches!(list(&d.target, &d.target), Err(ImportError::Invalid(_))));
    assert!(matches!(list(&d.root, &d.target), Err(ImportError::Invalid(_))));
}

#[test]
fn imports_copies_skips_existing_and_overwrites_on_request() {
    let d = dirs("import");
    skill(&d.source, "pdf", "Read PDFs");
    skill(&d.source, "review", "Review code");
    std::fs::create_dir_all(d.target.join("review")).unwrap();
    std::fs::write(d.target.join("review/SKILL.md"), "old").unwrap();
    std::fs::write(d.target.join("review/stale.txt"), "old").unwrap();
    let answer = import(&d.source, &d.target, &["pdf".into(), "review".into(), "ghost".into()], false).unwrap();
    assert_eq!(answer["imported"], json!(["pdf"]));
    assert_eq!(answer["skipped"][0]["reason"], "exists");
    assert_eq!(answer["failed"][0]["reason"], "notFound");
    assert_eq!(std::fs::read_to_string(d.target.join("pdf/scripts/run.sh")).unwrap(), "echo hi\n");
    assert_eq!(std::fs::read_to_string(d.target.join("review/SKILL.md")).unwrap(), "old");

    let answer = import(&d.source, &d.target, &["review".into()], true).unwrap();
    assert_eq!(answer["imported"], json!(["review"]));
    assert!(std::fs::read_to_string(d.target.join("review/SKILL.md")).unwrap().contains("Review code"));
    assert!(!d.target.join("review/stale.txt").exists(), "an overwrite replaces the whole skill");
    // Nothing staged is left behind, and the source is untouched.
    let leftovers: Vec<_> = std::fs::read_dir(&d.target).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX)).collect();
    assert!(leftovers.is_empty());
    assert!(d.source.join("pdf/SKILL.md").is_file() && d.source.join("review/scripts/run.sh").is_file());
}

#[test]
fn invalid_names_are_refused() {
    let d = dirs("names");
    skill(&d.source, "pdf", "x");
    let answer = import(&d.source, &d.target, &["../pdf".into(), ".pdf".into(), "a/b".into()], false).unwrap();
    assert_eq!(answer["failed"].as_array().unwrap().len(), 3);
    assert!(answer["failed"].as_array().unwrap().iter().all(|f| f["reason"] == "invalidName"));
    assert!(!d.target.exists());
}

#[cfg(unix)]
#[test]
fn symlinks_never_lead_out_of_the_source() {
    use std::os::unix::fs::symlink;
    let d = dirs("links");
    let outside = d.root.join("outside");
    skill(&outside, "secret", "outside");
    std::fs::write(outside.join("token.txt"), "secret").unwrap();
    // A skill directory linking out of the source is not importable.
    symlink(outside.join("secret"), d.source.join("linked")).unwrap();
    // A skill directory linking to another skill inside the source is.
    skill(&d.source, "pdf", "Read PDFs");
    symlink(d.source.join("pdf"), d.source.join("pdf-alias")).unwrap();
    // Links inside a skill: to a file outside -> left out; to a file inside -> copied; to a dir -> left out.
    symlink(outside.join("token.txt"), d.source.join("pdf/token.txt")).unwrap();
    symlink(d.source.join("pdf/scripts/run.sh"), d.source.join("pdf/run-link.sh")).unwrap();
    symlink(&outside, d.source.join("pdf/outside-dir")).unwrap();

    let listing = list(&d.source, &d.target).unwrap();
    let linked = listing["skills"].as_array().unwrap().iter().find(|s| s["name"] == "linked").unwrap();
    assert_eq!(linked["importable"], false);
    assert_eq!(linked["reason"], "symlinkOutsideSource");
    let pdf = listing["skills"].as_array().unwrap().iter().find(|s| s["name"] == "pdf").unwrap();
    assert_eq!(pdf["skippedLinks"], 2);

    let answer = import(&d.source, &d.target, &["linked".into(), "pdf".into(), "pdf-alias".into()], false).unwrap();
    assert_eq!(answer["imported"], json!(["pdf", "pdf-alias"]));
    assert_eq!(answer["failed"][0]["reason"], "symlinkOutsideSource");
    assert!(!d.target.join("pdf/token.txt").exists());
    assert!(!d.target.join("pdf/outside-dir").exists());
    let copied = d.target.join("pdf/run-link.sh");
    assert!(std::fs::symlink_metadata(&copied).unwrap().is_file(), "an inside link is copied as a file");
    assert!(std::fs::symlink_metadata(d.target.join("pdf-alias")).unwrap().is_dir());
}

#[test]
fn oversized_skills_are_not_importable() {
    let d = dirs("size");
    let dir = d.source.join("big");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), vec![b'a'; (MAX_SKILL_MD_BYTES + 1) as usize]).unwrap();
    let listing = list(&d.source, &d.target).unwrap();
    assert_eq!(listing["skills"][0]["reason"], "skillFileTooLarge");
    let answer = import(&d.source, &d.target, &["big".into()], false).unwrap();
    assert_eq!(answer["failed"][0]["reason"], "skillFileTooLarge");
}

#[test]
fn front_matter_without_header_has_no_metadata() {
    let d = dirs("fm");
    let dir = d.source.join("plain");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "no front matter").unwrap();
    let listing = list(&d.source, &d.target).unwrap();
    assert_eq!(listing["skills"][0]["title"], Value::Null);
    assert_eq!(listing["skills"][0]["importable"], true);
}
