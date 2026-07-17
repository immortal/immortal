//! Dependency validation and deterministic concurrent start-wave planning.
//!
//! Only enabled services participate in the graph. Each `requires` edge must
//! target another enabled desired service, then a stable topological pass groups
//! services whose dependencies have already appeared in earlier waves. Cycles and
//! unavailable dependencies are reported before launch ordering reaches callers.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display, Formatter},
};

use crate::config::ServiceConfig;

/// Deterministic groups of independent services which may start concurrently.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DependencyPlan {
    /// Ordered waves. Every dependency of a wave appears in an earlier wave.
    pub waves: Vec<Vec<String>>,
}

/// Invalid desired dependency graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DependencyError {
    /// Required definition is missing or explicitly disabled.
    Unavailable {
        /// Service with the requirement.
        service: String,
        /// Missing or disabled requirement.
        dependency: String,
    },
    /// Enabled definitions contain a dependency cycle.
    Cycle {
        /// Sorted services which could not be topologically ordered.
        services: Vec<String>,
    },
}

impl Display for DependencyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable {
                service,
                dependency,
            } => write!(
                formatter,
                "service `{service}` requires unavailable service `{dependency}`"
            ),
            Self::Cycle { services } => {
                write!(formatter, "dependency cycle among: {}", services.join(", "))
            }
        }
    }
}

impl Error for DependencyError {}

/// Validate enabled-service dependencies and compute concurrent start waves.
///
/// `requires` gates initial starts only. This plan intentionally says nothing
/// about cascading stops after a dependency later becomes unavailable.
///
/// # Errors
///
/// Returns an error for a missing/disabled dependency or any cycle.
pub fn dependency_plan(
    desired: &BTreeMap<String, ServiceConfig>,
) -> Result<DependencyPlan, DependencyError> {
    let enabled: BTreeSet<&str> = desired
        .iter()
        .filter_map(|(name, config)| config.enabled.then_some(name.as_str()))
        .collect();
    let mut remaining_requirements = BTreeMap::new();
    let mut dependents: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

    for (name, config) in desired.iter().filter(|(_, config)| config.enabled) {
        for dependency in &config.requires {
            if !enabled.contains(dependency.as_str()) {
                return Err(DependencyError::Unavailable {
                    service: name.clone(),
                    dependency: dependency.clone(),
                });
            }
            dependents
                .entry(dependency)
                .or_default()
                .insert(name.as_str());
        }
        remaining_requirements.insert(name.as_str(), config.requires.len());
    }

    let mut ready: Vec<&str> = remaining_requirements
        .iter()
        .filter_map(|(name, count)| (*count == 0).then_some(*name))
        .collect();
    let mut waves = Vec::new();
    let mut scheduled = 0_usize;
    while !ready.is_empty() {
        ready.sort_unstable();
        let wave = std::mem::take(&mut ready);
        scheduled = scheduled.saturating_add(wave.len());
        let mut next = BTreeSet::new();
        for completed in &wave {
            if let Some(children) = dependents.get(completed) {
                for child in children {
                    if let Some(count) = remaining_requirements.get_mut(child) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            next.insert(*child);
                        }
                    }
                }
            }
        }
        waves.push(wave.into_iter().map(ToOwned::to_owned).collect());
        ready.extend(next);
    }

    if scheduled == remaining_requirements.len() {
        Ok(DependencyPlan { waves })
    } else {
        let services = remaining_requirements
            .into_iter()
            .filter(|(_, count)| *count != 0)
            .map(|(name, _)| name.to_owned())
            .collect();
        Err(DependencyError::Cycle { services })
    }
}
