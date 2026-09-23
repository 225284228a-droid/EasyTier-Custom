include!(concat!(env!("OUT_DIR"), "/web.rs"));
include!(concat!(env!("OUT_DIR"), "/web.serde.rs"));

#[cfg(test)]
mod tests {
    use super::HeartbeatRequest;
    use prost::Message;

    // Current upstream uses tags 10/11; Custom capabilities must not change
    // their wire types or prevent upstream from ignoring extension fields.
    #[derive(Clone, PartialEq, Message)]
    struct UpstreamHeartbeat {
        #[prost(message, repeated, tag = "10")]
        failed_network_instances: Vec<crate::common::Uuid>,
        #[prost(bool, tag = "11")]
        support_heartbeat_policy: bool,
    }

    #[test]
    fn heartbeat_uses_current_upstream_wire_format() {
        let custom = HeartbeatRequest {
            failed_network_instances: vec![uuid::Uuid::new_v4().into()],
            support_heartbeat_policy: true,
            support_local_configs: true,
            ..Default::default()
        };
        let upstream = UpstreamHeartbeat::decode(custom.encode_to_vec().as_slice()).unwrap();
        assert_eq!(
            upstream.failed_network_instances,
            custom.failed_network_instances
        );
        assert!(upstream.support_heartbeat_policy);
        let decoded = HeartbeatRequest::decode(upstream.encode_to_vec().as_slice()).unwrap();
        assert_eq!(
            decoded.failed_network_instances,
            custom.failed_network_instances
        );
        assert!(decoded.support_heartbeat_policy);
        assert!(!decoded.support_local_configs);
    }
}
