//! Bundled language modules: the internal `bundled-module/0` contract between the daemon and its
//! own sealed executable running in the hidden `agent-ide module <language> <role>` mode.
//!
//! The core stays language-free: module ids (`bundled.<language id>`), capability sets and effect
//! recipes are data the root and language crates supply. The core owns sources, writes, effects
//! and joins; a module computes language facts and proposes edits and plans.
//!
//! - [`wire`](crate::modules::wire): framing, raw byte attachments and their exact assembly.
//! - [`contract`](crate::modules::contract): identity, `hello`, envelopes and fences, capabilities, typed failures.
//! - [`payload`](crate::modules::payload): the typed payload of every capability, edit proposals, effects, `linkage/0`.
//! - [`serve`](crate::modules::serve): the module-side [`ModuleServer`](crate::modules::serve::ModuleServer) trait and loop.
//! - [`host`](crate::modules::host): the core-side channel of one instance.
//! - [`fake`](crate::modules::fake): a fake module and fake host for conformance tests.
//! - [`mode`](crate::modules::mode): the `AGENT_IDE_LANGUAGE_MODE` fallback switch.
//! - [`recipe`](crate::modules::recipe): core expansion of effect recipes into run specifications.

pub mod contract;
pub mod fake;
pub mod host;
pub mod mode;
pub mod payload;
pub mod recipe;
pub mod serve;
pub mod wire;
