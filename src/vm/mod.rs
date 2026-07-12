#[allow(clippy::module_inception)]
pub mod vm;
mod builtins;
pub(crate) use builtins::uri_escapes_valid as builtins_uri_escapes_valid;
mod call;
mod embed;
mod json;
mod promise;
mod generator;
pub(crate) mod regexp;
mod map;
mod typedarray;

pub use vm::{Vm, VmError};
