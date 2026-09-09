//! Document-path rebasing: the critical invariant that turns an indexer's
//! root-local document paths into repo-root-relative ones so a merged
//! index is addressed from a single root.
//!
//! For each document, with `root_rel` the root dir relative to the repo
//! (`""` for the repo root), the rules are tried in order:
//!   1. candidate = lexical-normalize(root_rel / doc.relative_path)
//!   2. if `repo/candidate` exists -> use candidate
//!   3. else if `repo/doc.relative_path` exists -> keep it as-is (the
//!      indexer already emitted a repo-relative path)
//!   4. else if doc.relative_path is absolute and under `repo` -> strip the
//!      repo prefix
//!   5. else -> count it into `unmapped_documents`, but still keep the
//!      document under `candidate` (content is never dropped; the count is
//!      the honesty signal)
//!
//! Normalization is purely lexical -- `./` is dropped and `../` segments
//! are resolved by popping, never by touching the filesystem -- so a path
//! that escapes the repo stays escaped rather than silently resolving
//! through a symlink.

use std::path::{Component, Path, PathBuf};

use scip::types::Index;

/// Outcome of rebasing one index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RebaseStats {
    /// Documents that matched none of rules 2-4 (kept anyway, under the
    /// normalized candidate path).
    pub unmapped_documents: u32,
}

/// Rebase every document path in `index` in place. `root_rel` is the
/// root's dir relative to `repo` (`""` for the repo root).
pub fn rebase_index(index: &mut Index, root_rel: &Path, repo: &Path) -> RebaseStats {
    let mut stats = RebaseStats::default();
    for doc in &mut index.documents {
        let outcome = rebase_path(&doc.relative_path, root_rel, repo);
        if !outcome.mapped {
            stats.unmapped_documents += 1;
        }
        doc.relative_path = outcome.path;
    }
    stats
}

struct PathOutcome {
    path: String,
    mapped: bool,
}

fn rebase_path(original: &str, root_rel: &Path, repo: &Path) -> PathOutcome {
    let original_path = Path::new(original);
    let candidate = normalize_lexical(&root_rel.join(original_path));

    // Rule 2: the rebased candidate exists on disk. Only a relative
    // candidate is a valid repo-relative path; an absolute original makes
    // the join absolute, which rule 4 handles instead.
    if candidate.is_relative() && repo.join(&candidate).exists() {
        return PathOutcome {
            path: to_slash(&candidate),
            mapped: true,
        };
    }

    // Rule 3: the indexer already emitted a repo-relative path that exists.
    if original_path.is_relative() && repo.join(original_path).exists() {
        return PathOutcome {
            path: original.to_string(),
            mapped: true,
        };
    }

    // Rule 4: an absolute path under the repo -> strip the repo prefix.
    if original_path.is_absolute()
        && let Ok(stripped) = original_path.strip_prefix(repo)
    {
        return PathOutcome {
            path: to_slash(stripped),
            mapped: true,
        };
    }

    // Rule 5: unmapped. Keep the content under the normalized candidate.
    PathOutcome {
        path: to_slash(&candidate),
        mapped: false,
    }
}

/// Lexically normalize a path: drop `.` components and resolve `..` by
/// popping a preceding normal component. Never touches the filesystem.
pub fn normalize_lexical(p: &Path) -> PathBuf {
    let mut stack: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                // Can't climb above the repo root; a root-anchored `..`
                // just stays at root.
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                // Leading or stacked `..` (relative path escaping upward):
                // preserve it lexically.
                _ => stack.push(comp),
            },
            other => stack.push(other),
        }
    }
    let mut out = PathBuf::new();
    for comp in stack {
        out.push(comp.as_os_str());
    }
    out
}

/// Render a path with `/` separators (SCIP document paths are
/// forward-slash, regardless of host OS).
fn to_slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(rel: &str) -> scip::types::Document {
        let mut d = scip::types::Document::new();
        d.relative_path = rel.to_string();
        d
    }

    fn index_with(paths: &[&str]) -> Index {
        let mut idx = Index::new();
        for p in paths {
            idx.documents.push(doc(p));
        }
        idx
    }

    #[test]
    fn normalize_strips_curdir_and_resolves_parentdir() {
        assert_eq!(normalize_lexical(Path::new("./a/b")), PathBuf::from("a/b"));
        assert_eq!(
            normalize_lexical(Path::new("a/b/../c")),
            PathBuf::from("a/c")
        );
        assert_eq!(
            normalize_lexical(Path::new("backend/../shared/x.py")),
            PathBuf::from("shared/x.py")
        );
    }

    #[test]
    fn normalize_preserves_leading_parentdir() {
        assert_eq!(normalize_lexical(Path::new("../x")), PathBuf::from("../x"));
    }

    // Rule 1/2: root-prefixed candidate that exists.
    #[test]
    fn rule2_prefixes_root_rel_when_candidate_exists() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("backend/src")).unwrap();
        std::fs::write(repo.path().join("backend/src/app.py"), b"").unwrap();
        let mut idx = index_with(&["src/app.py"]);
        let stats = rebase_index(&mut idx, Path::new("backend"), repo.path());
        assert_eq!(idx.documents[0].relative_path, "backend/src/app.py");
        assert_eq!(stats.unmapped_documents, 0);
    }

    // Rule 3: indexer already emitted a repo-relative path.
    #[test]
    fn rule3_keeps_already_repo_relative_path() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("backend/src")).unwrap();
        std::fs::write(repo.path().join("backend/src/app.py"), b"").unwrap();
        // root_rel is "backend" but the indexer emitted the full repo path;
        // candidate "backend/backend/src/app.py" won't exist, so rule 3 keeps it.
        let mut idx = index_with(&["backend/src/app.py"]);
        let stats = rebase_index(&mut idx, Path::new("backend"), repo.path());
        assert_eq!(idx.documents[0].relative_path, "backend/src/app.py");
        assert_eq!(stats.unmapped_documents, 0);
    }

    // Rule 4: absolute path under the repo gets the prefix stripped.
    #[test]
    fn rule4_strips_absolute_repo_prefix() {
        let repo = tempfile::tempdir().unwrap();
        let abs = repo.path().join("pkg/main.go");
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, b"").unwrap();
        // Give a non-existent root_rel so the candidate (absolute) doesn't
        // resolve via rule 2, forcing rule 4.
        let mut idx = index_with(&[abs.to_str().unwrap()]);
        let stats = rebase_index(&mut idx, Path::new("gosvc"), repo.path());
        assert_eq!(idx.documents[0].relative_path, "pkg/main.go");
        assert_eq!(stats.unmapped_documents, 0);
    }

    // Rule 5: nothing matches -> counted, kept under candidate.
    #[test]
    fn rule5_counts_unmapped_but_keeps_content() {
        let repo = tempfile::tempdir().unwrap();
        let mut idx = index_with(&["src/ghost.py"]);
        let stats = rebase_index(&mut idx, Path::new("backend"), repo.path());
        // Kept under the normalized candidate even though nothing exists.
        assert_eq!(idx.documents[0].relative_path, "backend/src/ghost.py");
        assert_eq!(stats.unmapped_documents, 1);
    }

    #[test]
    fn parentdir_escape_maps_when_target_exists() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(repo.path().join("shared/util.py"), b"").unwrap();
        let mut idx = index_with(&["../shared/util.py"]);
        let stats = rebase_index(&mut idx, Path::new("backend"), repo.path());
        assert_eq!(idx.documents[0].relative_path, "shared/util.py");
        assert_eq!(stats.unmapped_documents, 0);
    }

    #[test]
    fn empty_root_rel_is_repo_root() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("main.py"), b"").unwrap();
        let mut idx = index_with(&["main.py"]);
        let stats = rebase_index(&mut idx, Path::new(""), repo.path());
        assert_eq!(idx.documents[0].relative_path, "main.py");
        assert_eq!(stats.unmapped_documents, 0);
    }
}
