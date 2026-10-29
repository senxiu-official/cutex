//! Services registry: web services agents start while working (Bridgeboard
//! replacement). One registry per host, in the cutex home.
//!
//! - [`store`]: the durable registry (`<cutex home>/services/registry.json`,
//!   fs2-locked so the Management daemon and the Agent Bus can both write it).
//! - [`supervisor`]: daemon-side health probes and process supervision for
//!   `managed` services; it is the only writer of the runtime
//!   state file and the only source of `cutex/services/changed` events.
//! - [`process`]: spawn a service command in its own process tree (Windows job
//!   object / Unix process group) and kill the whole tree.

pub mod agent_api;
mod authority;
pub mod health;
pub mod process;
pub mod proxy;
pub mod store;
pub mod supervisor;
pub mod tile;
pub mod traffic;

pub use store::RegisterRequest;
pub use store::ServiceCommand;
pub use store::ServiceRecord;
pub use store::ServiceRuntime;
pub use store::ServiceStore;
pub use store::ServicesError;
pub use store::ServicesSettings;
pub use store::UpdateRequest;

pub mod lifecycle;
pub mod reveal;
pub mod static_files;
