//! User defaults for list ordering; `default` preserves each panel's native order.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListSort {
    #[default]
    Default,
    NameAsc,
    NameDesc,
    Project,
    Pinned,
}
impl ListSort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::NameAsc => "Name A–Z",
            Self::Pinned => "Pin first",
            Self::Project => "Project first",
            Self::NameDesc => "Name Z–A",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NameAsc => "name_asc",
            Self::Pinned => "pinned",
            Self::Project => "project",
            Self::NameDesc => "name_desc",
        }
    }
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "default" => Ok(Self::Default),
            "name_asc" => Ok(Self::NameAsc),
            "name_desc" => Ok(Self::NameDesc),
            "project" => Ok(Self::Project),
            "pinned" => Ok(Self::Pinned),
            _ => anyhow::bail!("Unknown list sort order"),
        }
    }
    pub fn next_names(self) -> Self {
        match self {
            Self::Default => Self::NameAsc,
            Self::NameAsc => Self::NameDesc,
            _ => Self::Default,
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::NameAsc,
            Self::NameAsc => Self::NameDesc,
            Self::NameDesc => Self::Pinned,
            Self::Pinned => Self::Project,
            Self::Project => Self::Default,
        }
    }
    pub fn compare_names(self, left: &str, right: &str) -> std::cmp::Ordering {
        let order = left.to_lowercase().cmp(&right.to_lowercase());
        match self {
            Self::Default => std::cmp::Ordering::Equal,
            Self::NameAsc | Self::Project | Self::Pinned => order,
            Self::NameDesc => order.reverse(),
        }
    }
}

pub fn compare_projects(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(l), Some(r)) => l.to_lowercase().cmp(&r.to_lowercase()),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}
