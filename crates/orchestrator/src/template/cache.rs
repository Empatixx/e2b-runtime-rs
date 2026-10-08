use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use super::Template;
use crate::storage::backend::Storage;
use crate::storage::build::Build;

const TEMPLATE_EXPIRATION: Duration = Duration::from_secs(25 * 60 * 60);
const UNPINNED_GRACE: Duration = Duration::from_secs(60);
const EVICTION_TICK: Duration = Duration::from_secs(1);

pub struct TemplateCache {
    storage: Storage,
    dir: PathBuf,
    entries: Mutex<HashMap<Uuid, Entry>>,
}

struct Entry {
    template: Arc<Template>,
    expires: Instant,
}

impl TemplateCache {
    pub fn new(storage: Storage, dir: PathBuf) -> Self {
        Self {
            storage,
            dir,
            entries: Mutex::default(),
        }
    }

    pub fn get(&self, build_id: Uuid) -> Arc<Template> {
        let expires = Instant::now() + TEMPLATE_EXPIRATION;
        let mut entries = self.entries.lock();
        let entry = entries.entry(build_id).or_insert_with(|| Entry {
            template: self.fetched(build_id),
            expires,
        });
        entry.expires = expires;
        entry.template.clone()
    }

    pub fn instance_dir(&self, build_id: Uuid) -> PathBuf {
        self.dir
            .join(build_id.to_string())
            .join("cache")
            .join(Uuid::new_v4().to_string())
    }

    pub fn add_snapshot(&self, build: Build, snapfile: PathBuf, dir: PathBuf) -> Arc<Template> {
        let build_id = build.build_id;
        let template = Arc::new(Template::cached(self.storage.clone(), build, snapfile, dir));
        let entry = Entry {
            template: template.clone(),
            expires: Instant::now() + TEMPLATE_EXPIRATION,
        };
        if let Some(replaced) = self.entries.lock().insert(build_id, entry)
            && Arc::strong_count(&replaced.template) == 1
        {
            remove_dir(&replaced.template);
        }
        template
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
        drop(fixture.cache.get(build.build_id));
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

        fixture
            .cache
            .add_snapshot(build.clone(), dir.join("snapfile"), dir.clone());
        let template = fixture.cache.get(build_id);

        assert_eq!(template.build().await.unwrap(), build);
        assert_eq!(template.snapfile().await.unwrap(), dir.join("snapfile"));
    }
}
