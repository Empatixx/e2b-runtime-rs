use crate::storage::frame_table::FrameTable;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildData {
    pub size: i64,
    pub checksum: [u8; 32],
    pub frame_data: Option<FrameTable>,
}
