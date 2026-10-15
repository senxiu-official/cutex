//! Workflows (W1, design `docs/research/workflows-20261013.md` in the desktop
//! repo): an agent writes a JavaScript orchestration script in the Claude Code
//! dialect (`export const meta`, `agent()`, `parallel()`, `pipeline()`,
//! `phase()`, `log()`, `args`, `budget`); after the owner approves the plan,
//! cutex runs it in a QuickJS child process. Every `agent()` call is an owner
//! ask to a hidden one-shot step session. Runs are durable (journal, resume,
//! restart recovery) and capped (agents, concurrency, tokens).

pub mod asks_bridge;
pub mod engine;
pub mod exec;
pub mod facts;
pub mod model;
pub mod script;
pub mod store;
pub mod tools;

pub use engine::{service, WorkflowService, WorkflowSettings};
