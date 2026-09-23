include!(concat!(env!("OUT_DIR"), "/web.rs"));
include!(concat!(env!("OUT_DIR"), "/web.serde.rs"));

use bytes::Buf;
use prost::{
    Message,
    encoding::{self, DecodeContext, WireType},
};

/// Accept the pre-2.7 Custom bool at tag 10 without confusing it with the
/// upstream failed-instance list at the same tag. All new writers use 50001.
pub fn decode_heartbeat_request(
    mut input: bytes::Bytes,
) -> crate::rpc_types::error::Result<HeartbeatRequest> {
    let mut normalized = Vec::with_capacity(input.len());
    let mut legacy_local = None;
    let mut modern_fields = false;
    while input.has_remaining() {
        let original = input.clone();
        let (tag, wire) = encoding::decode_key(&mut input)?;
        if tag == 10 && wire == WireType::Varint {
            let mut local = false;
            encoding::bool::merge(wire, &mut local, &mut input, DecodeContext::default())?;
            legacy_local = Some(local);
        } else {
            modern_fields |= tag == 10 || tag == 50001;
            encoding::skip_field(wire, tag, &mut input, DecodeContext::default())?;
            normalized.extend_from_slice(&original[..original.len() - input.len()]);
        }
    }
    if let Some(local) = legacy_local {
        if modern_fields {
            return Err(crate::rpc_types::error::Error::MalformatRpcPacket(
                "mixed legacy and modern heartbeat fields".to_owned(),
            ));
        }
        encoding::bool::encode(50001, &local, &mut normalized);
    }
    Ok(HeartbeatRequest::decode(normalized.as_slice())?)
}

fn encode_legacy_heartbeat(mut request: HeartbeatRequest) -> bytes::Bytes {
    let local = request.support_local_configs;
    request.support_local_configs = false;
    // Old Custom servers interpret tag 10 as bool, never as a UUID list.
    request.failed_network_instances.clear();
    request.support_heartbeat_policy = false;
    let mut bytes = request.encode_to_vec();
    if local {
        encoding::bool::encode(10, &true, &mut bytes);
    }
    bytes.into()
}

/// Client factory for explicitly configured pre-migration Custom servers.
pub struct LegacyWebServerServiceClientFactory<C>(std::marker::PhantomData<C>);

impl<C> Clone for LegacyWebServerServiceClientFactory<C> {
    fn clone(&self) -> Self {
        Self(std::marker::PhantomData)
    }
}

#[derive(Clone)]
struct LegacyHeartbeatHandler<H>(H);

#[async_trait::async_trait]
impl<H> crate::rpc_types::handler::Handler for LegacyHeartbeatHandler<H>
where
    H: crate::rpc_types::handler::Handler<Descriptor = WebServerServiceDescriptor>,
{
    type Descriptor = WebServerServiceDescriptor;
    type Controller = H::Controller;
    async fn call(
        &self,
        ctrl: H::Controller,
        method: WebServerServiceMethodDescriptor,
        input: bytes::Bytes,
    ) -> crate::rpc_types::error::Result<bytes::Bytes> {
        let input = if method == WebServerServiceMethodDescriptor::Heartbeat {
            encode_legacy_heartbeat(HeartbeatRequest::decode(input)?)
        } else {
            input
        };
        self.0.call(ctrl, method, input).await
    }
}

impl<C: crate::rpc_types::controller::Controller> crate::rpc_types::__rt::RpcClientFactory
    for LegacyWebServerServiceClientFactory<C>
{
    type Descriptor = WebServerServiceDescriptor;
    type Controller = C;
    type ClientImpl = Box<dyn WebServerService<Controller = C> + Send + Sync + 'static>;
    fn new(
        handler: impl crate::rpc_types::handler::Handler<Descriptor = Self::Descriptor, Controller = C>,
    ) -> Self::ClientImpl {
        Box::new(WebServerServiceClient::new(LegacyHeartbeatHandler(handler)))
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[derive(Clone, PartialEq, Message)]
    struct LegacyHeartbeat {
        #[prost(bool, tag = "10")]
        support_local_configs: bool,
    }

    #[test]
    fn official_failures_and_custom_ownership_roundtrip_together() {
        let req = HeartbeatRequest {
            failed_network_instances: vec![uuid::Uuid::new_v4().into()],
            support_local_configs: true,
            support_heartbeat_policy: true,
            ..Default::default()
        };
        assert_eq!(
            decode_heartbeat_request(req.encode_to_vec().into()).unwrap(),
            req
        );
        let mut mixed = req.encode_to_vec();
        encoding::bool::encode(10, &true, &mut mixed);
        assert!(decode_heartbeat_request(mixed.into()).is_err());
    }

    #[derive(Clone)]
    struct Server;

    #[async_trait::async_trait]
    impl WebServerService for Server {
        type Controller = crate::rpc_types::controller::BaseController;
        async fn heartbeat(
            &self,
            _: Self::Controller,
            req: HeartbeatRequest,
        ) -> crate::rpc_types::error::Result<HeartbeatResponse> {
            assert!(req.support_local_configs);
            assert!(req.failed_network_instances.is_empty());
            Ok(HeartbeatResponse::default())
        }
        async fn get_feature(
            &self,
            _: Self::Controller,
            _: GetFeatureRequest,
        ) -> crate::rpc_types::error::Result<GetFeatureResponse> {
            Ok(GetFeatureResponse::default())
        }
    }

    #[tokio::test]
    async fn generated_rpc_dispatch_uses_compatibility_decoder() {
        let client = <LegacyWebServerServiceClientFactory<
            crate::rpc_types::controller::BaseController,
        > as crate::rpc_types::__rt::RpcClientFactory>::new(
            WebServerServiceServer::new(Server)
        );
        let request = HeartbeatRequest {
            support_local_configs: true,
            failed_network_instances: vec![uuid::Uuid::new_v4().into()],
            ..Default::default()
        };
        assert!(
            LegacyHeartbeat::decode(encode_legacy_heartbeat(request.clone()))
                .unwrap()
                .support_local_configs
        );
        client.heartbeat(Default::default(), request).await.unwrap();
    }
}
