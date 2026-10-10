use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use super::Template;
use super::lineage::{LayerKind, SnapshotLineage, UploadOutcome};
use super::metadata::TemplateMetadata;
use crate::featureflags::{FeatureFlags, SNAPSHOT_CACHE_ANCESTOR_STORAGE_FALLBACK, SNAPSHOT_CACHE_RELEASE_SUPERSEDED};
use crate::storage::backend::Storage;
use crate::storage::build::Build;
use crate::telemetry::template_cache::{
    MissReason, record_expired_held, record_hit, record_miss, record_released, record_superseded, record_suppressed,
};

const TEMPLATE_EXPIRATION: Duration = Duration::from_secs(25 * 60 * 60);
const UNPINNED_GRACE: Duration = Duration::from_secs(60);
const EVICTION_TICK: Duration = Duration::from_secs(1);

pub struct TemplateCache {
    storage: Storage,
    dir: PathBuf,
    flags: Option<FeatureFlags>,
    entries: Mutex<HashMap<Uuid, Entry>>,
    retired: Mutex<Vec<Arc<Template>>>,
    released: Mutex<HashMap<Uuid, Released>>,
}

struct Entry {
    template: Arc<Template>,
    expires: Instant,
    ttl: Duration,
}

impl Entry {
    fn unpinned(&self) -> bool {
        Arc::strong_count(&self.template) == 1
    }

    fn releasable(&self) -> bool {
        let mark = self.template.mark();
        mark.superseded && !mark.gone && mark.landed() && self.unpinned() && self.template.devices_resolved()
    }
}

struct Released {
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
            flags: None,
            entries: Mutex::default(),
            retired: Mutex::default(),
            released: Mutex::default(),
        }
    }

    pub fn with_flags(self, flags: FeatureFlags) -> Self {
        Self {
            flags: Some(flags),
            ..self
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
        match entries.contains_key(&build_id) {
            true => record_hit(),
            false => record_miss(self.miss_reason(build_id)),
        }
        let entry = entries.entry(build_id).or_insert_with(|| Entry {
            template: self.fetched(build_id),
            expires: Instant::now(),
            ttl,
        });
        entry.ttl = entry.ttl.max(ttl);
        entry.expires = Instant::now() + entry.ttl;
        entry.template.clone()
    }

    fn miss_reason(&self, build_id: Uuid) -> MissReason {
        match self.released_live(build_id) {
            true => MissReason::ReleasedLayer,
            false => MissReason::Cold,
        }
    }

    fn released_live(&self, build_id: Uuid) -> bool {
        self.released
            .lock()
            .get(&build_id)
            .is_some_and(|released| released.expires > Instant::now())
    }

    pub fn was_released(&self, build_id: Uuid) -> bool {
        let now = Instant::now();
        let mut released = self.released.lock();
        match released.get_mut(&build_id).filter(|released| released.expires > now) {
            Some(released) => {
                released.expires = now + released.ttl;
                true
            }
            None => false,
        }
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
        lineage: SnapshotLineage,
    ) -> Arc<Template> {
        let template = Template::cached(self.storage.clone(), build, snapfile, metadata, dir).with_kind(lineage.kind());
        self.admit_with(Arc::new(template), lineage)
    }

    pub fn add_filesystem_layer(
        &self,
        build: Build,
        metadata: TemplateMetadata,
        dir: PathBuf,
        lineage: SnapshotLineage,
    ) -> Arc<Template> {
        let template = Template::filesystem_layer(self.storage.clone(), build, metadata, dir).with_kind(lineage.kind());
        self.admit_with(Arc::new(template), lineage)
    }

    fn admit_with(&self, template: Arc<Template>, lineage: SnapshotLineage) -> Arc<Template> {
        record_miss(MissReason::Insert);
        let admitted = self.admit(template);
        if let Some(predecessor) = lineage
            .supersedes()
            .filter(|predecessor| *predecessor != admitted.build_id())
        {
            self.mark_superseded(predecessor);
        }
        admitted
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

    pub fn finish_upload(&self, build_id: Uuid, outcome: UploadOutcome) {
        if let Some(template) = self.lookup(build_id) {
            template.mark().finish_upload(outcome);
        }
    }

    fn mark_superseded(&self, build_id: Uuid) {
        let marked = self
            .entries
            .lock()
            .get(&build_id)
            .filter(|entry| entry.template.kind() == LayerKind::Pause)
            .is_some_and(|entry| {
                let mut mark = entry.template.mark();
                !std::mem::replace(&mut mark.superseded, true)
            });
        if marked {
            record_superseded();
            self.release_superseded();
        }
    }

    fn release_enabled(&self) -> bool {
        self.flags.as_ref().is_some_and(|flags| {
            flags.get(&SNAPSHOT_CACHE_RELEASE_SUPERSEDED, &[])
                && flags.get(&SNAPSHOT_CACHE_ANCESTOR_STORAGE_FALLBACK, &[])
        })
    }

    fn release_superseded(&self) {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        let releasable: Vec<Uuid> = entries
            .iter()
            .filter(|(_, entry)| entry.releasable())
            .map(|(build_id, _)| *build_id)
            .collect();
        if releasable.is_empty() {
            return;
        }
        if !self.release_enabled() {
            releasable
                .iter()
                .filter_map(|build_id| entries.get(build_id))
                .filter(|entry| !std::mem::replace(&mut entry.template.mark().suppressed, true))
                .for_each(|_| record_suppressed());
            return;
        }
        let released: Vec<Entry> = releasable
            .iter()
            .filter_map(|build_id| entries.remove(build_id))
            .collect();
        drop(entries);
        released.iter().for_each(|entry| self.release(entry, now));
    }

    fn release(&self, entry: &Entry, now: Instant) {
        entry.template.mark().gone = true;
        record_released();
        let build_id = entry.template.build_id();
        self.released.lock().insert(
            build_id,
            Released {
                expires: now + entry.ttl,
                ttl: entry.ttl,
            },
        );
        remove_dir(&entry.template);
        self.purge_peer_routing(build_id);
    }

    fn retire(&self, template: Arc<Template>) {
        template.mark().gone = true;
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
        self.release_superseded();
        self.forget_expired_releases();
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries
            .values_mut()
            .filter(|entry| entry.expires <= now && !entry.unpinned())
            .for_each(|entry| entry.expires = now + UNPINNED_GRACE);
        let expired: Vec<Uuid> = entries
            .iter()
            .filter(|(_, entry)| entry.expires <= now)
            .map(|(build_id, _)| *build_id)
            .collect();
        expired
            .iter()
            .filter_map(|build_id| entries.remove(build_id))
            .for_each(|entry| close_expired(&entry));
        drop(entries);
        expired.iter().for_each(|build_id| self.purge_peer_routing(*build_id));
    }

    fn forget_expired_releases(&self) {
        let now = Instant::now();
        self.released.lock().retain(|_, released| released.expires > now);
    }

    fn purge_peer_routing(&self, build_id: Uuid) {
        let superseded = self
            .retired
            .lock()
            .iter()
            .any(|template| template.build_id() == build_id);
        if !superseded && let Some(peers) = self.storage.peers() {
            peers.purge(&build_id.to_string());
        }
    }

    fn fetched(&self, build_id: Uuid) -> Arc<Template> {
        let template = Arc::new(Template::new(
            self.storage.for_template(),
            build_id,
            self.instance_dir(build_id),
        ));
        let fetching = template.clone();
        tokio::spawn(async move { fetching.fetch().await });
        template
    }
}

fn close_expired(entry: &Entry) {
    let held = {
        let mut mark = entry.template.mark();
        let held = mark.superseded && !mark.gone;
        mark.gone = true;
        held
    };
    if held {
        record_expired_held();
    }
    remove_dir(&entry.template);
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
    use crate::template::lineage::Origin;
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
        fixture.cache.add_snapshot(
            build.clone(),
            dir.join("snapfile"),
            metadata.clone(),
            dir.clone(),
            SnapshotLineage::build(),
        );
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
            SnapshotLineage::build(),
        );

        fixture.cache.add_snapshot(
            build,
            second_dir.join("snapfile"),
            TemplateMetadata::v1(),
            second_dir.clone(),
            SnapshotLineage::build(),
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
            .add_filesystem_layer(build.clone(), filesystem_only_metadata(), dir, SnapshotLineage::build());
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

    struct Layer {
        build_id: Uuid,
        dir: PathBuf,
    }

    fn cache_with(release: bool, fallback: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let flags = FeatureFlags::builder()
            .override_flag(SNAPSHOT_CACHE_RELEASE_SUPERSEDED.key, release)
            .override_flag(SNAPSHOT_CACHE_ANCESTOR_STORAGE_FALLBACK.key, fallback)
            .offline();
        Fixture {
            cache: TemplateCache::new(storage.clone(), dir.path().join("templates")).with_flags(flags),
            storage,
            _dir: dir,
        }
    }

    fn add_layer(cache: &TemplateCache, lineage: SnapshotLineage) -> (Layer, Arc<Template>) {
        let build_id = Uuid::new_v4();
        let build = Build {
            build_id,
            memfile: Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap(),
            rootfs: Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap(),
        };
        let dir = cache.instance_dir(build_id);
        std::fs::create_dir_all(&dir).unwrap();
        let template = cache.add_snapshot(
            build,
            dir.join("snapfile"),
            TemplateMetadata::v1(),
            dir.clone(),
            lineage,
        );
        (Layer { build_id, dir }, template)
    }

    fn pause_of(predecessor: Uuid) -> SnapshotLineage {
        SnapshotLineage::of_sandbox(Origin::Pause, predecessor, false)
    }

    fn released(fixture: &Fixture, layer: &Layer) -> bool {
        fixture.cache.lookup(layer.build_id).is_none() && !layer.dir.exists()
    }

    #[tokio::test]
    async fn a_superseded_landed_unheld_pause_layer_is_released() {
        for superseded in [false, true] {
            for held in [false, true] {
                for landed in [false, true] {
                    let fixture = cache_with(true, true);
                    let (layer, sandbox) =
                        add_layer(&fixture.cache, SnapshotLineage::without_predecessor(Origin::Pause));
                    let holder = held.then(|| fixture.cache.get(layer.build_id));
                    if landed {
                        fixture.cache.finish_upload(layer.build_id, UploadOutcome::Landed);
                    }
                    if superseded {
                        add_layer(&fixture.cache, pause_of(layer.build_id));
                    }
                    drop(sandbox);
                    fixture.cache.evict_expired();

                    let expected = superseded && !held && landed;
                    assert_eq!(
                        released(&fixture, &layer),
                        expected,
                        "superseded={superseded} held={held} landed={landed}"
                    );
                    drop(holder);
                }
            }
        }
    }

    #[tokio::test]
    async fn the_release_needs_both_flags_and_records_the_would_release() {
        for (release, fallback) in [(false, false), (true, false), (false, true)] {
            let fixture = cache_with(release, fallback);
            let (layer, sandbox) = add_layer(&fixture.cache, SnapshotLineage::without_predecessor(Origin::Pause));
            fixture.cache.finish_upload(layer.build_id, UploadOutcome::Landed);
            add_layer(&fixture.cache, pause_of(layer.build_id));
            drop(sandbox);

            fixture.cache.evict_expired();

            let template = fixture.cache.lookup(layer.build_id).expect("the layer stays resident");
            let mark = template.mark();
            assert!(mark.suppressed, "release={release} fallback={fallback}");
            assert!(!mark.gone);
        }
    }

    #[tokio::test]
    async fn only_a_pause_layer_is_ever_superseded() {
        for origin in [Origin::Checkpoint, Origin::Build] {
            let fixture = cache_with(true, true);
            let (layer, sandbox) = add_layer(&fixture.cache, SnapshotLineage::without_predecessor(origin));
            fixture.cache.finish_upload(layer.build_id, UploadOutcome::Landed);
            add_layer(&fixture.cache, pause_of(layer.build_id));
            drop(sandbox);

            fixture.cache.evict_expired();

            assert!(!released(&fixture, &layer), "{origin:?}");
            assert!(!fixture.cache.lookup(layer.build_id).unwrap().mark().superseded);
        }
    }

    #[tokio::test]
    async fn an_in_place_checkpoint_keeps_its_predecessor() {
        let fixture = cache_with(true, true);
        let (layer, sandbox) = add_layer(&fixture.cache, SnapshotLineage::without_predecessor(Origin::Pause));
        fixture.cache.finish_upload(layer.build_id, UploadOutcome::Landed);

        add_layer(
            &fixture.cache,
            SnapshotLineage::of_sandbox(Origin::Checkpoint, layer.build_id, true),
        );
        drop(sandbox);
        fixture.cache.evict_expired();

        assert!(!released(&fixture, &layer));
    }

    #[tokio::test]
    async fn a_failed_upload_keeps_the_only_copy_resident() {
        let fixture = cache_with(true, true);
        let (layer, sandbox) = add_layer(&fixture.cache, SnapshotLineage::without_predecessor(Origin::Pause));
        fixture.cache.finish_upload(layer.build_id, UploadOutcome::Failed);
        add_layer(&fixture.cache, pause_of(layer.build_id));
        drop(sandbox);

        fixture.cache.evict_expired();

        assert!(!released(&fixture, &layer));
    }

    #[tokio::test(start_paused = true)]
    async fn a_released_build_is_remembered_for_the_entry_ttl_and_each_ask_extends_it() {
        let fixture = cache_with(true, true);
        let (layer, sandbox) = add_layer(&fixture.cache, SnapshotLineage::without_predecessor(Origin::Pause));
        assert!(!fixture.cache.was_released(layer.build_id));
        fixture.cache.finish_upload(layer.build_id, UploadOutcome::Landed);
        add_layer(&fixture.cache, pause_of(layer.build_id));
        drop(sandbox);
        fixture.cache.evict_expired();
        assert!(released(&fixture, &layer));

        tokio::time::advance(TEMPLATE_EXPIRATION - Duration::from_secs(60)).await;
        assert!(fixture.cache.was_released(layer.build_id));
        tokio::time::advance(TEMPLATE_EXPIRATION - Duration::from_secs(60)).await;
        assert!(
            fixture.cache.was_released(layer.build_id),
            "the ask extended the record"
        );
        tokio::time::advance(TEMPLATE_EXPIRATION + Duration::from_secs(1)).await;
        fixture.cache.evict_expired();
        assert!(!fixture.cache.was_released(layer.build_id));
    }

    #[tokio::test(start_paused = true)]
    async fn a_superseded_layer_that_was_never_released_expires_held() {
        let fixture = cache_with(false, false);
        let (layer, sandbox) = add_layer(&fixture.cache, SnapshotLineage::without_predecessor(Origin::Pause));
        fixture.cache.finish_upload(layer.build_id, UploadOutcome::Landed);
        add_layer(&fixture.cache, pause_of(layer.build_id));
        let template = sandbox.clone();
        drop(sandbox);

        tokio::time::advance(TEMPLATE_EXPIRATION + Duration::from_secs(1)).await;
        fixture.cache.evict_expired();
        assert!(
            fixture.cache.lookup(layer.build_id).is_some(),
            "still held by the clone"
        );
        drop(template);
        tokio::time::advance(UNPINNED_GRACE + Duration::from_secs(1)).await;
        fixture.cache.evict_expired();

        assert!(fixture.cache.lookup(layer.build_id).is_none());
        assert!(!layer.dir.exists());
        assert!(
            !fixture.cache.was_released(layer.build_id),
            "an expiry is not a release"
        );
    }
}
