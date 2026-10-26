use std::str::FromStr;

use super::{Flags, FlagsPatch};

/// Automatic P2P disguise policy. Peer feature flags deliberately retain their
/// existing boolean wire representation for compatibility with older peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum P2pDisguiseMode {
    /// Negotiate disguise with raw fallback.
    #[default]
    Prefer,
    /// Do not initiate disguise, but accept a peer that requires it.
    Off,
    /// Never use disguise for automatic P2P.
    Disabled,
    /// Require disguise for automatic P2P.
    Only,
}

impl FromStr for P2pDisguiseMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "prefer" => Ok(Self::Prefer),
            "off" => Ok(Self::Off),
            "disabled" => Ok(Self::Disabled),
            "only" => Ok(Self::Only),
            _ => anyhow::bail!(
                "p2p_disguise_mode must be one of prefer, off, disabled, only (got {value:?})"
            ),
        }
    }
}

impl P2pDisguiseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prefer => "prefer",
            Self::Off => "off",
            Self::Disabled => "disabled",
            Self::Only => "only",
        }
    }

    /// Initiates disguise when the peer also prefers it.
    pub fn prefers(self) -> bool {
        self == Self::Prefer
    }

    /// Never dials a disguised automatic P2P transport.
    pub fn forbids(self) -> bool {
        self == Self::Disabled
    }

    /// Never dials a raw automatic P2P transport.
    pub fn requires(self) -> bool {
        self == Self::Only
    }

    /// The policy of the former three boolean flags (`prefer_wss_http3_for_p2p`,
    /// `disable_wss_http3_for_p2p`, `only_use_wss_http3_for_hole_punching`),
    /// with their defaults for omitted values.
    pub fn from_legacy_flags(
        prefer: Option<bool>,
        disable: Option<bool>,
        only: Option<bool>,
    ) -> anyhow::Result<Self> {
        match (disable == Some(true), only == Some(true)) {
            (true, true) => anyhow::bail!(
                "disable_wss_http3_for_p2p and only_use_wss_http3_for_hole_punching are mutually \
                 exclusive: the first forbids the disguised transport while the second forbids the raw transport"
            ),
            (true, false) => Ok(Self::Disabled),
            (false, true) => Ok(Self::Only),
            (false, false) if prefer == Some(false) => Ok(Self::Off),
            (false, false) => Ok(Self::Prefer),
        }
    }
}

impl Flags {
    /// The configured disguise policy. Resolved flags always carry a mode
    /// (configurations are validated when loaded); the empty mode of a bare
    /// `Flags::default()` does not initiate disguise, like the former boolean.
    pub fn p2p_disguise(&self) -> P2pDisguiseMode {
        self.p2p_disguise_mode
            .parse()
            .unwrap_or(P2pDisguiseMode::Off)
    }

    pub fn set_p2p_disguise(&mut self, mode: P2pDisguiseMode) {
        self.p2p_disguise_mode = mode.as_str().to_owned();
    }
}

impl FlagsPatch {
    pub fn validate_p2p_disguise(&self) -> anyhow::Result<()> {
        if let Some(mode) = self
            .p2p_disguise_mode
            .as_deref()
            .filter(|mode| !mode.trim().is_empty())
        {
            mode.parse::<P2pDisguiseMode>()?;
        }
        Ok(())
    }

    /// Store the policy in its canonical spelling; the default `prefer` is
    /// omitted.
    pub fn canonicalize_p2p_disguise(&mut self) {
        let mode = self
            .p2p_disguise_mode
            .as_deref()
            .filter(|mode| !mode.trim().is_empty())
            .map(|mode| mode.parse::<P2pDisguiseMode>());
        match mode {
            Some(Ok(mode)) => {
                self.p2p_disguise_mode =
                    (mode != P2pDisguiseMode::Prefer).then(|| mode.as_str().to_owned())
            }
            Some(Err(_)) => {}
            None => self.p2p_disguise_mode = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_flags_map_to_the_equivalent_mode() {
        for (prefer, disable, only, expected) in [
            (None, None, None, P2pDisguiseMode::Prefer),
            (
                Some(true),
                Some(false),
                Some(false),
                P2pDisguiseMode::Prefer,
            ),
            (Some(false), None, None, P2pDisguiseMode::Off),
            (Some(true), Some(true), None, P2pDisguiseMode::Disabled),
            (
                Some(false),
                Some(true),
                Some(false),
                P2pDisguiseMode::Disabled,
            ),
            (Some(true), None, Some(true), P2pDisguiseMode::Only),
            (Some(false), Some(false), Some(true), P2pDisguiseMode::Only),
        ] {
            assert_eq!(
                P2pDisguiseMode::from_legacy_flags(prefer, disable, only).unwrap(),
                expected,
                "{prefer:?} {disable:?} {only:?}"
            );
        }
        assert!(P2pDisguiseMode::from_legacy_flags(None, Some(true), Some(true)).is_err());
    }

    #[test]
    fn omitted_mode_resolves_to_the_default_prefer() {
        let flags = Flags::resolve(FlagsPatch::default());
        assert_eq!(flags.p2p_disguise(), P2pDisguiseMode::Prefer);
        let flags = Flags::resolve(FlagsPatch {
            p2p_disguise_mode: Some("  ".into()),
            ..Default::default()
        });
        assert_eq!(flags.p2p_disguise(), P2pDisguiseMode::Prefer);
        assert_eq!(Flags::default().p2p_disguise(), P2pDisguiseMode::Off);
        let flags = Flags::resolve(FlagsPatch {
            p2p_disguise_mode: Some(" Only ".into()),
            ..Default::default()
        });
        assert_eq!(flags.p2p_disguise(), P2pDisguiseMode::Only);
    }

    #[test]
    fn validates_modes_and_stores_them_canonically() {
        let invalid = FlagsPatch {
            p2p_disguise_mode: Some("disable".into()),
            ..Default::default()
        };
        assert!(invalid.validate_p2p_disguise().is_err());
        for (input, stored) in [
            ("prefer", None),
            ("", None),
            (" OFF ", Some("off")),
            ("only", Some("only")),
        ] {
            let mut patch = FlagsPatch {
                p2p_disguise_mode: Some(input.into()),
                ..Default::default()
            };
            patch.canonicalize_p2p_disguise();
            assert_eq!(patch.p2p_disguise_mode.as_deref(), stored, "{input}");
        }
    }
}
