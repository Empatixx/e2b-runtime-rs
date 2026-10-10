use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Pause,
    Checkpoint,
    Build,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotLineage {
    pub origin: Origin,
    pub predecessor: Option<Uuid>,
    pub abandons_predecessor: bool,
}

impl SnapshotLineage {
    pub fn build() -> Self {
        Self {
            origin: Origin::Build,
            predecessor: None,
            abandons_predecessor: false,
        }
    }

    pub fn without_predecessor(origin: Origin) -> Self {
        Self {
            origin,
            ..Self::build()
        }
    }

    pub fn of_sandbox(origin: Origin, predecessor: Uuid, maintain_sandbox: bool) -> Self {
        Self {
            origin,
            predecessor: Some(predecessor),
            abandons_predecessor: !maintain_sandbox,
        }
    }

    pub fn of_running_build(origin: Origin, build_id: &str, maintain_sandbox: bool) -> Self {
        Uuid::parse_str(build_id).map_or_else(
            |_| Self::without_predecessor(origin),
            |predecessor| Self::of_sandbox(origin, predecessor, maintain_sandbox),
        )
    }

    pub fn supersedes(&self) -> Option<Uuid> {
        self.predecessor.filter(|_| self.abandons_predecessor)
    }

    pub fn kind(&self) -> LayerKind {
        match self.origin {
            Origin::Pause => LayerKind::Pause,
            Origin::Checkpoint | Origin::Build => LayerKind::LocalTemplate,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Pause,
    LocalTemplate,
    Fetched,
}

impl LayerKind {
    pub fn label(self) -> &'static str {
        match self {
            LayerKind::Pause => "pause_layer",
            LayerKind::LocalTemplate => "local_template",
            LayerKind::Fetched => "fetched",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadOutcome {
    Landed,
    Failed,
    Abandoned,
}

#[derive(Debug, Default)]
pub struct LayerMark {
    pub upload: Option<UploadOutcome>,
    pub superseded: bool,
    pub gone: bool,
    pub suppressed: bool,
}

impl LayerMark {
    pub fn landed(&self) -> bool {
        self.upload == Some(UploadOutcome::Landed)
    }

    pub fn finish_upload(&mut self, outcome: UploadOutcome) {
        if !self.landed() {
            self.upload = Some(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_operation_that_leaves_its_predecessor_supersedes_it() {
        let predecessor = Uuid::new_v4();

        assert_eq!(
            SnapshotLineage::of_sandbox(Origin::Pause, predecessor, false).supersedes(),
            Some(predecessor)
        );
        assert_eq!(
            SnapshotLineage::of_sandbox(Origin::Checkpoint, predecessor, true).supersedes(),
            None
        );
        assert_eq!(SnapshotLineage::build().supersedes(), None);
    }

    #[test]
    fn only_a_pause_makes_a_pause_layer() {
        let predecessor = Uuid::new_v4();

        assert_eq!(
            SnapshotLineage::of_sandbox(Origin::Pause, predecessor, false).kind(),
            LayerKind::Pause
        );
        assert_eq!(
            SnapshotLineage::of_sandbox(Origin::Checkpoint, predecessor, false).kind(),
            LayerKind::LocalTemplate
        );
        assert_eq!(SnapshotLineage::build().kind(), LayerKind::LocalTemplate);
    }

    #[test]
    fn a_landed_upload_is_never_moved_off_landed() {
        let mut mark = LayerMark::default();

        mark.finish_upload(UploadOutcome::Landed);
        mark.finish_upload(UploadOutcome::Failed);

        assert!(mark.landed());
    }
}
