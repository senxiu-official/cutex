//! Component-aware prefix queries over paths normalized once by the caller.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
pub struct Index(BTreeSet<PathBuf>);
impl Index {
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self(paths.into_iter().collect())
    }
    pub fn descendant(&self, target: &Path) -> bool {
        self.0
            .range(target.to_path_buf()..)
            .next()
            .is_some_and(|p| p.starts_with(target))
    }
    pub fn either(&self, target: &Path) -> bool {
        self.descendant(target) || target.ancestors().any(|p| self.0.contains(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::runtime_prune_inventory::{normalized, overlaps};
    #[test]
    fn randomized_and_runtime_shaped_prefix_queries_equal_old_overlaps() {
        let _budget = crate::management::v2::runtime_prune_inventory::budget::Scope::enter(
            std::time::Duration::from_secs(30),
            Default::default(),
        );
        let mut paths = vec![
            PathBuf::from("D:/Apps/cutex/components/claude-runtime/r5"),
            PathBuf::from("D:/Apps/cutex/components/claude-runtime/r50/python/python.exe"),
            PathBuf::from("D:/Apps/cutex/runtimes/claude-r49/adapter/src/cute_claude"),
        ];
        #[cfg(windows)]
        paths.extend(
            [
                r"\\?\D:\Apps\cutex\components\claude-runtime\r5\python.exe",
                r"d:\Apps\CUTEX\components\CLAUDE-runtime\R5",
            ]
            .map(PathBuf::from),
        );
        let mut seed = 173u64;
        for _ in 0..400 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            paths.push(PathBuf::from(format!(
                "D:/Apps/cutex/{}/r{}/{}",
                ["components/claude-runtime", "runtimes", "native"][seed as usize % 3],
                (seed >> 8) % 30,
                ["", "python.exe", "adapter/src", "adapter/src2"][seed as usize % 4]
            )));
        }
        for chunk in paths.chunks(19) {
            let index = Index::new(chunk.iter().map(|p| normalized(p)));
            for target in &paths {
                let n = normalized(target);
                assert_eq!(
                    index.descendant(&n),
                    chunk.iter().any(|p| overlaps(target, p))
                );
                assert_eq!(
                    index.either(&n),
                    chunk
                        .iter()
                        .any(|p| overlaps(target, p) || overlaps(p, target))
                );
            }
        }
    }
}
