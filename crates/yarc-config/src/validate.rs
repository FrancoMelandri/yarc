use std::collections::{BTreeMap, BTreeSet};

use crate::{Condition, ConfigError, Project};

pub(crate) fn validate(p: &Project) -> Result<(), ConfigError> {
    if p.services.is_empty() {
        return Err(ConfigError::NoServices);
    }
    for (name, svc) in &p.services {
        for (target, cond) in &svc.needs {
            if target == name {
                return Err(ConfigError::SelfNeed(name.clone()));
            }
            let dep = p.services.get(target).ok_or_else(|| ConfigError::UnknownNeed {
                service: name.clone(),
                target: target.clone(),
            })?;
            if *cond == Condition::Healthy && dep.health.is_none() {
                return Err(ConfigError::NeedsHealthWithoutCheck {
                    service: name.clone(),
                    target: target.clone(),
                });
            }
        }
    }
    check_names(p)?;
    check_cycles(p)
}

/// A `container_name` is an extra network name: it must not shadow another service's
/// name or another service's `container_name` (its own service name is fine).
fn check_names(p: &Project) -> Result<(), ConfigError> {
    let mut taken: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, svc) in &p.services {
        let Some(cn) = svc.container_name.as_deref() else { continue };
        let collides = (cn != name && p.services.contains_key(cn))
            || taken.insert(cn, name).is_some_and(|other| other != name);
        if collides {
            return Err(ConfigError::NameCollision { service: name.clone(), name: cn.to_string() });
        }
    }
    Ok(())
}

fn check_cycles(p: &Project) -> Result<(), ConfigError> {
    // Kahn's algorithm: whatever cannot be removed is part of / behind a cycle.
    let mut indegree: BTreeMap<&str, usize> = p
        .services
        .iter()
        .map(|(n, s)| (n.as_str(), s.needs.len()))
        .collect();
    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut done: BTreeSet<&str> = BTreeSet::new();
    while let Some(n) = ready.pop() {
        done.insert(n);
        for (name, svc) in &p.services {
            if svc.needs.contains_key(n) {
                let d = indegree.get_mut(name.as_str()).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(name);
                }
            }
        }
    }
    match p.services.keys().find(|n| !done.contains(n.as_str())) {
        Some(n) => Err(ConfigError::Cycle(n.clone())),
        None => Ok(()),
    }
}
