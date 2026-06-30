use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// A file entry: physical relative path + tags derived from directory components.
pub struct FileEntry {
    /// Physical path relative to root (e.g., "tag1/tag2/file.txt")
    pub ffn: PathBuf,
    /// Set of tags (directory components of the path)
    pub tags: BTreeSet<String>,
}

/// Result of scanning a source directory.
pub struct ScanResult {
    /// basename (possibly with .__N suffix) -> FileEntry
    pub files: HashMap<String, FileEntry>,
    /// sorted tag set -> set of relative directory paths
    pub tagdirs: HashMap<BTreeSet<String>, HashSet<PathBuf>>,
    /// single tag -> set of basenames that have this tag
    pub by_tags: HashMap<String, HashSet<String>>,
}

/// Extract path components as tag strings, ignoring non-Normal components.
fn components_to_tags(path: &Path) -> BTreeSet<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect()
}

/// For a file path like "tag1/tag2/file.txt", extract tags from parent dir -> {tag1, tag2}.
fn file_path_to_tags(path: &Path) -> BTreeSet<String> {
    match path.parent() {
        Some(parent) => components_to_tags(parent),
        None => BTreeSet::new(),
    }
}

/// For a directory path like "tag1/tag2", all components are tags -> {tag1, tag2}.
fn dir_path_to_tags(path: &Path) -> BTreeSet<String> {
    components_to_tags(path)
}

/// Split filename into (stem, extension_with_dot), matching Python's os.path.splitext.
/// "file.txt" -> ("file", ".txt"), "file.tar.gz" -> ("file.tar", ".gz"), "file" -> ("file", "")
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(pos) if pos > 0 => (&name[..pos], &name[pos..]),
        _ => (name, ""),
    }
}

/// Resolve basename collision by appending .__N before extension.
/// "file.txt" with existing "file.txt" -> "file.__1.txt"
pub fn find_free_bn(bn: &str, existing: &HashSet<String>) -> String {
    if !existing.contains(bn) {
        return bn.to_string();
    }
    let (root, ext) = split_ext(bn);
    let mut i = 1u32;
    loop {
        let candidate = format!("{}.__{}{}", root, i, ext);
        if !existing.contains(&candidate) {
            return candidate;
        }
        i += 1;
    }
}

/// Recursively scan a directory, populating the ScanResult.
fn scan_dir(root: &Path, rel_dir: &Path, show_hidden: bool, result: &mut ScanResult) {
    let full_path = root.join(rel_dir);
    log::debug!("Entering directory: {:?}", full_path);
    let entries = match fs::read_dir(&full_path) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("Cannot read directory {:?}: {}", full_path, e);
            return;
        }
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if !show_hidden && name_str.starts_with('.') {
            continue;
        }

        let rel_path = if rel_dir.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            rel_dir.join(&name)
        };

        // Follow symlinks for type classification (like Python's entry.is_dir()/is_file())
        let metadata = match fs::metadata(root.join(&rel_path)) {
            Ok(m) => m,
            Err(_) => {
                log::debug!("Skipping broken symlink: {:?}", rel_path);
                continue;
            }
        };

        // Check real type (without following symlinks) for recursion decision
        let is_real_dir = fs::symlink_metadata(root.join(&rel_path))
            .map(|m| m.is_dir())
            .unwrap_or(false);

        if metadata.is_dir() {
            let tags = dir_path_to_tags(&rel_path);
            log::debug!("Directory: {:?}, tags: {:?}", rel_path, tags);
            result
                .tagdirs
                .entry(tags.clone())
                .or_default()
                .insert(rel_path.clone());
            for tag in &tags {
                result.by_tags.entry(tag.clone()).or_default();
            }
            // Only recurse into real directories, not symlinks (avoids infinite loops)
            if is_real_dir {
                scan_dir(root, &rel_path, show_hidden, result);
            } else {
                log::debug!("Not recursing into symlinked directory: {:?}", rel_path);
            }
        } else if metadata.is_file() {
            let tags = file_path_to_tags(&rel_path);
            let original_bn = name_str.to_string();
            let existing_keys: HashSet<String> = result.files.keys().cloned().collect();
            let bn = find_free_bn(&original_bn, &existing_keys);
            if bn != original_bn {
                log::debug!("Collision resolved: {:?} -> {:?}", original_bn, bn);
            }
            log::debug!("File: {:?}, basename: {:?}, tags: {:?}", rel_path, bn, tags);

            let dirname = rel_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();

            result
                .tagdirs
                .entry(tags.clone())
                .or_default()
                .insert(dirname);

            for tag in &tags {
                result
                    .by_tags
                    .entry(tag.clone())
                    .or_default()
                    .insert(bn.clone());
            }

            result.files.insert(
                bn,
                FileEntry {
                    ffn: rel_path,
                    tags,
                },
            );
        }
    }
}

/// Scan the source directory tree and build tag-based data structures.
pub fn scan_tree(root: &Path, show_hidden: bool) -> ScanResult {
    let mut result = ScanResult {
        files: HashMap::new(),
        tagdirs: HashMap::new(),
        by_tags: HashMap::new(),
    };
    scan_dir(root, Path::new(""), show_hidden, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs as unix_fs;
    use tempfile::tempdir;

    // --- split_ext ---

    #[test]
    fn split_ext_basic() {
        assert_eq!(split_ext("file.txt"), ("file", ".txt"));
    }

    #[test]
    fn split_ext_double() {
        assert_eq!(split_ext("file.tar.gz"), ("file.tar", ".gz"));
    }

    #[test]
    fn split_ext_none() {
        assert_eq!(split_ext("file"), ("file", ""));
    }

    #[test]
    fn split_ext_hidden() {
        assert_eq!(split_ext(".hidden"), (".hidden", ""));
    }

    #[test]
    fn split_ext_trailing_dot() {
        assert_eq!(split_ext("file."), ("file", "."));
    }

    // --- components_to_tags ---

    #[test]
    fn components_to_tags_basic() {
        let tags = components_to_tags(Path::new("tag1/tag2"));
        assert_eq!(tags, BTreeSet::from(["tag1".into(), "tag2".into()]));
    }

    #[test]
    fn components_to_tags_empty() {
        let tags = components_to_tags(Path::new(""));
        assert_eq!(tags, BTreeSet::new());
    }

    #[test]
    fn components_to_tags_single() {
        let tags = components_to_tags(Path::new("tag1"));
        assert_eq!(tags, BTreeSet::from(["tag1".into()]));
    }

    // --- file_path_to_tags ---

    #[test]
    fn file_path_to_tags_nested() {
        let tags = file_path_to_tags(Path::new("a/b/file.txt"));
        assert_eq!(tags, BTreeSet::from(["a".into(), "b".into()]));
    }

    #[test]
    fn file_path_to_tags_root() {
        let tags = file_path_to_tags(Path::new("file.txt"));
        assert_eq!(tags, BTreeSet::new());
    }

    #[test]
    fn file_path_to_tags_deep() {
        let tags = file_path_to_tags(Path::new("a/b/c/d/file.txt"));
        assert_eq!(
            tags,
            BTreeSet::from(["a".into(), "b".into(), "c".into(), "d".into()])
        );
    }

    // --- dir_path_to_tags ---

    #[test]
    fn dir_path_to_tags_basic() {
        let tags = dir_path_to_tags(Path::new("tag1/tag2"));
        assert_eq!(tags, BTreeSet::from(["tag1".into(), "tag2".into()]));
    }

    #[test]
    fn dir_path_to_tags_single() {
        let tags = dir_path_to_tags(Path::new("tag1"));
        assert_eq!(tags, BTreeSet::from(["tag1".into()]));
    }

    #[test]
    fn dir_path_to_tags_empty() {
        let tags = dir_path_to_tags(Path::new(""));
        assert_eq!(tags, BTreeSet::new());
    }

    // --- find_free_bn ---

    #[test]
    fn find_free_bn_no_collision() {
        let existing = HashSet::new();
        assert_eq!(find_free_bn("file.txt", &existing), "file.txt");
    }

    #[test]
    fn find_free_bn_one_collision() {
        let existing = HashSet::from(["file.txt".into()]);
        assert_eq!(find_free_bn("file.txt", &existing), "file.__1.txt");
    }

    #[test]
    fn find_free_bn_multi_collision() {
        let existing = HashSet::from(["file.txt".into(), "file.__1.txt".into()]);
        assert_eq!(find_free_bn("file.txt", &existing), "file.__2.txt");
    }

    #[test]
    fn find_free_bn_no_ext() {
        let existing = HashSet::from(["README".into()]);
        assert_eq!(find_free_bn("README", &existing), "README.__1");
    }

    #[test]
    fn find_free_bn_double_ext() {
        let existing = HashSet::from(["archive.tar.gz".into()]);
        assert_eq!(
            find_free_bn("archive.tar.gz", &existing),
            "archive.tar.__1.gz"
        );
    }

    // --- scan_tree integration tests ---

    fn create_file(root: &Path, rel: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, "test").unwrap();
    }

    #[test]
    fn scan_basic_structure() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "music/song.mp3");
        create_file(root, "photos/pic.jpg");

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 2);
        assert!(result.files.contains_key("song.mp3"));
        assert!(result.files.contains_key("pic.jpg"));

        assert!(result.by_tags.contains_key("music"));
        assert!(result.by_tags.contains_key("photos"));
        assert!(result.by_tags["music"].contains("song.mp3"));
        assert!(result.by_tags["photos"].contains("pic.jpg"));

        let music_tags = BTreeSet::from(["music".into()]);
        let photos_tags = BTreeSet::from(["photos".into()]);
        assert!(result.tagdirs.contains_key(&music_tags));
        assert!(result.tagdirs.contains_key(&photos_tags));
    }

    #[test]
    fn scan_files_at_root() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "readme.txt");
        create_file(root, "notes.txt");

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 2);
        assert!(result.files.contains_key("readme.txt"));
        assert!(result.files.contains_key("notes.txt"));
        assert!(result.files["readme.txt"].tags.is_empty());
        assert!(result.files["notes.txt"].tags.is_empty());

        // Root files have no tags, so by_tags should be empty
        assert!(result.by_tags.is_empty());

        // tagdirs should have an entry for the empty tag set
        let empty_tags: BTreeSet<String> = BTreeSet::new();
        assert!(result.tagdirs.contains_key(&empty_tags));
    }

    #[test]
    fn scan_deeply_nested() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "a/b/c/d/file.txt");

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 1);
        assert!(result.files.contains_key("file.txt"));
        assert_eq!(
            result.files["file.txt"].tags,
            BTreeSet::from(["a".into(), "b".into(), "c".into(), "d".into()])
        );

        // Intermediate directories should be in tagdirs
        assert!(result.tagdirs.contains_key(&BTreeSet::from(["a".into()])));
        assert!(result
            .tagdirs
            .contains_key(&BTreeSet::from(["a".into(), "b".into()])));
        assert!(result
            .tagdirs
            .contains_key(&BTreeSet::from(["a".into(), "b".into(), "c".into()])));
        assert!(result.tagdirs.contains_key(&BTreeSet::from([
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into()
        ])));
    }

    #[test]
    fn scan_hidden_excluded() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, ".hidden");
        create_file(root, ".dir/secret");
        create_file(root, "visible.txt");

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 1);
        assert!(result.files.contains_key("visible.txt"));
    }

    #[test]
    fn scan_hidden_included() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, ".hidden");
        create_file(root, ".dir/secret");
        create_file(root, "visible.txt");

        let result = scan_tree(root, true);

        assert!(result.files.len() >= 3);
        assert!(result.files.contains_key("visible.txt"));
        assert!(result.files.contains_key(".hidden"));
        assert!(result.files.contains_key("secret"));
    }

    #[test]
    fn scan_empty_dir() {
        let dir = tempdir().unwrap();
        let result = scan_tree(dir.path(), false);

        assert!(result.files.is_empty());
        assert!(result.tagdirs.is_empty());
        assert!(result.by_tags.is_empty());
    }

    #[test]
    fn scan_no_subdirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "a.txt");
        create_file(root, "b.txt");

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 2);
        assert!(result.by_tags.is_empty());
    }

    #[test]
    fn scan_filename_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "music/song.mp3");
        create_file(root, "photos/song.mp3");

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 2);
        // One keeps original name, other gets .__1 suffix
        let keys: HashSet<String> = result.files.keys().cloned().collect();
        assert!(keys.contains("song.mp3"));
        assert!(keys.contains("song.__1.mp3"));
    }

    #[test]
    fn scan_broken_symlink() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "valid.txt");
        unix_fs::symlink("/nonexistent/target", root.join("broken_link")).unwrap();

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 1);
        assert!(result.files.contains_key("valid.txt"));
    }

    #[test]
    fn scan_symlink_to_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "real.txt");
        unix_fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 2);
        assert!(result.files.contains_key("real.txt"));
        assert!(result.files.contains_key("link.txt"));
    }

    #[test]
    fn scan_symlink_to_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "realdir/file.txt");
        unix_fs::symlink(root.join("realdir"), root.join("linkdir")).unwrap();

        let result = scan_tree(root, false);

        // linkdir is treated as a dir (metadata follows symlink) but not recursed
        // So we get file.txt from realdir, and linkdir as a tagdir
        assert!(result.files.contains_key("file.txt"));

        let linkdir_tags = BTreeSet::from(["linkdir".into()]);
        assert!(result.tagdirs.contains_key(&linkdir_tags));

        // Files inside linkdir should NOT be scanned (no recursion into symlink dirs)
        // If recursed, we'd have 2 copies of file.txt
        assert_eq!(result.files.len(), 1);
    }

    // --- Corner case: scanner edge cases ---

    #[test]
    fn scan_duplicate_tag_path_components() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "rock/rock/file.txt");

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 1);
        assert!(result.files.contains_key("file.txt"));
        // BTreeSet deduplicates: rock/rock → tags = {"rock"}
        assert_eq!(
            result.files["file.txt"].tags,
            BTreeSet::from(["rock".into()])
        );
        // tagdirs should have {"rock"} containing both "rock" and "rock/rock" paths
        let rock_tags = BTreeSet::from(["rock".to_string()]);
        assert!(result.tagdirs.contains_key(&rock_tags));
        let rock_dirs = &result.tagdirs[&rock_tags];
        assert!(rock_dirs.contains(&PathBuf::from("rock")));
        assert!(rock_dirs.contains(&PathBuf::from("rock/rock")));
    }

    #[test]
    fn scan_three_way_filename_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "a/file.txt");
        create_file(root, "b/file.txt");
        create_file(root, "c/file.txt");

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 3);
        let keys: HashSet<String> = result.files.keys().cloned().collect();
        assert_eq!(
            keys,
            HashSet::from([
                "file.txt".into(),
                "file.__1.txt".into(),
                "file.__2.txt".into(),
            ])
        );
    }

    #[test]
    fn scan_collision_marker_in_original_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "a/file.txt");
        create_file(root, "b/file.txt");
        create_file(root, "c/file.__1.txt");

        let result = scan_tree(root, false);

        // All 3 files must be present with distinct keys
        assert_eq!(result.files.len(), 3);
        let keys: HashSet<String> = result.files.keys().cloned().collect();
        // Don't assert specific assignments since scan order is non-deterministic,
        // but all 3 must be distinct
        assert_eq!(keys.len(), 3);

        // All original paths must be represented in ffn values
        let ffns: HashSet<PathBuf> = result.files.values().map(|e| e.ffn.clone()).collect();
        assert!(ffns.contains(&PathBuf::from("a/file.txt")));
        assert!(ffns.contains(&PathBuf::from("b/file.txt")));
        assert!(ffns.contains(&PathBuf::from("c/file.__1.txt")));
    }

    #[test]
    fn scan_file_at_root_and_in_subdir_same_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, "file.txt");
        create_file(root, "tag1/file.txt");

        let result = scan_tree(root, false);

        assert_eq!(result.files.len(), 2);
        // One file has empty tags, the other has {"tag1"}
        let tags_sets: HashSet<BTreeSet<String>> = result
            .files
            .values()
            .map(|e| e.tags.clone())
            .collect();
        assert!(tags_sets.contains(&BTreeSet::new()));
        assert!(tags_sets.contains(&BTreeSet::from(["tag1".into()])));
    }

    #[test]
    fn scan_hidden_dir_contents_with_show_hidden() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        create_file(root, ".hidden_dir/.secret");
        create_file(root, "visible/normal.txt");

        // show_hidden=true: both files present
        let result = scan_tree(root, true);
        assert!(result.files.contains_key(".secret"));
        assert!(result.files.contains_key("normal.txt"));
        assert_eq!(
            result.files[".secret"].tags,
            BTreeSet::from([".hidden_dir".into()])
        );

        // show_hidden=false: only normal.txt
        let result = scan_tree(root, false);
        assert!(!result.files.contains_key(".secret"));
        assert!(result.files.contains_key("normal.txt"));
        assert_eq!(result.files.len(), 1);
    }

    // --- Basename collision / split_ext edge cases ---

    #[test]
    fn find_free_bn_hidden_file_collision() {
        // ".hidden" → split_ext returns (".hidden", "") since rfind('.') pos=0 fails pos > 0
        let existing = HashSet::from([".hidden".to_string()]);
        assert_eq!(find_free_bn(".hidden", &existing), ".hidden.__1");
    }

    #[test]
    fn find_free_bn_dot_only() {
        // "." → split_ext returns (".", "") — degenerate but shouldn't panic
        let existing = HashSet::from([".".to_string()]);
        assert_eq!(find_free_bn(".", &existing), "..__1");
    }

    #[test]
    fn split_ext_double_dot_prefix() {
        // "..foo" → rfind('.') finds dot at pos=1, pos > 0 passes → (".", ".foo")
        assert_eq!(split_ext("..foo"), (".", ".foo"));
    }

    #[test]
    fn find_free_bn_gap_in_sequence() {
        // Existing has a gap: 1 and 3 present but not 2. Should find 2 first.
        let existing = HashSet::from([
            "f.txt".to_string(),
            "f.__1.txt".to_string(),
            "f.__3.txt".to_string(),
        ]);
        assert_eq!(find_free_bn("f.txt", &existing), "f.__2.txt");
    }

    #[test]
    fn scan_dir_only_no_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("tag1/tag2")).unwrap();
        fs::create_dir_all(root.join("tag3")).unwrap();

        let result = scan_tree(root, false);

        assert!(result.files.is_empty());
        // tagdirs should be populated with the directory tag sets
        assert!(result
            .tagdirs
            .contains_key(&BTreeSet::from(["tag1".into()])));
        assert!(result
            .tagdirs
            .contains_key(&BTreeSet::from(["tag1".into(), "tag2".into()])));
        assert!(result
            .tagdirs
            .contains_key(&BTreeSet::from(["tag3".into()])));
    }
}
