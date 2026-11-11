use std::str::FromStr;

use super::{Flags, FlagsPatch, PeerFeatureFlag};

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

    /// Dials disguise without waiting for the peer to ask for it.
    pub fn initiates(self) -> bool {
        matches!(self, Self::Prefer | Self::Only)
    }

    /// Whether two endpoints with these policies disguise their automatic P2P.
    pub fn disguises_with(self, peer: Self) -> bool {
        !self.forbids()
            && !peer.forbids()
            && (self.requires() || peer.requires() || (self.prefers() && peer.prefers()))
    }

    /// Whether two endpoints with these policies may use a raw transport.
    pub fn allows_raw_with(self, peer: Self) -> bool {
        !self.requires() && !peer.requires()
    }

    /// The policy that the three wire booleans stand for. A peer that sets
    /// none of them, such as an official node, is `Off`. Nodes derive the
    /// booleans from one mode, so at most one is set; if several are,
    /// `disable` wins over `only`, which wins over `prefer`.
    pub fn from_wire(prefer: bool, disable: bool, only: bool) -> Self {
        if disable {
            Self::Disabled
        } else if only {
            Self::Only
        } else if prefer {
            Self::Prefer
        } else {
            Self::Off
        }
    }
}

impl PeerFeatureFlag {
    /// The disguise policy this peer advertises.
    pub fn p2p_disguise(&self) -> P2pDisguiseMode {
        P2pDisguiseMode::from_wire(
            self.prefer_wss_http3_for_p2p,
            self.disable_wss_http3_for_p2p,
            self.only_use_wss_http3_for_p2p,
        )
    }

    pub fn set_p2p_disguise(&mut self, mode: P2pDisguiseMode) {
        self.prefer_wss_http3_for_p2p = mode.prefers();
        self.disable_wss_http3_for_p2p = mode.forbids();
        self.only_use_wss_http3_for_p2p = mode.requires();
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
    fn peer_feature_flags_round_trip_every_mode() {
        for mode in [
            P2pDisguiseMode::Prefer,
            P2pDisguiseMode::Off,
            P2pDisguiseMode::Disabled,
            P2pDisguiseMode::Only,
        ] {
            let mut feature = PeerFeatureFlag::default();
            feature.set_p2p_disguise(mode);
            assert_eq!(feature.p2p_disguise(), mode);
        }
        // An official peer advertises none of the booleans.
        assert_eq!(
            PeerFeatureFlag::default().p2p_disguise(),
            P2pDisguiseMode::Off
        );
        assert_eq!(
            P2pDisguiseMode::from_wire(true, true, true),
            P2pDisguiseMode::Disabled
        );
        assert_eq!(
            P2pDisguiseMode::from_wire(true, false, true),
            P2pDisguiseMode::Only
        );
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
