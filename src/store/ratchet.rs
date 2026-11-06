//! Static guards that force stores onto the manifest and stop whole-state
//! access from growing (design 058 §2 "新存储的强制入口").
//!
//! These scans are tripwires, not proof of safety: the counters in
//! `store::metrics` and the scale gates prove the work per operation.
//!
//! 1. Every non-test file using a persistent-write primitive is the owner of
//!    a manifest store, or is listed in `NON_STORE_WRITERS` with a reason.
//! 2. `rusqlite` is used only by manifest owners, `src/store/**` and listed files.
//! 3. Whole-state access sites are counted per (file, pattern). A count above
//!    `ratchet_allowlist::ALLOWED` fails (a new site); a count below fails too
//!    until the allowlist is lowered, so the list only shrinks. Owner files of
//!    an `Access::Keyed` store may only keep entries whose note says why:
//!    `allow:` (tripwire false positive, or a migration/repair/export/owner
//!    trigger), `caller:` (a call into another store's whole-state API, retired
//!    with that store) or `debt:<slice>` (a known full read, owned by a slice).
//!
//! Regenerate the counts with
//! `CUTEX_STORE_RATCHET_PRINT=1 cargo test --lib store::ratchet -- --nocapture`
//! and review every changed line.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::manifest::{self, Access, MANIFEST};

#[path = "ratchet_allowlist.rs"]
mod ratchet_allowlist;
use ratchet_allowlist::{ALLOWED, NON_STORE_WRITERS, SQL_USERS};

const WRITE_PRIMITIVES: &[&str] = &[
    "Connection::open",
    "json_atomic(",
    "bytes_atomic(",
    ".append(true)",
    "hot_store::commit(",
    "hot_store::load_or_migrate(",
];

/// (name, needle, only in manifest owner files)
const WHOLE_STATE: &[(&str, &[&str], bool)] = &[
    ("with_state", &["fn with_state", ".with_state("], false),
    ("load_all", &["load_all(", "read_all_at(", "load_store(", "load_cutex_session_store"], false),
    ("verified_rows", &["verified_rows("], false),
    ("owned_snapshot", &["fn snapshot(&self)"], true),
    (
        "full_file_parse",
        &[
            "from_slice(&std::fs::read(",
            "from_slice(&fs::read(",
            "from_str(&std::fs::read_to_string(",
            "from_str(&fs::read_to_string(",
        ],
        true,
    ),
];

const KEYED_NOTE_PREFIXES: &[&str] = &["allow:", "caller:", "debt:"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn is_test_file(relative: &str) -> bool {
    relative.ends_with("tests.rs")
        || relative.ends_with("_test.rs")
        || relative.contains("/tests/")
        || relative.contains("_tests/")
        || relative.contains("/test_support")
        || relative.starts_with("src/store/")
}

/// Source without the trailing `#[cfg(test)] mod … {` block.
fn production_source(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() == "#[cfg(test)]" {
            if let Some(next) = lines.get(index + 1) {
                let next = next.trim_start();
                if next.starts_with("mod ") && next.trim_end().ends_with('{') {
                    break;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
        index += 1;
    }
    out
}

fn rust_files(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(root, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if is_test_file(&relative) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            out.push((relative, production_source(&text)));
        }
    }
}

fn sources() -> Vec<(String, String)> {
    let root = repo_root();
    let mut out = Vec::new();
    rust_files(&root, &root.join("src"), &mut out);
    out
}

fn owners() -> BTreeSet<&'static str> {
    MANIFEST.iter().flat_map(|spec| spec.owner.iter().copied()).collect()
}

fn count(text: &str, needles: &[&str]) -> usize {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .map(|line| needles.iter().map(|needle| line.matches(needle).count()).sum::<usize>())
        .sum()
}

fn unbounded_selects(text: &str) -> usize {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| {
            let upper = line.to_ascii_uppercase();
            upper.contains("SELECT ")
                && upper.contains(" FROM ")
                && !["WHERE", "LIMIT", "COUNT(", "EXISTS", "SQLITE_", "MAX(", "MIN(", "PRAGMA"]
                    .iter()
                    .any(|bounded| upper.contains(bounded))
        })
        .count()
}

fn current_counts() -> BTreeMap<(String, &'static str), usize> {
    let owners = owners();
    let mut counts = BTreeMap::new();
    for (file, text) in sources() {
        let owned = owners.contains(file.as_str());
        for (name, needles, owner_only) in WHOLE_STATE {
            if *owner_only && !owned {
                continue;
            }
            let found = count(&text, needles);
            if found > 0 {
                counts.insert((file.clone(), *name), found);
            }
        }
        let selects = unbounded_selects(&text);
        if selects > 0 {
            counts.insert((file.clone(), "unbounded_select"), selects);
        }
    }
    counts
}

fn label(file: &str) -> String {
    let stores: Vec<_> = manifest::owned_by(file).map(|spec| spec.id).collect();
    if stores.is_empty() {
        "caller".into()
    } else {
        stores.join(",")
    }
}

#[test]
fn print_ratchet_tables_on_request() {
    if std::env::var_os("CUTEX_STORE_RATCHET_PRINT").is_none() {
        return;
    }
    println!("pub(super) const ALLOWED: &[(&str, &str, usize, &str)] = &[");
    for ((file, name), found) in current_counts() {
        let note = ALLOWED
            .iter()
            .find(|(f, n, _, _)| *f == file && *n == name)
            .map(|(_, _, _, note)| note.to_string())
            .unwrap_or_else(|| label(&file));
        println!("    ({file:?}, {name:?}, {found}, {note:?}),");
    }
    println!("];");
}

#[test]
fn persistent_writers_are_registered() {
    let owners = owners();
    let listed: BTreeSet<&str> = NON_STORE_WRITERS.iter().map(|(file, _)| *file).collect();
    let mut problems = Vec::new();
    let mut writers = BTreeSet::new();
    for (file, text) in sources() {
        if count(&text, WRITE_PRIMITIVES) == 0 {
            continue;
        }
        writers.insert(file.clone());
        if !owners.contains(file.as_str()) && !listed.contains(file.as_str()) {
            problems.push(format!(
                "{file}: writes persistent state but owns no store in store::manifest \
                 (register the store, or list a non-store reason in NON_STORE_WRITERS)"
            ));
        }
    }
    for (file, _) in NON_STORE_WRITERS {
        if owners.contains(file) {
            problems.push(format!("{file}: in NON_STORE_WRITERS but owns a manifest store; remove it"));
        } else if !writers.contains(*file) {
            problems.push(format!("{file}: in NON_STORE_WRITERS but writes nothing now; remove it"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn rusqlite_is_used_only_by_repositories() {
    let owners = owners();
    let listed: BTreeSet<&str> = SQL_USERS
        .iter()
        .map(|(file, _)| *file)
        .chain(NON_STORE_WRITERS.iter().map(|(file, _)| *file))
        .collect();
    let mut problems = Vec::new();
    let mut users = BTreeSet::new();
    for (file, text) in sources() {
        if count(&text, &["rusqlite"]) == 0 {
            continue;
        }
        users.insert(file.clone());
        if !owners.contains(file.as_str()) && !listed.contains(file.as_str()) {
            problems.push(format!("{file}: uses rusqlite but is not a store repository (manifest owner)"));
        }
    }
    for (file, _) in SQL_USERS {
        if owners.contains(file) || !users.contains(*file) {
            problems.push(format!("{file}: stale SQL_USERS entry; remove it"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn whole_state_sites_do_not_grow() {
    let counts = current_counts();
    let mut problems = Vec::new();
    let allowed: BTreeMap<(&str, &str), (usize, &str)> = ALLOWED
        .iter()
        .map(|(file, name, count, note)| ((*file, *name), (*count, *note)))
        .collect();
    for ((file, name), found) in &counts {
        match allowed.get(&(file.as_str(), *name)) {
            None => problems.push(format!("{file}: new whole-state site ({name} x{found}); use a keyed accessor")),
            Some((limit, _)) if found > limit => {
                problems.push(format!("{file}: {name} grew from {limit} to {found}; use a keyed accessor"))
            }
            Some((limit, _)) if found < limit => problems.push(format!(
                "{file}: {name} fell from {limit} to {found}; lower ratchet_allowlist::ALLOWED"
            )),
            Some(_) => {}
        }
    }
    for ((file, name), _) in &allowed {
        if !counts.contains_key(&(file.to_string(), *name)) {
            problems.push(format!("{file}: {name} is gone; remove it from ratchet_allowlist::ALLOWED"));
        }
    }
    for spec in MANIFEST.iter().filter(|spec| spec.access == Access::Keyed) {
        for owner in spec.owner {
            for ((file, name), (_, note)) in &allowed {
                if file == owner && !KEYED_NOTE_PREFIXES.iter().any(|prefix| note.starts_with(prefix)) {
                    problems.push(format!(
                        "{file}: {} is Access::Keyed but keeps {name} without an `allow:` / `caller:` / `debt:<slice>` note",
                        spec.id
                    ));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn production_source_cuts_only_test_modules() {
    let text = "fn a() {}\n#[cfg(test)]\nmod helpers;\nfn b() {}\n#[cfg(test)]\nmod tests {\nfn c() {}\n}\n";
    let production = production_source(text);
    assert!(production.contains("fn b()"));
    assert!(!production.contains("fn c()"));
    assert_eq!(unbounded_selects("\"SELECT a FROM t\"\n\"SELECT a FROM t WHERE k = ?1\"\n"), 1);
}
