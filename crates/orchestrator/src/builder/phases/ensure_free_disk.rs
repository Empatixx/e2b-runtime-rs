pub mod sizing;

use std::time::Duration;

use uuid::Uuid;

use crate::builder::phase::PhaseMeta;
use crate::template::metadata::{CURRENT_VERSION, TemplateMetadata, TemplateVersions};

pub const RESIZE_DISK: &str = "resize-disk";
pub const RESIZE_DISK_TEXT: &str = "Resizing disk";
pub const RESIZE_DISK_TIMEOUT: Duration = Duration::from_secs(3600);
const FILESYSTEM_ONLY_VERSION: u64 = CURRENT_VERSION;
const MB_SHIFT: u32 = 20;

pub fn phase_meta() -> PhaseMeta {
    PhaseMeta {
        phase: RESIZE_DISK,
        step_type: RESIZE_DISK.to_owned(),
        step_number: None,
    }
}

pub fn free_disk_size_mb(free_disk_size_mb: Option<i32>, disk_size_mb: i32) -> i64 {
    free_disk_size_mb.unwrap_or(disk_size_mb).into()
}

pub fn fresh_layer_metadata(
    source: &TemplateMetadata,
    kernel_version: &str,
    firecracker_version: &str,
) -> TemplateMetadata {
    TemplateMetadata {
        template: TemplateVersions {
            build_id: Uuid::new_v4().to_string(),
            kernel_version: kernel_version.to_owned(),
            firecracker_version: firecracker_version.to_owned(),
        },
        ..source.clone()
    }
}

pub fn mark_filesystem_only(metadata: TemplateMetadata) -> TemplateMetadata {
    TemplateMetadata {
        filesystem_only: true,
        version: metadata.version.max(FILESYSTEM_ONLY_VERSION),
        ..metadata
    }
}

pub fn no_resize_needed(free_before: i64, target: i64) -> String {
    format!(
        "resize-disk: {} MiB free already >= target {} MiB, no resize needed",
        free_before >> MB_SHIFT,
        target >> MB_SHIFT
    )
}

pub fn resized(free_before: i64, free_after: i64) -> String {
    format!(
        "resize-disk: free {} -> {} MiB",
        free_before >> MB_SHIFT,
        free_after >> MB_SHIFT
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phase_reads_like_upstream() {
        let meta = phase_meta();

        assert_eq!(meta.step(), "resize-disk");
        assert_eq!(meta.phase, "resize-disk");
        assert_eq!(meta.log_fields().get("step_number").map(String::as_str), Some("<nil>"));
        assert_eq!(RESIZE_DISK_TEXT, "Resizing disk");
    }

    #[test]
    fn the_free_disk_size_falls_back_to_the_disk_size() {
        assert_eq!(free_disk_size_mb(Some(512), 2048), 512);
        assert_eq!(free_disk_size_mb(Some(0), 2048), 0);
        assert_eq!(free_disk_size_mb(None, 2048), 2048);
    }

    #[test]
    fn a_fresh_layer_keeps_the_source_and_gets_a_new_build_id() {
        let source = TemplateMetadata {
            template: TemplateVersions {
                build_id: "old".to_owned(),
                kernel_version: "k0".to_owned(),
                firecracker_version: "f0".to_owned(),
            },
            from_image: Some("debian".to_owned()),
            ..TemplateMetadata::default()
        };

        let fresh = fresh_layer_metadata(&source, "k1", "f1");

        assert!(Uuid::parse_str(&fresh.template.build_id).is_ok());
        assert_eq!(fresh.template.kernel_version, "k1");
        assert_eq!(fresh.template.firecracker_version, "f1");
        assert_eq!(fresh.from_image.as_deref(), Some("debian"));
        assert_eq!(source.template.build_id, "old");
    }

    #[test]
    fn marking_filesystem_only_raises_the_version_to_at_least_two() {
        let old = TemplateMetadata {
            version: 1,
            ..TemplateMetadata::default()
        };
        let newer = TemplateMetadata {
            version: 3,
            ..TemplateMetadata::default()
        };

        assert_eq!(
            (
                mark_filesystem_only(old.clone()).filesystem_only,
                mark_filesystem_only(old).version
            ),
            (true, 2)
        );
        assert_eq!(mark_filesystem_only(newer).version, 3);
    }

    #[test]
    fn the_user_log_lines_are_whole_mebibytes() {
        assert_eq!(
            no_resize_needed(600 << 20, 512 << 20),
            "resize-disk: 600 MiB free already >= target 512 MiB, no resize needed"
        );
        assert_eq!(
            no_resize_needed((512 << 20) + 5, 512 << 20),
            "resize-disk: 512 MiB free already >= target 512 MiB, no resize needed"
        );
        assert_eq!(resized(-(3 << 20), 700 << 20), "resize-disk: free -3 -> 700 MiB");
    }
}
