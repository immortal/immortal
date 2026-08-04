//! Dependency validation and deterministic concurrent start-wave planning.
//!
//! Only enabled services participate in the graph. Each `requires` edge should
//! target another enabled desired service, then a stable topological pass groups
//! services whose dependencies have already appeared in earlier waves.
//!
//! Planning is total: an unavailable requirement or a cycle removes only the
//! affected service and whatever transitively requires it, and the remaining
//! graph is still scheduled. A single broken definition therefore cannot stop a
//! reconciliation pass for every unrelated service, which is what an all-or-
//! nothing plan did. Callers report the skipped set as isolated per-service
//! failures.

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
    /// Enabled services excluded from the waves, sorted by service name.
    pub unresolvable: Vec<UnresolvableService>,
}

/// One enabled service which cannot be scheduled, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnresolvableService {
    /// Enabled desired service which will not be started.
    pub service: String,
    /// Reason the service was excluded from the start waves.
    pub reason: DependencyError,
}

/// Reason one enabled service cannot be scheduled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DependencyError {
    /// Required definition is missing or explicitly disabled.
    Unavailable {
        /// Missing or disabled requirement.
        dependency: String,
    },
    /// Required definition exists but is itself unresolvable.
    Blocked {
        /// Requirement which cannot be scheduled.
        dependency: String,
    },
    /// The service belongs to a dependency cycle.
    Cycle {
        /// Sorted services which could not be topologically ordered.
        services: Vec<String>,
    },
}

impl Display for DependencyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { dependency } => {
                write!(formatter, "requires unavailable service `{dependency}`")
            }
            Self::Blocked { dependency } => write!(
                formatter,
                "requires service `{dependency}`, which cannot start"
            ),
            Self::Cycle { services } => {
                write!(formatter, "dependency cycle among: {}", services.join(", "))
            }
        }
    }
}

impl Error for DependencyError {}

/// Compute concurrent start waves and the enabled services which cannot start.
///
/// `requires` gates initial starts only. This plan intentionally says nothing
/// about cascading stops after a dependency later becomes unavailable.
///
/// Planning never fails: a service whose requirement is missing, disabled, or
/// itself unresolvable is reported in [`DependencyPlan::unresolvable`] together
/// with every service that transitively requires it, and cycle members are
/// reported the same way. Everything else is still scheduled, so one broken
/// definition cannot stop an entire reconciliation pass.
#[must_use]
pub fn dependency_plan(desired: &BTreeMap<String, ServiceConfig>) -> DependencyPlan {
    let enabled: BTreeSet<&str> = desired
        .iter()
        .filter_map(|(name, config)| config.enabled.then_some(name.as_str()))
        .collect();
    let mut unresolvable: BTreeMap<&str, DependencyError> = BTreeMap::new();
    let mut remaining_requirements = BTreeMap::new();
    let mut dependents: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

    for (name, config) in desired.iter().filter(|(_, config)| config.enabled) {
        for dependency in &config.requires {
            if !enabled.contains(dependency.as_str()) {
                unresolvable
                    .entry(name.as_str())
                    .or_insert_with(|| DependencyError::Unavailable {
                        dependency: dependency.clone(),
                    });
                continue;
            }
            dependents
                .entry(dependency.as_str())
                .or_default()
                .insert(name.as_str());
        }
        remaining_requirements.insert(name.as_str(), config.requires.len());
    }

    propagate_unresolvable(&dependents, &mut unresolvable);

    let mut ready: Vec<&str> = remaining_requirements
        .iter()
        .filter_map(|(name, count)| {
            (*count == 0 && !unresolvable.contains_key(*name)).then_some(*name)
        })
        .collect();
    let mut waves = Vec::new();
    let mut scheduled: BTreeSet<&str> = BTreeSet::new();
    while !ready.is_empty() {
        ready.sort_unstable();
        let wave = std::mem::take(&mut ready);
        scheduled.extend(wave.iter().copied());
        let mut next = BTreeSet::new();
        for completed in &wave {
            let Some(children) = dependents.get(completed) else {
                continue;
            };
            for child in children {
                let Some(count) = remaining_requirements.get_mut(child) else {
                    continue;
                };
                *count = count.saturating_sub(1);
                if *count == 0 && !unresolvable.contains_key(*child) {
                    next.insert(*child);
                }
            }
        }
        waves.push(wave.into_iter().map(ToOwned::to_owned).collect());
        ready.extend(next);
    }

    // Anything still unscheduled and not already excluded belongs to a cycle,
    // because every acyclic service reachable from a ready root was drained
    // above.
    let stalled: Vec<&str> = remaining_requirements
        .keys()
        .filter(|name| !scheduled.contains(*name) && !unresolvable.contains_key(*name))
        .copied()
        .collect();
    record_cycle(&stalled, &mut unresolvable);

    DependencyPlan {
        waves,
        unresolvable: unresolvable
            .into_iter()
            .map(|(service, reason)| UnresolvableService {
                service: service.to_owned(),
                reason,
            })
            .collect(),
    }
}

/// Exclude every service which transitively requires an unresolvable service.
///
/// Iterating to a fixed point over the dependent edges keeps the result
/// independent of definition order, so the reported set is deterministic.
fn propagate_unresolvable<'graph>(
    dependents: &BTreeMap<&'graph str, BTreeSet<&'graph str>>,
    unresolvable: &mut BTreeMap<&'graph str, DependencyError>,
) {
    loop {
        let mut discovered: BTreeMap<&str, DependencyError> = BTreeMap::new();
        for (dependency, children) in dependents {
            if !unresolvable.contains_key(dependency) {
                continue;
            }
            for child in children {
                if unresolvable.contains_key(child) || discovered.contains_key(child) {
                    continue;
                }
                discovered.insert(
                    child,
                    DependencyError::Blocked {
                        dependency: (*dependency).to_owned(),
                    },
                );
            }
        }
        if discovered.is_empty() {
            return;
        }
        unresolvable.extend(discovered);
    }
}

/// Report every remaining service as a member of the surviving cycle.
fn record_cycle<'graph>(
    stalled: &[&'graph str],
    unresolvable: &mut BTreeMap<&'graph str, DependencyError>,
) {
    if stalled.is_empty() {
        return;
    }
    let services: Vec<String> = stalled.iter().map(|name| (*name).to_owned()).collect();
    for name in stalled {
        unresolvable.insert(
            name,
            DependencyError::Cycle {
                services: services.clone(),
            },
        );
    }
}
