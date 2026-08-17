//! Compile-time provider registry: construct, register and look up providers.
//!
//! First version only registers `OmpProvider`. No PTY or UI here.

use std::collections::HashMap;

use crate::config::AmuxConfig;
use crate::provider::api::{AgentProvider, ProviderId};
use crate::provider::omp::OmpProvider;
use anyhow::{bail, Result};

pub struct ProviderRegistry {
    default: ProviderId,
    providers: HashMap<ProviderId, Box<dyn AgentProvider>>,
}

impl ProviderRegistry {
    /// Build the registry from amux config. First version: only OMP.
    pub fn from_config(config: &AmuxConfig) -> Result<Self> {
        let mut reg = Self {
            default: ProviderId::OMP,
            providers: HashMap::new(),
        };
        let omp = OmpProvider::from_config(config);
        reg.register(Box::new(omp))?;
        Ok(reg)
    }

    pub fn default_id(&self) -> ProviderId {
        self.default
    }

    pub fn get(&self, id: ProviderId) -> Result<&(dyn AgentProvider + '_)> {
        self.providers
            .get(&id)
            .map(|p| p.as_ref())
            .ok_or_else(|| anyhow::anyhow!("unknown provider: {}", id.as_str()))
    }

    pub fn get_mut(&mut self, id: ProviderId) -> Result<&mut (dyn AgentProvider + '_)> {
        let p = self
            .providers
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown provider: {}", id.as_str()))?;
        Ok(p.as_mut())
    }

    /// Register a provider; rejects duplicate IDs to prevent silent override.
    pub fn register(&mut self, provider: Box<dyn AgentProvider>) -> Result<()> {
        let id = provider.id();
        if self.providers.contains_key(&id) {
            bail!("provider already registered: {}", id.as_str());
        }
        self.providers.insert(id, provider);
        Ok(())
    }

    /// All registered provider IDs (for future provider selection UI).
    pub fn ids(&self) -> Vec<ProviderId> {
        self.providers.keys().copied().collect()
    }

    /// Empty registry for tests — no `from_config`, caller registers fakes.
    pub fn empty_for_test(default: ProviderId) -> Self {
        Self {
            default,
            providers: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::api::ProviderId;
    use crate::provider::test_support::FakeProvider;

    #[test]
    fn registry_defaults_to_omp() {
        let config = AmuxConfig::default();
        let reg = ProviderRegistry::from_config(&config).unwrap();
        assert_eq!(reg.default_id(), ProviderId::OMP);
        // Can get the OMP instance.
        let p = reg.get(ProviderId::OMP).unwrap();
        assert_eq!(p.id(), ProviderId::OMP);
    }

    #[test]
    fn registry_rejects_duplicate_provider_id() {
        let mut reg = ProviderRegistry {
            default: ProviderId::OMP,
            providers: HashMap::new(),
        };
        let (fake, _) = FakeProvider::new(ProviderId::new("fake"));
        reg.register(Box::new(fake)).unwrap();
        // Second registration of same ID fails.
        let (dup, _) = FakeProvider::new(ProviderId::new("fake"));
        let err = reg.register(Box::new(dup)).err().unwrap();
        assert!(format!("{err}").contains("fake"));
        // Original still there.
        assert!(reg.get(ProviderId::new("fake")).is_ok());
    }

    #[test]
    fn registry_reports_unknown_provider() {
        let reg = ProviderRegistry {
            default: ProviderId::OMP,
            providers: HashMap::new(),
        };
        let unknown = ProviderId::new("nonexistent");
        assert!(reg.get(unknown).is_err());
        let err = reg.get(unknown).err().unwrap();
        assert!(format!("{err}").contains("nonexistent"));
    }
}
