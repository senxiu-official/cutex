//! Project memory managed by cutex (cutex-desktop DESIGN §3.2).
//!
//! Memory is a Claude-Code / ZCode style directory of one-fact markdown files
//! with YAML frontmatter plus a `MEMORY.md` one-line-per-memory index. It is
//! scoped per Agent Management project and stored under
//! `~/.cutex/projects/<project-key>/memory/`. Agents write the files with their
//! own file tools; cutex only resolves the directory, injects the `# Memory`
//! instruction section at launch, and serves the files over Management v2.

pub mod harness;
pub mod instructions;
pub mod scope;
pub mod store;

pub use instructions::*;
pub use scope::*;
pub use store::*;
