use std::path::Path;

/// Build a latest `RaftStorage` implementation for compatibility test.
#[async_trait::async_trait]
pub trait StoreBuilder {
    type C: crate::RaftTypeConfig<NodeId = u64, Node = crate::EmptyNode>;
    type S: crate::RaftStorage<Self::C>;

    /// Build a store that is backed by data stored on file system.
    async fn build(&self, p: &Path) -> Self::S;

    /// Build an `AppData` for testing. It has to always produce the same data.
    fn sample_app_data(&self) -> <<Self as StoreBuilder>::C as crate::RaftTypeConfig>::D;
}

#[cfg(feature = "compat-07")]
pub use crate::compat::compat07::testing::StoreBuilder07;
#[cfg(feature = "compat-07")] pub use crate::compat::compat07::testing::Suite07;
