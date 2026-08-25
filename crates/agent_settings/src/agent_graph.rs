//! Validation of the profile delegation graph.
//!
//! Profiles declare outgoing delegation edges via `delegation.allowed`; this
//! module checks the whole graph when settings load so that misconfiguration
//! (cycles, references to unknown profiles) is reported up front instead of
//! surfacing as confusing `spawn_agent` failures at runtime.

use collections::{HashMap, IndexMap};

use crate::{AgentProfileId, AgentProfileSettings};

/// Validates the delegation graph. Returns one human-readable message per
/// problem found; empty means the configuration is valid.
///
/// Profile id uniqueness is guaranteed by construction: profiles are stored in
/// an `IndexMap` keyed by id, and project-level settings override user-level
/// entries with the same id rather than duplicating them.
pub fn validate_profiles(profiles: &IndexMap<AgentProfileId, AgentProfileSettings>) -> Vec<String> {
    let mut errors = Vec::new();

    for (id, profile) in profiles {
        let Some(delegation) = profile.delegation.as_ref() else {
            continue;
        };
        if delegation.allowed.is_empty() {
            errors.push(format!(
                "profile '{}' has a delegation block with an empty 'allowed' list",
                id
            ));
            continue;
        }
        for target in &delegation.allowed {
            if !profiles.contains_key(target) {
                errors.push(format!(
                    "profile '{}' delegates to unknown profile '{}'",
                    id, target
                ));
            }
        }
    }

    errors.extend(detect_cycles(profiles));

    errors
}

/// Checks one `spawn_agent` call against the parent profile's delegation
/// rules. Returns a model-facing error message when the call must be
/// rejected, or `None` when it may proceed.
///
/// `parent_depth` is the depth of the calling thread (0 = root). A profile
/// may spawn while `parent_depth < delegation.max_depth`, so the default of 1
/// lets a root agent spawn one level of children that cannot delegate
/// further.
pub fn check_delegation(
    parent_profile_id: &AgentProfileId,
    parent_profile: Option<&AgentProfileSettings>,
    parent_depth: u8,
    requested: Option<&AgentProfileId>,
) -> Option<String> {
    let Some(profile) = parent_profile else {
        return Some(format!(
            "Unknown profile '{}'. Fix the profile configuration and try again.",
            parent_profile_id
        ));
    };
    let Some(delegation) = profile.delegation.as_ref() else {
        return Some(format!(
            "Profile '{}' is a solo agent: it has no 'delegation' block, so it cannot spawn \
             other agents. Complete the task yourself.",
            parent_profile_id
        ));
    };
    let Some(target) = requested else {
        let allowed: Vec<String> = delegation.allowed.iter().map(|id| id.to_string()).collect();
        return Some(format!(
            "Profile '{}' must delegate with an explicit profile. Allowed profiles: {}",
            parent_profile_id,
            allowed.join(", ")
        ));
    };
    if !delegation.allowed.contains(target) {
        let allowed: Vec<String> = delegation.allowed.iter().map(|id| id.to_string()).collect();
        return Some(format!(
            "Profile '{}' is not allowed to spawn profile '{}'. Allowed profiles: {}",
            parent_profile_id,
            target,
            allowed.join(", ")
        ));
    }
    if parent_depth >= delegation.max_depth {
        return Some(format!(
            "Maximum delegation depth ({}) for profile '{}' reached. Complete the task \
             yourself instead of delegating further.",
            delegation.max_depth, parent_profile_id
        ));
    }
    None
}

/// Finds cycles in the delegation graph via iterative depth-first search,
/// reporting every cycle once with its participants.
fn detect_cycles(profiles: &IndexMap<AgentProfileId, AgentProfileSettings>) -> Vec<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum NodeState {
        Unvisited,
        InProgress,
        Done,
    }

    let mut states: HashMap<&AgentProfileId, NodeState> =
        profiles.keys().map(|id| (id, NodeState::Unvisited)).collect();
    let mut errors = Vec::new();

    for start in profiles.keys() {
        if states.get(start) != Some(&NodeState::Unvisited) {
            continue;
        }
        // Stack of (node, next-edge-index) pairs so very deep graphs cannot
        // overflow the real stack.
        let mut stack: Vec<(&AgentProfileId, usize)> = vec![(start, 0)];
        states.insert(start, NodeState::InProgress);
        while let Some(&mut (node, ref mut edge_ix)) = stack.last_mut() {
            let edges: &[AgentProfileId] = profiles[node]
                .delegation
                .as_ref()
                .map(|delegation| delegation.allowed.as_slice())
                .unwrap_or(&[]);
            if *edge_ix >= edges.len() {
                states.insert(node, NodeState::Done);
                stack.pop();
                continue;
            }
            let target = &edges[*edge_ix];
            *edge_ix += 1;
            match states.get(target) {
                Some(NodeState::InProgress) => {
                    let cycle_start = stack
                        .iter()
                        .position(|&(id, _)| id == target)
                        .unwrap_or(0);
                    let participants: Vec<String> = stack[cycle_start..]
                        .iter()
                        .map(|&(id, _)| id.to_string())
                        .chain(std::iter::once(target.to_string()))
                        .collect();
                    errors.push(format!(
                        "delegation cycle detected: {}",
                        participants.join(" -> ")
                    ));
                }
                Some(NodeState::Unvisited) => {
                    states.insert(target, NodeState::InProgress);
                    stack.push((target, 0));
                }
                _ => {}
            }
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use collections::IndexMap as Map;

    fn profile_with_delegation(allowed: &[&str]) -> AgentProfileSettings {
        AgentProfileSettings {
            delegation: Some(crate::Delegation {
                allowed: allowed.iter().map(|id| AgentProfileId((*id).into())).collect(),
                ..Default::default()
            }),
            ..test_profile()
        }
    }

    fn test_profile() -> AgentProfileSettings {
        AgentProfileSettings {
            name: "test".into(),
            tools: IndexMap::default(),
            enable_all_context_servers: false,
            context_servers: IndexMap::default(),
            default_model: None,
            custom_prompt: None,
            description: None,
            skills: None,
            delegation: None,
        }
    }

    fn profiles(
        entries: Vec<(&str, AgentProfileSettings)>,
    ) -> IndexMap<AgentProfileId, AgentProfileSettings> {
        entries
            .into_iter()
            .map(|(id, profile)| (AgentProfileId(id.into()), profile))
            .collect()
    }

    #[test]
    fn valid_graph_passes() {
        let map = profiles(vec![
            ("root", profile_with_delegation(&["child"])),
            ("child", test_profile()),
        ]);
        assert!(validate_profiles(&map).is_empty());
    }

    #[test]
    fn dangling_reference_is_reported() {
        let map = profiles(vec![("root", profile_with_delegation(&["ghost"]))]);
        let errors = validate_profiles(&map);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("unknown profile 'ghost'"));
    }

    #[test]
    fn empty_allowed_list_is_reported() {
        let map = profiles(vec![("root", profile_with_delegation(&[]))]);
        let errors = validate_profiles(&map);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty 'allowed'"));
    }

    #[test]
    fn two_node_cycle_is_reported_with_participants() {
        let map = profiles(vec![
            ("a", profile_with_delegation(&["b"])),
            ("b", profile_with_delegation(&["a"])),
        ]);
        let errors = validate_profiles(&map);
        assert_eq!(errors.len(), 1, "cycle reported once: {errors:?}");
        assert!(errors[0].contains("a -> b -> a"), "unexpected: {}", errors[0]);
    }

    #[test]
    fn self_cycle_is_reported() {
        let map = profiles(vec![("a", profile_with_delegation(&["a"]))]);
        let errors = validate_profiles(&map);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("a -> a"));
    }

    #[test]
    fn longer_cycle_is_reported() {
        let map = profiles(vec![
            ("a", profile_with_delegation(&["b"])),
            ("b", profile_with_delegation(&["c"])),
            ("c", profile_with_delegation(&["a"])),
        ]);
        let errors = validate_profiles(&map);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("a -> b -> c -> a"), "unexpected: {}", errors[0]);
    }

    #[test]
    fn deep_chain_without_cycle_passes() {
        let entries: Vec<(&str, AgentProfileSettings)> = (0..100)
            .map(|i| {
                let id: &str = if i == 0 {
                    "root"
                } else {
                    // Leaked strings keep the test simple; bounded by 100.
                    Box::leak(format!("level-{i}").into_boxed_str())
                };
                if i == 99 {
                    (id, test_profile())
                } else {
                    let target: &str =
                        Box::leak(format!("level-{}", i + 1).into_boxed_str());
                    (id, profile_with_delegation(&[target]))
                }
            })
            .collect();
        assert!(validate_profiles(&profiles(entries)).is_empty());
    }

    fn id(name: &str) -> AgentProfileId {
        AgentProfileId(name.into())
    }

    #[test]
    fn check_delegation_allows_listed_target_within_depth() {
        let parent = profile_with_delegation(&["child"]);
        assert_eq!(
            check_delegation(&id("parent"), Some(&parent), 0, Some(&id("child"))),
            None
        );
    }

    #[test]
    fn check_delegation_rejects_solo_profile() {
        let parent = test_profile();
        let error = check_delegation(&id("parent"), Some(&parent), 0, Some(&id("child")))
            .expect("solo profile must be rejected");
        assert!(error.contains("solo agent"), "unexpected: {error}");
    }

    #[test]
    fn check_delegation_rejects_target_not_in_allowed() {
        let parent = profile_with_delegation(&["child"]);
        let error = check_delegation(&id("parent"), Some(&parent), 0, Some(&id("other")))
            .expect("unlisted target must be rejected");
        assert!(error.contains("'other'"), "unexpected: {error}");
        assert!(error.contains("Allowed profiles: child"), "unexpected: {error}");
    }

    #[test]
    fn check_delegation_requires_explicit_target() {
        let parent = profile_with_delegation(&["child"]);
        let error = check_delegation(&id("parent"), Some(&parent), 0, None)
            .expect("missing target must be rejected");
        assert!(error.contains("explicit profile"), "unexpected: {error}");
    }

    #[test]
    fn check_delegation_enforces_max_depth() {
        let parent = profile_with_delegation(&["child"]);
        // max_depth defaults to 1: an agent already at depth 1 cannot spawn.
        let error = check_delegation(&id("parent"), Some(&parent), 1, Some(&id("child")))
            .expect("depth limit must be enforced");
        assert!(error.contains("depth"), "unexpected: {error}");

        let deep = crate::Delegation {
            allowed: vec![id("child")],
            max_depth: 2,
        };
        let mut parent = test_profile();
        parent.delegation = Some(deep);
        assert_eq!(
            check_delegation(&id("parent"), Some(&parent), 1, Some(&id("child"))),
            None
        );
    }

    #[test]
    fn check_delegation_rejects_unknown_parent_profile() {
        let error =
            check_delegation(&id("ghost"), None, 0, Some(&id("child")))
                .expect("unknown parent profile must be rejected");
        assert!(error.contains("Unknown profile"), "unexpected: {error}");
    }
}
