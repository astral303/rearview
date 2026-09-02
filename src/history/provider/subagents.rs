//! Sessions and their sub-agent transcripts, from the parent each transcript
//! names.
//!
//! Codex and OpenCode record parentage on the sub-agent transcript: a rollout
//! header names its `parent_thread_id`, a session row its `parent_id`.
//! Discovery turns those links around here, so every session is listed once
//! with the sub-agent transcripts merged into it, nested ones included.

use std::collections::{BTreeMap, BTreeSet};

/// Every transcript a provider found, each with the parent it names.
pub(crate) struct SubagentForest<Id: Ord + Clone> {
    parent_of: BTreeMap<Id, Option<Id>>,
    direct_subagents_of: BTreeMap<Id, Vec<Id>>,
}

impl<Id: Ord + Clone> SubagentForest<Id> {
    pub(crate) fn new(transcripts: impl IntoIterator<Item = (Id, Option<Id>)>) -> Self {
        let parent_of: BTreeMap<Id, Option<Id>> = transcripts.into_iter().collect();
        let mut direct_subagents_of: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
        for (id, parent) in &parent_of {
            if let Some(parent) = parent {
                direct_subagents_of
                    .entry(parent.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        for subagents in direct_subagents_of.values_mut() {
            subagents.sort();
        }
        Self {
            parent_of,
            direct_subagents_of,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.parent_of.len()
    }

    pub(crate) fn contains(&self, id: &Id) -> bool {
        self.parent_of.contains_key(id)
    }

    /// The same forest without `ids` and everything beneath them.
    pub(crate) fn without(&self, ids: impl IntoIterator<Item = Id>) -> Self {
        let mut removed = BTreeSet::new();
        for id in ids {
            removed.extend(self.subagents_of(&id));
            removed.insert(id);
        }
        Self::new(
            self.parent_of
                .iter()
                .filter(|(id, _)| !removed.contains(*id))
                .map(|(id, parent)| (id.clone(), parent.clone())),
        )
    }

    /// Every session with its sub-agent transcripts, in id order. A transcript
    /// is a session when it names no parent or names one the forest does not
    /// hold. A chain of parents that loops back on itself reaches no session
    /// that way, so its first id in order becomes one.
    pub(crate) fn sessions(&self) -> Vec<(Id, Vec<Id>)> {
        let mut visited = BTreeSet::new();
        let mut sessions = Vec::new();
        let roots = self
            .parent_of
            .iter()
            .filter(|(_, parent)| {
                parent
                    .as_ref()
                    .is_none_or(|parent| !self.parent_of.contains_key(parent))
            })
            .map(|(id, _)| id.clone());
        let leftovers = self.parent_of.keys().cloned();
        for id in roots.chain(leftovers) {
            if !visited.insert(id.clone()) {
                continue;
            }
            let subagents = self.descend(&id, &mut visited);
            sessions.push((id, subagents));
        }
        sessions
    }

    /// The sub-agent transcripts beneath `id`, nested ones included, in id
    /// order. A chain that loops back stops at the first repeated id.
    pub(crate) fn subagents_of(&self, id: &Id) -> Vec<Id> {
        let mut visited = BTreeSet::from([id.clone()]);
        self.descend(id, &mut visited)
    }

    fn descend(&self, id: &Id, visited: &mut BTreeSet<Id>) -> Vec<Id> {
        let mut frontier = vec![id.clone()];
        let mut subagents = Vec::new();
        while let Some(current) = frontier.pop() {
            for subagent in self.direct_subagents_of.get(&current).into_iter().flatten() {
                if !visited.insert(subagent.clone()) {
                    continue;
                }
                frontier.push(subagent.clone());
                subagents.push(subagent.clone());
            }
        }
        subagents.sort();
        subagents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forest(transcripts: &[(&str, Option<&str>)]) -> SubagentForest<String> {
        SubagentForest::new(
            transcripts
                .iter()
                .map(|(id, parent)| ((*id).to_owned(), parent.map(str::to_owned))),
        )
    }

    #[test]
    fn a_session_lists_once_with_its_nested_sub_agents() {
        let forest = forest(&[
            ("root", None),
            ("middle", Some("root")),
            ("leaf", Some("middle")),
            ("other", None),
        ]);

        assert_eq!(
            forest.sessions(),
            vec![
                ("other".to_owned(), vec![]),
                (
                    "root".to_owned(),
                    vec!["leaf".to_owned(), "middle".to_owned()]
                ),
            ]
        );
        assert_eq!(forest.subagents_of(&"middle".to_owned()), vec!["leaf"]);
    }

    /// Hiding a transcript whose parent is missing would make it unreachable,
    /// since nothing else lists it.
    #[test]
    fn a_sub_agent_whose_parent_is_absent_is_a_session() {
        let forest = forest(&[("orphan", Some("absent")), ("nested", Some("orphan"))]);

        assert_eq!(
            forest.sessions(),
            vec![("orphan".to_owned(), vec!["nested".to_owned()])]
        );
    }

    #[test]
    fn a_chain_that_loops_back_still_lists_once() {
        let forest = forest(&[
            ("first", Some("second")),
            ("second", Some("first")),
            ("its_own_parent", Some("its_own_parent")),
        ]);

        assert_eq!(
            forest.sessions(),
            vec![
                ("first".to_owned(), vec!["second".to_owned()]),
                ("its_own_parent".to_owned(), vec![]),
            ]
        );
    }

    #[test]
    fn without_drops_the_named_transcripts_and_everything_beneath_them() {
        let forest = forest(&[
            ("root", None),
            ("review", Some("root")),
            ("under_review", Some("review")),
            ("worker", Some("root")),
        ])
        .without(["review".to_owned()]);

        assert_eq!(
            forest.sessions(),
            vec![("root".to_owned(), vec!["worker".to_owned()])]
        );
    }
}
