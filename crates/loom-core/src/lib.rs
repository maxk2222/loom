//! loom-core: shared domain types — OpenAI-compatible API models, capability
//! flags, config schemas, error taxonomy. No runtime dependencies beyond serde.

pub mod capability;
pub mod config;
pub mod error;
pub mod ids;
pub mod openai;
pub mod tokens;

pub use capability::*;
pub use config::*;
pub use error::*;
pub use ids::*;
pub use openai::*;
