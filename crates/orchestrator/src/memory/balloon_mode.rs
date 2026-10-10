use crate::template::metadata::Balloon;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BalloonMode {
    #[default]
    Unknown,
    None,
    Reporting,
    Hinting,
}

impl BalloonMode {
    pub fn stamped(balloon: Option<Balloon>) -> Self {
        match balloon {
            None => Self::Unknown,
            Some(Balloon { reporting: true, .. }) => Self::Reporting,
            Some(Balloon { hinting: true, .. }) => Self::Hinting,
            Some(_) => Self::None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::None => "none",
            Self::Reporting => "reporting",
            Self::Hinting => "hinting",
        }
    }

    pub fn raw(self) -> u8 {
        self as u8
    }

    pub fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::None,
            2 => Self::Reporting,
            3 => Self::Hinting,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_labels_read_like_upstream() {
        assert_eq!(BalloonMode::Unknown.as_str(), "unknown");
        assert_eq!(BalloonMode::Reporting.as_str(), "reporting");
        assert_eq!(BalloonMode::Hinting.as_str(), "hinting");
        assert_eq!(BalloonMode::None.as_str(), "none");
        assert_eq!(
            BalloonMode::from_raw(200),
            BalloonMode::Unknown,
            "out of range reads unknown"
        );
    }

    #[test]
    fn the_raw_value_round_trips() {
        for mode in [
            BalloonMode::Unknown,
            BalloonMode::None,
            BalloonMode::Reporting,
            BalloonMode::Hinting,
        ] {
            assert_eq!(BalloonMode::from_raw(mode.raw()), mode);
        }
    }
}
