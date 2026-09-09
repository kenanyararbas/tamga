//! Loading, merging, and writing SCIP indexes.
//!
//! The pipeline parses each root's `.scip` output in isolation (a malformed
//! output degrades only its own root), rebases the document paths
//! ([`rebase`]), then merges every root's documents and external symbols
//! into one [`Index`] rooted at the repo. Duplicate document paths across
//! roots are counted but both copies are kept -- dropping content is never
//! correct, and the count is the signal that something overlapped.

pub mod rebase;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use protobuf::{Message, MessageField};
use scip::types::{Index, Metadata, ToolInfo};

/// Errors from reading/decoding/writing a SCIP index file.
#[derive(Debug, thiserror::Error)]
pub enum ScipError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed SCIP output in {path}: {source}")]
    Decode {
        path: String,
        #[source]
        source: protobuf::Error,
    },
    #[error("failed to encode SCIP index: {0}")]
    Encode(#[source] protobuf::Error),
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Parse a SCIP index from raw protobuf bytes.
pub fn parse_index_bytes(bytes: &[u8]) -> Result<Index, protobuf::Error> {
    Index::parse_from_bytes(bytes)
}

/// Read and decode a SCIP index from a file. Read and decode failures are
/// distinct variants so the caller can tell "no output" from "malformed
/// output".
pub fn read_index(path: &Path) -> Result<Index, ScipError> {
    let bytes = std::fs::read(path).map_err(|source| ScipError::Read {
        path: path.display().to_string(),
        source,
    })?;
    parse_index_bytes(&bytes).map_err(|source| ScipError::Decode {
        path: path.display().to_string(),
        source,
    })
}

/// Serialize a SCIP index to a file.
pub fn write_index(path: &Path, index: &Index) -> Result<(), ScipError> {
    let bytes = index.write_to_bytes().map_err(ScipError::Encode)?;
    std::fs::write(path, bytes).map_err(|source| ScipError::Write {
        path: path.display().to_string(),
        source,
    })
}

/// Document and occurrence counts for a parsed index.
pub fn index_stats(index: &Index) -> (u32, u32) {
    let documents = index.documents.len() as u32;
    let occurrences = index
        .documents
        .iter()
        .map(|d| d.occurrences.len() as u32)
        .sum();
    (documents, occurrences)
}

/// The result of merging several rooted indexes.
#[derive(Debug)]
pub struct MergeOutput {
    pub index: Index,
    /// Document `relative_path`s that appeared in more than one input
    /// (counted once per extra copy; all copies are kept).
    pub duplicate_documents: u32,
}

/// Merge already-rebased indexes into one index rooted at `repo_abs`.
/// `metadata.project_root` is set to `file://<repo_abs>`; protocol version
/// and text encoding are inherited from the first input that carries them.
pub fn merge_indices(inputs: Vec<Index>, repo_abs: &Path) -> MergeOutput {
    let mut merged = Index::new();

    let mut metadata = Metadata::new();
    metadata.project_root = format!("file://{}", repo_abs.display());
    if let Some(first) = inputs.iter().find(|i| i.metadata.is_some()) {
        let src = first.metadata.as_ref().unwrap();
        metadata.version = src.version;
        metadata.text_document_encoding = src.text_document_encoding;
    }
    let mut tool = ToolInfo::new();
    tool.name = "tamga".to_string();
    tool.version = env!("CARGO_PKG_VERSION").to_string();
    metadata.tool_info = MessageField::some(tool);
    merged.metadata = MessageField::some(metadata);

    let mut seen: HashSet<String> = HashSet::new();
    let mut duplicate_documents = 0u32;
    for input in inputs {
        for doc in input.documents {
            if !seen.insert(doc.relative_path.clone()) {
                duplicate_documents += 1;
            }
            merged.documents.push(doc);
        }
        for ext in input.external_symbols {
            merged.external_symbols.push(ext);
        }
    }

    MergeOutput {
        index: merged,
        duplicate_documents,
    }
}

/// Absolute form of a repo-root argument, falling back to the path as
/// given if it can't be canonicalized (e.g. it doesn't exist).
fn absolutize(repo_root: &Path) -> PathBuf {
    std::fs::canonicalize(repo_root).unwrap_or_else(|_| {
        if repo_root.is_absolute() {
            repo_root.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(repo_root))
                .unwrap_or_else(|_| repo_root.to_path_buf())
        }
    })
}

/// Back the standalone `tamga merge a b --repo-root P -o out` subcommand:
/// load both inputs, rebase each as a root at the repo root, merge, and
/// write. Returns the process exit code (0 success, 2 bad input / write
/// failure).
pub fn run_merge(a: &Path, b: &Path, repo_root: &Path, output: &Path) -> i32 {
    let repo_abs = absolutize(repo_root);

    let mut inputs = Vec::new();
    for path in [a, b] {
        match read_index(path) {
            Ok(mut index) => {
                // Each input is treated as a root at the repo root: rebase
                // with an empty root-rel (rules 2-5 only).
                rebase::rebase_index(&mut index, Path::new(""), &repo_abs);
                inputs.push(index);
            }
            Err(e) => {
                eprintln!("tamga merge: {e}");
                return 2;
            }
        }
    }

    let merged = merge_indices(inputs, &repo_abs);
    match write_index(output, &merged.index) {
        Ok(()) => {
            println!(
                "merged {} documents into {}",
                merged.index.documents.len(),
                output.display()
            );
            0
        }
        Err(e) => {
            eprintln!("tamga merge: {e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scip::types::{Document, Occurrence};

    fn doc(rel: &str) -> Document {
        let mut d = Document::new();
        d.relative_path = rel.to_string();
        let mut occ = Occurrence::new();
        occ.range = vec![0, 0, 1];
        d.occurrences.push(occ);
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
    fn merge_counts_duplicate_paths_and_keeps_both() {
        let a = index_with(&["src/a.py", "src/shared.py"]);
        let b = index_with(&["src/b.py", "src/shared.py"]);
        let merged = merge_indices(vec![a, b], Path::new("/repo"));
        assert_eq!(merged.duplicate_documents, 1);
        assert_eq!(merged.index.documents.len(), 4); // both copies kept
    }

    #[test]
    fn merge_sets_project_root_to_file_uri() {
        let merged = merge_indices(vec![index_with(&["x.py"])], Path::new("/repo/abs"));
        assert_eq!(
            merged.index.metadata.project_root,
            "file:///repo/abs".to_string()
        );
    }

    #[test]
    fn index_stats_counts_documents_and_occurrences() {
        let idx = index_with(&["a.py", "b.py"]);
        let (docs, occs) = index_stats(&idx);
        assert_eq!(docs, 2);
        assert_eq!(occs, 2);
    }

    #[test]
    fn parse_rejects_garbage_bytes() {
        // Arbitrary non-protobuf bytes must fail to decode, isolating a
        // malformed root rather than poisoning the merge.
        let garbage = b"\xff\xff\xff\xffnot a scip index at all";
        assert!(parse_index_bytes(garbage).is_err());
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.scip");
        let idx = index_with(&["a.py"]);
        write_index(&path, &idx).unwrap();
        let back = read_index(&path).unwrap();
        assert_eq!(back.documents.len(), 1);
        assert_eq!(back.documents[0].relative_path, "a.py");
    }
}
