//! Working out what order to tackle issues in.
//!
//! Two sources of truth, in order of trust:
//!
//! 1. GitHub's native issue dependencies (`blocked_by` / `blocking`), when the
//!    repo has them.
//! 2. Prose in the issue body — "Blocked by #3", "Depends on #7" — which is how
//!    most repositories actually record this.
//!
//! The result is a topological order. Ties break on the order the issues were
//! given on the command line, so a run is reproducible and the queue rain
//! prints is the queue rain works.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::sync::OnceLock;

use regex::Regex;

/// "`blocked` cannot start until `blocker` is done."
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Relation {
    pub blocked: u64,
    pub blocker: u64,
    pub source: RelationSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RelationSource {
    /// GitHub's issue dependencies API.
    Forge,
    /// Parsed out of the issue body.
    Body,
}

impl RelationSource {
    pub fn label(self) -> &'static str {
        match self {
            RelationSource::Forge => "github dependencies",
            RelationSource::Body => "issue body",
        }
    }
}

/// The queue, plus everything a human needs to understand why it looks like this.
#[derive(Debug, Default)]
pub struct Sequenced {
    pub order: Vec<u64>,
    /// `blocked -> blockers`, restricted to issues in this run.
    pub edges: BTreeMap<u64, BTreeSet<u64>>,
    pub warnings: Vec<String>,
}

fn blocked_by_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:blocked[\s_\-]*by|depends?[\s_\-]+(?:on|upon)|dependent[\s_\-]+(?:on|upon)|requires?)\b",
        )
        .expect("static regex")
    })
}

fn blocks_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(?:blocks|blocking)\b").expect("static regex"))
}

fn issue_ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:https?://[^\s]*?/issues/(?P<url>\d+))|(?:#(?P<hash>\d+))")
            .expect("static regex")
    })
}

/// Read relationship prose out of one issue body.
pub fn extract_from_body(number: u64, body: &str) -> Vec<Relation> {
    let mut out = Vec::new();
    let mut in_code_fence = false;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence || line.is_empty() {
            continue;
        }

        let blocked_at = blocked_by_re().find(line);
        let blocks_at = blocks_re().find(line);

        // A line can only express one relationship direction; if both keywords
        // appear, the earlier one wins ("Blocked by #1, which blocks #2" is
        // ambiguous enough that guessing further would be worse than stopping).
        let (is_blocked_by, m) = match (blocked_at, blocks_at) {
            (Some(a), Some(b)) => {
                if a.start() <= b.start() {
                    (true, a)
                } else {
                    (false, b)
                }
            }
            (Some(a), None) => (true, a),
            (None, Some(b)) => (false, b),
            (None, None) => continue,
        };

        for other in issue_refs(clause(&line[m.end()..])) {
            if other == number {
                continue;
            }
            let relation = if is_blocked_by {
                Relation {
                    blocked: number,
                    blocker: other,
                    source: RelationSource::Body,
                }
            } else {
                Relation {
                    blocked: other,
                    blocker: number,
                    source: RelationSource::Body,
                }
            };
            out.push(relation);
        }
    }

    out.sort();
    out.dedup();
    out
}

/// Trim the text after a keyword to the clause it belongs to, so
/// "Blocked by #1. See also #9." does not pick up #9.
fn clause(rest: &str) -> &str {
    let mut end = rest.len();
    for (idx, ch) in rest.char_indices() {
        let is_break = matches!(ch, ';')
            || (ch == '.' && rest[idx + 1..].starts_with(char::is_whitespace))
            || (ch == '.' && idx + 1 == rest.len());
        if is_break {
            end = idx;
            break;
        }
    }
    &rest[..end]
}

fn issue_refs(text: &str) -> Vec<u64> {
    issue_ref_re()
        .captures_iter(text)
        .filter_map(|c| {
            c.name("url")
                .or_else(|| c.name("hash"))
                .and_then(|m| m.as_str().parse().ok())
        })
        .collect()
}

/// Topologically order `requested` under `relations`.
///
/// Relations pointing outside the requested set are reported and dropped —
/// rain cannot act on an issue it was not asked to work on, and silently
/// refusing to schedule would be worse than proceeding with a warning.
pub fn sequence(requested: &[u64], relations: &[Relation]) -> Sequenced {
    let present: BTreeSet<u64> = requested.iter().copied().collect();
    let rank: BTreeMap<u64, usize> = requested.iter().enumerate().map(|(i, n)| (*n, i)).collect();

    let mut result = Sequenced::default();
    let mut blockers: BTreeMap<u64, BTreeSet<u64>> =
        present.iter().map(|n| (*n, BTreeSet::new())).collect();
    let mut dependents: BTreeMap<u64, BTreeSet<u64>> =
        present.iter().map(|n| (*n, BTreeSet::new())).collect();

    let mut dropped: BTreeSet<(u64, u64)> = BTreeSet::new();
    for rel in relations {
        if rel.blocked == rel.blocker {
            continue;
        }
        if !present.contains(&rel.blocked) || !present.contains(&rel.blocker) {
            dropped.insert((rel.blocked, rel.blocker));
            continue;
        }
        blockers
            .get_mut(&rel.blocked)
            .expect("present")
            .insert(rel.blocker);
        dependents
            .get_mut(&rel.blocker)
            .expect("present")
            .insert(rel.blocked);
    }

    for (blocked, blocker) in dropped {
        let missing = if present.contains(&blocked) {
            blocker
        } else {
            blocked
        };
        result.warnings.push(format!(
            "#{blocked} is related to #{blocker}, but #{missing} is not in this run — the relationship is ignored"
        ));
    }

    result.edges = blockers.clone();

    // Kahn's algorithm, with the command-line order as the tie-breaker.
    let mut ready: BinaryHeap<Reverse<(usize, u64)>> = blockers
        .iter()
        .filter(|(_, b)| b.is_empty())
        .map(|(n, _)| Reverse((rank[n], *n)))
        .collect();

    let mut remaining = blockers.clone();
    while let Some(Reverse((_, next))) = ready.pop() {
        result.order.push(next);
        remaining.remove(&next);
        for dependent in &dependents[&next] {
            if let Some(set) = remaining.get_mut(dependent) {
                set.remove(&next);
                if set.is_empty() {
                    ready.push(Reverse((rank[dependent], *dependent)));
                }
            }
        }
    }

    if !remaining.is_empty() {
        let mut stuck: Vec<u64> = remaining.keys().copied().collect();
        stuck.sort_by_key(|n| rank[n]);
        result.warnings.push(format!(
            "circular dependency between {} — falling back to the order they were given in",
            stuck
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        result.order.extend(stuck);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_rel(blocked: u64, blocker: u64) -> Relation {
        Relation {
            blocked,
            blocker,
            source: RelationSource::Body,
        }
    }

    #[test]
    fn reads_blocked_by_prose() {
        let rels = extract_from_body(2, "Blocked by #1");
        assert_eq!(rels, vec![body_rel(2, 1)]);
    }

    #[test]
    fn reads_several_refs_in_one_clause() {
        let rels = extract_from_body(4, "Depends on #1, #2 and #3");
        assert_eq!(rels, vec![body_rel(4, 1), body_rel(4, 2), body_rel(4, 3)]);
    }

    #[test]
    fn reads_the_blocking_direction() {
        let rels = extract_from_body(1, "This blocks #2 and #3.");
        assert_eq!(rels, vec![body_rel(2, 1), body_rel(3, 1)]);
    }

    #[test]
    fn stops_at_the_end_of_the_clause() {
        let rels = extract_from_body(5, "Blocked by #1. See also #9 for background.");
        assert_eq!(rels, vec![body_rel(5, 1)]);
    }

    #[test]
    fn reads_full_issue_urls() {
        let rels = extract_from_body(7, "Depends on https://github.com/o/n/issues/3");
        assert_eq!(rels, vec![body_rel(7, 3)]);
    }

    #[test]
    fn ignores_fenced_code() {
        let body = "```\nblocked by #99\n```\nBlocked by #2";
        assert_eq!(extract_from_body(5, body), vec![body_rel(5, 2)]);
    }

    #[test]
    fn ignores_plain_mentions() {
        assert!(extract_from_body(5, "Related to #2, see #3").is_empty());
        assert!(extract_from_body(5, "This is a follow-up to #2").is_empty());
    }

    #[test]
    fn ignores_self_reference() {
        assert!(extract_from_body(5, "Blocked by #5").is_empty());
    }

    #[test]
    fn orders_by_dependency() {
        let rels = vec![body_rel(1, 2), body_rel(2, 3)];
        let seq = sequence(&[1, 2, 3], &rels);
        assert_eq!(seq.order, vec![3, 2, 1]);
        assert!(seq.warnings.is_empty());
    }

    #[test]
    fn keeps_command_line_order_when_independent() {
        let seq = sequence(&[7, 3, 5], &[]);
        assert_eq!(seq.order, vec![7, 3, 5]);
    }

    #[test]
    fn breaks_ties_on_command_line_order() {
        // 1 and 2 both free; 3 waits on 1. Given as 2, 1, 3.
        let seq = sequence(&[2, 1, 3], &[body_rel(3, 1)]);
        assert_eq!(seq.order, vec![2, 1, 3]);
    }

    #[test]
    fn warns_about_relations_outside_the_run() {
        let seq = sequence(&[1], &[body_rel(1, 42)]);
        assert_eq!(seq.order, vec![1]);
        assert_eq!(seq.warnings.len(), 1);
        assert!(seq.warnings[0].contains("#42"));
    }

    #[test]
    fn survives_a_cycle() {
        let seq = sequence(&[1, 2], &[body_rel(1, 2), body_rel(2, 1)]);
        assert_eq!(seq.order, vec![1, 2]);
        assert_eq!(seq.warnings.len(), 1);
        assert!(seq.warnings[0].contains("circular"));
    }

    #[test]
    fn every_issue_is_scheduled_exactly_once() {
        let rels = vec![
            body_rel(2, 1),
            body_rel(3, 1),
            body_rel(4, 2),
            body_rel(4, 3),
        ];
        let seq = sequence(&[4, 3, 2, 1], &rels);
        let mut sorted = seq.order.clone();
        sorted.sort();
        assert_eq!(sorted, vec![1, 2, 3, 4]);
        assert_eq!(seq.order.first(), Some(&1));
        assert_eq!(seq.order.last(), Some(&4));
    }
}
