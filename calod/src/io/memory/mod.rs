#[cfg(feature="tiered-memory")]
mod tired_mem;
// Re-export intentionally removed during scaffold to reduce unused import warnings.

#[cfg(not(feature="tiered-memory"))]
pub struct TieredMemoryConfig; // placeholder