//! The engine as the IDE sees it: one process per application, spawned with
//! a generated token, watched, restarted, and reached over loopback HTTP.
//! Nothing here holds a credential; the launch token is the only secret.

pub mod environment;
pub mod process;
pub mod settings;

pub use settings::EngineSettings;
