//! Factory for the Deriverse data client (spec §2.4). Holds the shared
//! [`FeedFreshness`] so the data client's drain writes into the same handle
//! the M1b exec client's MARKET gate will read (pacifica BboCache pattern:
//! construct one handle, hand it to BOTH factories).

use std::{any::Any, cell::RefCell, rc::Rc};

use nautilus_common::{
    cache::CacheView,
    clients::DataClient,
    clock::Clock,
    factories::{ClientConfig, DataClientFactory},
};
use nautilus_model::identifiers::ClientId;

use crate::config::{DeriverseDataClientConfig, DeriverseExecClientConfig};
use crate::data::{DeriverseDataClient, FeedFreshness};

impl ClientConfig for DeriverseDataClientConfig {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl ClientConfig for DeriverseExecClientConfig {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Default)]
pub struct DeriverseDataClientFactory {
    freshness: FeedFreshness,
}

impl DeriverseDataClientFactory {
    /// Build a factory sharing `freshness` with the (M1b) exec factory.
    #[must_use]
    pub fn with_shared_freshness(freshness: FeedFreshness) -> Self {
        Self { freshness }
    }

    /// The shared handle (clone into the M1b exec factory).
    #[must_use]
    pub fn freshness(&self) -> FeedFreshness {
        self.freshness.clone()
    }
}

impl DataClientFactory for DeriverseDataClientFactory {
    fn create(
        &self,
        name: &str,
        config: &dyn ClientConfig,
        _cache: CacheView,
        _clock: Rc<RefCell<dyn Clock>>,
    ) -> anyhow::Result<Box<dyn DataClient>> {
        let cfg = config
            .as_any()
            .downcast_ref::<DeriverseDataClientConfig>()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid config type for DeriverseDataClientFactory; expected DeriverseDataClientConfig, was {config:?}"
                )
            })?
            .clone();
        let client = DeriverseDataClient::new(ClientId::from(name), cfg)?
            .with_freshness(self.freshness.clone());
        Ok(Box::new(client))
    }

    fn name(&self) -> &str {
        "DERIVERSE"
    }

    fn config_type(&self) -> &str {
        "DeriverseDataClientConfig"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_shares_one_freshness_handle() {
        let shared = FeedFreshness::new(50);
        let factory = DeriverseDataClientFactory::with_shared_freshness(shared.clone());
        shared.note_heartbeat(123);
        assert_eq!(factory.freshness().heartbeat_slot(), 123);
        assert_eq!(factory.name(), "DERIVERSE");
        assert_eq!(factory.config_type(), "DeriverseDataClientConfig");
    }
}
