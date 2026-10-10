use std::ops::Deref;

use super::chunker::ChunkSlice;

const HUGE_PAGE: usize = 2 << 20;

static EMPTY_HUGE_PAGE: [u8; HUGE_PAGE] = [0; HUGE_PAGE];

pub enum PageData {
    Mapped(ChunkSlice),
    Zeroes(usize),
    Owned(Vec<u8>),
}

impl PageData {
    pub fn zeroes(length: usize) -> Self {
        match length <= HUGE_PAGE {
            true => Self::Zeroes(length),
            false => Self::Owned(vec![0; length]),
        }
    }
}

impl Deref for PageData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Mapped(slice) => slice,
            Self::Zeroes(length) => &EMPTY_HUGE_PAGE[..*length],
            Self::Owned(bytes) => bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeroes_borrow_the_empty_huge_page_up_to_its_size() {
        assert!(matches!(PageData::zeroes(4096), PageData::Zeroes(4096)));
        assert!(matches!(PageData::zeroes(HUGE_PAGE + 1), PageData::Owned(_)));
        assert_eq!(&*PageData::zeroes(4096), &[0; 4096]);
        assert_eq!(PageData::zeroes(HUGE_PAGE + 1).len(), HUGE_PAGE + 1);
    }
}
