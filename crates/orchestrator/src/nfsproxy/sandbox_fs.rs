use std::path::PathBuf;
use std::sync::OnceLock;

use async_trait::async_trait;
use nfsserve::nfs::{fattr3, fileid3, filename3, nfspath3, nfsstat3, sattr3, set_gid3, set_mode3, set_size3, set_uid3};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use super::attributes;
use super::handles::{Handles, Node, ROOT};
use crate::volumes::chrooted::error::PathError;
use crate::volumes::chrooted::{Chrooted, RenameError};

const FILE_PERMISSIONS: u32 = 0o644;
const DIRECTORY_PERMISSIONS: u32 = 0o755;

pub struct VolumeRoot {
    name: String,
    source: PathBuf,
    root: OnceLock<Chrooted>,
}

impl VolumeRoot {
    pub fn new(name: String, source: PathBuf) -> Self {
        Self {
            name,
            source,
            root: OnceLock::new(),
        }
    }

    fn open(&self) -> Result<&Chrooted, nfsstat3> {
        if let Some(root) = self.root.get() {
            return usable(root).then_some(root).ok_or(nfsstat3::NFS3ERR_STALE);
        }
        let opened = Chrooted::open(&self.source).map_err(|error| {
            tracing::warn!(path = %self.source.display(), %error, "failed to open the volume root");
            nfsstat3::NFS3ERR_ACCES
        })?;
        Ok(self.root.get_or_init(|| opened))
    }
}

fn usable(root: &Chrooted) -> bool {
    root.lstat_raw("/").is_ok_and(|stat| stat.st_nlink > 0)
}

pub struct SandboxVolumes {
    volumes: Vec<VolumeRoot>,
    handles: Handles,
}

enum Target {
    Root,
    Volume(Node),
}

impl SandboxVolumes {
    pub fn new(volumes: Vec<VolumeRoot>) -> Self {
        Self {
            volumes,
            handles: Handles::default(),
        }
    }

    fn target(&self, id: fileid3) -> Result<Target, nfsstat3> {
        match id {
            ROOT => Ok(Target::Root),
            id => self.handles.node(id).map(Target::Volume).ok_or(nfsstat3::NFS3ERR_STALE),
        }
    }

    fn node(&self, id: fileid3) -> Result<Node, nfsstat3> {
        match self.target(id)? {
            Target::Root => Err(nfsstat3::NFS3ERR_ACCES),
            Target::Volume(node) => Ok(node),
        }
    }

    fn chroot(&self, node: &Node) -> Result<&Chrooted, nfsstat3> {
        self.volumes
            .get(node.volume)
            .and_then(|volume| volume.root.get())
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn path(node: &Node) -> String {
        format!("/{}", node.path)
    }

    fn attributes(&self, node: &Node) -> Result<fattr3, nfsstat3> {
        self.chroot(node)?
            .lstat_raw(&Self::path(node))
            .map(|stat| attributes::of_stat(&stat, self.handles.id_of(node.clone())))
            .map_err(|error| status(&error))
    }

    fn child(&self, dirid: fileid3, name: &filename3) -> Result<Node, nfsstat3> {
        let name = std::str::from_utf8(name).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        match name {
            "" | "." | ".." => Err(nfsstat3::NFS3ERR_INVAL),
            name if name.contains('/') => Err(nfsstat3::NFS3ERR_INVAL),
            name => self.node(dirid).map(|dir| dir.child(name)),
        }
    }

    fn created(&self, node: Node, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        self.apply(&node, attr)?;
        self.attributes(&node).map(|attr| (attr.fileid, attr))
    }

    fn apply(&self, node: &Node, attr: sattr3) -> Result<(), nfsstat3> {
        let chroot = self.chroot(node)?;
        let path = Self::path(node);
        if let set_mode3::mode(mode) = attr.mode {
            chroot.chmod(&path, mode).map_err(|error| status(&error))?;
        }
        let uid = match attr.uid {
            set_uid3::uid(uid) => Some(uid),
            set_uid3::Void => None,
        };
        let gid = match attr.gid {
            set_gid3::gid(gid) => Some(gid),
            set_gid3::Void => None,
        };
        if uid.is_some() || gid.is_some() {
            chroot.lchown(&path, uid, gid).map_err(|error| status(&error))?;
        }
        if let set_size3::size(size) = attr.size {
            chroot.truncate(&path, size).map_err(|error| status(&error))?;
        }
        let accessed = attributes::access_time(attr.atime);
        let modified = attributes::modification_time(attr.mtime);
        if accessed.is_some() || modified.is_some() {
            chroot
                .chtimes(&path, accessed, modified)
                .map_err(|error| status(&error))?;
        }
        Ok(())
    }

    fn root_entries(&self) -> Vec<DirEntry> {
        (0..self.volumes.len())
            .filter(|volume| self.volumes[*volume].root.get().is_some())
            .filter_map(|volume| {
                let node = Node::volume_root(volume);
                self.attributes(&node).ok().map(|attr| DirEntry {
                    fileid: attr.fileid,
                    name: self.volumes[volume].name.as_bytes().into(),
                    attr,
                })
            })
            .collect()
    }

    fn volume_entries(&self, dir: &Node) -> Result<Vec<DirEntry>, nfsstat3> {
        let listed = self
            .chroot(dir)?
            .read_dir(&Self::path(dir))
            .map_err(|error| status(&error))?;
        Ok(listed
            .into_iter()
            .filter_map(|info| {
                let node = dir.child(&info.name);
                self.attributes(&node).ok().map(|attr| DirEntry {
                    fileid: attr.fileid,
                    name: info.name.as_bytes().into(),
                    attr,
                })
            })
            .collect())
    }
}

fn status(error: &PathError) -> nfsstat3 {
    attributes::status(error.errno)
}

fn rename_status(error: &RenameError) -> nfsstat3 {
    match error {
        RenameError::Path(path) => status(path),
        RenameError::Link(link) => attributes::status(link.errno),
    }
}

#[async_trait]
impl NFSFileSystem for SandboxVolumes {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        match (self.target(dirid)?, name) {
            (Target::Root, "." | "..") => Ok(ROOT),
            (Target::Root, name) => {
                let volume = self
                    .volumes
                    .iter()
                    .position(|volume| volume.name == name)
                    .ok_or(nfsstat3::NFS3ERR_NOENT)?;
                self.volumes[volume].open()?;
                Ok(self.handles.id_of(Node::volume_root(volume)))
            }
            (Target::Volume(dir), ".") => Ok(self.handles.id_of(dir)),
            (Target::Volume(dir), "..") => Ok(dir.parent().map_or(ROOT, |parent| self.handles.id_of(parent))),
            (Target::Volume(_), _) => {
                let node = self.child(dirid, filename)?;
                self.attributes(&node).map(|attr| attr.fileid)
            }
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        match self.target(id)? {
            Target::Root => Ok(attributes::of_root(ROOT)),
            Target::Volume(node) => self.attributes(&node),
        }
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let node = self.node(id)?;
        self.apply(&node, setattr)?;
        self.attributes(&node)
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        let node = self.node(id)?;
        let file = self
            .chroot(&node)?
            .open_read(&Self::path(&node))
            .map_err(|error| status(&error))?;
        let mut buffer = vec![0; count as usize];
        let mut filled = 0;
        while filled < buffer.len() {
            match file.read_at(&mut buffer[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) => return Err(status(&error)),
            }
        }
        buffer.truncate(filled);
        let size = self.attributes(&node)?.size;
        Ok((buffer, offset + filled as u64 >= size))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let node = self.node(id)?;
        self.chroot(&node)?
            .open_read_write(&Self::path(&node))
            .and_then(|file| file.write_all_at(data, offset))
            .map_err(|error| status(&error))?;
        self.attributes(&node)
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let node = self.child(dirid, filename)?;
        let permissions = match attr.mode {
            set_mode3::mode(mode) => mode,
            set_mode3::Void => FILE_PERMISSIONS,
        };
        self.chroot(&node)?
            .create_or_open(&Self::path(&node), permissions)
            .map_err(|error| status(&error))?;
        self.created(node, attr)
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let node = self.child(dirid, filename)?;
        self.chroot(&node)?
            .create_new(&Self::path(&node), FILE_PERMISSIONS)
            .map_err(|error| status(&error))?;
        self.attributes(&node).map(|attr| attr.fileid)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let node = self.child(dirid, dirname)?;
        self.chroot(&node)?
            .mkdir(&Self::path(&node), DIRECTORY_PERMISSIONS)
            .map_err(|error| status(&error))?;
        self.attributes(&node).map(|attr| (attr.fileid, attr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let node = self.child(dirid, filename)?;
        self.chroot(&node)?
            .remove(&Self::path(&node))
            .map_err(|error| status(&error))?;
        self.handles.forget(&node);
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from = self.child(from_dirid, from_filename)?;
        let to = self.child(to_dirid, to_filename)?;
        if from.volume != to.volume {
            return Err(nfsstat3::NFS3ERR_XDEV);
        }
        self.chroot(&from)?
            .rename(&Self::path(&from), &Self::path(&to))
            .map_err(|error| rename_status(&error))?;
        self.handles.moved(&from, &to);
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let entries = match self.target(dirid)? {
            Target::Root => self.root_entries(),
            Target::Volume(dir) => self.volume_entries(&dir)?,
        };
        let start = match start_after {
            0 => 0,
            after => entries
                .iter()
                .position(|entry| entry.fileid == after)
                .map(|index| index + 1)
                .ok_or(nfsstat3::NFS3ERR_BAD_COOKIE)?,
        };
        let page: Vec<DirEntry> = entries.into_iter().skip(start).collect();
        let end = page.len() <= max_entries;
        Ok(ReadDirResult {
            entries: page.into_iter().take(max_entries).collect(),
            end,
        })
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let node = self.child(dirid, linkname)?;
        let target = std::str::from_utf8(symlink).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        self.chroot(&node)?
            .symlink(target, &Self::path(&node))
            .map_err(|error| rename_status(&error))?;
        let ownership = sattr3 {
            mode: set_mode3::Void,
            size: set_size3::Void,
            ..*attr
        };
        self.created(node, ownership)
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let node = self.node(id)?;
        self.chroot(&node)?
            .readlink(&Self::path(&node))
            .map(|target| target.as_bytes().into())
            .map_err(|error| status(&error))
    }
}

#[cfg(test)]
mod tests {
    use nfsserve::nfs::{set_atime, set_mtime};

    use super::*;

    fn unset() -> sattr3 {
        sattr3 {
            mode: set_mode3::Void,
            uid: set_uid3::Void,
            gid: set_gid3::Void,
            size: set_size3::Void,
            atime: set_atime::DONT_CHANGE,
            mtime: set_mtime::DONT_CHANGE,
        }
    }

    fn volumes(dir: &tempfile::TempDir) -> SandboxVolumes {
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        SandboxVolumes::new(vec![VolumeRoot::new("data".to_owned(), dir.path().join("data"))])
    }

    async fn volume(fs: &SandboxVolumes) -> fileid3 {
        fs.path_to_id(b"/data").await.unwrap()
    }

    #[tokio::test]
    async fn a_mount_path_resolves_to_its_volume_only() {
        let dir = tempfile::tempdir().unwrap();
        let fs = volumes(&dir);

        assert!(volume(&fs).await != ROOT);
        assert!(matches!(fs.path_to_id(b"/other").await, Err(nfsstat3::NFS3ERR_NOENT)));
        assert!(matches!(fs.path_to_id(b"/data/../..").await, Ok(ROOT)));
    }

    #[tokio::test]
    async fn files_round_trip_through_the_volume() {
        let dir = tempfile::tempdir().unwrap();
        let fs = volumes(&dir);
        let root = volume(&fs).await;

        let (file, _) = fs.create(root, &b"hello.txt".as_slice().into(), unset()).await.unwrap();
        fs.write(file, 0, b"hello from volume").await.unwrap();
        let (data, eof) = fs.read(file, 0, 1024).await.unwrap();

        assert_eq!(data, b"hello from volume");
        assert!(eof);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("data/hello.txt")).unwrap(),
            "hello from volume"
        );
        assert_eq!(fs.lookup(root, &b"hello.txt".as_slice().into()).await.unwrap(), file);
    }

    #[tokio::test]
    async fn a_rename_keeps_the_handle_and_a_remove_makes_it_stale() {
        let dir = tempfile::tempdir().unwrap();
        let fs = volumes(&dir);
        let root = volume(&fs).await;
        let (subdir, _) = fs.mkdir(root, &b"a".as_slice().into()).await.unwrap();
        let (file, _) = fs.create(subdir, &b"f".as_slice().into(), unset()).await.unwrap();

        fs.rename(root, &b"a".as_slice().into(), root, &b"b".as_slice().into())
            .await
            .unwrap();
        assert!(fs.getattr(file).await.is_ok());
        assert!(dir.path().join("data/b/f").exists());

        fs.remove(subdir, &b"f".as_slice().into()).await.unwrap();
        assert!(matches!(fs.getattr(file).await, Err(nfsstat3::NFS3ERR_STALE)));
    }

    #[tokio::test]
    async fn readdir_pages_after_the_last_seen_entry() {
        let dir = tempfile::tempdir().unwrap();
        let fs = volumes(&dir);
        let root = volume(&fs).await;
        for name in ["a", "b", "c"] {
            fs.create(root, &name.as_bytes().into(), unset()).await.unwrap();
        }

        let first = fs.readdir(root, 0, 2).await.unwrap();
        let rest = fs.readdir(root, first.entries[1].fileid, 2).await.unwrap();

        let names = |result: &ReadDirResult| -> Vec<Vec<u8>> {
            result.entries.iter().map(|entry| entry.name.0.clone()).collect()
        };
        assert_eq!(names(&first), vec![b"a".to_vec(), b"b".to_vec()]);
        assert!(!first.end);
        assert_eq!(names(&rest), vec![b"c".to_vec()]);
        assert!(rest.end);
    }

    #[tokio::test]
    async fn the_root_lists_only_opened_volumes_and_refuses_writes() {
        let dir = tempfile::tempdir().unwrap();
        let fs = volumes(&dir);
        assert!(fs.readdir(ROOT, 0, 10).await.unwrap().entries.is_empty());

        volume(&fs).await;

        assert_eq!(fs.readdir(ROOT, 0, 10).await.unwrap().entries.len(), 1);
        assert!(matches!(
            fs.create(ROOT, &b"x".as_slice().into(), unset()).await,
            Err(nfsstat3::NFS3ERR_ACCES)
        ));
    }

    #[tokio::test]
    async fn a_missing_volume_directory_refuses_the_mount() {
        let dir = tempfile::tempdir().unwrap();
        let fs = SandboxVolumes::new(vec![VolumeRoot::new("gone".to_owned(), dir.path().join("gone"))]);

        assert!(matches!(fs.path_to_id(b"/gone").await, Err(nfsstat3::NFS3ERR_ACCES)));
    }
}
