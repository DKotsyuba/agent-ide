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
//! - [`runtime`](crate::modules::runtime): supervision of one instance slot.
//! - [`launch`](crate::modules::launch): the Execution-admitted launcher of the pinned executable.
//! - [`adapter`](crate::modules::adapter): the module-side server of a language's own support.
//! - [`router`](crate::modules::router): daemon-side routing in process or to a module.
//! - [`provider`](crate::modules::provider): the module-side host of a language's provider.
//! - [`analyzer`](crate::modules::analyzer): the core-side start of a module-hosted provider.
//! - [`calls`](crate::modules::calls): the async facade every core call site uses.
//!
//! # Growth beyond version 0: linters and debugging
//!
//! Version 0 blocks neither. Every addition is a new contract version both ends negotiate in
//! `hello` (`versions`), never a silent change of version 0: both ends ship in one sealed binary,
//! and a version-0 peer refuses an unknown capability, control `type` or frame kind, so nothing
//! half-understood runs. Reserved, not implemented: the capability families `lint` and `debug`,
//! the control type `event`, and frame kind `2`.
//!
//! - `lint` (ruff, clippy, eslint, stylelint, a markup linter): a `check_plan`/`check_parse`-shaped
//!   request whose processes are core-expanded effect recipes run under Execution; results reuse
//!   the problems path (bounded problems, full counts, coverage, unavailable reasons), and a hint's
//!   suggested fix is an edit proposal the core validates and applies through its own change path.
//! - `debug` (the Debug Adapter Protocol: breakpoints, call stacks, variables, stepping,
//!   evaluation): a session is a module-minted opaque handle carried in payloads and fenced like
//!   any request; `event` control messages (stopped, output, exited) are the one module-initiated
//!   message outside a request, with bulk output as ordinary attachments (kind `2` stays free for a
//!   stream that cannot be one); launching or attaching to a debuggee is a core-admitted effect
//!   recipe, never a module spawn, and the debug adapter runs under a provider grant.

pub mod adapter;
pub mod analyzer;
pub mod calls;
pub mod contract;
pub mod fake;
pub mod host;
pub mod launch;
pub mod mode;
pub mod payload;
pub mod provider;
pub mod recipe;
pub mod router;
pub mod runtime;
pub mod serve;
pub mod wire;
