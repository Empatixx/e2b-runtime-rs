use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    BadTarget,
    BadIdentityKnown,
    Suppressed,
    InFlight,
    Panicked,
    Superseded,
    BadImage,
    CacheUnusable,
    Other,
}

impl Kind {
    fn text(self) -> &'static str {
        match self {
            Kind::BadTarget => "envd binary target is not usable",
            Kind::BadIdentityKnown => "envd binary target is already known to be unusable",
            Kind::Suppressed => "envd binary cache is in its post-failure backoff",
            Kind::InFlight => "a warm of this envd binary is already in flight",
            Kind::Panicked => "envd binary warm panicked",
            Kind::Superseded => "envd binary copy describes a superseded generation",
            Kind::BadImage => "envd binary copy is not a complete image",
            Kind::CacheUnusable => "envd binary cache directory is unusable",
            Kind::Other => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmError {
    kinds: Vec<Kind>,
    message: String,
}

impl WarmError {
    pub fn of(kind: Kind) -> Self {
        Self {
            kinds: vec![kind],
            message: kind.text().to_owned(),
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self {
            kinds: vec![Kind::Other],
            message: message.into(),
        }
    }

    pub fn tagged(kind: Kind, detail: impl fmt::Display) -> Self {
        Self {
            kinds: vec![kind],
            message: format!("{}: {detail}", kind.text()),
        }
    }

    pub fn wrapping(kind: Kind, subject: &str, cause: &WarmError) -> Self {
        Self {
            kinds: std::iter::once(kind).chain(cause.kinds.iter().copied()).collect(),
            message: format!("{}: {subject}: {}", kind.text(), cause.message),
        }
    }

    pub fn context(prefix: impl fmt::Display, cause: &WarmError) -> Self {
        Self {
            kinds: cause.kinds.clone(),
            message: format!("{prefix}: {}", cause.message),
        }
    }

    pub fn is(&self, kind: Kind) -> bool {
        self.kinds.contains(&kind)
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for WarmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WarmError {}

pub const WARM_OK: &str = "ok";
pub const WARM_SUPERSEDED: &str = "superseded";
pub const WARM_SUPPRESSED: &str = "suppressed";
pub const WARM_PINNED: &str = "pinned";
pub const WARM_ALREADY_WARMING: &str = "already_warming";
pub const WARM_BAD_TARGET: &str = "bad_target";
pub const WARM_FAILED: &str = "failed";

pub const WARM_RESULTS: [&str; 7] = [
    WARM_OK,
    WARM_SUPERSEDED,
    WARM_SUPPRESSED,
    WARM_PINNED,
    WARM_ALREADY_WARMING,
    WARM_BAD_TARGET,
    WARM_FAILED,
];

pub fn warm_result(error: &WarmError) -> &'static str {
    match error {
        e if e.is(Kind::Suppressed) => WARM_SUPPRESSED,
        e if e.is(Kind::BadIdentityKnown) => WARM_PINNED,
        e if e.is(Kind::InFlight) => WARM_ALREADY_WARMING,
        e if e.is(Kind::BadTarget) => WARM_BAD_TARGET,
        _ => WARM_FAILED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppression_is_told_apart_from_failure() {
        assert_eq!(warm_result(&WarmError::of(Kind::Suppressed)), "suppressed");
        assert_eq!(
            warm_result(&WarmError::context("wrapped", &WarmError::of(Kind::Suppressed))),
            "suppressed"
        );
        assert_eq!(warm_result(&WarmError::other("no space left on device")), "failed");
        assert_eq!(
            warm_result(&WarmError::context("copy", &WarmError::of(Kind::CacheUnusable))),
            "failed"
        );
    }

    #[test]
    fn a_pin_and_a_suppression_are_opposites() {
        assert_eq!(warm_result(&WarmError::of(Kind::BadIdentityKnown)), WARM_PINNED);
        assert_eq!(warm_result(&WarmError::of(Kind::Suppressed)), WARM_SUPPRESSED);
        assert_eq!(warm_result(&WarmError::of(Kind::InFlight)), WARM_ALREADY_WARMING);
        assert_eq!(warm_result(&WarmError::of(Kind::BadTarget)), WARM_BAD_TARGET);
    }

    #[test]
    fn a_wrapped_cause_keeps_every_kind_it_carried() {
        let image = WarmError::tagged(Kind::BadImage, "bad ELF magic");

        let target = WarmError::wrapping(Kind::BadTarget, "\"/fc-envd/envd\"", &image);

        assert!(target.is(Kind::BadTarget) && target.is(Kind::BadImage));
        assert!(!target.is(Kind::Panicked));
        assert_eq!(
            target.to_string(),
            "envd binary target is not usable: \"/fc-envd/envd\": envd binary copy is not a complete image: bad ELF magic"
        );
    }
}
