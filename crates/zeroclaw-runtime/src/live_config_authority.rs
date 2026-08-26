use std::sync::Arc;

use parking_lot::RwLock;
use zeroclaw_config::schema::Config;

/// The live configuration state shared by one supervised daemon generation.
///
/// The write lock is deliberately paired with the config Arc so every
/// mutation path uses the same serialization witness as the live state.
#[derive(Clone)]
pub struct LiveConfigAuthority {
    config: Arc<RwLock<Config>>,
    config_write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl LiveConfigAuthority {
    /// Create the authority for one daemon generation.
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Pair an existing live config handle with a local mutation witness.
    ///
    /// This preserves standalone callers that already own an `Arc<RwLock<Config>>`
    /// without claiming that their config participates in a supervised daemon's
    /// shared mutation domain.
    pub fn from_config(config: Arc<RwLock<Config>>) -> Self {
        Self {
            config,
            config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Return the live config Arc shared by all consumers of this authority.
    pub fn config(&self) -> Arc<RwLock<Config>> {
        Arc::clone(&self.config)
    }

    /// Return the mutation witness shared by all consumers of this authority.
    pub fn config_write_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.config_write_lock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloned_authority_preserves_config_and_write_lock_identity() {
        let authority = LiveConfigAuthority::new(Config::default());
        let cloned = authority.clone();

        assert!(Arc::ptr_eq(&authority.config(), &cloned.config()));
        assert!(Arc::ptr_eq(
            &authority.config_write_lock(),
            &cloned.config_write_lock()
        ));
    }

    #[test]
    fn from_config_preserves_config_and_allocates_local_write_lock() {
        let config = Arc::new(RwLock::new(Config::default()));
        let authority = LiveConfigAuthority::from_config(Arc::clone(&config));
        let other = LiveConfigAuthority::from_config(config.clone());

        assert!(Arc::ptr_eq(&config, &authority.config()));
        assert!(!Arc::ptr_eq(
            &authority.config_write_lock(),
            &other.config_write_lock()
        ));
    }
}
