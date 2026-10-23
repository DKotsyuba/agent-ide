//! Operator-selected test concurrency; the engine owns its workers and test isolation.

use crate::lang::environment::CommandEnv;
use serde::{Deserialize, Deserializer};
use std::num::NonZeroU16;

/// Restart-only launcher configuration; absence preserves the project's defaults.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestExecutionConfig {
    /// A count from 1 through 256, or `max` for a load-aware per-invocation estimate.
    pub workers: Workers,
}

/// A fixed count or a conservative estimate, never an unlimited worker pool.
#[derive(Clone, Debug)]
pub enum Workers {
    /// Operator-selected worker count (configuration accepts 1 through 256).
    Count(NonZeroU16),
    /// Available logical CPUs minus half the recent runnable load, rounded up.
    Max,
}

impl<'de> Deserialize<'de> for Workers {
    /// Rejects zero, oversized counts, fractional values and unknown strings.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Input {
            Count(u16),
            Name(String),
        }
        match Input::deserialize(deserializer)? {
            Input::Count(count @ 1..=256) => Ok(Self::Count(NonZeroU16::new(count).unwrap())),
            Input::Name(name) if name == "max" => Ok(Self::Max),
            _ => Err(serde::de::Error::custom(
                "test workers must be 1..256 or \"max\"",
            )),
        }
    }
}

impl Workers {
    /// Resolves once for the invocation; load is a one-minute estimate, not a reservation.
    /// Unknown CPU capacity or load uses one worker. Existing runs are never resized.
    pub fn resolve(&self) -> NonZeroU16 {
        match self {
            Self::Count(count) => NonZeroU16::new(count.get().min(256)).unwrap(),
            Self::Max => estimate(
                std::thread::available_parallelism().ok().map(usize::from),
                load(),
            ),
        }
    }
}

/// Keeps at least one worker and at most 256; invalid samples fail conservatively.
fn estimate(cpus: Option<usize>, load: Option<f64>) -> NonZeroU16 {
    let count = match (cpus, load) {
        (Some(cpus), Some(load)) if cpus > 0 && load.is_finite() && load >= 0.0 => cpus
            .saturating_sub((load / 2.0).ceil() as usize)
            .clamp(1, 256),
        _ => 1,
    };
    NonZeroU16::new(count as u16).unwrap()
}

/// Reads the OS load without subprocesses, sleeps or a global mutable sampler.
fn load() -> Option<f64> {
    #[cfg(unix)]
    {
        let mut value = 0.0;
        // SAFETY: a valid exclusive pointer to one initialized double is supplied for one
        // result; getloadavg does not retain it and reports whether it wrote that result.
        (unsafe { libc::getloadavg(&mut value, 1) } == 1).then_some(value)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Language-declared engine option, also represented by a closed module recipe parameter.
#[derive(Clone, Debug)]
pub enum WorkerSetting {
    /// Native runner environment option; actual replay retains its value.
    Environment { name: &'static str, value: String },
    /// Appends the worker option while preserving existing option-string variables.
    EnvironmentOptions { name: &'static str, value: String },
    /// Native runner CLI option placed before the generated selection arguments.
    Argument { prefix: &'static str, value: String },
}

impl WorkerSetting {
    /// The value admitted under the recipe's optional `test_workers` parameter.
    pub fn value(&self) -> &str {
        match self {
            Self::Environment { value, .. }
            | Self::EnvironmentOptions { value, .. }
            | Self::Argument { value, .. } => value,
        }
    }

    /// Adds the option to the already resolved native command, preserving other settings.
    /// Called only for nonempty generated commands claimed by the language adapter.
    pub fn apply(self, argv: &[String], env: Option<CommandEnv>) -> CommandEnv {
        let mut env = env.unwrap_or_else(|| CommandEnv {
            argv_prefix: vec![argv[0].clone().into()],
            path_prefix: None,
            vars: Vec::new(),
        });
        match self {
            Self::Environment { name, value } => {
                env.vars.retain(|(key, _)| key != name);
                env.vars.push((name.into(), value.into()));
            }
            Self::EnvironmentOptions { name, value } => {
                let existing = env
                    .vars
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
                    .or_else(|| std::env::var_os(name));
                let mut combined = existing.unwrap_or_default();
                if !combined.is_empty() {
                    combined.push(" ");
                }
                combined.push(value);
                env.vars.retain(|(key, _)| key != name);
                env.vars.push((name.into(), combined));
            }
            Self::Argument { prefix, value } => {
                env.argv_prefix.push(format!("{prefix}{value}").into())
            }
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No malformed user setting becomes an unbounded or silently defaulted pool.
    #[test]
    fn configuration_is_closed_and_bounded() {
        for text in ["0", "257", "-1", "1.5", "\"auto\"", "null", "true"] {
            assert!(serde_json::from_str::<Workers>(text).is_err(), "{text}");
        }
        for count in [1, 4, 256] {
            let workers: Workers = serde_json::from_str(&count.to_string()).unwrap();
            assert_eq!(workers.resolve().get(), count);
        }
        assert!(matches!(
            serde_json::from_str::<Workers>("\"max\"").unwrap(),
            Workers::Max
        ));
        assert!(
            serde_json::from_str::<TestExecutionConfig>(r#"{"workers":4,"extra":true}"#).is_err()
        );
    }

    /// Capacity shrinks with load and stays finite for missing, invalid and overloaded samples.
    #[test]
    fn max_discounts_smoothed_load_without_an_extra_reserved_cpu() {
        assert_eq!(estimate(Some(8), Some(0.0)).get(), 8);
        assert_eq!(estimate(Some(8), Some(2.2)).get(), 6);
        assert_eq!(estimate(Some(8), Some(50.0)).get(), 1);
        assert_eq!(estimate(Some(10), Some(3.89)).get(), 8);
        assert_eq!(estimate(Some(10), Some(6.0)).get(), 7);
        assert_eq!(estimate(Some(1), Some(0.0)).get(), 1);
        for load in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-1.0)] {
            assert_eq!(estimate(Some(8), load).get(), 1);
        }
        assert_eq!(estimate(None, Some(0.0)).get(), 1);
        assert_eq!(estimate(Some(1000), Some(0.0)).get(), 256);
    }
}
