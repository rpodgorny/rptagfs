use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::{Errno, Result as FuseResult, Timestamp};
use futures_util::stream;
use futures_util::Stream;

use crate::scanner::{FileEntry, ScanResult};

const ROOT_INODE: u64 = 1;
const ALL_INODE: u64 = 2;
const UNTAGGED_INODE: u64 = 3;
const SPECIAL_ALL_NAME: &str = "_all";
const SPECIAL_UNTAGGED_NAME: &str = "_untagged";
const TTL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
enum SpecialKind {
    All,
    Untagged,
}

#[derive(Clone, Debug)]
enum InodeEntry {
    /// Virtual directory identified by its tag set
    Dir(BTreeSet<String>),
    /// Special flat directory (_all or _untagged)
    SpecialDir(SpecialKind),
    /// File identified by its basename (key into files map)
    File(String),
}

/// Inode tables that grow dynamically as paths are looked up.
struct InodeTable {
    inodes: HashMap<u64, InodeEntry>,
    dir_to_inode: HashMap<BTreeSet<String>, u64>,
    file_to_inode: HashMap<String, u64>,
    next_inode: u64,
}

impl InodeTable {
    fn new() -> Self {
        let mut tbl = Self {
            inodes: HashMap::new(),
            dir_to_inode: HashMap::new(),
            file_to_inode: HashMap::new(),
            next_inode: 4, // 1=root, 2=_all, 3=_untagged
        };
        // Pre-register root inode
        tbl.inodes
            .insert(ROOT_INODE, InodeEntry::Dir(BTreeSet::new()));
        tbl.dir_to_inode.insert(BTreeSet::new(), ROOT_INODE);
        // Pre-register special directory inodes
        tbl.inodes
            .insert(ALL_INODE, InodeEntry::SpecialDir(SpecialKind::All));
        tbl.inodes
            .insert(UNTAGGED_INODE, InodeEntry::SpecialDir(SpecialKind::Untagged));
        tbl
    }

    fn get_or_alloc_dir(&mut self, tags: &BTreeSet<String>) -> u64 {
        if let Some(&ino) = self.dir_to_inode.get(tags) {
            return ino;
        }
        let ino = self.next_inode;
        self.next_inode += 1;
        self.dir_to_inode.insert(tags.clone(), ino);
        self.inodes.insert(ino, InodeEntry::Dir(tags.clone()));
        log::debug!("Allocated dir inode {} for tags {:?}", ino, tags);
        ino
    }

    fn get_or_alloc_file(&mut self, basename: &str) -> u64 {
        if let Some(&ino) = self.file_to_inode.get(basename) {
            return ino;
        }
        let ino = self.next_inode;
        self.next_inode += 1;
        self.file_to_inode.insert(basename.to_string(), ino);
        self.inodes
            .insert(ino, InodeEntry::File(basename.to_string()));
        log::debug!("Allocated file inode {} for {:?}", ino, basename);
        ino
    }

    fn get(&self, ino: u64) -> Option<&InodeEntry> {
        self.inodes.get(&ino)
    }

    fn remove_file(&mut self, basename: &str) {
        if let Some(ino) = self.file_to_inode.remove(basename) {
            self.inodes.remove(&ino);
            log::debug!("Removed file inode {} for {:?}", ino, basename);
        }
    }

    fn remove_dir(&mut self, tags: &BTreeSet<String>) {
        if let Some(ino) = self.dir_to_inode.remove(tags) {
            self.inodes.remove(&ino);
            log::debug!("Removed dir inode {} for tags {:?}", ino, tags);
        }
    }
}

/// All mutable filesystem state, protected by a single RwLock.
struct FsState {
    files: HashMap<String, FileEntry>,
    tagdirs: HashMap<BTreeSet<String>, HashSet<PathBuf>>,
    by_tags: HashMap<String, HashSet<String>>,
    inode_table: InodeTable,
}

impl FsState {
    /// Return tags that can still be navigated into from the current tag set.
    fn get_avail_tags(&self, current_tags: &BTreeSet<String>) -> BTreeSet<String> {
        let mut result = BTreeSet::new();
        for key in self.tagdirs.keys() {
            if current_tags.is_subset(key) {
                result.extend(key.iter().cloned());
            }
        }
        for tag in current_tags {
            result.remove(tag);
        }
        result
    }

    /// Return basenames of files with no tags.
    fn get_untagged_files(&self) -> HashSet<String> {
        self.files
            .iter()
            .filter(|(_, entry)| entry.tags.is_empty())
            .map(|(basename, _)| basename.clone())
            .collect()
    }

    /// Return basenames of files matching all the given tags.
    fn get_matching_files(&self, tags: &BTreeSet<String>) -> HashSet<String> {
        if tags.is_empty() {
            return self.files.keys().cloned().collect();
        }
        let mut tags_iter = tags.iter();
        let first_tag = tags_iter.next().unwrap();
        let mut result = self.by_tags.get(first_tag).cloned().unwrap_or_default();
        for tag in tags_iter {
            if let Some(tag_files) = self.by_tags.get(tag) {
                result = result.intersection(tag_files).cloned().collect();
            } else {
                return HashSet::new();
            }
        }
        result
    }

    /// Determine file type (symlink vs regular) for a file basename.
    fn file_kind(&self, root: &Path, basename: &str) -> FileType {
        if let Some(fe) = self.files.get(basename) {
            let full_path = root.join(&fe.ffn);
            match std::fs::symlink_metadata(&full_path) {
                Ok(m) if m.file_type().is_symlink() => FileType::Symlink,
                _ => FileType::RegularFile,
            }
        } else {
            FileType::RegularFile
        }
    }

    /// Get or create a physical directory for a tag set.
    /// Returns the relative path to the directory.
    fn get_dir_for_tags(&mut self, tags: &BTreeSet<String>, root: &Path) -> std::io::Result<PathBuf> {
        let key = tags.clone();

        // If tagdirs already has this key, return the lexicographically first path
        if let Some(dirs) = self.tagdirs.get(&key) {
            if let Some(first) = dirs.iter().min() {
                return Ok(first.clone());
            }
        }

        // For empty tag set, return empty path (source root)
        if tags.is_empty() {
            let path = PathBuf::from("");
            self.tagdirs.entry(key).or_default().insert(path.clone());
            return Ok(path);
        }

        // Construct path from sorted tags
        let sorted_tags: Vec<&String> = tags.iter().collect();
        let rel_path: PathBuf = sorted_tags.iter().map(|s| s.as_str()).collect();
        let full_path = root.join(&rel_path);

        std::fs::create_dir_all(&full_path)?;

        self.tagdirs.entry(key).or_default().insert(rel_path.clone());

        // Ensure all tags have entries in by_tags
        for tag in tags {
            self.by_tags.entry(tag.clone()).or_default();
        }

        Ok(rel_path)
    }

    /// Rename a file (change basename only, no tag changes).
    fn rename_file(&mut self, old_bn: &str, new_bn: &str, root: &Path) -> std::io::Result<()> {
        // Check new basename doesn't already exist
        if self.files.contains_key(new_bn) {
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
        }

        let entry = self.files.get(old_bn)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
        let old_ffn = entry.ffn.clone();
        let tags = entry.tags.clone();

        // Compute new physical path by replacing basename
        let new_ffn = if let Some(parent) = old_ffn.parent() {
            if parent.as_os_str().is_empty() {
                PathBuf::from(new_bn)
            } else {
                parent.join(new_bn)
            }
        } else {
            PathBuf::from(new_bn)
        };

        // Check physical destination doesn't exist
        let full_new = root.join(&new_ffn);
        if full_new.exists() {
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
        }

        // Physical rename
        let full_old = root.join(&old_ffn);
        std::fs::rename(&full_old, &full_new)?;

        // Update state
        let mut entry = self.files.remove(old_bn).unwrap();
        entry.ffn = new_ffn;
        self.files.insert(new_bn.to_string(), entry);

        // Update by_tags
        for tag in &tags {
            if let Some(set) = self.by_tags.get_mut(tag) {
                set.remove(old_bn);
                set.insert(new_bn.to_string());
            }
        }

        // Update inode_table
        self.inode_table.remove_file(old_bn);
        self.inode_table.get_or_alloc_file(new_bn);

        Ok(())
    }

    /// Add and/or remove tags from a file by moving it physically.
    fn add_remove_tags(
        &mut self,
        bn: &str,
        tags_to_add: &BTreeSet<String>,
        tags_to_remove: &BTreeSet<String>,
        root: &Path,
    ) -> std::io::Result<()> {
        let entry = self.files.get(bn)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
        let old_ffn = entry.ffn.clone();
        let old_tags = entry.tags.clone();

        // Compute new tags
        let mut new_tags = old_tags.clone();
        for tag in tags_to_add {
            new_tags.insert(tag.clone());
        }
        for tag in tags_to_remove {
            new_tags.remove(tag);
        }

        // Get or create target directory
        let dir = self.get_dir_for_tags(&new_tags, root)?;

        // Build new ffn
        let new_ffn = if dir.as_os_str().is_empty() {
            PathBuf::from(bn)
        } else {
            dir.join(bn)
        };

        // Check physical destination doesn't exist (unless it's the same file)
        if new_ffn != old_ffn {
            let full_new = root.join(&new_ffn);
            if full_new.exists() {
                return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
            }

            // Physical move
            let full_old = root.join(&old_ffn);
            std::fs::rename(&full_old, &full_new)?;
        }

        // Update files map
        let entry = self.files.get_mut(bn).unwrap();
        entry.ffn = new_ffn;
        entry.tags = new_tags;

        // Update by_tags: add
        for tag in tags_to_add {
            self.by_tags.entry(tag.clone()).or_default().insert(bn.to_string());
        }
        // Update by_tags: remove
        for tag in tags_to_remove {
            if let Some(set) = self.by_tags.get_mut(tag) {
                set.remove(bn);
            }
        }

        Ok(())
    }

    /// Replace a tag name in a relative path string.
    fn replace_tag_in_path(path: &Path, old_tag: &str, new_tag: &str) -> PathBuf {
        let components: Vec<String> = path
            .components()
            .map(|c| {
                let s = c.as_os_str().to_string_lossy().to_string();
                if s == old_tag { new_tag.to_string() } else { s }
            })
            .collect();
        if components.is_empty() {
            PathBuf::new()
        } else {
            components.iter().collect()
        }
    }

    /// Rename a tag across the entire filesystem.
    fn rename_tag(&mut self, old_tag: &str, new_tag: &str, root: &Path) -> std::io::Result<()> {
        if old_tag == new_tag {
            return Ok(());
        }
        if self.by_tags.contains_key(new_tag) {
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
        }

        // Collect the unique directory rename operations needed.
        // For each path containing old_tag as a component, extract the "rename point":
        // the path prefix up to and including the old_tag component.
        // E.g., for path "music/rock" renaming tag "music": rename point is "music"
        // E.g., for path "alt/rock" renaming tag "rock": rename point is "alt/rock"
        let mut rename_points: HashSet<(PathBuf, PathBuf)> = HashSet::new();
        for (tag_set, paths) in &self.tagdirs {
            if tag_set.contains(old_tag) {
                for p in paths {
                    // Find the component matching old_tag and build the rename pair
                    let mut prefix = PathBuf::new();
                    for component in p.components() {
                        let s = component.as_os_str().to_string_lossy();
                        prefix.push(component);
                        if s == old_tag {
                            let mut new_prefix = prefix.parent()
                                .map(|p| p.to_path_buf())
                                .unwrap_or_default();
                            new_prefix.push(new_tag);
                            rename_points.insert((prefix.clone(), new_prefix));
                            break;
                        }
                    }
                }
            }
        }

        // Sort rename points deepest-first so children are renamed before parents
        let mut rename_list: Vec<(PathBuf, PathBuf)> = rename_points.into_iter().collect();
        rename_list.sort_by(|a, b| {
            let ac = a.0.components().count();
            let bc = b.0.components().count();
            bc.cmp(&ac)
        });

        // Execute physical renames
        for (old_path, new_path) in &rename_list {
            let full_old = root.join(old_path);
            let full_new = root.join(new_path);
            if full_old.exists() && !full_new.exists() {
                std::fs::rename(&full_old, &full_new)?;
            }
        }

        // Update tagdirs: replace old_tag with new_tag in keys and paths
        let old_tagdirs: Vec<(BTreeSet<String>, HashSet<PathBuf>)> =
            self.tagdirs.drain().collect();
        for (mut tag_set, paths) in old_tagdirs {
            if tag_set.contains(old_tag) {
                tag_set.remove(old_tag);
                tag_set.insert(new_tag.to_string());
                let new_paths: HashSet<PathBuf> = paths
                    .iter()
                    .map(|p| Self::replace_tag_in_path(p, old_tag, new_tag))
                    .collect();
                // Remove old dir inode, allocate new one
                let old_key_for_inode: BTreeSet<String> = {
                    let mut k = tag_set.clone();
                    k.remove(new_tag);
                    k.insert(old_tag.to_string());
                    k
                };
                self.inode_table.remove_dir(&old_key_for_inode);
                self.tagdirs.entry(tag_set).or_default().extend(new_paths);
            } else {
                self.tagdirs.entry(tag_set).or_default().extend(paths);
            }
        }

        // Update by_tags
        let file_set = self.by_tags.remove(old_tag).unwrap_or_default();
        self.by_tags.insert(new_tag.to_string(), file_set);

        // Update files: replace tag in tags set and ffn
        for (_bn, entry) in self.files.iter_mut() {
            if entry.tags.contains(old_tag) {
                entry.tags.remove(old_tag);
                entry.tags.insert(new_tag.to_string());
                entry.ffn = Self::replace_tag_in_path(&entry.ffn, old_tag, new_tag);
            }
        }

        Ok(())
    }
}

pub struct TagFs {
    root: PathBuf,
    state: RwLock<FsState>,
}

impl TagFs {
    pub fn new(root: PathBuf, scan: ScanResult) -> Self {
        Self {
            root,
            state: RwLock::new(FsState {
                files: scan.files,
                tagdirs: scan.tagdirs,
                by_tags: scan.by_tags,
                inode_table: InodeTable::new(),
            }),
        }
    }

    /// Synthetic directory attributes.
    fn dir_attr(&self, ino: u64) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: now.into(),
            mtime: now.into(),
            ctime: now.into(),
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
        }
    }

    /// File attributes from the physical file, with our inode number.
    fn file_attr(&self, ino: u64, basename: &str, state: &FsState) -> FuseResult<FileAttr> {
        let entry = state
            .files
            .get(basename)
            .ok_or_else(|| Errno::from(libc::ENOENT))?;
        let full_path = self.root.join(&entry.ffn);
        // Use symlink_metadata (lstat) like Python does
        let meta = std::fs::symlink_metadata(&full_path)
            .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;

        let kind = if meta.file_type().is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };

        let ctime_secs = meta.ctime();
        let ctime_nsecs = meta.ctime_nsec();

        Ok(FileAttr {
            ino,
            size: meta.size(),
            blocks: meta.blocks(),
            atime: meta.accessed().unwrap_or(UNIX_EPOCH).into(),
            mtime: meta.modified().unwrap_or(UNIX_EPOCH).into(),
            ctime: Timestamp::new(ctime_secs, ctime_nsecs as u32),
            kind,
            perm: (meta.mode() & 0o7777) as u16,
            nlink: meta.nlink() as u32,
            uid: meta.uid(),
            gid: meta.gid(),
            rdev: meta.rdev() as u32,
            blksize: meta.blksize() as u32,
        })
    }

    /// Resolve a file inode to its physical path.
    fn resolve_file_path(&self, basename: &str, state: &FsState) -> FuseResult<PathBuf> {
        let entry = state
            .files
            .get(basename)
            .ok_or_else(|| Errno::from(libc::ENOENT))?;
        Ok(self.root.join(&entry.ffn))
    }

}

impl Filesystem for TagFs {
    async fn init(&self, _req: Request) -> FuseResult<ReplyInit> {
        log::info!("Filesystem initialized");
        Ok(ReplyInit {
            max_write: NonZeroU32::new(16 * 1024).unwrap(),
        })
    }

    async fn destroy(&self, _req: Request) {
        log::info!("Filesystem destroyed");
    }

    async fn lookup(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<ReplyEntry> {
        let name_str = name.to_string_lossy().to_string();
        let state = self.state.read().unwrap();

        let parent_entry = state.inode_table.get(parent).cloned();

        match parent_entry {
            Some(InodeEntry::Dir(ref parent_tags)) => {
                let is_root = parent == ROOT_INODE;

                // At root, check for special directory names first
                if is_root {
                    if name_str == SPECIAL_ALL_NAME {
                        let attr = self.dir_attr(ALL_INODE);
                        return Ok(ReplyEntry { ttl: TTL, attr, generation: 0 });
                    }
                    if name_str == SPECIAL_UNTAGGED_NAME {
                        let attr = self.dir_attr(UNTAGGED_INODE);
                        return Ok(ReplyEntry { ttl: TTL, attr, generation: 0 });
                    }
                }

                // Non-root: check matching files first (files take priority)
                if !is_root {
                    let matching_files = state.get_matching_files(parent_tags);
                    if matching_files.contains(&name_str) {
                        if let Some(fe) = state.files.get(&name_str) {
                            if parent_tags.is_subset(&fe.tags) {
                                drop(state);
                                let mut state = self.state.write().unwrap();
                                let ino = state.inode_table.get_or_alloc_file(&name_str);
                                let attr = self.file_attr(ino, &name_str, &state)?;
                                log::debug!("lookup: parent={} name={:?} -> file inode {}", parent, name_str, ino);
                                return Ok(ReplyEntry { ttl: TTL, attr, generation: 0 });
                            }
                        }
                    }
                }

                // Check if name is an available tag
                let avail_tags = state.get_avail_tags(parent_tags);
                if avail_tags.contains(&name_str) {
                    let mut child_tags = parent_tags.clone();
                    child_tags.insert(name_str.clone());
                    drop(state);
                    let mut state = self.state.write().unwrap();
                    let ino = state.inode_table.get_or_alloc_dir(&child_tags);
                    let attr = self.dir_attr(ino);
                    log::debug!("lookup: parent={} name={:?} -> dir inode {}", parent, name_str, ino);
                    return Ok(ReplyEntry { ttl: TTL, attr, generation: 0 });
                }

                log::debug!("lookup: parent={} name={:?} -> ENOENT", parent, name_str);
                Err(libc::ENOENT.into())
            }
            Some(InodeEntry::SpecialDir(ref kind)) => {
                let file_set = match kind {
                    SpecialKind::All => state.get_matching_files(&BTreeSet::new()),
                    SpecialKind::Untagged => state.get_untagged_files(),
                };

                if file_set.contains(&name_str) {
                    drop(state);
                    let mut state = self.state.write().unwrap();
                    let ino = state.inode_table.get_or_alloc_file(&name_str);
                    let attr = self.file_attr(ino, &name_str, &state)?;
                    log::debug!("lookup: parent={} (special) name={:?} -> file inode {}", parent, name_str, ino);
                    return Ok(ReplyEntry { ttl: TTL, attr, generation: 0 });
                }

                log::debug!("lookup: parent={} (special) name={:?} -> ENOENT", parent, name_str);
                Err(libc::ENOENT.into())
            }
            _ => Err(libc::ENOENT.into()),
        }
    }

    async fn getattr(
        &self,
        _req: Request,
        inode: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> FuseResult<ReplyAttr> {
        let state = self.state.read().unwrap();
        match state.inode_table.get(inode) {
            Some(InodeEntry::Dir(_)) | Some(InodeEntry::SpecialDir(_)) => {
                log::debug!("getattr: inode {} -> dir", inode);
                Ok(ReplyAttr {
                    ttl: TTL,
                    attr: self.dir_attr(inode),
                })
            }
            Some(InodeEntry::File(basename)) => {
                log::debug!("getattr: inode {} -> file {:?}", inode, basename);
                let basename = basename.clone();
                let attr = self.file_attr(inode, &basename, &state)?;
                Ok(ReplyAttr { ttl: TTL, attr })
            }
            None => Err(libc::ENOENT.into()),
        }
    }

    async fn setattr(
        &self,
        _req: Request,
        inode: u64,
        _fh: Option<u64>,
        set_attr: SetAttr,
    ) -> FuseResult<ReplyAttr> {
        let state = self.state.read().unwrap();
        match state.inode_table.get(inode) {
            Some(InodeEntry::Dir(_)) | Some(InodeEntry::SpecialDir(_)) => {
                // For directories, return synthetic attrs unchanged
                Ok(ReplyAttr {
                    ttl: TTL,
                    attr: self.dir_attr(inode),
                })
            }
            Some(InodeEntry::File(basename)) => {
                let basename = basename.clone();
                let full_path = self.resolve_file_path(&basename, &state)?;
                drop(state);

                let path_c = std::ffi::CString::new(full_path.as_os_str().as_encoded_bytes())
                    .map_err(|_| Errno::from(libc::EINVAL))?;

                if let Some(mode) = set_attr.mode {
                    let ret = unsafe { libc::chmod(path_c.as_ptr(), mode) };
                    if ret != 0 {
                        return Err(Errno::from(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)));
                    }
                }

                if set_attr.uid.is_some() || set_attr.gid.is_some() {
                    let uid = set_attr.uid.unwrap_or(u32::MAX);
                    let gid = set_attr.gid.unwrap_or(u32::MAX);
                    let ret = unsafe { libc::chown(path_c.as_ptr(), uid, gid) };
                    if ret != 0 {
                        return Err(Errno::from(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)));
                    }
                }

                if let Some(size) = set_attr.size {
                    let ret = unsafe { libc::truncate(path_c.as_ptr(), size as libc::off_t) };
                    if ret != 0 {
                        return Err(Errno::from(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)));
                    }
                }

                if set_attr.atime.is_some() || set_attr.mtime.is_some() {
                    let to_timespec = |ts: Option<Timestamp>| -> libc::timespec {
                        match ts {
                            Some(t) => libc::timespec {
                                tv_sec: t.sec,
                                tv_nsec: t.nsec as i64,
                            },
                            None => libc::timespec {
                                tv_sec: 0,
                                tv_nsec: libc::UTIME_OMIT,
                            },
                        }
                    };
                    let times = [
                        to_timespec(set_attr.atime),
                        to_timespec(set_attr.mtime),
                    ];
                    let ret = unsafe {
                        libc::utimensat(libc::AT_FDCWD, path_c.as_ptr(), times.as_ptr(), 0)
                    };
                    if ret != 0 {
                        return Err(Errno::from(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)));
                    }
                }

                let state = self.state.read().unwrap();
                let attr = self.file_attr(inode, &basename, &state)?;
                Ok(ReplyAttr { ttl: TTL, attr })
            }
            None => Err(libc::ENOENT.into()),
        }
    }

    async fn readdir(
        &self,
        _req: Request,
        inode: u64,
        _fh: u64,
        offset: i64,
    ) -> FuseResult<ReplyDirectory<impl Stream<Item = FuseResult<DirectoryEntry>> + Send + '_>> {
        let mut state = self.state.write().unwrap();

        let inode_entry = state.inode_table.get(inode).cloned();

        let mut entries = Vec::new();
        let mut idx: i64 = 1;

        // "." entry
        entries.push(Ok(DirectoryEntry {
            inode,
            kind: FileType::Directory,
            name: OsString::from("."),
            offset: idx,
        }));
        idx += 1;

        // ".." entry (point to root for simplicity)
        entries.push(Ok(DirectoryEntry {
            inode: ROOT_INODE,
            kind: FileType::Directory,
            name: OsString::from(".."),
            offset: idx,
        }));
        idx += 1;

        match inode_entry {
            Some(InodeEntry::Dir(ref tags)) => {
                let is_root = inode == ROOT_INODE;
                let avail_tags = state.get_avail_tags(tags);

                // At root, emit special directories
                if is_root {
                    entries.push(Ok(DirectoryEntry {
                        inode: ALL_INODE,
                        kind: FileType::Directory,
                        name: OsString::from(SPECIAL_ALL_NAME),
                        offset: idx,
                    }));
                    idx += 1;
                    entries.push(Ok(DirectoryEntry {
                        inode: UNTAGGED_INODE,
                        kind: FileType::Directory,
                        name: OsString::from(SPECIAL_UNTAGGED_NAME),
                        offset: idx,
                    }));
                    idx += 1;
                }

                // Available tags as directories
                for tag in &avail_tags {
                    let mut child_tags = tags.clone();
                    child_tags.insert(tag.clone());
                    let child_ino = state.inode_table.get_or_alloc_dir(&child_tags);
                    entries.push(Ok(DirectoryEntry {
                        inode: child_ino,
                        kind: FileType::Directory,
                        name: OsString::from(tag),
                        offset: idx,
                    }));
                    idx += 1;
                }

                // Non-root: also emit matching files
                if !is_root {
                    let matching_files = state.get_matching_files(tags);
                    let root = self.root.clone();
                    for basename in &matching_files {
                        if avail_tags.contains(basename) {
                            continue;
                        }
                        let file_ino = state.inode_table.get_or_alloc_file(basename);
                        entries.push(Ok(DirectoryEntry {
                            inode: file_ino,
                            kind: state.file_kind(&root, basename),
                            name: OsString::from(basename),
                            offset: idx,
                        }));
                        idx += 1;
                    }
                }
            }
            Some(InodeEntry::SpecialDir(ref kind)) => {
                let file_set = match kind {
                    SpecialKind::All => state.get_matching_files(&BTreeSet::new()),
                    SpecialKind::Untagged => state.get_untagged_files(),
                };
                let root = self.root.clone();
                for basename in &file_set {
                    let file_ino = state.inode_table.get_or_alloc_file(basename);
                    entries.push(Ok(DirectoryEntry {
                        inode: file_ino,
                        kind: state.file_kind(&root, basename),
                        name: OsString::from(basename),
                        offset: idx,
                    }));
                    idx += 1;
                }
            }
            _ => return Err(libc::ENOTDIR.into()),
        }

        log::debug!("readdir: inode {} -> {} entries", inode, entries.len());

        let entries: Vec<_> = if offset > 0 {
            entries.into_iter().skip(offset as usize).collect()
        } else {
            entries
        };

        Ok(ReplyDirectory {
            entries: stream::iter(entries),
        })
    }

    async fn readdirplus(
        &self,
        _req: Request,
        parent: u64,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> FuseResult<ReplyDirectoryPlus<impl Stream<Item = FuseResult<DirectoryEntryPlus>> + Send + '_>> {
        let mut state = self.state.write().unwrap();

        let inode_entry = state.inode_table.get(parent).cloned();

        let mut entries = Vec::new();
        let mut idx: i64 = 1;

        // "." entry
        let dot_attr = self.dir_attr(parent);
        entries.push(Ok(DirectoryEntryPlus {
            inode: parent,
            generation: 0,
            kind: FileType::Directory,
            name: OsString::from("."),
            offset: idx,
            attr: dot_attr,
            entry_ttl: TTL,
            attr_ttl: TTL,
        }));
        idx += 1;

        // ".." entry
        let dotdot_attr = self.dir_attr(ROOT_INODE);
        entries.push(Ok(DirectoryEntryPlus {
            inode: ROOT_INODE,
            generation: 0,
            kind: FileType::Directory,
            name: OsString::from(".."),
            offset: idx,
            attr: dotdot_attr,
            entry_ttl: TTL,
            attr_ttl: TTL,
        }));
        idx += 1;

        match inode_entry {
            Some(InodeEntry::Dir(ref tags)) => {
                let is_root = parent == ROOT_INODE;
                let avail_tags = state.get_avail_tags(tags);

                // At root, emit special directories
                if is_root {
                    let all_attr = self.dir_attr(ALL_INODE);
                    entries.push(Ok(DirectoryEntryPlus {
                        inode: ALL_INODE,
                        generation: 0,
                        kind: FileType::Directory,
                        name: OsString::from(SPECIAL_ALL_NAME),
                        offset: idx,
                        attr: all_attr,
                        entry_ttl: TTL,
                        attr_ttl: TTL,
                    }));
                    idx += 1;
                    let untagged_attr = self.dir_attr(UNTAGGED_INODE);
                    entries.push(Ok(DirectoryEntryPlus {
                        inode: UNTAGGED_INODE,
                        generation: 0,
                        kind: FileType::Directory,
                        name: OsString::from(SPECIAL_UNTAGGED_NAME),
                        offset: idx,
                        attr: untagged_attr,
                        entry_ttl: TTL,
                        attr_ttl: TTL,
                    }));
                    idx += 1;
                }

                // Available tags as directories
                for tag in &avail_tags {
                    let mut child_tags = tags.clone();
                    child_tags.insert(tag.clone());
                    let child_ino = state.inode_table.get_or_alloc_dir(&child_tags);
                    let attr = self.dir_attr(child_ino);
                    entries.push(Ok(DirectoryEntryPlus {
                        inode: child_ino,
                        generation: 0,
                        kind: FileType::Directory,
                        name: OsString::from(tag),
                        offset: idx,
                        attr,
                        entry_ttl: TTL,
                        attr_ttl: TTL,
                    }));
                    idx += 1;
                }

                // Non-root: also emit matching files
                if !is_root {
                    let matching_files = state.get_matching_files(tags);
                    for basename in &matching_files {
                        if avail_tags.contains(basename) {
                            continue;
                        }
                        let file_ino = state.inode_table.get_or_alloc_file(basename);
                        let attr = match self.file_attr(file_ino, basename, &state) {
                            Ok(a) => a,
                            Err(_) => continue,
                        };
                        entries.push(Ok(DirectoryEntryPlus {
                            inode: file_ino,
                            generation: 0,
                            kind: attr.kind,
                            name: OsString::from(basename),
                            offset: idx,
                            attr,
                            entry_ttl: TTL,
                            attr_ttl: TTL,
                        }));
                        idx += 1;
                    }
                }
            }
            Some(InodeEntry::SpecialDir(ref kind)) => {
                let file_set = match kind {
                    SpecialKind::All => state.get_matching_files(&BTreeSet::new()),
                    SpecialKind::Untagged => state.get_untagged_files(),
                };
                for basename in &file_set {
                    let file_ino = state.inode_table.get_or_alloc_file(basename);
                    let attr = match self.file_attr(file_ino, basename, &state) {
                        Ok(a) => a,
                        Err(_) => continue,
                    };
                    entries.push(Ok(DirectoryEntryPlus {
                        inode: file_ino,
                        generation: 0,
                        kind: attr.kind,
                        name: OsString::from(basename),
                        offset: idx,
                        attr,
                        entry_ttl: TTL,
                        attr_ttl: TTL,
                    }));
                    idx += 1;
                }
            }
            _ => return Err(libc::ENOTDIR.into()),
        }

        log::debug!("readdirplus: inode {} -> {} entries", parent, entries.len());

        let entries: Vec<_> = if offset > 0 {
            entries.into_iter().skip(offset as usize).collect()
        } else {
            entries
        };

        Ok(ReplyDirectoryPlus {
            entries: stream::iter(entries),
        })
    }

    async fn open(&self, _req: Request, inode: u64, flags: u32) -> FuseResult<ReplyOpen> {
        let state = self.state.read().unwrap();
        let basename = match state.inode_table.get(inode) {
            Some(InodeEntry::File(bn)) => bn.clone(),
            Some(InodeEntry::Dir(_)) | Some(InodeEntry::SpecialDir(_)) => return Err(libc::EISDIR.into()),
            None => return Err(libc::ENOENT.into()),
        };

        let full_path = self.resolve_file_path(&basename, &state)?;
        drop(state);

        log::debug!("open: {:?} -> {:?}", basename, full_path);

        let path_c = std::ffi::CString::new(full_path.as_os_str().as_encoded_bytes())
            .map_err(|_| Errno::from(libc::EINVAL))?;

        // Pass through flags, but strip O_CREAT, O_EXCL, O_NOCTTY
        let open_flags = (flags as i32) & !(libc::O_CREAT | libc::O_EXCL | libc::O_NOCTTY);

        let fd = unsafe { libc::open(path_c.as_ptr(), open_flags) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(Errno::from(err.raw_os_error().unwrap_or(libc::EIO)));
        }

        Ok(ReplyOpen {
            fh: fd as u64,
            flags: 0,
        })
    }

    async fn read(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> FuseResult<ReplyData> {
        let mut buf = vec![0u8; size as usize];
        let n = unsafe {
            libc::pread(
                fh as i32,
                buf.as_mut_ptr() as *mut libc::c_void,
                size as usize,
                offset as i64,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            return Err(Errno::from(err.raw_os_error().unwrap_or(libc::EIO)));
        }
        buf.truncate(n as usize);
        Ok(ReplyData {
            data: Bytes::from(buf),
        })
    }

    async fn write(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        _flags: u32,
    ) -> FuseResult<ReplyWrite> {
        let n = unsafe {
            libc::pwrite(
                fh as i32,
                data.as_ptr() as *const libc::c_void,
                data.len(),
                offset as i64,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            return Err(Errno::from(err.raw_os_error().unwrap_or(libc::EIO)));
        }
        Ok(ReplyWrite {
            written: n as u32,
        })
    }

    async fn release(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> FuseResult<()> {
        unsafe { libc::close(fh as i32) };
        Ok(())
    }

    async fn flush(&self, _req: Request, _inode: u64, fh: u64, _lock_owner: u64) -> FuseResult<()> {
        let ret = unsafe { libc::fsync(fh as i32) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(Errno::from(err.raw_os_error().unwrap_or(libc::EIO)));
        }
        Ok(())
    }

    async fn fsync(&self, _req: Request, _inode: u64, fh: u64, datasync: bool) -> FuseResult<()> {
        let ret = if datasync {
            unsafe { libc::fdatasync(fh as i32) }
        } else {
            unsafe { libc::fsync(fh as i32) }
        };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(Errno::from(err.raw_os_error().unwrap_or(libc::EIO)));
        }
        Ok(())
    }

    async fn readlink(&self, _req: Request, inode: u64) -> FuseResult<ReplyData> {
        let state = self.state.read().unwrap();
        let basename = match state.inode_table.get(inode) {
            Some(InodeEntry::File(bn)) => bn.clone(),
            _ => return Err(libc::EINVAL.into()),
        };

        let full_path = self.resolve_file_path(&basename, &state)?;
        drop(state);

        log::debug!("readlink: {:?}", basename);
        let target = std::fs::read_link(&full_path)
            .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;
        Ok(ReplyData {
            data: Bytes::from(target.into_os_string().into_encoded_bytes()),
        })
    }

    async fn access(&self, _req: Request, inode: u64, _mask: u32) -> FuseResult<()> {
        let state = self.state.read().unwrap();
        match state.inode_table.get(inode) {
            Some(InodeEntry::Dir(_)) | Some(InodeEntry::SpecialDir(_)) => Ok(()),
            Some(InodeEntry::File(_)) => Ok(()),
            None => Err(libc::ENOENT.into()),
        }
    }

    async fn statfs(&self, _req: Request, _inode: u64) -> FuseResult<ReplyStatFs> {
        let path_bytes = self.root.as_os_str().as_encoded_bytes();
        let path_c = std::ffi::CString::new(path_bytes)
            .map_err(|_| Errno::from(libc::EINVAL))?;

        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::statvfs(path_c.as_ptr(), &mut stat) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(Errno::from(err.raw_os_error().unwrap_or(libc::EIO)));
        }

        Ok(ReplyStatFs {
            bsize: stat.f_bsize as u32,
            frsize: stat.f_frsize as u32,
            blocks: stat.f_blocks,
            bfree: stat.f_bfree,
            bavail: stat.f_bavail,
            files: stat.f_files,
            ffree: stat.f_ffree,
            namelen: stat.f_namemax as u32,
        })
    }

    async fn opendir(&self, _req: Request, inode: u64, _flags: u32) -> FuseResult<ReplyOpen> {
        let state = self.state.read().unwrap();
        match state.inode_table.get(inode) {
            Some(InodeEntry::Dir(_)) | Some(InodeEntry::SpecialDir(_)) => {
                Ok(ReplyOpen { fh: 0, flags: 0 })
            }
            _ => Err(libc::ENOTDIR.into()),
        }
    }

    async fn releasedir(
        &self,
        _req: Request,
        _inode: u64,
        _fh: u64,
        _flags: u32,
    ) -> FuseResult<()> {
        Ok(())
    }

    async fn unlink(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<()> {
        let name_str = name.to_string_lossy().to_string();
        log::debug!("unlink: parent={} name={:?}", parent, name_str);

        let mut state = self.state.write().unwrap();

        // Verify file exists
        let entry = state.files.get(&name_str)
            .ok_or_else(|| Errno::from(libc::ENOENT))?;
        let ffn = entry.ffn.clone();
        let tags = entry.tags.clone();

        // Physical delete
        let full_path = self.root.join(&ffn);
        std::fs::remove_file(&full_path)
            .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;

        // Update state
        state.files.remove(&name_str);
        for tag in &tags {
            if let Some(set) = state.by_tags.get_mut(tag) {
                set.remove(&name_str);
            }
        }
        state.inode_table.remove_file(&name_str);

        Ok(())
    }

    async fn rmdir(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<()> {
        let name_str = name.to_string_lossy().to_string();
        log::debug!("rmdir: parent={} name={:?}", parent, name_str);

        let mut state = self.state.write().unwrap();

        // Disallow on special dirs
        if name_str == SPECIAL_ALL_NAME || name_str == SPECIAL_UNTAGGED_NAME {
            return Err(libc::EPERM.into());
        }

        // Compute child tag set
        let parent_tags = match state.inode_table.get(parent) {
            Some(InodeEntry::Dir(tags)) => tags.clone(),
            _ => return Err(libc::ENOENT.into()),
        };
        let mut child_tags = parent_tags;
        child_tags.insert(name_str.clone());

        // Remove physical dirs
        if let Some(dirs) = state.tagdirs.get(&child_tags) {
            for dir in dirs.clone() {
                let full_path = self.root.join(&dir);
                if full_path.exists() {
                    std::fs::remove_dir(&full_path)
                        .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;
                }
            }
        }

        // Remove from state
        state.tagdirs.remove(&child_tags);
        state.inode_table.remove_dir(&child_tags);

        Ok(())
    }

    async fn mkdir(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
    ) -> FuseResult<ReplyEntry> {
        let name_str = name.to_string_lossy().to_string();
        log::debug!("mkdir: parent={} name={:?}", parent, name_str);

        let mut state = self.state.write().unwrap();

        // Disallow inside _all or _untagged
        match state.inode_table.get(parent) {
            Some(InodeEntry::SpecialDir(_)) => return Err(libc::EPERM.into()),
            Some(InodeEntry::Dir(_)) => {}
            _ => return Err(libc::ENOENT.into()),
        }

        // Compute child tag set
        let parent_tags = match state.inode_table.get(parent) {
            Some(InodeEntry::Dir(tags)) => tags.clone(),
            _ => return Err(libc::ENOENT.into()),
        };
        let mut child_tags = parent_tags;
        child_tags.insert(name_str.clone());

        // Create physical directory via helper
        state.get_dir_for_tags(&child_tags, &self.root)
            .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;

        // Allocate inode
        let ino = state.inode_table.get_or_alloc_dir(&child_tags);
        let attr = self.dir_attr(ino);

        Ok(ReplyEntry { ttl: TTL, attr, generation: 0 })
    }

    async fn create(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        flags: u32,
    ) -> FuseResult<ReplyCreated> {
        let name_str = name.to_string_lossy().to_string();
        log::debug!("create: parent={} name={:?} mode={:o} flags={}", parent, name_str, mode, flags);

        let mut state = self.state.write().unwrap();

        // Determine tags from parent
        let tags = match state.inode_table.get(parent) {
            Some(InodeEntry::Dir(t)) => {
                if parent == ROOT_INODE { BTreeSet::new() } else { t.clone() }
            }
            Some(InodeEntry::SpecialDir(SpecialKind::Untagged)) => BTreeSet::new(),
            Some(InodeEntry::SpecialDir(SpecialKind::All)) => BTreeSet::new(),
            _ => return Err(libc::ENOENT.into()),
        };

        // Check for collision
        if state.files.contains_key(&name_str) {
            return Err(libc::EEXIST.into());
        }

        // Get or create physical directory
        let dir = state.get_dir_for_tags(&tags, &self.root)
            .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;

        // Build physical path
        let ffn = if dir.as_os_str().is_empty() {
            PathBuf::from(&name_str)
        } else {
            dir.join(&name_str)
        };
        let full_path = self.root.join(&ffn);

        // Physical create
        let path_c = std::ffi::CString::new(full_path.as_os_str().as_encoded_bytes())
            .map_err(|_| Errno::from(libc::EINVAL))?;
        let open_flags = (flags as i32) | libc::O_CREAT | libc::O_EXCL;
        let fd = unsafe { libc::open(path_c.as_ptr(), open_flags, mode) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(Errno::from(err.raw_os_error().unwrap_or(libc::EIO)));
        }

        // Update state
        for tag in &tags {
            state.by_tags.entry(tag.clone()).or_default().insert(name_str.clone());
        }
        state.files.insert(name_str.clone(), FileEntry {
            ffn,
            tags,
        });
        let ino = state.inode_table.get_or_alloc_file(&name_str);
        let attr = self.file_attr(ino, &name_str, &state)?;

        Ok(ReplyCreated {
            ttl: TTL,
            attr,
            generation: 0,
            fh: fd as u64,
            flags: 0,
        })
    }

    async fn rename(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
    ) -> FuseResult<()> {
        let name_str = name.to_string_lossy().to_string();
        let new_name_str = new_name.to_string_lossy().to_string();
        log::debug!("rename: parent={} name={:?} -> new_parent={} new_name={:?}",
            parent, name_str, new_parent, new_name_str);

        let mut state = self.state.write().unwrap();

        // Resolve parent tag sets
        let old_tags = match state.inode_table.get(parent) {
            Some(InodeEntry::Dir(tags)) => {
                if parent == ROOT_INODE { BTreeSet::new() } else { tags.clone() }
            }
            Some(InodeEntry::SpecialDir(SpecialKind::All)) => {
                // Use the file's actual tags
                if let Some(fe) = state.files.get(&name_str) {
                    fe.tags.clone()
                } else {
                    BTreeSet::new()
                }
            }
            Some(InodeEntry::SpecialDir(SpecialKind::Untagged)) => BTreeSet::new(),
            _ => return Err(libc::ENOENT.into()),
        };

        let new_tags = match state.inode_table.get(new_parent) {
            Some(InodeEntry::Dir(tags)) => {
                if new_parent == ROOT_INODE { BTreeSet::new() } else { tags.clone() }
            }
            Some(InodeEntry::SpecialDir(SpecialKind::All)) => {
                // Use the file's actual tags
                if let Some(fe) = state.files.get(&name_str) {
                    fe.tags.clone()
                } else {
                    BTreeSet::new()
                }
            }
            Some(InodeEntry::SpecialDir(SpecialKind::Untagged)) => BTreeSet::new(),
            _ => return Err(libc::ENOENT.into()),
        };

        // Check if name is a tag (directory rename = tag rename)
        let is_tag = state.by_tags.contains_key(&name_str) && !state.files.contains_key(&name_str);
        if is_tag {
            state.rename_tag(&name_str, &new_name_str, &self.root)
                .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;
            return Ok(());
        }

        // File rename/retag
        if !state.files.contains_key(&name_str) {
            return Err(libc::ENOENT.into());
        }

        if name_str != new_name_str {
            // Basename changed: rename file first
            state.rename_file(&name_str, &new_name_str, &self.root)
                .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;

            // Then handle tag changes on the new basename
            let tags_to_add: BTreeSet<String> = new_tags.difference(&old_tags).cloned().collect();
            let tags_to_remove: BTreeSet<String> = old_tags.difference(&new_tags).cloned().collect();
            if !tags_to_add.is_empty() || !tags_to_remove.is_empty() {
                state.add_remove_tags(&new_name_str, &tags_to_add, &tags_to_remove, &self.root)
                    .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;
            }
        } else {
            // Same basename: just add/remove tags
            let tags_to_add: BTreeSet<String> = new_tags.difference(&old_tags).cloned().collect();
            let tags_to_remove: BTreeSet<String> = old_tags.difference(&new_tags).cloned().collect();
            if !tags_to_add.is_empty() || !tags_to_remove.is_empty() {
                state.add_remove_tags(&name_str, &tags_to_add, &tags_to_remove, &self.root)
                    .map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::FileEntry;
    use std::fs;
    use tempfile::tempdir;

    // --- InodeTable tests ---

    #[test]
    fn inode_table_new_has_root() {
        let tbl = InodeTable::new();
        match tbl.get(ROOT_INODE) {
            Some(InodeEntry::Dir(tags)) => assert!(tags.is_empty()),
            other => panic!("Expected Dir({{}}), got {:?}", other),
        }
    }

    #[test]
    fn inode_table_alloc_dir() {
        let mut tbl = InodeTable::new();
        let tags = BTreeSet::from(["a".to_string()]);
        let ino = tbl.get_or_alloc_dir(&tags);
        assert!(ino >= 4);
        match tbl.get(ino) {
            Some(InodeEntry::Dir(t)) => assert_eq!(t, &tags),
            other => panic!("Expected Dir({{a}}), got {:?}", other),
        }
    }

    #[test]
    fn inode_table_alloc_dir_idempotent() {
        let mut tbl = InodeTable::new();
        let tags = BTreeSet::from(["a".to_string()]);
        let ino1 = tbl.get_or_alloc_dir(&tags);
        let ino2 = tbl.get_or_alloc_dir(&tags);
        assert_eq!(ino1, ino2);
    }

    #[test]
    fn inode_table_alloc_file() {
        let mut tbl = InodeTable::new();
        let ino = tbl.get_or_alloc_file("f.txt");
        assert!(ino >= 4);
        match tbl.get(ino) {
            Some(InodeEntry::File(bn)) => assert_eq!(bn, "f.txt"),
            other => panic!("Expected File(\"f.txt\"), got {:?}", other),
        }
    }

    #[test]
    fn inode_table_alloc_file_idempotent() {
        let mut tbl = InodeTable::new();
        let ino1 = tbl.get_or_alloc_file("f.txt");
        let ino2 = tbl.get_or_alloc_file("f.txt");
        assert_eq!(ino1, ino2);
    }

    #[test]
    fn inode_table_dir_file_distinct() {
        let mut tbl = InodeTable::new();
        let dir_ino = tbl.get_or_alloc_dir(&BTreeSet::from(["a".to_string()]));
        let file_ino = tbl.get_or_alloc_file("a");
        assert_ne!(dir_ino, file_ino);
    }

    #[test]
    fn inode_table_get_nonexistent() {
        let tbl = InodeTable::new();
        assert!(tbl.get(9999).is_none());
    }

    #[test]
    fn inode_table_remove_file() {
        let mut tbl = InodeTable::new();
        let ino = tbl.get_or_alloc_file("f.txt");
        tbl.remove_file("f.txt");
        assert!(tbl.get(ino).is_none());
        assert!(tbl.file_to_inode.get("f.txt").is_none());
    }

    #[test]
    fn inode_table_remove_dir() {
        let mut tbl = InodeTable::new();
        let tags = BTreeSet::from(["a".to_string()]);
        let ino = tbl.get_or_alloc_dir(&tags);
        tbl.remove_dir(&tags);
        assert!(tbl.get(ino).is_none());
        assert!(tbl.dir_to_inode.get(&tags).is_none());
    }

    // --- Helper to build a test FsState ---

    /// Write the standard test directory layout into `root` and return FsState via scan_tree.
    fn write_test_layout(root: &std::path::Path) {
        fs::create_dir_all(root.join("music/rock")).unwrap();
        fs::create_dir_all(root.join("photos")).unwrap();
        fs::write(root.join("music/song.mp3"), "song data").unwrap();
        fs::write(root.join("music/rock/heavy.mp3"), "heavy data").unwrap();
        fs::write(root.join("photos/pic.jpg"), "pic data").unwrap();
        fs::write(root.join("readme.txt"), "readme data").unwrap();
    }

    fn scan_to_state(root: &std::path::Path) -> FsState {
        let scan = crate::scanner::scan_tree(root, false);
        FsState {
            files: scan.files,
            tagdirs: scan.tagdirs,
            by_tags: scan.by_tags,
            inode_table: InodeTable::new(),
        }
    }

    fn make_test_state() -> FsState {
        let dir = tempdir().unwrap();
        write_test_layout(dir.path());
        scan_to_state(dir.path())
    }

    // --- get_avail_tags tests ---

    #[test]
    fn avail_tags_at_root() {
        let state = make_test_state();
        let tags = state.get_avail_tags(&BTreeSet::new());
        assert_eq!(
            tags,
            BTreeSet::from([
                "music".to_string(),
                "rock".to_string(),
                "photos".to_string()
            ])
        );
    }

    #[test]
    fn avail_tags_single() {
        let state = make_test_state();
        let tags = state.get_avail_tags(&BTreeSet::from(["music".to_string()]));
        assert_eq!(tags, BTreeSet::from(["rock".to_string()]));
    }

    #[test]
    fn avail_tags_full() {
        let state = make_test_state();
        let tags = state.get_avail_tags(&BTreeSet::from([
            "music".to_string(),
            "rock".to_string(),
        ]));
        assert!(tags.is_empty());
    }

    #[test]
    fn avail_tags_nonexistent() {
        let state = make_test_state();
        let tags = state.get_avail_tags(&BTreeSet::from(["nonexistent".to_string()]));
        assert!(tags.is_empty());
    }

    #[test]
    fn avail_tags_partial() {
        let state = make_test_state();
        let tags = state.get_avail_tags(&BTreeSet::from(["photos".to_string()]));
        assert!(tags.is_empty());
    }

    // --- get_matching_files tests ---

    #[test]
    fn matching_files_root() {
        let state = make_test_state();
        let files = state.get_matching_files(&BTreeSet::new());
        assert_eq!(files.len(), 4);
        assert!(files.contains("song.mp3"));
        assert!(files.contains("heavy.mp3"));
        assert!(files.contains("pic.jpg"));
        assert!(files.contains("readme.txt"));
    }

    #[test]
    fn matching_files_single_tag() {
        let state = make_test_state();
        let files = state.get_matching_files(&BTreeSet::from(["music".to_string()]));
        assert_eq!(files.len(), 2);
        assert!(files.contains("song.mp3"));
        assert!(files.contains("heavy.mp3"));
    }

    #[test]
    fn matching_files_two_tags() {
        let state = make_test_state();
        let files = state.get_matching_files(&BTreeSet::from([
            "music".to_string(),
            "rock".to_string(),
        ]));
        assert_eq!(files.len(), 1);
        assert!(files.contains("heavy.mp3"));
    }

    #[test]
    fn matching_files_nonexistent() {
        let state = make_test_state();
        let files = state.get_matching_files(&BTreeSet::from(["nonexistent".to_string()]));
        assert!(files.is_empty());
    }

    #[test]
    fn matching_files_photos() {
        let state = make_test_state();
        let files = state.get_matching_files(&BTreeSet::from(["photos".to_string()]));
        assert_eq!(files.len(), 1);
        assert!(files.contains("pic.jpg"));
    }

    // --- Edge case tests ---

    #[test]
    fn file_and_tag_same_name() {
        let mut files = HashMap::new();
        files.insert(
            "music".to_string(),
            FileEntry {
                ffn: PathBuf::from("rock/music"),
                tags: BTreeSet::from(["rock".to_string()]),
            },
        );
        files.insert(
            "song.mp3".to_string(),
            FileEntry {
                ffn: PathBuf::from("music/song.mp3"),
                tags: BTreeSet::from(["music".to_string()]),
            },
        );

        let mut tagdirs: HashMap<BTreeSet<String>, HashSet<PathBuf>> = HashMap::new();
        tagdirs
            .entry(BTreeSet::from(["music".to_string()]))
            .or_default()
            .insert(PathBuf::from("music"));
        tagdirs
            .entry(BTreeSet::from(["rock".to_string()]))
            .or_default()
            .insert(PathBuf::from("rock"));

        let mut by_tags: HashMap<String, HashSet<String>> = HashMap::new();
        by_tags
            .entry("music".to_string())
            .or_default()
            .insert("song.mp3".to_string());
        by_tags
            .entry("rock".to_string())
            .or_default()
            .insert("music".to_string());

        let state = FsState {
            files,
            tagdirs,
            by_tags,
            inode_table: InodeTable::new(),
        };

        // "music" file should be findable via matching_files
        let rock_files = state.get_matching_files(&BTreeSet::from(["rock".to_string()]));
        assert!(rock_files.contains("music"));

        // "music" tag should appear in avail_tags at root
        let root_tags = state.get_avail_tags(&BTreeSet::new());
        assert!(root_tags.contains("music"));
    }

    #[test]
    fn empty_filesystem() {
        let state = FsState {
            files: HashMap::new(),
            tagdirs: HashMap::new(),
            by_tags: HashMap::new(),
            inode_table: InodeTable::new(),
        };

        assert!(state.get_avail_tags(&BTreeSet::new()).is_empty());
        assert!(state.get_matching_files(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn single_file_no_tags() {
        let mut files = HashMap::new();
        files.insert(
            "lonely.txt".to_string(),
            FileEntry {
                ffn: PathBuf::from("lonely.txt"),
                tags: BTreeSet::new(),
            },
        );

        let state = FsState {
            files,
            tagdirs: HashMap::new(),
            by_tags: HashMap::new(),
            inode_table: InodeTable::new(),
        };

        let root_files = state.get_matching_files(&BTreeSet::new());
        assert_eq!(root_files.len(), 1);
        assert!(root_files.contains("lonely.txt"));

        assert!(state.get_avail_tags(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn many_tags_deep() {
        let mut files = HashMap::new();
        files.insert(
            "file.txt".to_string(),
            FileEntry {
                ffn: PathBuf::from("a/b/c/d/e/file.txt"),
                tags: BTreeSet::from([
                    "a".to_string(),
                    "b".to_string(),
                    "c".to_string(),
                    "d".to_string(),
                    "e".to_string(),
                ]),
            },
        );

        let all_tags: BTreeSet<String> =
            ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();

        let mut tagdirs: HashMap<BTreeSet<String>, HashSet<PathBuf>> = HashMap::new();
        tagdirs
            .entry(all_tags.clone())
            .or_default()
            .insert(PathBuf::from("a/b/c/d/e"));

        let mut by_tags: HashMap<String, HashSet<String>> = HashMap::new();
        for t in &all_tags {
            by_tags
                .entry(t.clone())
                .or_default()
                .insert("file.txt".to_string());
        }

        let state = FsState {
            files,
            tagdirs,
            by_tags,
            inode_table: InodeTable::new(),
        };

        // At root, all 5 tags available
        let root_avail = state.get_avail_tags(&BTreeSet::new());
        assert_eq!(root_avail.len(), 5);

        // After selecting {a}, remaining are {b,c,d,e}
        let after_a = state.get_avail_tags(&BTreeSet::from(["a".to_string()]));
        assert_eq!(after_a.len(), 4);
        assert!(!after_a.contains("a"));

        // After selecting {a,b,c,d}, only {e} left
        let after_abcd = state.get_avail_tags(&BTreeSet::from([
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]));
        assert_eq!(after_abcd, BTreeSet::from(["e".to_string()]));

        // After selecting all 5, no more tags
        let after_all = state.get_avail_tags(&all_tags);
        assert!(after_all.is_empty());
    }

    #[test]
    fn tag_order_canonical_inode() {
        let mut state = make_test_state();

        // Insert {a, b} in one order
        let tags1 = BTreeSet::from(["a".to_string(), "b".to_string()]);
        let ino1 = state.inode_table.get_or_alloc_dir(&tags1);

        // BTreeSet is always sorted, so this is the same set
        let mut tags2 = BTreeSet::new();
        tags2.insert("b".to_string());
        tags2.insert("a".to_string());
        let ino2 = state.inode_table.get_or_alloc_dir(&tags2);

        assert_eq!(ino1, ino2);
    }

    // --- Special directory tests ---

    #[test]
    fn inode_table_has_special_dirs() {
        let tbl = InodeTable::new();
        match tbl.get(ALL_INODE) {
            Some(InodeEntry::SpecialDir(kind)) => assert_eq!(*kind, SpecialKind::All),
            other => panic!("Expected SpecialDir(All), got {:?}", other),
        }
        match tbl.get(UNTAGGED_INODE) {
            Some(InodeEntry::SpecialDir(kind)) => assert_eq!(*kind, SpecialKind::Untagged),
            other => panic!("Expected SpecialDir(Untagged), got {:?}", other),
        }
    }

    #[test]
    fn dynamic_inodes_start_after_reserved() {
        let mut tbl = InodeTable::new();
        let ino = tbl.get_or_alloc_dir(&BTreeSet::from(["x".to_string()]));
        assert!(ino >= 4, "Dynamic inode {} should be >= 4", ino);
    }

    #[test]
    fn get_untagged_files_basic() {
        let state = make_test_state();
        let untagged = state.get_untagged_files();
        assert_eq!(untagged.len(), 1);
        assert!(untagged.contains("readme.txt"));
    }

    #[test]
    fn get_untagged_files_none() {
        let mut files = HashMap::new();
        files.insert(
            "song.mp3".to_string(),
            FileEntry {
                ffn: PathBuf::from("music/song.mp3"),
                tags: BTreeSet::from(["music".to_string()]),
            },
        );
        let state = FsState {
            files,
            tagdirs: HashMap::new(),
            by_tags: HashMap::new(),
            inode_table: InodeTable::new(),
        };
        assert!(state.get_untagged_files().is_empty());
    }

    #[test]
    fn get_untagged_files_all_untagged() {
        let mut files = HashMap::new();
        files.insert(
            "a.txt".to_string(),
            FileEntry {
                ffn: PathBuf::from("a.txt"),
                tags: BTreeSet::new(),
            },
        );
        files.insert(
            "b.txt".to_string(),
            FileEntry {
                ffn: PathBuf::from("b.txt"),
                tags: BTreeSet::new(),
            },
        );
        let state = FsState {
            files,
            tagdirs: HashMap::new(),
            by_tags: HashMap::new(),
            inode_table: InodeTable::new(),
        };
        let untagged = state.get_untagged_files();
        assert_eq!(untagged.len(), 2);
        assert!(untagged.contains("a.txt"));
        assert!(untagged.contains("b.txt"));
    }

    #[test]
    fn empty_filesystem_untagged() {
        let state = FsState {
            files: HashMap::new(),
            tagdirs: HashMap::new(),
            by_tags: HashMap::new(),
            inode_table: InodeTable::new(),
        };
        assert!(state.get_untagged_files().is_empty());
    }

    // --- FsState mutation tests ---

    #[test]
    fn get_dir_for_tags_existing() {
        let mut state = make_test_state();
        let root = PathBuf::from("/tmp/nonexistent");
        let tags = BTreeSet::from(["music".to_string()]);
        let dir = state.get_dir_for_tags(&tags, &root).unwrap();
        assert_eq!(dir, PathBuf::from("music"));
    }

    #[test]
    fn get_dir_for_tags_empty() {
        let mut state = make_test_state();
        let root = PathBuf::from("/tmp/nonexistent");
        let tags = BTreeSet::new();
        let dir = state.get_dir_for_tags(&tags, &root).unwrap();
        assert_eq!(dir, PathBuf::from(""));
    }

    #[test]
    fn replace_tag_in_path_basic() {
        let path = PathBuf::from("music/rock");
        let result = FsState::replace_tag_in_path(&path, "rock", "metal");
        assert_eq!(result, PathBuf::from("music/metal"));
    }

    #[test]
    fn replace_tag_in_path_first() {
        let path = PathBuf::from("rock/heavy.mp3");
        let result = FsState::replace_tag_in_path(&path, "rock", "metal");
        assert_eq!(result, PathBuf::from("metal/heavy.mp3"));
    }

    #[test]
    fn replace_tag_in_path_no_match() {
        let path = PathBuf::from("music/jazz");
        let result = FsState::replace_tag_in_path(&path, "rock", "metal");
        assert_eq!(result, PathBuf::from("music/jazz"));
    }

    // --- Physical I/O mutation tests (using tempdir) ---

    /// Create a physical file and matching FsState for mutation testing.
    fn make_physical_state(root: &std::path::Path) -> FsState {
        write_test_layout(root);
        scan_to_state(root)
    }

    // --- rename_file tests ---

    #[test]
    fn rename_file_basic() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        state.rename_file("song.mp3", "track.mp3", &root).unwrap();

        // Physical file moved
        assert!(!root.join("music/song.mp3").exists());
        assert!(root.join("music/track.mp3").exists());
        assert_eq!(fs::read_to_string(root.join("music/track.mp3")).unwrap(), "song data");

        // State updated
        assert!(!state.files.contains_key("song.mp3"));
        assert!(state.files.contains_key("track.mp3"));
        assert_eq!(state.files["track.mp3"].ffn, PathBuf::from("music/track.mp3"));
        assert_eq!(state.files["track.mp3"].tags, BTreeSet::from(["music".to_string()]));

        // by_tags updated
        assert!(state.by_tags["music"].contains("track.mp3"));
        assert!(!state.by_tags["music"].contains("song.mp3"));
    }

    #[test]
    fn rename_file_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Renaming to an existing basename should fail
        let err = state.rename_file("song.mp3", "heavy.mp3", &root).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST));

        // Original file unchanged
        assert!(root.join("music/song.mp3").exists());
        assert!(state.files.contains_key("song.mp3"));
    }

    #[test]
    fn rename_file_nonexistent() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let err = state.rename_file("nope.txt", "new.txt", &root).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn rename_file_untagged() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        state.rename_file("readme.txt", "notes.txt", &root).unwrap();

        assert!(!root.join("readme.txt").exists());
        assert!(root.join("notes.txt").exists());
        assert!(state.files.contains_key("notes.txt"));
        assert!(state.files["notes.txt"].tags.is_empty());
    }

    // --- add_remove_tags tests ---

    #[test]
    fn add_tag_to_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let to_add = BTreeSet::from(["rock".to_string()]);
        let to_remove = BTreeSet::new();
        state.add_remove_tags("song.mp3", &to_add, &to_remove, &root).unwrap();

        // File should now be in music/rock/
        assert!(!root.join("music/song.mp3").exists());
        assert!(root.join("music/rock/song.mp3").exists());
        assert_eq!(
            state.files["song.mp3"].tags,
            BTreeSet::from(["music".to_string(), "rock".to_string()])
        );
        assert!(state.by_tags["rock"].contains("song.mp3"));
    }

    #[test]
    fn remove_tag_from_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let to_add = BTreeSet::new();
        let to_remove = BTreeSet::from(["rock".to_string()]);
        state.add_remove_tags("heavy.mp3", &to_add, &to_remove, &root).unwrap();

        // File should now be in music/ only
        assert!(!root.join("music/rock/heavy.mp3").exists());
        assert!(root.join("music/heavy.mp3").exists());
        assert_eq!(
            state.files["heavy.mp3"].tags,
            BTreeSet::from(["music".to_string()])
        );
        assert!(!state.by_tags["rock"].contains("heavy.mp3"));
        assert!(state.by_tags["music"].contains("heavy.mp3"));
    }

    #[test]
    fn remove_all_tags_from_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let to_add = BTreeSet::new();
        let to_remove = BTreeSet::from(["music".to_string()]);
        state.add_remove_tags("song.mp3", &to_add, &to_remove, &root).unwrap();

        // File should be at source root (untagged)
        assert!(!root.join("music/song.mp3").exists());
        assert!(root.join("song.mp3").exists());
        assert!(state.files["song.mp3"].tags.is_empty());
        assert!(!state.by_tags["music"].contains("song.mp3"));
    }

    #[test]
    fn swap_tags_on_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let to_add = BTreeSet::from(["photos".to_string()]);
        let to_remove = BTreeSet::from(["music".to_string()]);
        state.add_remove_tags("song.mp3", &to_add, &to_remove, &root).unwrap();

        assert!(!root.join("music/song.mp3").exists());
        assert!(root.join("photos/song.mp3").exists());
        assert_eq!(
            state.files["song.mp3"].tags,
            BTreeSet::from(["photos".to_string()])
        );
        assert!(!state.by_tags["music"].contains("song.mp3"));
        assert!(state.by_tags["photos"].contains("song.mp3"));
    }

    #[test]
    fn add_tags_creates_new_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Add a brand new tag that doesn't exist yet
        let to_add = BTreeSet::from(["jazz".to_string()]);
        let to_remove = BTreeSet::from(["music".to_string()]);
        state.add_remove_tags("song.mp3", &to_add, &to_remove, &root).unwrap();

        assert!(root.join("jazz").is_dir());
        assert!(root.join("jazz/song.mp3").exists());
        assert_eq!(
            state.files["song.mp3"].tags,
            BTreeSet::from(["jazz".to_string()])
        );
        assert!(state.by_tags.contains_key("jazz"));
        assert!(state.by_tags["jazz"].contains("song.mp3"));
    }

    #[test]
    fn add_remove_tags_noop() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // No adds, no removes = no-op
        state.add_remove_tags("song.mp3", &BTreeSet::new(), &BTreeSet::new(), &root).unwrap();

        assert!(root.join("music/song.mp3").exists());
        assert_eq!(
            state.files["song.mp3"].tags,
            BTreeSet::from(["music".to_string()])
        );
    }

    // --- rename_tag tests ---

    #[test]
    fn rename_tag_basic() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        state.rename_tag("music", "audio", &root).unwrap();

        // Physical dirs renamed
        assert!(!root.join("music").exists());
        assert!(root.join("audio").is_dir());
        assert!(root.join("audio/rock").is_dir());

        // Files moved
        assert!(root.join("audio/song.mp3").exists());
        assert!(root.join("audio/rock/heavy.mp3").exists());
        assert_eq!(fs::read_to_string(root.join("audio/song.mp3")).unwrap(), "song data");

        // State: files updated
        assert_eq!(
            state.files["song.mp3"].tags,
            BTreeSet::from(["audio".to_string()])
        );
        assert_eq!(state.files["song.mp3"].ffn, PathBuf::from("audio/song.mp3"));
        assert_eq!(
            state.files["heavy.mp3"].tags,
            BTreeSet::from(["audio".to_string(), "rock".to_string()])
        );
        assert_eq!(state.files["heavy.mp3"].ffn, PathBuf::from("audio/rock/heavy.mp3"));

        // State: by_tags updated
        assert!(!state.by_tags.contains_key("music"));
        assert!(state.by_tags.contains_key("audio"));
        assert!(state.by_tags["audio"].contains("song.mp3"));
        assert!(state.by_tags["audio"].contains("heavy.mp3"));

        // State: tagdirs updated
        assert!(state.tagdirs.contains_key(&BTreeSet::from(["audio".to_string()])));
        assert!(state.tagdirs.contains_key(&BTreeSet::from(["audio".to_string(), "rock".to_string()])));
        assert!(!state.tagdirs.contains_key(&BTreeSet::from(["music".to_string()])));
    }

    #[test]
    fn rename_tag_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Renaming to an existing tag should fail
        let err = state.rename_tag("music", "rock", &root).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
    }

    #[test]
    fn rename_tag_noop_same_name() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        state.rename_tag("music", "music", &root).unwrap();
        // Nothing should change
        assert!(root.join("music/song.mp3").exists());
    }

    #[test]
    fn rename_tag_leaf() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        state.rename_tag("rock", "metal", &root).unwrap();

        // Physical
        assert!(!root.join("music/rock").exists());
        assert!(root.join("music/metal").is_dir());
        assert!(root.join("music/metal/heavy.mp3").exists());

        // State
        assert_eq!(
            state.files["heavy.mp3"].tags,
            BTreeSet::from(["music".to_string(), "metal".to_string()])
        );
        assert_eq!(state.files["heavy.mp3"].ffn, PathBuf::from("music/metal/heavy.mp3"));
        assert!(!state.by_tags.contains_key("rock"));
        assert!(state.by_tags["metal"].contains("heavy.mp3"));

        // song.mp3 unaffected (only has "music" tag)
        assert_eq!(
            state.files["song.mp3"].tags,
            BTreeSet::from(["music".to_string()])
        );
    }

    // --- get_dir_for_tags with physical creation ---

    #[test]
    fn get_dir_for_tags_creates_new() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let tags = BTreeSet::from(["jazz".to_string()]);
        let result = state.get_dir_for_tags(&tags, &root).unwrap();

        assert_eq!(result, PathBuf::from("jazz"));
        assert!(root.join("jazz").is_dir());
        assert!(state.tagdirs.contains_key(&tags));
        assert!(state.by_tags.contains_key("jazz"));
    }

    #[test]
    fn get_dir_for_tags_creates_multi() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let tags = BTreeSet::from(["alpha".to_string(), "beta".to_string()]);
        let result = state.get_dir_for_tags(&tags, &root).unwrap();

        assert_eq!(result, PathBuf::from("alpha/beta"));
        assert!(root.join("alpha/beta").is_dir());
    }

    // --- Integration-style: rename_file + add_remove_tags combined ---

    #[test]
    fn rename_and_retag_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Rename song.mp3 -> track.mp3 and move from music to photos
        state.rename_file("song.mp3", "track.mp3", &root).unwrap();
        let to_add = BTreeSet::from(["photos".to_string()]);
        let to_remove = BTreeSet::from(["music".to_string()]);
        state.add_remove_tags("track.mp3", &to_add, &to_remove, &root).unwrap();

        assert!(!root.join("music/song.mp3").exists());
        assert!(!root.join("music/track.mp3").exists());
        assert!(root.join("photos/track.mp3").exists());
        assert_eq!(
            state.files["track.mp3"].tags,
            BTreeSet::from(["photos".to_string()])
        );
    }

    // --- Simulate unlink state changes ---

    #[test]
    fn unlink_state_cleanup() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let ffn = state.files["song.mp3"].ffn.clone();
        let tags = state.files["song.mp3"].tags.clone();

        // Physical delete
        fs::remove_file(root.join(&ffn)).unwrap();

        // State cleanup (mirrors what the FUSE unlink method does)
        state.files.remove("song.mp3");
        for tag in &tags {
            if let Some(set) = state.by_tags.get_mut(tag) {
                set.remove("song.mp3");
            }
        }
        state.inode_table.get_or_alloc_file("song.mp3"); // alloc first so we can remove
        state.inode_table.remove_file("song.mp3");

        assert!(!state.files.contains_key("song.mp3"));
        assert!(!state.by_tags["music"].contains("song.mp3"));
        // heavy.mp3 still in music
        assert!(state.by_tags["music"].contains("heavy.mp3"));

        // Matching files should no longer include song.mp3
        let music_files = state.get_matching_files(&BTreeSet::from(["music".to_string()]));
        assert!(!music_files.contains("song.mp3"));
        assert!(music_files.contains("heavy.mp3"));
    }

    // --- Simulate mkdir state changes ---

    #[test]
    fn mkdir_creates_tag_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let child_tags = BTreeSet::from(["jazz".to_string()]);
        state.get_dir_for_tags(&child_tags, &root).unwrap();
        state.inode_table.get_or_alloc_dir(&child_tags);

        assert!(root.join("jazz").is_dir());
        assert!(state.tagdirs.contains_key(&child_tags));
        let avail = state.get_avail_tags(&BTreeSet::new());
        assert!(avail.contains("jazz"));
    }

    #[test]
    fn mkdir_nested_creates_tag_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Create a nested tag dir: music + jazz
        let child_tags = BTreeSet::from(["music".to_string(), "jazz".to_string()]);
        state.get_dir_for_tags(&child_tags, &root).unwrap();

        assert!(root.join("jazz/music").is_dir());
        assert!(state.tagdirs.contains_key(&child_tags));
    }

    // --- Simulate rmdir state changes ---

    #[test]
    fn rmdir_removes_empty_tag_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // First move the file out of photos so the dir is empty
        let to_add = BTreeSet::from(["music".to_string()]);
        let to_remove = BTreeSet::from(["photos".to_string()]);
        state.add_remove_tags("pic.jpg", &to_add, &to_remove, &root).unwrap();

        // Now rmdir photos
        let photos_tags = BTreeSet::from(["photos".to_string()]);
        if let Some(dirs) = state.tagdirs.get(&photos_tags) {
            for d in dirs.clone() {
                fs::remove_dir(root.join(&d)).unwrap();
            }
        }
        state.tagdirs.remove(&photos_tags);
        state.inode_table.remove_dir(&photos_tags);

        assert!(!root.join("photos").exists());
        assert!(!state.tagdirs.contains_key(&photos_tags));
    }

    // --- Simulate create state changes ---

    #[test]
    fn create_file_with_tags() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let tags = BTreeSet::from(["music".to_string()]);
        let dir_path = state.get_dir_for_tags(&tags, &root).unwrap();
        let ffn = dir_path.join("new_song.mp3");
        fs::write(root.join(&ffn), "new song data").unwrap();

        state.files.insert("new_song.mp3".to_string(), FileEntry {
            ffn,
            tags: tags.clone(),
        });
        for tag in &tags {
            state.by_tags.entry(tag.clone()).or_default().insert("new_song.mp3".to_string());
        }

        assert!(root.join("music/new_song.mp3").exists());
        assert!(state.files.contains_key("new_song.mp3"));
        assert!(state.by_tags["music"].contains("new_song.mp3"));

        let music_files = state.get_matching_files(&BTreeSet::from(["music".to_string()]));
        assert!(music_files.contains("new_song.mp3"));
    }

    #[test]
    fn create_file_untagged() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let tags = BTreeSet::new();
        let dir_path = state.get_dir_for_tags(&tags, &root).unwrap();
        let ffn = if dir_path.as_os_str().is_empty() {
            PathBuf::from("new.txt")
        } else {
            dir_path.join("new.txt")
        };
        fs::write(root.join(&ffn), "new data").unwrap();

        state.files.insert("new.txt".to_string(), FileEntry {
            ffn,
            tags: tags.clone(),
        });

        assert!(root.join("new.txt").exists());
        assert!(state.files["new.txt"].tags.is_empty());
        assert!(state.get_untagged_files().contains("new.txt"));
    }

    // --- Edge case & write operation tests ---

    /// Helper: build state with "rock" at two independent locations:
    /// rock/a.txt (tags={rock}) and pop/rock/b.txt (tags={pop, rock}).
    fn make_multi_location_state(root: &std::path::Path) -> FsState {
        fs::create_dir_all(root.join("rock")).unwrap();
        fs::create_dir_all(root.join("pop/rock")).unwrap();
        fs::write(root.join("rock/a.txt"), "a data").unwrap();
        fs::write(root.join("pop/rock/b.txt"), "b data").unwrap();
        fs::write(root.join("readme.txt"), "readme").unwrap();

        scan_to_state(root)
    }

    #[test]
    fn rename_tag_multiple_independent_locations() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_multi_location_state(&root);

        state.rename_tag("rock", "metal", &root).unwrap();

        // Physical dirs renamed
        assert!(!root.join("rock").exists());
        assert!(root.join("metal").is_dir());
        assert!(!root.join("pop/rock").exists());
        assert!(root.join("pop/metal").is_dir());

        // Files moved
        assert!(root.join("metal/a.txt").exists());
        assert!(root.join("pop/metal/b.txt").exists());
        assert_eq!(fs::read_to_string(root.join("metal/a.txt")).unwrap(), "a data");
        assert_eq!(fs::read_to_string(root.join("pop/metal/b.txt")).unwrap(), "b data");

        // State: files updated
        assert_eq!(state.files["a.txt"].tags, BTreeSet::from(["metal".to_string()]));
        assert_eq!(state.files["a.txt"].ffn, PathBuf::from("metal/a.txt"));
        assert_eq!(
            state.files["b.txt"].tags,
            BTreeSet::from(["metal".to_string(), "pop".to_string()])
        );
        assert_eq!(state.files["b.txt"].ffn, PathBuf::from("pop/metal/b.txt"));

        // State: by_tags updated
        assert!(!state.by_tags.contains_key("rock"));
        assert!(state.by_tags["metal"].contains("a.txt"));
        assert!(state.by_tags["metal"].contains("b.txt"));
        assert!(state.by_tags["pop"].contains("b.txt"));

        // State: tagdirs updated
        assert!(state.tagdirs.contains_key(&BTreeSet::from(["metal".to_string()])));
        assert!(state.tagdirs.contains_key(&BTreeSet::from(["metal".to_string(), "pop".to_string()])));
        assert!(!state.tagdirs.contains_key(&BTreeSet::from(["rock".to_string()])));

        // Untagged file untouched
        assert_eq!(state.files["readme.txt"].ffn, PathBuf::from("readme.txt"));
        assert!(state.files["readme.txt"].tags.is_empty());
    }

    #[test]
    fn add_remove_tags_preserves_superset() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // heavy.mp3 has {music, rock}. Remove only {music} (simulating move from /music/ level).
        let to_add = BTreeSet::new();
        let to_remove = BTreeSet::from(["music".to_string()]);
        state.add_remove_tags("heavy.mp3", &to_add, &to_remove, &root).unwrap();

        // "rock" tag should be preserved
        assert_eq!(state.files["heavy.mp3"].tags, BTreeSet::from(["rock".to_string()]));
        // File should now be at rock/heavy.mp3 (not music/rock/heavy.mp3)
        assert_eq!(state.files["heavy.mp3"].ffn, PathBuf::from("rock/heavy.mp3"));
        assert!(!root.join("music/rock/heavy.mp3").exists());
        assert!(root.join("rock/heavy.mp3").exists());
        assert_eq!(fs::read_to_string(root.join("rock/heavy.mp3")).unwrap(), "heavy data");

        // by_tags: removed from music, still in rock
        assert!(!state.by_tags["music"].contains("heavy.mp3"));
        assert!(state.by_tags["rock"].contains("heavy.mp3"));
    }

    #[test]
    fn add_remove_tags_strip_all_tags() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // heavy.mp3 has {music, rock}. Remove both.
        let to_add = BTreeSet::new();
        let to_remove = BTreeSet::from(["music".to_string(), "rock".to_string()]);
        state.add_remove_tags("heavy.mp3", &to_add, &to_remove, &root).unwrap();

        // File should be at source root with no tags
        assert!(state.files["heavy.mp3"].tags.is_empty());
        assert_eq!(state.files["heavy.mp3"].ffn, PathBuf::from("heavy.mp3"));
        assert!(!root.join("music/rock/heavy.mp3").exists());
        assert!(root.join("heavy.mp3").exists());
        assert_eq!(fs::read_to_string(root.join("heavy.mp3")).unwrap(), "heavy data");

        // by_tags updated
        assert!(!state.by_tags["music"].contains("heavy.mp3"));
        assert!(!state.by_tags["rock"].contains("heavy.mp3"));

        // Should now show as untagged
        assert!(state.get_untagged_files().contains("heavy.mp3"));
    }

    #[test]
    fn add_tags_to_untagged_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // readme.txt is untagged at root. Add {photos}.
        let to_add = BTreeSet::from(["photos".to_string()]);
        let to_remove = BTreeSet::new();
        state.add_remove_tags("readme.txt", &to_add, &to_remove, &root).unwrap();

        // File should now be in photos/
        assert!(!root.join("readme.txt").exists());
        assert!(root.join("photos/readme.txt").exists());
        assert_eq!(state.files["readme.txt"].tags, BTreeSet::from(["photos".to_string()]));
        assert_eq!(state.files["readme.txt"].ffn, PathBuf::from("photos/readme.txt"));
        assert!(state.by_tags["photos"].contains("readme.txt"));

        // No longer untagged
        assert!(!state.get_untagged_files().contains("readme.txt"));
    }

    #[test]
    fn rename_file_and_retag_combined() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Change basename AND tags: heavy.mp3 -> riff.mp3, from {music,rock} to {photos}
        state.rename_file("heavy.mp3", "riff.mp3", &root).unwrap();
        let to_add = BTreeSet::from(["photos".to_string()]);
        let to_remove = BTreeSet::from(["music".to_string(), "rock".to_string()]);
        state.add_remove_tags("riff.mp3", &to_add, &to_remove, &root).unwrap();

        // Physical: old gone, new exists
        assert!(!root.join("music/rock/heavy.mp3").exists());
        assert!(!root.join("music/rock/riff.mp3").exists());
        assert!(root.join("photos/riff.mp3").exists());
        assert_eq!(fs::read_to_string(root.join("photos/riff.mp3")).unwrap(), "heavy data");

        // State
        assert!(!state.files.contains_key("heavy.mp3"));
        assert_eq!(state.files["riff.mp3"].tags, BTreeSet::from(["photos".to_string()]));
        assert_eq!(state.files["riff.mp3"].ffn, PathBuf::from("photos/riff.mp3"));
        assert!(state.by_tags["photos"].contains("riff.mp3"));
        assert!(!state.by_tags["music"].contains("riff.mp3"));
        assert!(!state.by_tags["rock"].contains("riff.mp3"));
    }

    #[test]
    fn unlink_last_file_with_tag() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // pic.jpg is the only file with "photos" tag. Unlink it.
        let ffn = state.files["pic.jpg"].ffn.clone();
        let tags = state.files["pic.jpg"].tags.clone();
        fs::remove_file(root.join(&ffn)).unwrap();
        state.files.remove("pic.jpg");
        for tag in &tags {
            if let Some(set) = state.by_tags.get_mut(tag) {
                set.remove("pic.jpg");
            }
        }
        state.inode_table.remove_file("pic.jpg");

        // by_tags["photos"] is now empty but still exists
        assert!(state.by_tags.contains_key("photos"));
        assert!(state.by_tags["photos"].is_empty());

        // No files match the "photos" tag
        let photos_files = state.get_matching_files(&BTreeSet::from(["photos".to_string()]));
        assert!(photos_files.is_empty());

        // Tag still visible in avail_tags because tagdirs still has {photos}
        let avail = state.get_avail_tags(&BTreeSet::new());
        assert!(avail.contains("photos"));
    }

    #[test]
    fn create_then_unlink_lifecycle() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Create a new file with tags
        let tags = BTreeSet::from(["music".to_string(), "rock".to_string()]);
        let dir_path = state.get_dir_for_tags(&tags, &root).unwrap();
        let ffn = dir_path.join("new.mp3");
        fs::write(root.join(&ffn), "new data").unwrap();
        state.files.insert("new.mp3".to_string(), FileEntry {
            ffn: ffn.clone(),
            tags: tags.clone(),
        });
        for tag in &tags {
            state.by_tags.entry(tag.clone()).or_default().insert("new.mp3".to_string());
        }
        state.inode_table.get_or_alloc_file("new.mp3");

        // Verify creation
        assert!(root.join(&ffn).exists());
        assert!(state.files.contains_key("new.mp3"));
        assert!(state.by_tags["music"].contains("new.mp3"));
        assert!(state.by_tags["rock"].contains("new.mp3"));
        let matching = state.get_matching_files(&BTreeSet::from(["music".to_string(), "rock".to_string()]));
        assert!(matching.contains("new.mp3"));

        // Now unlink
        fs::remove_file(root.join(&ffn)).unwrap();
        state.files.remove("new.mp3");
        for tag in &tags {
            if let Some(set) = state.by_tags.get_mut(tag) {
                set.remove("new.mp3");
            }
        }
        state.inode_table.remove_file("new.mp3");

        // Verify cleanup
        assert!(!root.join(&ffn).exists());
        assert!(!state.files.contains_key("new.mp3"));
        assert!(!state.by_tags["music"].contains("new.mp3"));
        assert!(!state.by_tags["rock"].contains("new.mp3"));
        let matching = state.get_matching_files(&BTreeSet::from(["music".to_string(), "rock".to_string()]));
        assert!(!matching.contains("new.mp3"));
        // heavy.mp3 still there
        assert!(matching.contains("heavy.mp3"));
    }

    #[test]
    fn mkdir_then_rmdir_lifecycle() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // mkdir: create "jazz" tag dir
        let child_tags = BTreeSet::from(["jazz".to_string()]);
        state.get_dir_for_tags(&child_tags, &root).unwrap();
        state.inode_table.get_or_alloc_dir(&child_tags);

        // Verify mkdir
        assert!(root.join("jazz").is_dir());
        assert!(state.tagdirs.contains_key(&child_tags));
        let avail = state.get_avail_tags(&BTreeSet::new());
        assert!(avail.contains("jazz"));

        // rmdir: remove "jazz" tag dir
        if let Some(dirs) = state.tagdirs.get(&child_tags) {
            for d in dirs.clone() {
                fs::remove_dir(root.join(&d)).unwrap();
            }
        }
        state.tagdirs.remove(&child_tags);
        state.inode_table.remove_dir(&child_tags);

        // Verify rmdir
        assert!(!root.join("jazz").exists());
        assert!(!state.tagdirs.contains_key(&child_tags));
        let avail = state.get_avail_tags(&BTreeSet::new());
        assert!(!avail.contains("jazz"));
    }

    #[test]
    fn rename_tag_empty_tag_no_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Create an empty "jazz" tag dir (no files have this tag)
        let jazz_tags = BTreeSet::from(["jazz".to_string()]);
        state.get_dir_for_tags(&jazz_tags, &root).unwrap();
        // by_tags["jazz"] exists (from get_dir_for_tags) but has no files

        state.rename_tag("jazz", "blues", &root).unwrap();

        // Physical dir renamed
        assert!(!root.join("jazz").exists());
        assert!(root.join("blues").is_dir());

        // State: tagdirs updated
        assert!(!state.tagdirs.contains_key(&BTreeSet::from(["jazz".to_string()])));
        assert!(state.tagdirs.contains_key(&BTreeSet::from(["blues".to_string()])));

        // State: by_tags updated
        assert!(!state.by_tags.contains_key("jazz"));
        assert!(state.by_tags.contains_key("blues"));
        assert!(state.by_tags["blues"].is_empty());

        // No files were affected
        assert_eq!(state.files["song.mp3"].tags, BTreeSet::from(["music".to_string()]));
        assert_eq!(state.files["heavy.mp3"].tags, BTreeSet::from(["music".to_string(), "rock".to_string()]));
    }

    #[test]
    fn add_remove_tags_dest_physical_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Create a physical file at the destination that isn't tracked by state
        fs::write(root.join("photos/song.mp3"), "collision").unwrap();

        // Try to move song.mp3 from music to photos — should fail with EEXIST
        let to_add = BTreeSet::from(["photos".to_string()]);
        let to_remove = BTreeSet::from(["music".to_string()]);
        let err = state.add_remove_tags("song.mp3", &to_add, &to_remove, &root).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST));

        // Original file unchanged
        assert!(root.join("music/song.mp3").exists());
        assert_eq!(state.files["song.mp3"].ffn, PathBuf::from("music/song.mp3"));
        assert_eq!(state.files["song.mp3"].tags, BTreeSet::from(["music".to_string()]));
    }

    #[test]
    fn rename_file_same_name_is_eexist() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // rename_file("song.mp3", "song.mp3") — files.contains_key("song.mp3") is true → EEXIST
        let err = state.rename_file("song.mp3", "song.mp3", &root).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST));

        // File unchanged
        assert!(root.join("music/song.mp3").exists());
        assert!(state.files.contains_key("song.mp3"));
    }

    #[test]
    fn rename_tag_untagged_files_unaffected() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let old_ffn = state.files["readme.txt"].ffn.clone();
        let old_tags = state.files["readme.txt"].tags.clone();

        state.rename_tag("music", "audio", &root).unwrap();

        // readme.txt (untagged) completely untouched
        assert_eq!(state.files["readme.txt"].ffn, old_ffn);
        assert_eq!(state.files["readme.txt"].tags, old_tags);
        assert!(state.files["readme.txt"].tags.is_empty());
        assert!(root.join("readme.txt").exists());
    }

    // --- Corner case: error/edge paths for add_remove_tags ---

    #[test]
    fn add_remove_tags_nonexistent_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let err = state
            .add_remove_tags("ghost.txt", &BTreeSet::from(["music".to_string()]), &BTreeSet::new(), &root)
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn add_remove_tags_add_already_present_tag() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let old_ffn = state.files["song.mp3"].ffn.clone();
        let old_tags = state.files["song.mp3"].tags.clone();

        // "music" is already on song.mp3
        state
            .add_remove_tags("song.mp3", &BTreeSet::from(["music".to_string()]), &BTreeSet::new(), &root)
            .unwrap();

        // File should be unchanged (new_ffn == old_ffn guard skips rename)
        assert_eq!(state.files["song.mp3"].ffn, old_ffn);
        assert_eq!(state.files["song.mp3"].tags, old_tags);
        assert!(root.join("music/song.mp3").exists());
    }

    #[test]
    fn add_remove_tags_remove_nonexistent_tag() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let old_ffn = state.files["song.mp3"].ffn.clone();
        let old_tags = state.files["song.mp3"].tags.clone();

        // Remove a tag that song.mp3 doesn't have
        state
            .add_remove_tags("song.mp3", &BTreeSet::new(), &BTreeSet::from(["nonexistent".to_string()]), &root)
            .unwrap();

        // File unchanged
        assert_eq!(state.files["song.mp3"].ffn, old_ffn);
        assert_eq!(state.files["song.mp3"].tags, old_tags);
        assert!(root.join("music/song.mp3").exists());
    }

    #[test]
    fn add_remove_tags_simultaneous_add_and_remove_same_tag() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Pass "music" in both add and remove. Insert then remove on BTreeSet → net removed.
        state
            .add_remove_tags(
                "song.mp3",
                &BTreeSet::from(["music".to_string()]),
                &BTreeSet::from(["music".to_string()]),
                &root,
            )
            .unwrap();

        // Net effect: "music" tag removed. File goes to root, tags empty.
        assert!(state.files["song.mp3"].tags.is_empty());
        assert_eq!(state.files["song.mp3"].ffn, PathBuf::from("song.mp3"));
        assert!(root.join("song.mp3").exists());
        assert!(!root.join("music/song.mp3").exists());
    }

    #[test]
    fn add_remove_tags_round_trip_root_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Tag readme.txt (untagged) with "photos"
        state
            .add_remove_tags("readme.txt", &BTreeSet::from(["photos".to_string()]), &BTreeSet::new(), &root)
            .unwrap();
        assert_eq!(state.files["readme.txt"].ffn, PathBuf::from("photos/readme.txt"));
        assert!(root.join("photos/readme.txt").exists());

        // Remove "photos" → back to root
        state
            .add_remove_tags("readme.txt", &BTreeSet::new(), &BTreeSet::from(["photos".to_string()]), &root)
            .unwrap();

        // ffn should be "readme.txt" (not "./readme.txt")
        assert_eq!(state.files["readme.txt"].ffn, PathBuf::from("readme.txt"));
        assert!(state.files["readme.txt"].tags.is_empty());
        assert!(root.join("readme.txt").exists());
    }

    // --- Multi-step mutation sequences ---

    #[test]
    fn rename_file_then_rename_tag() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        state.rename_file("song.mp3", "track.mp3", &root).unwrap();
        state.rename_tag("music", "audio", &root).unwrap();

        assert_eq!(state.files["track.mp3"].ffn, PathBuf::from("audio/track.mp3"));
        assert_eq!(
            state.files["track.mp3"].tags,
            BTreeSet::from(["audio".to_string()])
        );
        assert!(root.join("audio/track.mp3").exists());
        assert!(state.by_tags["audio"].contains("track.mp3"));
        assert!(!state.by_tags.contains_key("music"));
    }

    #[test]
    fn rename_tag_then_add_remove_tags() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        state.rename_tag("music", "audio", &root).unwrap();
        state
            .add_remove_tags("song.mp3", &BTreeSet::new(), &BTreeSet::from(["audio".to_string()]), &root)
            .unwrap();

        // song.mp3 now untagged at root
        assert!(state.files["song.mp3"].tags.is_empty());
        assert_eq!(state.files["song.mp3"].ffn, PathBuf::from("song.mp3"));
        assert!(root.join("song.mp3").exists());
    }

    #[test]
    fn unlink_then_rename_tag() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Unlink song.mp3
        let ffn = state.files["song.mp3"].ffn.clone();
        let tags = state.files["song.mp3"].tags.clone();
        fs::remove_file(root.join(&ffn)).unwrap();
        state.files.remove("song.mp3");
        for tag in &tags {
            if let Some(set) = state.by_tags.get_mut(tag) {
                set.remove("song.mp3");
            }
        }
        state.inode_table.remove_file("song.mp3");

        // Now rename "music" → "audio"
        state.rename_tag("music", "audio", &root).unwrap();

        // by_tags["audio"] should have only heavy.mp3
        assert!(state.by_tags.contains_key("audio"));
        assert!(state.by_tags["audio"].contains("heavy.mp3"));
        assert!(!state.by_tags["audio"].contains("song.mp3"));
        assert_eq!(state.by_tags["audio"].len(), 1);
    }

    #[test]
    fn rename_tag_twice_sequential() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        state.rename_tag("rock", "metal", &root).unwrap();
        state.rename_tag("metal", "heavy_tag", &root).unwrap();

        assert_eq!(
            state.files["heavy.mp3"].tags,
            BTreeSet::from(["music".to_string(), "heavy_tag".to_string()])
        );
        assert_eq!(
            state.files["heavy.mp3"].ffn,
            PathBuf::from("music/heavy_tag/heavy.mp3")
        );

        // No stale entries
        assert!(!state.by_tags.contains_key("rock"));
        assert!(!state.by_tags.contains_key("metal"));
        assert!(state.by_tags.contains_key("heavy_tag"));
        assert!(!state.tagdirs.contains_key(&BTreeSet::from(["rock".to_string()])));
        assert!(!state.tagdirs.contains_key(&BTreeSet::from(["metal".to_string()])));
        assert!(!state.tagdirs.contains_key(&BTreeSet::from(["music".to_string(), "rock".to_string()])));
        assert!(!state.tagdirs.contains_key(&BTreeSet::from(["music".to_string(), "metal".to_string()])));
        assert!(state.tagdirs.contains_key(&BTreeSet::from(["music".to_string(), "heavy_tag".to_string()])));

        // Physical
        assert!(root.join("music/heavy_tag/heavy.mp3").exists());
        assert!(!root.join("music/rock").exists());
        assert!(!root.join("music/metal").exists());
    }

    #[test]
    fn create_file_then_retag() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        // Create file in "music" dir
        let tags = BTreeSet::from(["music".to_string()]);
        let dir_path = state.get_dir_for_tags(&tags, &root).unwrap();
        let ffn = dir_path.join("new_song.mp3");
        fs::write(root.join(&ffn), "new song data").unwrap();
        state.files.insert("new_song.mp3".to_string(), FileEntry {
            ffn,
            tags: tags.clone(),
        });
        for tag in &tags {
            state.by_tags.entry(tag.clone()).or_default().insert("new_song.mp3".to_string());
        }

        // Now retag: move from music to rock
        state
            .add_remove_tags(
                "new_song.mp3",
                &BTreeSet::from(["rock".to_string()]),
                &BTreeSet::from(["music".to_string()]),
                &root,
            )
            .unwrap();

        assert_eq!(
            state.files["new_song.mp3"].tags,
            BTreeSet::from(["rock".to_string()])
        );
        assert!(state.by_tags["rock"].contains("new_song.mp3"));
        assert!(!state.by_tags["music"].contains("new_song.mp3"));
        assert!(root.join("rock/new_song.mp3").exists());
        assert!(!root.join("music/new_song.mp3").exists());
    }

    // --- Inode consistency ---

    #[test]
    fn rename_file_inode_consistency() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut state = make_physical_state(&root);

        let old_ino = state.inode_table.get_or_alloc_file("song.mp3");
        state.rename_file("song.mp3", "track.mp3", &root).unwrap();

        // Old inode gone
        assert!(state.inode_table.get(old_ino).is_none());
        assert!(state.inode_table.file_to_inode.get("song.mp3").is_none());

        // New inode allocated for track.mp3
        let new_ino = *state.inode_table.file_to_inode.get("track.mp3").unwrap();
        assert!(state.inode_table.get(new_ino).is_some());

        // Re-allocating "song.mp3" gives a fresh, different inode
        let fresh_ino = state.inode_table.get_or_alloc_file("song.mp3");
        assert_ne!(fresh_ino, old_ino);
        assert_ne!(fresh_ino, new_ino);
    }

    #[test]
    fn rmdir_nonempty_fails() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        // Try to rmdir "photos" which contains pic.jpg
        let photos_tags = BTreeSet::from(["photos".to_string()]);
        let dirs = state.tagdirs.get(&photos_tags).unwrap().clone();
        let result = dirs.iter()
            .map(|d| fs::remove_dir(root.join(d)))
            .find(|r| r.is_err());

        // Should fail because directory is not empty
        assert!(result.is_some());
        let err = result.unwrap().unwrap_err();
        // On Linux this is ENOTEMPTY
        assert!(
            err.raw_os_error() == Some(libc::ENOTEMPTY) ||
            err.raw_os_error() == Some(libc::EEXIST), // Some systems use EEXIST
            "Expected ENOTEMPTY or EEXIST, got {:?}", err
        );

        // Directory still exists, state unchanged
        assert!(root.join("photos").exists());
        assert!(root.join("photos/pic.jpg").exists());
        assert!(state.tagdirs.contains_key(&photos_tags));
    }

    // --- External source directory changes ---
    // These tests document behavior when files/dirs are changed outside FUSE.
    // The filesystem is snapshot-based: scanner runs once at mount, never rescans.

    fn make_test_tagfs(root: &std::path::Path) -> TagFs {
        write_test_layout(root);
        let scan = crate::scanner::scan_tree(root, false);
        TagFs::new(root.to_path_buf(), scan)
    }

    // --- 1. File deleted from source ---

    #[test]
    fn external_delete_file_still_in_state() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::remove_file(root.join("music/song.mp3")).unwrap();

        assert!(state.files.contains_key("song.mp3"));
        assert!(state.by_tags["music"].contains("song.mp3"));
    }

    #[test]
    fn external_delete_file_attr_fails() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let tagfs = make_test_tagfs(&root);

        let ino = {
            let mut state = tagfs.state.write().unwrap();
            state.inode_table.get_or_alloc_file("song.mp3")
        };

        fs::remove_file(root.join("music/song.mp3")).unwrap();

        let state = tagfs.state.read().unwrap();
        let result = tagfs.file_attr(ino, "song.mp3", &state);
        assert!(result.is_err());
    }

    #[test]
    fn external_delete_file_kind_fallback() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::remove_file(root.join("music/song.mp3")).unwrap();

        // file_kind falls back to RegularFile when stat fails
        assert_eq!(state.file_kind(&root, "song.mp3"), FileType::RegularFile);
    }

    #[test]
    fn external_delete_resolve_path_still_works() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let tagfs = make_test_tagfs(&root);

        fs::remove_file(root.join("music/song.mp3")).unwrap();

        // resolve_file_path only queries in-memory state
        let state = tagfs.state.read().unwrap();
        let result = tagfs.resolve_file_path("song.mp3", &state);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), root.join("music/song.mp3"));
    }

    #[test]
    fn external_delete_avail_tags_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::remove_file(root.join("music/song.mp3")).unwrap();

        let tags = state.get_avail_tags(&BTreeSet::new());
        assert!(tags.contains("music"));
        assert!(tags.contains("rock"));
        assert!(tags.contains("photos"));
    }

    #[test]
    fn external_delete_all_files_in_tag_still_shows_tag() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        // pic.jpg is the only file in photos; delete it
        fs::remove_file(root.join("photos/pic.jpg")).unwrap();

        let tags = state.get_avail_tags(&BTreeSet::new());
        assert!(tags.contains("photos"));
    }

    // --- 2. File added to source ---

    #[test]
    fn external_add_file_invisible() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::write(root.join("music/new_track.mp3"), "new data").unwrap();

        assert!(!state.files.contains_key("new_track.mp3"));
        let matching = state.get_matching_files(&BTreeSet::from(["music".to_string()]));
        assert!(!matching.contains("new_track.mp3"));
    }

    #[test]
    fn external_add_file_in_new_dir_invisible() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::create_dir_all(root.join("jazz")).unwrap();
        fs::write(root.join("jazz/tune.mp3"), "jazz data").unwrap();

        assert!(!state.files.contains_key("tune.mp3"));
        let tags = state.get_avail_tags(&BTreeSet::new());
        assert!(!tags.contains("jazz"));
    }

    #[test]
    fn external_add_untagged_file_invisible() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::write(root.join("newfile.txt"), "new data").unwrap();

        let untagged = state.get_untagged_files();
        assert!(!untagged.contains("newfile.txt"));
    }

    // --- 3. File renamed in source ---

    #[test]
    fn external_rename_old_name_still_in_state() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::rename(root.join("music/song.mp3"), root.join("music/track.mp3")).unwrap();

        assert!(state.files.contains_key("song.mp3"));
        assert_eq!(state.files["song.mp3"].ffn, PathBuf::from("music/song.mp3"));
    }

    #[test]
    fn external_rename_old_name_file_attr_fails() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let tagfs = make_test_tagfs(&root);

        let ino = {
            let mut state = tagfs.state.write().unwrap();
            state.inode_table.get_or_alloc_file("song.mp3")
        };

        fs::rename(root.join("music/song.mp3"), root.join("music/track.mp3")).unwrap();

        let state = tagfs.state.read().unwrap();
        let result = tagfs.file_attr(ino, "song.mp3", &state);
        assert!(result.is_err());
    }

    #[test]
    fn external_rename_new_name_invisible() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::rename(root.join("music/song.mp3"), root.join("music/track.mp3")).unwrap();

        let matching = state.get_matching_files(&BTreeSet::from(["music".to_string()]));
        assert!(matching.contains("song.mp3"));
        assert!(!matching.contains("track.mp3"));
    }

    #[test]
    fn external_rename_by_tags_stale() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::rename(root.join("music/song.mp3"), root.join("music/track.mp3")).unwrap();

        assert!(state.by_tags["music"].contains("song.mp3"));
        assert!(!state.by_tags["music"].contains("track.mp3"));
    }

    // --- 4. File content modified ---

    #[test]
    fn external_modify_file_attr_reflects_new_size() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let tagfs = make_test_tagfs(&root);

        let ino = {
            let mut state = tagfs.state.write().unwrap();
            state.inode_table.get_or_alloc_file("song.mp3")
        };

        let new_content = "much longer song data content here";
        fs::write(root.join("music/song.mp3"), new_content).unwrap();

        let state = tagfs.state.read().unwrap();
        let attr = tagfs.file_attr(ino, "song.mp3", &state).unwrap();
        assert_eq!(attr.size, new_content.len() as u64);
    }

    #[test]
    fn external_modify_state_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::write(root.join("music/song.mp3"), "different content").unwrap();

        assert_eq!(state.files["song.mp3"].ffn, PathBuf::from("music/song.mp3"));
        assert_eq!(state.files["song.mp3"].tags, BTreeSet::from(["music".to_string()]));
    }

    #[test]
    fn external_modify_file_kind_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::write(root.join("music/song.mp3"), "different content").unwrap();

        assert_eq!(state.file_kind(&root, "song.mp3"), FileType::RegularFile);
    }

    // --- 5. Directory deleted from source ---

    #[test]
    fn external_delete_dir_tags_still_available() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::remove_dir_all(root.join("music")).unwrap();

        let tags = state.get_avail_tags(&BTreeSet::new());
        assert!(tags.contains("music"));
        assert!(tags.contains("rock"));
    }

    #[test]
    fn external_delete_dir_files_still_in_state() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::remove_dir_all(root.join("photos")).unwrap();

        assert!(state.files.contains_key("pic.jpg"));
        assert_eq!(state.files["pic.jpg"].ffn, PathBuf::from("photos/pic.jpg"));
    }

    #[test]
    fn external_delete_dir_file_attr_fails() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let tagfs = make_test_tagfs(&root);

        let ino = {
            let mut state = tagfs.state.write().unwrap();
            state.inode_table.get_or_alloc_file("pic.jpg")
        };

        fs::remove_dir_all(root.join("photos")).unwrap();

        let state = tagfs.state.read().unwrap();
        let result = tagfs.file_attr(ino, "pic.jpg", &state);
        assert!(result.is_err());
    }

    // --- 6. Directory added to source ---

    #[test]
    fn external_add_dir_invisible() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::create_dir(root.join("jazz")).unwrap();

        let tags = state.get_avail_tags(&BTreeSet::new());
        assert!(!tags.contains("jazz"));
    }

    #[test]
    fn external_add_dir_with_files_invisible() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::create_dir_all(root.join("jazz")).unwrap();
        fs::write(root.join("jazz/smooth.mp3"), "smooth data").unwrap();

        let tags = state.get_avail_tags(&BTreeSet::new());
        assert!(!tags.contains("jazz"));
        assert!(!state.files.contains_key("smooth.mp3"));
    }

    #[test]
    fn external_add_nested_dir_invisible() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::create_dir_all(root.join("music/jazz")).unwrap();

        let music_tags = BTreeSet::from(["music".to_string()]);
        let avail = state.get_avail_tags(&music_tags);
        assert!(!avail.contains("jazz"));
    }

    // --- 7. Cross-cutting edge cases ---

    #[test]
    fn external_delete_then_fuse_unlink() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let state = make_physical_state(&root);

        fs::remove_file(root.join("music/song.mp3")).unwrap();

        // File still in state
        assert!(state.files.contains_key("song.mp3"));

        // Simulate what FUSE unlink would do: try physical delete
        let ffn = state.files["song.mp3"].ffn.clone();
        let result = fs::remove_file(root.join(&ffn));
        assert!(result.is_err());
    }

    #[test]
    fn external_replace_file_in_place() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let tagfs = make_test_tagfs(&root);

        let ino = {
            let mut state = tagfs.state.write().unwrap();
            state.inode_table.get_or_alloc_file("song.mp3")
        };

        // Delete and recreate with different content
        let new_content = "brand new song data with more bytes";
        fs::remove_file(root.join("music/song.mp3")).unwrap();
        fs::write(root.join("music/song.mp3"), new_content).unwrap();

        let state = tagfs.state.read().unwrap();
        let attr = tagfs.file_attr(ino, "song.mp3", &state).unwrap();
        assert_eq!(attr.size, new_content.len() as u64);
    }

    #[test]
    fn external_delete_and_recreate_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let tagfs = make_test_tagfs(&root);

        let ino = {
            let mut state = tagfs.state.write().unwrap();
            state.inode_table.get_or_alloc_file("pic.jpg")
        };

        // rm -rf photos/, recreate empty
        fs::remove_dir_all(root.join("photos")).unwrap();
        fs::create_dir(root.join("photos")).unwrap();

        let state = tagfs.state.read().unwrap();
        assert!(state.files.contains_key("pic.jpg"));

        let result = tagfs.file_attr(ino, "pic.jpg", &state);
        assert!(result.is_err());
    }
}
