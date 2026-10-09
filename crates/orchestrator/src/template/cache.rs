use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use super::Template;
use super::metadata::TemplateMetadata;
use crate::storage::backend::Storage;
use crate::storage::build::Build;

const TEMPLATE_EXPIRATION: Duration = Duration::from_secs(25 * 60 * 60);
const UNPINNED_GRACE: Duration = Duration::from_secs(60);
const EVICTION_TICK: Duration = Duration::from_secs(1);

pub struct TemplateCache {
    storage: Storage,
    dir: PathBuf,
    entries: Mutex<HashMap<Uuid, Entry>>,
    retired: Mutex<Vec<Arc<Template>>>,
}

struct Entry {
    template: Arc<Template>,
    expires: Instant,
    ttl: Duration,
}

fn expiration(max_sandbox_length_hours: i64) -> Duration {
    match max_sandbox_length_hours {
        hours if hours > 0 => TEMPLATE_EXPIRATION.max(Duration::from_secs((hours as u64 + 1) * 3600)),
        _ => TEMPLATE_EXPIRATION,
    }
}

impl TemplateCache {
    pub fn new(storage: Storage, dir: PathBuf) -> Self {
        Self {
            storage,
            dir,
            entries: Mutex::default(),
            retired: Mutex::default(),
        }
    }

    pub fn get(&self, build_id: Uuid) -> Arc<Template> {
        self.get_for(build_id, 0)
    }

    pub fn lookup(&self, build_id: Uuid) -> Option<Arc<Template>> {
        self.entries.lock().get(&build_id).map(|entry| entry.template.clone())
    }

    pub fn get_for(&self, build_id: Uuid, max_sandbox_length_hours: i64) -> Arc<Template> {
        let ttl = expiration(max_sandbox_length_hours);
        let mut entries = self.entries.lock();
        let entry = entries.entry(build_id).or_insert_with(|| Entry {
            template: self.fetched(build_id),
            expires: Instant::now(),
            ttl,
        });
        entry.ttl = entry.ttl.max(ttl);
        entry.expires = Instant::now() + entry.ttl;
        entry.template.clone()
    }

    pub fn instance_dir(&self, build_id: Uuid) -> PathBuf {
        self.dir
            .join(build_id.to_string())
            .join("cache")
            .join(Uuid::new_v4().to_string())
    }

    pub fn add_snapshot(
        &self,
        build: Build,
        snapfile: PathBuf,
        metadata: TemplateMetadata,
        dir: PathBuf,
    ) -> Arc<Template> {
        let template = Template::cached(self.storage.clone(), build, snapfile, metadata, dir);
        self.admit(Arc::new(template))
    }

    pub fn add_filesystem_layer(&self, build: Build, metadata: TemplateMetadata, dir: PathBuf) -> Arc<Template> {
        let template = Template::filesystem_layer(self.storage.clone(), build, metadata, dir);
        self.admit(Arc::new(template))
    }

    fn admit(&self, template: Arc<Template>) -> Arc<Template> {
        let entry = Entry {
            template: template.clone(),
            expires: Instant::now() + TEMPLATE_EXPIRATION,
            ttl: TEMPLATE_EXPIRATION,
        };
        if let Some(replaced) = self.entries.lock().insert(template.build_id(), entry) {
            self.retire(replaced.template);
        }
        template
    }

    fn retire(&self, template: Arc<Template>) {
        match Arc::strong_count(&template) {
            1 => remove_dir(&template),
            _ => self.retired.lock().push(template),
        }
    }

    fn remove_released_retired(&self) {
        let mut retired = self.retired.lock();
        let (pinned, released): (Vec<_>, Vec<_>) = std::mem::take(&mut *retired)
            .into_iter()
            .partition(|template| Arc::strong_count(template) > 1);
        *retired = pinned;
        drop(retired);
        released.iter().for_each(|template| remove_dir(template));
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    pub async fn run_eviction(self: Arc<Self>) {
        let mut ticks = tokio::time::interval(EVICTION_TICK);
        loop {
            ticks.tick().await;
            self.evict_expired();
        }
    }

    pub fn evict_expired(&self) {
        self.remove_released_retired();
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries
            .values_mut()
            .filter(|entry| entry.expires <= now && Arc::strong_count(&entry.template) > 1)
            .for_each(|entry| entry.expires = now + UNPINNED_GRACE);
        let expired: Vec<Uuid> = entries
            .iter()
            .filter(|(_, entry)| entry.expires <= now)
            .map(|(build_id, _)| *build_id)
            .collect();
        expired
            .iter()
            .filter_map(|build_id| entries.remove(build_id))
            .for_each(|entry| remove_dir(&entry.template));
    }

    fn fetched(&self, build_id: Uuid) -> Arc<Template> {
        let template = Arc::new(Template::new(
            self.storage.clone(),
            build_id,
            self.instance_dir(build_id),
        ));
        let fetching = template.clone();
        tokio::spawn(async move { fetching.fetch().await });
        template
    }
}

fn remove_dir(template: &Template) {
    if let Err(e) = std::fs::remove_dir_all(template.dir())
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(build = %template.build_id(), error = %e, "could not remove an evicted template");
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::storage::header::{Header, Metadata};
    use crate::storage::paths::BuildPaths;
    use crate::template::TemplateError;
    use crate::template::metadata::TemplateVersions;

    struct Fixture {
        cache: TemplateCache,
        storage: Storage,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        Fixture {
            cache: TemplateCache::new(storage.clone(), dir.path().join("templates")),
            storage,
            _dir: dir,
        }
    }

    async fn upload(storage: &Storage) -> Build {
        let build_id = Uuid::new_v4();
        let build = Build {
            build_id,
            memfile: Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap(),
            rootfs: Header::new(Metadata::template(build_id, 4096, 16384), Vec::new()).unwrap(),
        };
        build.store(storage).await.unwrap();
        storage
            .put(&BuildPaths::new(build_id).snapfile(), Bytes::from_static(b"snap"))
            .await
            .unwrap();
        build
    }

    #[tokio::test]
    async fn lookups_share_one_template_and_one_download() {
        let fixture = fixture();
        let build = upload(&fixture.storage).await;

        let first = fixture.cache.get(build.build_id);
        let second = fixture.cache.get(build.build_id);

        assert!(Arc::ptr_eq(&first, &second));
        let snapfile = first.snapfile().await.unwrap();
        assert_eq!(snapfile, second.snapfile().await.unwrap());
        assert_eq!(std::fs::read(&snapfile).unwrap(), b"snap");
        assert_eq!(first.build().await.unwrap(), build);
        assert_eq!(first.metadata().await.unwrap(), TemplateMetadata::v1());
    }

    #[tokio::test]
    async fn missing_objects_are_reported_as_not_found_and_stay_cached() {
        let fixture = fixture();
        let build_id = Uuid::new_v4();

        let template = fixture.cache.get(build_id);

        let missing = TemplateError::NotFound(BuildPaths::new(build_id).snapfile());
        assert_eq!(template.snapfile().await, Err(missing.clone()));
        assert!(matches!(template.build().await, Err(TemplateError::NotFound(_))));
        assert_eq!(fixture.cache.get(build_id).snapfile().await, Err(missing));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unused_template_expires_after_25_hours_and_lookups_slide_it() {
        let fixture = fixture();
        let build = upload(&fixture.storage).await;
        let dir = {
            let template = fixture.cache.get(build.build_id);
            template.snapfile().await.unwrap();
            template.dir().to_owned()
        };

        tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
        let looked_up = fixture.cache.get(build.build_id);
        looked_up.fetch().await;
        drop(looked_up);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(2 * 60 * 60)).await;
        fixture.cache.evict_expired();
        assert_eq!(fixture.cache.len(), 1);

        tokio::time::advance(TEMPLATE_EXPIRATION).await;
        fixture.cache.evict_expired();
        assert!(fixture.cache.is_empty());
        assert!(!dir.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn a_pinned_template_outlives_its_ttl_until_a_minute_after_release() {
        let fixture = fixture();
        let build = upload(&fixture.storage).await;
        let pinned = fixture.cache.get(build.build_id);
        pinned.fetch().await;

        tokio::time::advance(TEMPLATE_EXPIRATION).await;
        fixture.cache.evict_expired();
        assert_eq!(fixture.cache.len(), 1);

        drop(pinned);
        tokio::time::advance(Duration::from_secs(30)).await;
        fixture.cache.evict_expired();
        assert_eq!(fixture.cache.len(), 1);
        tokio::time::advance(Duration::from_secs(31)).await;
        fixture.cache.evict_expired();
        assert!(fixture.cache.is_empty());
    }

    #[tokio::test]
    async fn a_snapshot_added_locally_is_served_without_storage() {
        let fixture = fixture();
        let build_id = Uuid::new_v4();
        let build = Build {
            build_id,
            memfile: Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap(),
            rootfs: Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap(),
        };
        let dir = fixture.cache.instance_dir(build_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("snapfile"), b"local").unwrap();

        let metadata = TemplateMetadata::v1().same_version(TemplateVersions {
            build_id: build_id.to_string(),
            ..TemplateVersions::default()
        });
        fixture
            .cache
            .add_snapshot(build.clone(), dir.join("snapfile"), metadata.clone(), dir.clone());
        let template = fixture.cache.get(build_id);

        assert_eq!(template.build().await.unwrap(), build);
        assert_eq!(template.metadata().await.unwrap(), metadata);
        assert_eq!(template.snapfile().await.unwrap(), dir.join("snapfile"));
    }

    #[tokio::test]
    async fn a_replaced_pinned_template_loses_its_dir_after_the_last_release() {
        let fixture = fixture();
        let build_id = Uuid::new_v4();
        let build = Build {
            build_id,
            memfile: Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap(),
            rootfs: Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap(),
        };
        let first_dir = fixture.cache.instance_dir(build_id);
        let second_dir = fixture.cache.instance_dir(build_id);
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        let first = fixture.cache.add_snapshot(
            build.clone(),
            first_dir.join("snapfile"),
            TemplateMetadata::v1(),
            first_dir.clone(),
        );

        fixture.cache.add_snapshot(
            build,
            second_dir.join("snapfile"),
            TemplateMetadata::v1(),
            second_dir.clone(),
        );
        fixture.cache.evict_expired();
        assert!(first_dir.exists());

        drop(first);
        fixture.cache.evict_expired();
        assert!(!first_dir.exists());
        assert!(second_dir.exists());
    }

    fn filesystem_only_metadata() -> TemplateMetadata {
        TemplateMetadata {
            version: 2,
            filesystem_only: true,
            ..TemplateMetadata::default()
        }
    }

    #[tokio::test]
    async fn a_filesystem_only_layer_loads_from_storage_without_memory_objects() {
        let fixture = fixture();
        let build_id = Uuid::new_v4();
        let rootfs = Header::new(Metadata::template(build_id, 4096, 16384), Vec::new()).unwrap();
        let paths = BuildPaths::new(build_id);
        fixture
            .storage
            .store_header(&paths.rootfs_header(), &rootfs)
            .await
            .unwrap();
        fixture
            .storage
            .put(&paths.metadata(), Bytes::from(filesystem_only_metadata().to_json()))
            .await
            .unwrap();

        let template = fixture.cache.get(build_id);

        assert_eq!(
            template.build().await.unwrap(),
            Build::filesystem_only(build_id, rootfs)
        );
        assert_eq!(template.metadata().await.unwrap(), filesystem_only_metadata());
    }

    #[tokio::test]
    async fn a_filesystem_layer_added_locally_has_no_snapfile() {
        let fixture = fixture();
        let build_id = Uuid::new_v4();
        let rootfs = Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap();
        let build = Build::filesystem_only(build_id, rootfs);
        let dir = fixture.cache.instance_dir(build_id);

        fixture
            .cache
            .add_filesystem_layer(build.clone(), filesystem_only_metadata(), dir);
        let template = fixture.cache.get(build_id);

        assert_eq!(template.build().await.unwrap(), build);
        assert_eq!(template.metadata().await.unwrap(), filesystem_only_metadata());
        assert_eq!(
            template.snapfile().await,
            Err(TemplateError::NotFound(BuildPaths::new(build_id).snapfile()))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_sandbox_extends_the_template_ttl() {
        let fixture = fixture();
        let build = upload(&fixture.storage).await;
        fixture.cache.get_for(build.build_id, 48).fetch().await;

        tokio::time::advance(Duration::from_secs(30 * 3600)).await;
        fixture.cache.evict_expired();
        assert_eq!(fixture.cache.len(), 1);

        tokio::time::advance(Duration::from_secs(20 * 3600)).await;
        fixture.cache.evict_expired();
        assert!(fixture.cache.is_empty());
    }
}
