use std::str::FromStr;

use super::{Flags, FlagsPatch};

/// Local configuration policy. Peer feature flags deliberately retain their
/// existing boolean wire representation for compatibility with older peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum P2pDisguiseMode {
    Prefer,
    Off,
    Disabled,
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

    pub(super) fn apply(self, flags: &mut Flags) {
        flags.p2p_disguise_mode = self.as_str().to_owned();
        flags.prefer_wss_http3_for_p2p = self == Self::Prefer;
        flags.disable_wss_http3_for_p2p = self == Self::Disabled;
        flags.only_use_wss_http3_for_hole_punching = self == Self::Only;
    }
}

impl FlagsPatch {
    /// A new mode overrides legacy booleans. Without it, retain the legacy
    /// conflict check rather than accepting a policy with no dialable path.
    pub fn validate_p2p_disguise(&self) -> anyhow::Result<()> {
        if let Some(mode) = self
            .p2p_disguise_mode
            .as_deref()
            .filter(|mode| !mode.trim().is_empty())
        {
            mode.parse::<P2pDisguiseMode>()?;
        } else if self.disable_wss_http3_for_p2p == Some(true)
            && self.only_use_wss_http3_for_hole_punching == Some(true)
        {
            anyhow::bail!(
                "disable_wss_http3_for_p2p and only_use_wss_http3_for_hole_punching are mutually \
                 exclusive: the first forbids the disguised transport while the second forbids the raw transport"
            );
        }
        Ok(())
    }

    /// Store one policy when the new setting is present. Default `prefer` can
    /// be omitted safely after clearing legacy overrides; legacy-only patches
    /// remain untouched, including explicit false values.
    pub fn canonicalize_p2p_disguise(&mut self) {
        let Some(mode) = self
            .p2p_disguise_mode
            .as_deref()
            .and_then(|mode| mode.parse::<P2pDisguiseMode>().ok())
        else {
            return;
        };
        self.prefer_wss_http3_for_p2p = None;
        self.disable_wss_http3_for_p2p = None;
        self.only_use_wss_http3_for_hole_punching = None;
        self.p2p_disguise_mode =
            (mode != P2pDisguiseMode::Prefer).then(|| mode.as_str().to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_overrides_legacy_flags_and_preserves_peer_wire_policy() {
        for (mode, expected) in [
            ("prefer", (true, false, false)),
            ("off", (false, false, false)),
            ("disabled", (false, true, false)),
            ("only", (false, false, true)),
        ] {
            let patch = FlagsPatch {
                p2p_disguise_mode: Some(mode.into()),
                prefer_wss_http3_for_p2p: Some(true),
                disable_wss_http3_for_p2p: Some(true),
                only_use_wss_http3_for_hole_punching: Some(true),
                ..Default::default()
            };
            patch.validate_p2p_disguise().unwrap();
            let flags = Flags::resolve(patch);
            assert_eq!(
                (
                    flags.prefer_wss_http3_for_p2p,
                    flags.disable_wss_http3_for_p2p,
                    flags.only_use_wss_http3_for_hole_punching
                ),
                expected
            );
        }
    }

    #[test]
    fn legacy_off_is_not_changed_to_the_default_prefer() {
        let patch = FlagsPatch {
            prefer_wss_http3_for_p2p: Some(false),
            ..Default::default()
        };
        assert!(!Flags::resolve(patch.clone()).prefer_wss_http3_for_p2p);
        let mut canonical = patch.clone();
        canonical.canonicalize_p2p_disguise();
        assert_eq!(canonical, patch);
        assert!(Flags::resolve(FlagsPatch::default()).prefer_wss_http3_for_p2p);
    }

    #[test]
    fn validates_modes_and_canonicalizes_only_new_settings() {
        let invalid = FlagsPatch {
            p2p_disguise_mode: Some("disable".into()),
            ..Default::default()
        };
        assert!(invalid.validate_p2p_disguise().is_err());
        let mut patch = FlagsPatch {
            p2p_disguise_mode: Some("prefer".into()),
            prefer_wss_http3_for_p2p: Some(false),
            ..Default::default()
        };
        patch.canonicalize_p2p_disguise();
        assert_eq!(patch, FlagsPatch::default());
        let contradictory = FlagsPatch {
            disable_wss_http3_for_p2p: Some(true),
            only_use_wss_http3_for_hole_punching: Some(true),
            ..Default::default()
        };
        assert!(contradictory.validate_p2p_disguise().is_err());
    }
}
