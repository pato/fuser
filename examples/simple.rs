#![allow(clippy::needless_return)]

use clap::{crate_version, App, Arg};
use fuser::{
    Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, FUSE_ROOT_ID,
};
use log::LevelFilter;
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use std::cmp::min;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::raw::c_int;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use std::{env, fs, io};

const BLOCK_SIZE: u64 = 512;
const MAX_NAME_LENGTH: u32 = 255;
const MAX_FILE_SIZE: u64 = 1024 * 1024 * 1024 * 1024;

// Top two file handle bits are used to store permissions
// Note: This isn't safe, since the client can modify those bits. However, this implementation
// is just a toy
const FILE_HANDLE_READ_BIT: u64 = 1 << 63;
const FILE_HANDLE_WRITE_BIT: u64 = 1 << 62;

const FMODE_EXEC: i32 = 0x20;

type Inode = u64;

type DirectoryDescriptor = BTreeMap<String, (Inode, FileKind)>;

#[derive(Serialize, Deserialize, Copy, Clone, PartialEq)]
enum FileKind {
    File,
    Directory,
    Symlink,
}

impl From<FileKind> for fuser::FileType {
    fn from(kind: FileKind) -> Self {
        match kind {
            FileKind::File => fuser::FileType::RegularFile,
            FileKind::Directory => fuser::FileType::Directory,
            FileKind::Symlink => fuser::FileType::Symlink,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct InodeAttributes {
    pub inode: Inode,
    pub open_file_handles: u64, // Ref count of open file handles to this inode
    pub size: u64,
    pub last_accessed: SystemTime,
    pub last_modified: SystemTime,
    pub last_metadata_changed: SystemTime,
    pub kind: FileKind,
    // Permissions and special mode bits
    pub mode: u16,
    pub hardlinks: u32,
    pub uid: u32,
    pub gid: u32,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl From<InodeAttributes> for fuser::FileAttr {
    fn from(attrs: InodeAttributes) -> Self {
        fuser::FileAttr {
            ino: attrs.inode,
            size: attrs.size,
            blocks: (attrs.size + BLOCK_SIZE - 1) / BLOCK_SIZE,
            atime: attrs.last_accessed,
            mtime: attrs.last_modified,
            ctime: attrs.last_metadata_changed,
            crtime: SystemTime::UNIX_EPOCH,
            kind: attrs.kind.into(),
            perm: attrs.mode,
            nlink: attrs.hardlinks,
            uid: attrs.uid,
            gid: attrs.gid,
            rdev: 0,
            blksize: BLOCK_SIZE as u32,
            padding: 0,
            flags: 0,
        }
    }
}

// Stores inode metadata data in "$data_dir/inodes" and file contents in "$data_dir/contents"
// Directory data is stored in the file's contents, as a serialized DirectoryDescriptor
struct SimpleFS {
    data_dir: String,
    next_file_handle: AtomicU64,
}

impl SimpleFS {
    fn new(data_dir: String) -> SimpleFS {
        SimpleFS {
            data_dir,
            next_file_handle: AtomicU64::new(1),
        }
    }

    fn allocate_next_inode(&self) -> Inode {
        let path = Path::new(&self.data_dir).join("superblock");
        let current_inode = if let Ok(file) = File::open(&path) {
            bincode::deserialize_from(file).unwrap()
        } else {
            fuser::FUSE_ROOT_ID
        };

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        bincode::serialize_into(file, &(current_inode + 1)).unwrap();

        current_inode + 1
    }

    fn allocate_next_file_handle(&self, read: bool, write: bool) -> u64 {
        let mut fh = self.next_file_handle.fetch_add(1, Ordering::SeqCst);
        // Assert that we haven't run out of file handles
        assert!(fh < FILE_HANDLE_WRITE_BIT && fh < FILE_HANDLE_READ_BIT);
        if read {
            fh |= FILE_HANDLE_READ_BIT;
        }
        if write {
            fh |= FILE_HANDLE_WRITE_BIT;
        }

        fh
    }

    fn check_file_handle_read(&self, file_handle: u64) -> bool {
        (file_handle & FILE_HANDLE_READ_BIT) != 0
    }

    fn check_file_handle_write(&self, file_handle: u64) -> bool {
        (file_handle & FILE_HANDLE_WRITE_BIT) != 0
    }

    fn content_path(&self, inode: Inode) -> PathBuf {
        Path::new(&self.data_dir)
            .join("contents")
            .join(inode.to_string())
    }

    fn get_directory_content(&self, inode: Inode) -> Result<DirectoryDescriptor, c_int> {
        let path = Path::new(&self.data_dir)
            .join("contents")
            .join(inode.to_string());
        if let Ok(file) = File::open(&path) {
            Ok(bincode::deserialize_from(file).unwrap())
        } else {
            Err(libc::ENOENT)
        }
    }

    fn write_directory_content(&self, inode: Inode, entries: DirectoryDescriptor) {
        let path = Path::new(&self.data_dir)
            .join("contents")
            .join(inode.to_string());
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        bincode::serialize_into(file, &entries).unwrap();
    }

    fn get_inode(&self, inode: Inode) -> Result<InodeAttributes, c_int> {
        let path = Path::new(&self.data_dir)
            .join("inodes")
            .join(inode.to_string());
        if let Ok(file) = File::open(&path) {
            Ok(bincode::deserialize_from(file).unwrap())
        } else {
            Err(libc::ENOENT)
        }
    }

    fn write_inode(&self, inode: &InodeAttributes) {
        let path = Path::new(&self.data_dir)
            .join("inodes")
            .join(inode.inode.to_string());
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        bincode::serialize_into(file, inode).unwrap();
    }

    // Check whether a file should be removed from storage. Should be called after decrementing
    // the link count, or closing a file handle
    fn gc_inode(&self, inode: &InodeAttributes) -> bool {
        if inode.hardlinks == 0 && inode.open_file_handles == 0 {
            let inode_path = Path::new(&self.data_dir)
                .join("inodes")
                .join(inode.inode.to_string());
            fs::remove_file(inode_path).unwrap();
            let content_path = Path::new(&self.data_dir)
                .join("contents")
                .join(inode.inode.to_string());
            fs::remove_file(content_path).unwrap();

            return true;
        }

        return false;
    }

    fn truncate(
        &self,
        inode: Inode,
        new_length: u64,
        uid: u32,
        gid: u32,
    ) -> Result<InodeAttributes, c_int> {
        if new_length > MAX_FILE_SIZE {
            return Err(libc::EFBIG);
        }

        let mut attrs = self.get_inode(inode)?;

        if !check_access(
            attrs.uid,
            attrs.gid,
            attrs.mode,
            uid,
            gid,
            libc::W_OK as u32,
        ) {
            return Err(libc::EACCES);
        }

        let path = self.content_path(inode);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(new_length).unwrap();

        attrs.size = new_length;
        attrs.last_metadata_changed = SystemTime::now();
        attrs.last_modified = SystemTime::now();

        self.write_inode(&attrs);

        Ok(attrs)
    }

    fn lookup_name(&self, parent: u64, name: &OsStr) -> Result<InodeAttributes, c_int> {
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            return Err(libc::EINVAL);
        };

        let entries = self.get_directory_content(parent)?;
        if let Some((inode, _)) = entries.get(name) {
            return self.get_inode(*inode);
        } else {
            return Err(libc::ENOENT);
        }
    }

    fn insert_link(
        &self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        inode: u64,
        kind: FileKind,
    ) -> Result<(), c_int> {
        if self.lookup_name(parent, name).is_ok() {
            return Err(libc::EEXIST);
        }

        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            return Err(libc::EINVAL);
        };

        let mut parent_attrs = self.get_inode(parent)?;

        if !check_access(
            parent_attrs.uid,
            parent_attrs.gid,
            parent_attrs.mode,
            req.uid(),
            req.gid(),
            libc::W_OK as u32,
        ) {
            return Err(libc::EACCES);
        }
        parent_attrs.last_modified = SystemTime::now();
        parent_attrs.last_metadata_changed = SystemTime::now();
        self.write_inode(&parent_attrs);

        let mut entries = self.get_directory_content(parent).unwrap();
        entries.insert(name.to_string(), (inode, kind));
        self.write_directory_content(parent, entries);

        Ok(())
    }
}

impl Filesystem for SimpleFS {
    fn init(&mut self, _req: &Request) -> Result<(), c_int> {
        fs::create_dir_all(Path::new(&self.data_dir).join("inodes")).unwrap();
        fs::create_dir_all(Path::new(&self.data_dir).join("contents")).unwrap();
        if self.get_inode(FUSE_ROOT_ID).is_err() {
            // Initialize with empty filesystem
            let root = InodeAttributes {
                inode: FUSE_ROOT_ID,
                open_file_handles: 0,
                size: 0,
                last_accessed: SystemTime::now(),
                last_modified: SystemTime::now(),
                last_metadata_changed: SystemTime::now(),
                kind: FileKind::Directory,
                mode: 0o777,
                hardlinks: 2,
                uid: 0,
                gid: 0,
                xattrs: Default::default(),
            };
            self.write_inode(&root);
            let mut entries = BTreeMap::new();
            entries.insert(".".to_string(), (FUSE_ROOT_ID, FileKind::Directory));
            self.write_directory_content(FUSE_ROOT_ID, entries);
        }
        Ok(())
    }

    fn destroy(&mut self, _req: &Request) {}

    fn lookup(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        if name.len() > MAX_NAME_LENGTH as usize {
            reply.error(libc::ENAMETOOLONG);
            return;
        }
        let parent_attrs = self.get_inode(parent).unwrap();
        if !check_access(
            parent_attrs.uid,
            parent_attrs.gid,
            parent_attrs.mode,
            req.uid(),
            req.gid(),
            libc::X_OK as u32,
        ) {
            reply.error(libc::EACCES);
            return;
        }

        match self.lookup_name(parent, name) {
            Ok(attrs) => reply.entry(&Duration::new(0, 0), &attrs.into(), 0),
            Err(error_code) => reply.error(error_code),
        }
    }

    fn forget(&mut self, _req: &Request, _ino: u64, _nlookup: u64) {}

    fn getattr(&mut self, _req: &Request, inode: u64, reply: ReplyAttr) {
        match self.get_inode(inode) {
            Ok(attrs) => reply.attr(&Duration::new(0, 0), &attrs.into()),
            Err(error_code) => reply.error(error_code),
        }
    }

    fn setattr(
        &mut self,
        req: &Request,
        inode: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<SystemTime>,
        atime_now: bool,
        mtime: Option<SystemTime>,
        mtime_now: bool,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let mut attrs = match self.get_inode(inode) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        if let Some(mode) = mode {
            debug!("chmod() called with {:?}, {:o}", inode, mode);
            if req.uid() != 0 && req.uid() != attrs.uid {
                reply.error(libc::EPERM);
                return;
            }
            attrs.mode = mode as u16;
            attrs.last_metadata_changed = SystemTime::now();
            self.write_inode(&attrs);
            reply.attr(&Duration::new(0, 0), &attrs.into());
            return;
        }

        if uid.is_some() || gid.is_some() {
            debug!("chown() called with {:?} {:?} {:?}", inode, uid, gid);
            if let Some(gid) = gid {
                // Non-root users can only change gid to a group they're in
                if req.uid() != 0 && !get_groups(req.pid()).contains(&gid) {
                    reply.error(libc::EPERM);
                    return;
                }
            }
            if let Some(uid) = uid {
                if req.uid() != 0
                    // but no-op changes by the owner are not an error
                    && !(uid == attrs.uid && req.uid() == attrs.uid)
                {
                    reply.error(libc::EPERM);
                    return;
                }
            }
            // Only owner may change the group
            if gid.is_some() && req.uid() != 0 && req.uid() != attrs.uid {
                reply.error(libc::EPERM);
                return;
            }

            if let Some(uid) = uid {
                attrs.uid = uid;
            }
            if let Some(gid) = gid {
                attrs.gid = gid;
            }
            attrs.last_metadata_changed = SystemTime::now();
            self.write_inode(&attrs);
            reply.attr(&Duration::new(0, 0), &attrs.into());
            return;
        }

        if let Some(size) = size {
            debug!("truncate() called with {:?} {:?}", inode, size);
            if let Some(handle) = fh {
                // If the file handle is available, check access locally.
                // This is important as it preserves the semantic that a file handle opened
                // with W_OK will never fail to truncate, even if the file has been subsequently
                // chmod'ed
                if self.check_file_handle_write(handle) {
                    if let Err(error_code) = self.truncate(inode, size, 0, 0) {
                        reply.error(error_code);
                        return;
                    }
                } else {
                    reply.error(libc::EACCES);
                    return;
                }
            } else if let Err(error_code) = self.truncate(inode, size, req.uid(), req.gid()) {
                reply.error(error_code);
                return;
            }
        }

        if atime.is_some() || mtime.is_some() {
            debug!(
                "utimens() called with {:?}, {:?}, {:?}",
                inode, atime, mtime
            );
            let now = SystemTime::now();
            let atime = if atime_now { Some(now) } else { atime };
            let mtime = if mtime_now { Some(now) } else { mtime };

            if attrs.uid != req.uid() && req.uid() != 0 && (!atime_now || !mtime_now) {
                reply.error(libc::EPERM);
                return;
            }

            if attrs.uid != req.uid()
                && !check_access(
                    attrs.uid,
                    attrs.gid,
                    attrs.mode,
                    req.uid(),
                    req.gid(),
                    libc::W_OK as u32,
                )
            {
                reply.error(libc::EACCES);
                return;
            }

            if let Some(atime) = atime {
                attrs.last_accessed = atime;
            }
            if let Some(mtime) = mtime {
                attrs.last_modified = mtime;
            }
            self.write_inode(&attrs);
        }

        let attrs = self.get_inode(inode).unwrap();
        reply.attr(&Duration::new(0, 0), &attrs.into());
        return;
    }

    fn readlink(&mut self, _req: &Request, inode: u64, reply: ReplyData) {
        debug!("readlink() called on {:?}", inode);
        let path = self.content_path(inode);
        if let Ok(mut file) = File::open(&path) {
            let file_size = file.metadata().unwrap().len();
            let mut buffer = vec![0; file_size as usize];
            file.read_exact(&mut buffer).unwrap();
            reply.data(&buffer);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn mknod(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        let file_type = mode & libc::S_IFMT as u32;

        if file_type != libc::S_IFREG as u32
            && file_type != libc::S_IFLNK as u32
            && file_type != libc::S_IFDIR as u32
        {
            // TODO
            warn!("mknod() implementation is incomplete. Only supports regular files, symlinks, and directories. Got {:o}", mode);
            reply.error(libc::ENOSYS);
            return;
        }

        if self.lookup_name(parent, name).is_ok() {
            reply.error(libc::EEXIST);
            return;
        }

        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };

        let mut parent_attrs = match self.get_inode(parent) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        if !check_access(
            parent_attrs.uid,
            parent_attrs.gid,
            parent_attrs.mode,
            req.uid(),
            req.gid(),
            libc::W_OK as u32,
        ) {
            reply.error(libc::EACCES);
            return;
        }
        parent_attrs.last_modified = SystemTime::now();
        parent_attrs.last_metadata_changed = SystemTime::now();
        self.write_inode(&parent_attrs);

        let inode = self.allocate_next_inode();
        let attrs = InodeAttributes {
            inode,
            open_file_handles: 0,
            size: 0,
            last_accessed: SystemTime::now(),
            last_modified: SystemTime::now(),
            last_metadata_changed: SystemTime::now(),
            kind: as_file_kind(mode),
            // TODO: suid/sgid not supported
            mode: (mode & !(libc::S_ISUID | libc::S_ISGID) as u32) as u16,
            hardlinks: 1,
            uid: req.uid(),
            gid: req.gid(),
            xattrs: Default::default(),
        };
        self.write_inode(&attrs);
        File::create(self.content_path(inode)).unwrap();

        if as_file_kind(mode) == FileKind::Directory {
            let mut entries = BTreeMap::new();
            entries.insert(".".to_string(), (inode, FileKind::Directory));
            entries.insert("..".to_string(), (parent, FileKind::Directory));
            self.write_directory_content(inode, entries);
        }

        let mut entries = self.get_directory_content(parent).unwrap();
        entries.insert(name.to_string(), (inode, attrs.kind));
        self.write_directory_content(parent, entries);

        // TODO: implement flags
        reply.entry(&Duration::new(0, 0), &attrs.into(), 0);
    }

    fn mkdir(&mut self, req: &Request, parent: u64, name: &OsStr, mode: u32, reply: ReplyEntry) {
        debug!("mkdir() called with {:?} {:?} {:o}", parent, name, mode);
        if self.lookup_name(parent, name).is_ok() {
            reply.error(libc::EEXIST);
            return;
        }

        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };

        let mut parent_attrs = match self.get_inode(parent) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        if !check_access(
            parent_attrs.uid,
            parent_attrs.gid,
            parent_attrs.mode,
            req.uid(),
            req.gid(),
            libc::W_OK as u32,
        ) {
            reply.error(libc::EACCES);
            return;
        }
        parent_attrs.last_modified = SystemTime::now();
        parent_attrs.last_metadata_changed = SystemTime::now();
        self.write_inode(&parent_attrs);

        let inode = self.allocate_next_inode();
        let attrs = InodeAttributes {
            inode,
            open_file_handles: 0,
            size: BLOCK_SIZE,
            last_accessed: SystemTime::now(),
            last_modified: SystemTime::now(),
            last_metadata_changed: SystemTime::now(),
            kind: FileKind::Directory,
            // TODO: suid/sgid not supported
            mode: (mode & !(libc::S_ISUID | libc::S_ISGID) as u32) as u16,
            hardlinks: 2, // Directories start with link count of 2, since they have a self link
            uid: req.uid(),
            gid: req.gid(),
            xattrs: Default::default(),
        };
        self.write_inode(&attrs);

        let mut entries = BTreeMap::new();
        entries.insert(".".to_string(), (inode, FileKind::Directory));
        entries.insert("..".to_string(), (parent, FileKind::Directory));
        self.write_directory_content(inode, entries);

        let mut entries = self.get_directory_content(parent).unwrap();
        entries.insert(name.to_string(), (inode, FileKind::Directory));
        self.write_directory_content(parent, entries);

        reply.entry(&Duration::new(0, 0), &attrs.into(), 0);
    }

    fn unlink(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        debug!("unlink() called with {:?} {:?}", parent, name);
        let mut attrs = match self.lookup_name(parent, name) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };

        let mut parent_attrs = match self.get_inode(parent) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        if !check_access(
            parent_attrs.uid,
            parent_attrs.gid,
            parent_attrs.mode,
            req.uid(),
            req.gid(),
            libc::W_OK as u32,
        ) {
            reply.error(libc::EACCES);
            return;
        }

        let uid = req.uid();
        // "Sticky bit" handling
        if parent_attrs.mode & libc::S_ISVTX as u16 != 0
            && uid != 0
            && uid != parent_attrs.uid
            && uid != attrs.uid
        {
            reply.error(libc::EACCES);
            return;
        }

        parent_attrs.last_metadata_changed = SystemTime::now();
        parent_attrs.last_modified = SystemTime::now();
        self.write_inode(&parent_attrs);

        attrs.hardlinks -= 1;
        attrs.last_metadata_changed = SystemTime::now();
        self.write_inode(&attrs);
        self.gc_inode(&attrs);

        let mut entries = self.get_directory_content(parent).unwrap();
        entries.remove(name);
        self.write_directory_content(parent, entries);

        reply.ok();
    }

    fn rmdir(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        debug!("rmdir() called with {:?} {:?}", parent, name);
        let mut attrs = match self.lookup_name(parent, name) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };

        let mut parent_attrs = match self.get_inode(parent) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        // Directories always have a self and parent link
        if self.get_directory_content(attrs.inode).unwrap().len() > 2 {
            reply.error(libc::ENOTEMPTY);
            return;
        }
        if !check_access(
            parent_attrs.uid,
            parent_attrs.gid,
            parent_attrs.mode,
            req.uid(),
            req.gid(),
            libc::W_OK as u32,
        ) {
            reply.error(libc::EACCES);
            return;
        }

        // "Sticky bit" handling
        if parent_attrs.mode & libc::S_ISVTX as u16 != 0
            && req.uid() != 0
            && req.uid() != parent_attrs.uid
            && req.uid() != attrs.uid
        {
            reply.error(libc::EACCES);
            return;
        }

        parent_attrs.last_metadata_changed = SystemTime::now();
        parent_attrs.last_modified = SystemTime::now();
        self.write_inode(&parent_attrs);

        attrs.hardlinks = 0;
        attrs.last_metadata_changed = SystemTime::now();
        self.write_inode(&attrs);
        self.gc_inode(&attrs);

        let mut entries = self.get_directory_content(parent).unwrap();
        entries.remove(name);
        self.write_directory_content(parent, entries);

        reply.ok();
    }

    fn symlink(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        link: &Path,
        reply: ReplyEntry,
    ) {
        debug!("symlink() called with {:?} {:?} {:?}", parent, name, link);
        let link = if let Some(value) = link.to_str() {
            value
        } else {
            error!("Link is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };

        let inode = self.allocate_next_inode();
        let attrs = InodeAttributes {
            inode,
            open_file_handles: 0,
            size: link.as_bytes().len() as u64,
            last_accessed: SystemTime::now(),
            last_modified: SystemTime::now(),
            last_metadata_changed: SystemTime::now(),
            kind: FileKind::Symlink,
            mode: 0o777,
            hardlinks: 1,
            uid: req.uid(),
            gid: req.gid(),
            xattrs: Default::default(),
        };

        if let Err(error_code) = self.insert_link(req, parent, name, inode, FileKind::Symlink) {
            reply.error(error_code);
            return;
        }
        self.write_inode(&attrs);

        let path = self.content_path(inode);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(link.as_bytes()).unwrap();

        reply.entry(&Duration::new(0, 0), &attrs.into(), 0);
    }

    fn rename(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        reply: ReplyEmpty,
    ) {
        let name_str = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        let new_name_str = if let Some(value) = new_name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };

        let mut inode_attrs = match self.lookup_name(parent, name) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        let mut parent_attrs = match self.get_inode(parent) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        if !check_access(
            parent_attrs.uid,
            parent_attrs.gid,
            parent_attrs.mode,
            req.uid(),
            req.gid(),
            libc::W_OK as u32,
        ) {
            reply.error(libc::EACCES);
            return;
        }

        // "Sticky bit" handling
        if parent_attrs.mode & libc::S_ISVTX as u16 != 0
            && req.uid() != 0
            && req.uid() != parent_attrs.uid
            && req.uid() != inode_attrs.uid
        {
            reply.error(libc::EACCES);
            return;
        }

        let mut new_parent_attrs = match self.get_inode(new_parent) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        if !check_access(
            new_parent_attrs.uid,
            new_parent_attrs.gid,
            new_parent_attrs.mode,
            req.uid(),
            req.gid(),
            libc::W_OK as u32,
        ) {
            reply.error(libc::EACCES);
            return;
        }

        // "Sticky bit" handling in new_parent
        if new_parent_attrs.mode & libc::S_ISVTX as u16 != 0 {
            if let Ok(existing_attrs) = self.lookup_name(new_parent, new_name) {
                if req.uid() != 0
                    && req.uid() != new_parent_attrs.uid
                    && req.uid() != existing_attrs.uid
                {
                    reply.error(libc::EACCES);
                    return;
                }
            }
        }

        // Only overwrite an existing directory if it's empty
        if let Ok(new_name_attrs) = self.lookup_name(new_parent, new_name) {
            if new_name_attrs.kind == FileKind::Directory
                && self
                    .get_directory_content(new_name_attrs.inode)
                    .unwrap()
                    .len()
                    > 2
            {
                reply.error(libc::ENOTEMPTY);
                return;
            }
        }

        // Only move an existing directory to a new parent, if we have write access to it,
        // because that will change the ".." link in it
        if inode_attrs.kind == FileKind::Directory
            && parent != new_parent
            && !check_access(
                inode_attrs.uid,
                inode_attrs.gid,
                inode_attrs.mode,
                req.uid(),
                req.gid(),
                libc::W_OK as u32,
            )
        {
            reply.error(libc::EACCES);
            return;
        }

        // If target already exists decrement its hardlink count
        if let Ok(mut existing_inode_attrs) = self.lookup_name(new_parent, new_name) {
            let mut entries = self.get_directory_content(new_parent).unwrap();
            entries.remove(new_name_str);
            self.write_directory_content(new_parent, entries);

            if existing_inode_attrs.kind == FileKind::Directory {
                existing_inode_attrs.hardlinks = 0;
            } else {
                existing_inode_attrs.hardlinks -= 1;
            }
            existing_inode_attrs.last_metadata_changed = SystemTime::now();
            self.write_inode(&existing_inode_attrs);
            self.gc_inode(&existing_inode_attrs);
        }

        let mut entries = self.get_directory_content(parent).unwrap();
        entries.remove(name_str);
        self.write_directory_content(parent, entries);

        let mut entries = self.get_directory_content(new_parent).unwrap();
        entries.insert(
            new_name_str.to_string(),
            (inode_attrs.inode, inode_attrs.kind),
        );
        self.write_directory_content(new_parent, entries);

        parent_attrs.last_metadata_changed = SystemTime::now();
        parent_attrs.last_modified = SystemTime::now();
        self.write_inode(&parent_attrs);
        new_parent_attrs.last_metadata_changed = SystemTime::now();
        new_parent_attrs.last_modified = SystemTime::now();
        self.write_inode(&new_parent_attrs);
        inode_attrs.last_metadata_changed = SystemTime::now();
        self.write_inode(&inode_attrs);

        if inode_attrs.kind == FileKind::Directory {
            let mut entries = self.get_directory_content(inode_attrs.inode).unwrap();
            entries.insert("..".to_string(), (new_parent, FileKind::Directory));
            self.write_directory_content(inode_attrs.inode, entries);
        }

        reply.ok();
    }

    fn link(
        &mut self,
        req: &Request,
        inode: u64,
        new_parent: u64,
        new_name: &OsStr,
        reply: ReplyEntry,
    ) {
        debug!(
            "link() called for {}, {}, {:?}",
            inode, new_parent, new_name
        );
        let mut attrs = match self.get_inode(inode) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };
        if let Err(error_code) = self.insert_link(&req, new_parent, new_name, inode, attrs.kind) {
            reply.error(error_code);
        } else {
            attrs.hardlinks += 1;
            attrs.last_metadata_changed = SystemTime::now();
            self.write_inode(&attrs);
            reply.entry(&Duration::new(0, 0), &attrs.into(), 0);
        }
    }

    fn open(&mut self, req: &Request, inode: u64, flags: u32, reply: ReplyOpen) {
        debug!("open() called for {:?}", inode);
        let (access_mask, read, write) = match flags as i32 & libc::O_ACCMODE {
            libc::O_RDONLY => {
                // Behavior is undefined, but most filesystems return EACCES
                if flags as i32 & libc::O_TRUNC != 0 {
                    reply.error(libc::EACCES);
                    return;
                }
                if flags as i32 & FMODE_EXEC != 0 {
                    // Open is from internal exec syscall
                    (libc::X_OK, true, false)
                } else {
                    (libc::R_OK, true, false)
                }
            }
            libc::O_WRONLY => (libc::W_OK, false, true),
            libc::O_RDWR => (libc::R_OK | libc::W_OK, true, true),
            // Exactly one access mode flag must be specified
            _ => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        match self.get_inode(inode) {
            Ok(attr) => {
                if check_access(
                    attr.uid,
                    attr.gid,
                    attr.mode,
                    req.uid(),
                    req.gid(),
                    access_mask as u32,
                ) {
                    reply.opened(self.allocate_next_file_handle(read, write), 0);
                    return;
                } else {
                    reply.error(libc::EACCES);
                    return;
                }
            }
            Err(error_code) => reply.error(error_code),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        inode: u64,
        fh: u64,
        offset: i64,
        size: u32,
        reply: ReplyData,
    ) {
        debug!("read() called on {:?}", inode);
        assert!(offset >= 0);
        if !self.check_file_handle_read(fh) {
            reply.error(libc::EACCES);
            return;
        }

        let path = self.content_path(inode);
        if let Ok(file) = File::open(&path) {
            let file_size = file.metadata().unwrap().len();
            // Could underflow if file length is less than local_start
            let read_size = min(size, file_size.saturating_sub(offset as u64) as u32);

            let mut buffer = vec![0; read_size as usize];
            file.read_exact_at(&mut buffer, offset as u64).unwrap();
            reply.data(&buffer);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        inode: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _flags: u32,
        reply: ReplyWrite,
    ) {
        debug!("write() called with {:?}", inode);
        assert!(offset >= 0);
        if !self.check_file_handle_write(fh) {
            reply.error(libc::EACCES);
            return;
        }

        let path = self.content_path(inode);
        if let Ok(mut file) = OpenOptions::new().write(true).open(&path) {
            file.seek(SeekFrom::Start(offset as u64)).unwrap();
            file.write_all(data).unwrap();

            let mut attrs = self.get_inode(inode).unwrap();
            attrs.last_metadata_changed = SystemTime::now();
            attrs.last_modified = SystemTime::now();
            if data.len() + offset as usize > attrs.size as usize {
                attrs.size = (data.len() + offset as usize) as u64;
            }
            self.write_inode(&attrs);

            reply.written(data.len() as u32);
        } else {
            reply.error(libc::EBADF);
        }
    }

    fn opendir(&mut self, req: &Request, inode: u64, flags: u32, reply: ReplyOpen) {
        debug!("opendir() called on {:?}", inode);
        let (access_mask, read, write) = match flags as i32 & libc::O_ACCMODE {
            libc::O_RDONLY => {
                // Behavior is undefined, but most filesystems return EACCES
                if flags as i32 & libc::O_TRUNC != 0 {
                    reply.error(libc::EACCES);
                    return;
                }
                (libc::R_OK, true, false)
            }
            libc::O_WRONLY => (libc::W_OK, false, true),
            libc::O_RDWR => (libc::R_OK | libc::W_OK, true, true),
            // Exactly one access mode flag must be specified
            _ => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        match self.get_inode(inode) {
            Ok(attr) => {
                if check_access(
                    attr.uid,
                    attr.gid,
                    attr.mode,
                    req.uid(),
                    req.gid(),
                    access_mask as u32,
                ) {
                    reply.opened(self.allocate_next_file_handle(read, write), 0);
                    return;
                } else {
                    reply.error(libc::EACCES);
                    return;
                }
            }
            Err(error_code) => reply.error(error_code),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        inode: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!("readdir() called with {:?}", inode);
        assert!(offset >= 0);
        let entries = match self.get_directory_content(inode) {
            Ok(entries) => entries,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        for (index, entry) in entries.iter().skip(offset as usize).enumerate() {
            let (name, (inode, file_type)) = entry;

            let buffer_full: bool =
                reply.add(*inode, offset + index as i64 + 1, (*file_type).into(), name);

            if buffer_full {
                break;
            }
        }

        reply.ok();
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        warn!("statfs() implementation is a stub");
        // TODO: real implementation of this
        reply.statfs(
            10,
            10,
            10,
            1,
            10,
            BLOCK_SIZE as u32,
            MAX_NAME_LENGTH,
            BLOCK_SIZE as u32,
        );
    }

    fn access(&mut self, req: &Request, inode: u64, mask: u32, reply: ReplyEmpty) {
        debug!("access() called with {:?} {:?}", inode, mask);
        match self.get_inode(inode) {
            Ok(attr) => {
                if check_access(attr.uid, attr.gid, attr.mode, req.uid(), req.gid(), mask) {
                    reply.ok();
                } else {
                    reply.error(libc::EACCES);
                }
            }
            Err(error_code) => reply.error(error_code),
        }
    }

    fn create(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        flags: u32,
        reply: ReplyCreate,
    ) {
        debug!("create() called with {:?} {:?}", parent, name);
        if self.lookup_name(parent, name).is_ok() {
            reply.error(libc::EEXIST);
            return;
        }

        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        let (read, write) = match flags as i32 & libc::O_ACCMODE {
            libc::O_RDONLY => (true, false),
            libc::O_WRONLY => (false, true),
            libc::O_RDWR => (true, true),
            // Exactly one access mode flag must be specified
            _ => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let mut parent_attrs = match self.get_inode(parent) {
            Ok(attrs) => attrs,
            Err(error_code) => {
                reply.error(error_code);
                return;
            }
        };

        if !check_access(
            parent_attrs.uid,
            parent_attrs.gid,
            parent_attrs.mode,
            req.uid(),
            req.gid(),
            libc::W_OK as u32,
        ) {
            reply.error(libc::EACCES);
            return;
        }
        parent_attrs.last_modified = SystemTime::now();
        parent_attrs.last_metadata_changed = SystemTime::now();
        self.write_inode(&parent_attrs);

        let inode = self.allocate_next_inode();
        let attrs = InodeAttributes {
            inode,
            open_file_handles: 0,
            size: 0,
            last_accessed: SystemTime::now(),
            last_modified: SystemTime::now(),
            last_metadata_changed: SystemTime::now(),
            kind: as_file_kind(mode),
            // TODO: suid/sgid not supported
            mode: (mode & !(libc::S_ISUID | libc::S_ISGID) as u32) as u16,
            hardlinks: 1,
            uid: req.uid(),
            gid: req.gid(),
            xattrs: Default::default(),
        };
        self.write_inode(&attrs);
        File::create(self.content_path(inode)).unwrap();

        if as_file_kind(mode) == FileKind::Directory {
            let mut entries = BTreeMap::new();
            entries.insert(".".to_string(), (inode, FileKind::Directory));
            entries.insert("..".to_string(), (parent, FileKind::Directory));
            self.write_directory_content(inode, entries);
        }

        let mut entries = self.get_directory_content(parent).unwrap();
        entries.insert(name.to_string(), (inode, attrs.kind));
        self.write_directory_content(parent, entries);

        // TODO: implement flags
        reply.created(
            &Duration::new(0, 0),
            &attrs.into(),
            0,
            self.allocate_next_file_handle(read, write),
            0,
        );
    }
}

pub fn check_access(
    file_uid: u32,
    file_gid: u32,
    file_mode: u16,
    uid: u32,
    gid: u32,
    mut access_mask: u32,
) -> bool {
    // F_OK tests for existence of file
    if access_mask == libc::F_OK as u32 {
        return true;
    }
    let file_mode = u32::from(file_mode);

    // root is allowed to read & write anything
    if uid == 0 {
        // root only allowed to exec if one of the X bits is set
        access_mask &= libc::X_OK as u32;
        access_mask -= access_mask & (file_mode >> 6);
        access_mask -= access_mask & (file_mode >> 3);
        access_mask -= access_mask & file_mode;
        return access_mask == 0;
    }

    if uid == file_uid {
        access_mask -= access_mask & (file_mode >> 6);
    } else if gid == file_gid {
        access_mask -= access_mask & (file_mode >> 3);
    } else {
        access_mask -= access_mask & file_mode;
    }

    return access_mask == 0;
}

fn as_file_kind(mut mode: u32) -> FileKind {
    mode &= libc::S_IFMT as u32;

    if mode == libc::S_IFREG as u32 {
        return FileKind::File;
    } else if mode == libc::S_IFLNK as u32 {
        return FileKind::Symlink;
    } else if mode == libc::S_IFDIR as u32 {
        return FileKind::Directory;
    } else {
        unimplemented!("{}", mode);
    }
}

fn get_groups(pid: u32) -> Vec<u32> {
    let path = format!("/proc/{}/task/{}/status", pid, pid);
    let file = File::open(path).unwrap();
    for line in BufReader::new(file).lines() {
        let line = line.unwrap();
        if line.starts_with("Groups:") {
            return line["Groups: ".len()..]
                .split(' ')
                .filter(|x| !x.trim().is_empty())
                .map(|x| x.parse::<u32>().unwrap())
                .collect();
        }
    }

    vec![]
}

fn fuse_allow_other_enabled() -> io::Result<bool> {
    let file = File::open("/etc/fuse.conf")?;
    for line in BufReader::new(file).lines() {
        if line?.trim_start().starts_with("user_allow_other") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn main() {
    let matches = App::new("Fuser")
        .version(crate_version!())
        .author("Christopher Berner")
        .arg(
            Arg::with_name("data-dir")
                .long("data-dir")
                .value_name("DIR")
                .default_value("/tmp/fuser")
                .help("Set local directory used to store data")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("mount-point")
                .long("mount-point")
                .value_name("MOUNT_POINT")
                .default_value("")
                .help("Act as a client, and mount FUSE at given path")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("direct-io")
                .long("direct-io")
                .requires("mount-point")
                .help("Mount FUSE with direct IO"),
        )
        .arg(
            Arg::with_name("fsck")
                .long("fsck")
                .help("Run a filesystem check"),
        )
        .arg(
            Arg::with_name("v")
                .short("v")
                .multiple(true)
                .help("Sets the level of verbosity"),
        )
        .get_matches();

    let verbosity: u64 = matches.occurrences_of("v");
    let log_level = match verbosity {
        0 => LevelFilter::Error,
        1 => LevelFilter::Warn,
        2 => LevelFilter::Info,
        3 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };
    env_logger::builder()
        .format_timestamp_nanos()
        .filter_level(log_level)
        .init();

    let direct_io: bool = matches.is_present("direct-io");
    let mut options = vec![
        MountOption::FSName("fuser".to_string()),
        MountOption::AutoUnmount,
    ];
    if direct_io {
        println!("Using Direct IO");
        options.push(MountOption::DirectIO);
    }
    if let Ok(enabled) = fuse_allow_other_enabled() {
        if enabled {
            options.push(MountOption::AllowOther);
        }
    } else {
        eprintln!("Unable to read /etc/fuse.conf");
    }

    let data_dir: String = matches.value_of("data-dir").unwrap_or_default().to_string();

    let mountpoint: String = matches
        .value_of("mount-point")
        .unwrap_or_default()
        .to_string();

    fuser::mount2(SimpleFS::new(data_dir), mountpoint, &options).unwrap();
}
