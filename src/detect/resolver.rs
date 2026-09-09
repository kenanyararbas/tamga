//! Generic, family-agnostic subsume/dedup of candidates into roots.
//!
//! Per family, candidates are processed shallowest-first (fewest path
//! components, lexicographic tie-break). The first is accepted; each later
//! candidate is swallowed by the nearest accepted same-family ancestor
//! that `subsumes` it, otherwise accepted as an independent root.
//! Cross-family candidates never interact. The final root order is
//! `(dir, family)` lexicographic — two runs on one tree are byte-identical.

use std::collections::BTreeMap;
use std::path::Path;

use crate::detect::{ResolvedRoot, RootCandidate, RootStrength};
use crate::families::{Family, FamilyId, root_id_with};

/// Resolve candidates into the final, deterministically-ordered roots.
pub fn resolve(
    candidates: Vec<RootCandidate>,
    families: &[Box<dyn Family>],
    repo: &Path,
) -> Vec<ResolvedRoot> {
    let mut by_family: BTreeMap<FamilyId, Vec<RootCandidate>> = BTreeMap::new();
    for c in candidates {
        by_family.entry(c.family).or_default().push(c);
    }

    let mut resolved: Vec<ResolvedRoot> = Vec::new();
    for (fam_id, mut cands) in by_family {
        let family = families
            .iter()
            .find(|f| f.id() == fam_id)
            .expect("candidate from an unregistered family");

        // Shallowest-first; lexicographic tie-break. Both are hard
        // determinism requirements.
        cands.sort_by(|a, b| {
            depth(&a.dir)
                .cmp(&depth(&b.dir))
                .then_with(|| a.dir.cmp(&b.dir))
        });

        let mut accepted: Vec<ResolvedRoot> = Vec::new();
        for c in cands {
            // Nearest (deepest) accepted same-family ancestor that subsumes.
            let mut best: Option<usize> = None;
            let mut best_depth = 0usize;
            for (i, r) in accepted.iter().enumerate() {
                if is_strict_ancestor(&r.candidate.dir, &c.dir)
                    && family.subsumes(&r.candidate, &c, repo)
                {
                    let d = depth(&r.candidate.dir);
                    if best.is_none() || d > best_depth {
                        best = Some(i);
                        best_depth = d;
                    }
                }
            }
            match best {
                Some(i) => {
                    let reason = family
                        .subsume_reason(&accepted[i].candidate, &c)
                        .unwrap_or_else(|| generic_subsume_reason(&accepted[i].candidate, &c));
                    accepted[i].subsumed.push((c.dir.clone(), reason));
                }
                None => {
                    let disc = family.root_discriminator(&c);
                    let id = root_id_with(&c.dir, c.family, disc.as_deref());
                    accepted.push(ResolvedRoot {
                        id,
                        candidate: c,
                        subsumed: Vec::new(),
                    });
                }
            }
        }

        for r in &mut accepted {
            r.subsumed.sort_by(|a, b| a.0.cmp(&b.0));
        }
        resolved.extend(accepted);
    }

    // Final order: (dir, family) lexicographic.
    resolved.sort_by(|a, b| {
        dir_key(a)
            .cmp(&dir_key(b))
            .then_with(|| a.candidate.family.slug().cmp(b.candidate.family.slug()))
    });
    resolved
}

fn dir_key(root: &ResolvedRoot) -> String {
    root.candidate.dir.to_string_lossy().replace('\\', "/")
}

fn depth(dir: &Path) -> usize {
    dir.components().count()
}

/// `anc` is a strict path ancestor of `desc` (the repo root `""` is an
/// ancestor of every non-root dir).
fn is_strict_ancestor(anc: &Path, desc: &Path) -> bool {
    if anc == desc {
        return false;
    }
    if anc.as_os_str().is_empty() {
        return !desc.as_os_str().is_empty();
    }
    desc.starts_with(anc)
}

/// Human-readable reason a child was swallowed, derived generically from
/// the ancestor/child strengths -- the fallback when a family's
/// `subsume_reason` has nothing more specific to say.
fn generic_subsume_reason(ancestor: &RootCandidate, child: &RootCandidate) -> String {
    let anc = ancestor_label(&ancestor.dir);
    if ancestor.strength == RootStrength::Workspace {
        format!("workspace member of {anc}")
    } else {
        format!("nested {} root under {anc}", child.strength.lower())
    }
}

fn ancestor_label(dir: &Path) -> String {
    if dir.as_os_str().is_empty() {
        "<repo root>".to_string()
    } else {
        dir.to_string_lossy().replace('\\', "/")
    }
}
