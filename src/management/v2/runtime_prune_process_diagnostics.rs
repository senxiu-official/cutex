//! Bounded, owner-visible explanations; never cleanup authority or raw command lines.
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const MAX_PROCESSES: usize = 20;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnresolvedProcess {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Observed canonical session-directory basename, not an authenticated binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnresolvedClaude {
    pub unresolved_process_count: usize,
    pub processes: Vec<UnresolvedProcess>,
    pub message: &'static str,
}

impl Default for UnresolvedClaude {
    fn default() -> Self {
        Self {
            unresolved_process_count: 0,
            processes: Vec::new(),
            message: crate::runtime::claude::process_receipt::WARNING,
        }
    }
}

impl UnresolvedClaude {
    pub fn note(&mut self, pid: Option<u32>, args: &[String]) {
        self.unresolved_process_count = self.unresolved_process_count.saturating_add(1);
        if self.processes.len() >= MAX_PROCESSES {
            return;
        }
        let session_id = args
            .windows(2)
            .find(|p| p[0] == "--state-dir")
            .and_then(|p| Path::new(&p[1]).file_name())
            .and_then(|p| p.to_str())
            .filter(|id| {
                id.strip_prefix("cutex.")
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            })
            .map(str::to_owned);
        self.processes.push(UnresolvedProcess {
            pid: pid.filter(|pid| *pid != 0),
            session_id,
        });
    }
}

#[derive(Default)]
pub struct ProcessInspection {
    pub paths: Vec<PathBuf>,
    pub unresolved_claude: UnresolvedClaude,
}

impl ProcessInspection {
    pub fn from_paths(paths: Vec<PathBuf>) -> Self {
        let mut unresolved_claude = UnresolvedClaude::default();
        for _ in paths
            .iter()
            .filter(|p| *p == Path::new(super::UNKNOWN_CLAUDE))
        {
            unresolved_claude.note(None, &[]);
        }
        Self {
            paths,
            unresolved_claude,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unresolved_summary_caps_observations_and_does_not_expose_paths_or_arguments() {
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let state = std::env::temp_dir()
            .join("private-credential-path")
            .join(&id);
        let args = vec![
            "python".into(),
            "-m".into(),
            "cute_claude".into(),
            "--state-dir".into(),
            state.to_string_lossy().into_owned(),
            "--token".into(),
            "secret-never-expose".into(),
        ];
        let mut summary = UnresolvedClaude::default();
        for pid in 1..=56 {
            summary.note(Some(pid), &args);
        }
        assert_eq!(summary.unresolved_process_count, 56);
        assert_eq!(summary.processes.len(), MAX_PROCESSES);
        assert_eq!(summary.processes[0].pid, Some(1));
        assert_eq!(
            summary.processes[0].session_id.as_deref(),
            Some(id.as_str())
        );
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["unresolvedProcessCount"], 56);
        assert!(value["message"]
            .as_str()
            .unwrap()
            .contains("before receipts existed"));
        let encoded = value.to_string();
        assert!(!encoded.contains("private-credential-path"));
        assert!(!encoded.contains("secret-never-expose"));
        assert!(!encoded.contains("--state-dir"));
    }
}
