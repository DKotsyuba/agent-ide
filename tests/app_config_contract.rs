//! Contract checks for immutable, provenance-carrying Application configuration.

use std::num::NonZeroU64;
use std::time::Duration;

use agent_ide::app::config::{
    AppConfigPatch, ConfigError, ConfigKey, ConfigLayer, ConfigOrigin, effective_config,
};

/// Proves named ceiling merging records the layer that actually narrowed each effective setting.
#[test]
fn lower_authority_layers_narrow_without_expanding_host_ceilings() {
    let config = effective_config(
        NonZeroU64::new(7).unwrap(),
        &[
            ConfigLayer {
                origin: ConfigOrigin::Host,
                values: AppConfigPatch {
                    ipc_max_connections: Some(4),
                    store_request_deadline: Some(Duration::from_secs(1)),
                    ..Default::default()
                },
            },
            ConfigLayer {
                origin: ConfigOrigin::Project,
                values: AppConfigPatch {
                    ipc_max_connections: Some(12),
                    store_queue_capacity: Some(3),
                    ..Default::default()
                },
            },
        ],
    )
    .unwrap();
    assert_eq!(config.generation().get(), 7);
    assert_eq!(config.ipc().max_connections, 4);
    assert_eq!(config.store().queue_capacity, 3);
    assert_eq!(
        config.provenance().origin_for(ConfigKey::IpcMaxConnections),
        ConfigOrigin::Host
    );
    assert_eq!(
        config
            .provenance()
            .origin_for(ConfigKey::StoreQueueCapacity),
        ConfigOrigin::Project
    );
}

/// Rejects unsafe zero limits and unordered layers instead of silently keeping a previous value.
#[test]
fn invalid_layers_are_rejected() {
    let generation = NonZeroU64::new(1).unwrap();
    assert_eq!(
        effective_config(
            generation,
            &[ConfigLayer {
                origin: ConfigOrigin::User,
                values: AppConfigPatch {
                    store_queue_capacity: Some(0),
                    ..Default::default()
                },
            }],
        ),
        Err(ConfigError::ZeroLimit {
            key: ConfigKey::StoreQueueCapacity
        })
    );
    assert_eq!(
        effective_config(
            generation,
            &[
                ConfigLayer {
                    origin: ConfigOrigin::Session,
                    values: AppConfigPatch::default(),
                },
                ConfigLayer {
                    origin: ConfigOrigin::Project,
                    values: AppConfigPatch::default(),
                },
            ],
        ),
        Err(ConfigError::InvalidLayerOrder)
    );
}
