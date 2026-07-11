//! Process-level bootstrap helpers for the binary.
//!
//! These are *not* subcommands — they are layered initialization steps
//! (logging, work_dir resolution, builtin-skill materialization, database
//! init) that subcommands compose to start the application.

mod builtin_skills;
mod env_aliases;
mod environment;
mod error;
mod parent_exit;
mod tracing_init;
mod work_dir;

pub(crate) use env_aliases::apply_centaurai_core_env_aliases;
pub use environment::{ServerEnvironment, init_data_layer, init_environment};
pub(crate) use error::{BootstrapError, BootstrapErrorCode};
pub(crate) use parent_exit::{ParentExitSignal, parent_exit_signal};
