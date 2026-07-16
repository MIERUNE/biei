//! Storage integrations for local chunked reads and peer forwarding.

mod chunked_store;
mod peer;
mod pmtiles;
mod resolver;
mod routing;
mod store_registry;

pub(crate) use peer::TILE_SOURCE_HEADER;
#[cfg(not(feature = "simulator-support"))]
pub(crate) use peer::{InternalTileSource, internal_resource_kind};
pub use resolver::{
    ResourceResolver, ResourceResolverConfig, TileSource, TilesetError, TilesetInfo,
};
pub use store_registry::ObjectStoreRegistry;

#[cfg(feature = "simulator-support")]
#[doc(hidden)]
pub use crate::interned::TilesetId;
#[cfg(feature = "simulator-support")]
#[doc(hidden)]
pub use chunked_store::BackendLatencyModel;
#[cfg(feature = "simulator-support")]
#[doc(hidden)]
pub use chunked_store::plan_chunk_fetch_ranges;
#[cfg(feature = "simulator-support")]
#[doc(hidden)]
pub use peer::{
    FetchFuture, InternalFetchResponse, InternalTileSource, InternalTransport, PeerBackend,
    PeerDirectory, PeerFetchError, PeerFuture, internal_resource_kind,
};
#[cfg(feature = "simulator-support")]
#[doc(hidden)]
pub use resolver::{PeerTileCachePolicy, ResourceResolverStorageConfig};
#[cfg(feature = "simulator-support")]
#[doc(hidden)]
pub use routing::HrwRouter;
