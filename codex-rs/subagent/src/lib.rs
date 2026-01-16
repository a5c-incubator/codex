#![deny(missing_docs)]
#![doc = "Subagent manifest schema, validation, and loaders as described in docs/subagents/architecture.md."]

mod error;
#[cfg(feature = "fs-loader")]
mod loader;
#[cfg(feature = "schema")]
mod manifest;
#[cfg(feature = "schema")]
mod priority;
#[cfg(feature = "schema")]
mod validation;

pub use crate::error::*;
#[cfg(feature = "fs-loader")]
pub use crate::loader::*;
#[cfg(feature = "schema")]
pub use crate::manifest::*;
#[cfg(feature = "schema")]
pub use crate::priority::*;
#[cfg(feature = "schema")]
pub use crate::validation::*;
