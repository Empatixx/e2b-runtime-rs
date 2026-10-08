use std::collections::BTreeMap;

use uuid::Uuid;

use crate::proto::SchedulingMetadata;
use crate::storage::header::Header;

const CHAIN_LIMIT: usize = 128;

struct ArtifactBuilds {
    ids: Vec<String>,
    bytes: Vec<u64>,
    dropped: u32,
}

pub fn from_headers(
    build_id: Uuid,
    memfile: Option<&Header>,
    rootfs: &Header,
    new_memfile_bytes: u64,
) -> SchedulingMetadata {
    let rootfs_base = rootfs.metadata.base_build_id;
    let rootfs_builds = artifact_builds(rootfs, rootfs_base, build_id, 0);
    let memfile_builds = memfile.map(|memfile| {
        (
            memfile.metadata.base_build_id,
            artifact_builds(memfile, memfile.metadata.base_build_id, build_id, new_memfile_bytes),
        )
    });
    let (memfile_base, memfile_builds) =
        memfile_builds.map_or((String::new(), None), |(base, builds)| (base.to_string(), Some(builds)));
    let memfile_builds = memfile_builds.unwrap_or(ArtifactBuilds {
        ids: Vec::new(),
        bytes: Vec::new(),
        dropped: 0,
    });
    SchedulingMetadata {
        memfile_base_build_id: memfile_base,
        build_id: build_id.to_string(),
        memfile_build_ids: memfile_builds.ids,
        rootfs_build_ids: rootfs_builds.ids,
        memfile_dropped_builds: memfile_builds.dropped,
        rootfs_dropped_builds: rootfs_builds.dropped,
        memfile_build_bytes: memfile_builds.bytes,
        rootfs_build_bytes: rootfs_builds.bytes,
        rootfs_base_build_id: rootfs_base.to_string(),
    }
}

fn artifact_builds(header: &Header, base: Uuid, build: Uuid, inject_build_bytes: u64) -> ArtifactBuilds {
    let mut bytes_by_id: BTreeMap<Uuid, u64> = BTreeMap::new();
    header
        .mappings
        .iter()
        .filter(|mapping| !mapping.build_id.is_nil())
        .for_each(|mapping| *bytes_by_id.entry(mapping.build_id).or_default() += mapping.length);
    if !base.is_nil() {
        bytes_by_id.entry(base).or_insert(0);
    }
    if !build.is_nil() {
        bytes_by_id.entry(build).or_insert(inject_build_bytes);
    }
    let mut ids: Vec<Uuid> = bytes_by_id.keys().copied().collect();
    let dropped = ids.len().saturating_sub(CHAIN_LIMIT);
    if dropped > 0 {
        ids.sort_by(|a, b| {
            let pinned = |id: &Uuid| *id == base || *id == build;
            pinned(b)
                .cmp(&pinned(a))
                .then(bytes_by_id[b].cmp(&bytes_by_id[a]))
                .then(a.to_string().cmp(&b.to_string()))
        });
        ids.truncate(CHAIN_LIMIT);
    }
    ids.sort_by_key(Uuid::to_string);
    ArtifactBuilds {
        bytes: ids.iter().map(|id| bytes_by_id[id]).collect(),
        ids: ids.iter().map(Uuid::to_string).collect(),
        dropped: dropped as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::header::Metadata;
    use crate::storage::header::mapping::BuildMapping;

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn mapping(offset: u64, length: u64, build_id: Uuid) -> BuildMapping {
        BuildMapping {
            offset,
            length,
            build_id,
            storage_offset: 0,
        }
    }

    #[test]
    fn builds_are_listed_with_their_bytes_and_the_new_layer_is_injected() {
        let metadata = Metadata::template(id(1), 4096, 4 * 4096).next_generation(id(2));
        let header = Header::new(
            metadata,
            vec![
                mapping(0, 2 * 4096, id(1)),
                mapping(2 * 4096, 4096, Uuid::nil()),
                mapping(3 * 4096, 4096, id(2)),
            ],
        )
        .unwrap();

        let scheduling = from_headers(id(3), Some(&header), &header, 777);

        assert_eq!(scheduling.build_id, id(3).to_string());
        assert_eq!(scheduling.rootfs_base_build_id, id(1).to_string());
        assert_eq!(
            scheduling.memfile_build_ids,
            vec![id(1).to_string(), id(2).to_string(), id(3).to_string()]
        );
        assert_eq!(scheduling.memfile_build_bytes, vec![2 * 4096, 4096, 777]);
        assert_eq!(scheduling.rootfs_build_bytes, vec![2 * 4096, 4096, 0]);
        assert_eq!(scheduling.memfile_dropped_builds, 0);
    }

    #[test]
    fn long_chains_keep_base_build_and_the_heaviest_layers() {
        let base = id(1);
        let mappings: Vec<BuildMapping> = (0..130u64)
            .map(|index| {
                mapping(
                    index * 4096,
                    4096 * (index + 1),
                    Uuid::from_u128(1000 + u128::from(index)),
                )
            })
            .collect();
        let mut header = Header::new(Metadata::template(base, 4096, 4096), Vec::new()).unwrap();
        header.mappings = mappings;

        let builds = artifact_builds(&header, base, id(2), 0);

        assert_eq!((builds.ids.len(), builds.dropped), (128, 4));
        assert!(builds.ids.contains(&base.to_string()));
        assert!(builds.ids.contains(&id(2).to_string()));
        assert!(!builds.ids.contains(&Uuid::from_u128(1000).to_string()));
    }
}
