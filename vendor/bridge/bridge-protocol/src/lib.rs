//! Universal producer wire v1, independent of core, native carriers and products.
//! Decoding/validation never creates native authority. See the canonical wire doc.
mod dto;
mod fingerprint;
mod validate;
mod values;
mod wire;
pub use dto::*;
pub use fingerprint::{publication_intent_fingerprint, registration_request_fingerprint};
pub use values::*;
pub use wire::*;
