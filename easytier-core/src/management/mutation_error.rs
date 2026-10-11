//! Stable identities for the local-config mutation errors that cross the
//! management RPC boundary.
//!
//! The RPC transport flattens an anyhow error into a string, so a peer on the
//! other end can only classify it by text. Every variant's Display IS its
//! stable code, and [`LocalConfigMutationError::classify`] is the single place
//! that matches: an in-process error downcasts through the type, an error that
//! crossed the wire falls back to the code (anyhow's `Debug` keeps the root
//! code of a context-wrapped error in its `Caused by:` chain), and prose from
//! older peers is honored last so a rolling upgrade never misclassifies a
//! conflict as a generic failure.

use anyhow::Error as AnyhowError;
use easytier_proto::rpc_types;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LocalConfigMutationError {
    /// The local configuration changed under us; re-observe and retry.
    #[error("local_config_revision_conflict")]
    RevisionConflict,
    /// The configuration refuses mutation (protection flag, ambiguous
    /// identity, catalog status, offline session).
    #[error("local_config_protected")]
    Protected,
    /// The configuration file itself is read-only.
    #[error("local_config_read_only")]
    ReadOnly,
}

impl LocalConfigMutationError {
    pub const ALL: [Self; 3] = [Self::RevisionConflict, Self::Protected, Self::ReadOnly];

    pub const fn code(self) -> &'static str {
        match self {
            Self::RevisionConflict => "local_config_revision_conflict",
            Self::Protected => "local_config_protected",
            Self::ReadOnly => "local_config_read_only",
        }
    }

    /// Classify an error raised by a local-config mutation. Unrelated
    /// failures yield `None`.
    pub fn classify(error: &AnyhowError) -> Option<Self> {
        if let Some(typed) = error.downcast_ref::<Self>() {
            return Some(*typed);
        }
        let text = error.to_string();
        if let Some(typed) = Self::ALL.iter().find(|typed| text.contains(typed.code())) {
            return Some(*typed);
        }
        // Prose errors emitted by older peers; the stable codes above always
        // win when present.
        if text.contains("read-only") {
            Some(Self::ReadOnly)
        } else if text.contains("protected") {
            Some(Self::Protected)
        } else {
            None
        }
    }

    /// Classify an error still wrapped in the RPC transport's own error enum,
    /// as an RPC caller receives it. Only the execution variant carries a
    /// mutation failure; every other kind is a transport-level failure.
    pub fn classify_rpc(error: &rpc_types::error::Error) -> Option<Self> {
        match error {
            rpc_types::error::Error::ExecutionError(error) => Self::classify(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_errors_downcast_through_anyhow_context() {
        let error = anyhow::Error::from(LocalConfigMutationError::ReadOnly)
            .context("instance 123 cannot be overwritten");
        assert_eq!(
            LocalConfigMutationError::classify(&error),
            Some(LocalConfigMutationError::ReadOnly)
        );
    }

    #[test]
    fn wire_flattened_strings_classify_by_stable_code() {
        for typed in LocalConfigMutationError::ALL {
            let flattened = anyhow::anyhow!(format!(
                "{:?}",
                anyhow::Error::from(typed).context("peer detail")
            ));
            assert_eq!(
                LocalConfigMutationError::classify(&flattened),
                Some(typed),
                "{} must survive the RPC flattening",
                typed.code()
            );
        }
    }

    #[test]
    fn prose_from_older_peers_still_classifies() {
        let cases = [
            (
                "configuration is read-only",
                LocalConfigMutationError::ReadOnly,
            ),
            (
                "configuration file is read-only",
                LocalConfigMutationError::ReadOnly,
            ),
            (
                "configuration for instance 9e3b is read-only",
                LocalConfigMutationError::ReadOnly,
            ),
            (
                "configuration is protected",
                LocalConfigMutationError::Protected,
            ),
            (
                "configuration is protected or unavailable",
                LocalConfigMutationError::Protected,
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(
                LocalConfigMutationError::classify(&anyhow::anyhow!(text)),
                Some(expected),
                "{text}"
            );
        }
    }

    #[test]
    fn unrelated_errors_do_not_classify() {
        assert_eq!(
            LocalConfigMutationError::classify(&anyhow::anyhow!("dial failed")),
            None
        );
        assert_eq!(
            LocalConfigMutationError::classify(&anyhow::anyhow!(
                "configuration could not be removed"
            )),
            None
        );
    }

    #[test]
    fn rpc_transport_errors_classify_through_their_execution_payload() {
        use rpc_types::error::Error as RpcError;

        let conflict = RpcError::ExecutionError(anyhow::anyhow!(format!(
            "{:?}",
            anyhow::Error::from(LocalConfigMutationError::RevisionConflict)
        )));
        assert_eq!(
            LocalConfigMutationError::classify_rpc(&conflict),
            Some(LocalConfigMutationError::RevisionConflict)
        );
        assert_eq!(
            LocalConfigMutationError::classify_rpc(&RpcError::DecodeError),
            None
        );
    }
}
