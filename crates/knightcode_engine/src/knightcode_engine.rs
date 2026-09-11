//! The engine as the IDE sees it: one process per application, spawned with
//! a generated token, watched, restarted, and reached over loopback HTTP.
//! Nothing here holds a credential; the launch token is the only secret.

pub mod client;
pub mod engine;
pub mod environment;
pub mod login;
pub mod process;
pub mod settings;

pub use client::{Endpoint, EngineClient, LoginKind, LoginOption};
pub use engine::{Engine, EngineEvent, EngineStatus, global, init, try_global};
pub use settings::EngineSettings;
