//! loom-core: shared domain types — OpenAI-compatible API models, capability
//! flags, config schemas, error taxonomy. No runtime dependencies beyond serde.

pub mod openai;
pub mod capability;
pub mod error;
pub mod config;
pub mod ids;

pub use openai::*;
pub use capability::*;
pub use error::*;
pub use config::*;
pub use ids::*;
