use anyhow::{Context, Result};
use hickory_proto::rr;
use hickory_proto::rr::LowerName;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::name_server::GenericConnector;
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_server::ServerFuture;
use hickory_server::authority::{AuthorityObject, Catalog, ZoneType};
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::store::forwarder::ForwardConfig;
use hickory_server::store::{forwarder::ForwardAuthority, in_memory::InMemoryAuthority};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{RwLock, RwLockReadGuard};

use crate::common::dns::{get_default_resolver_config, policy_forwarder_resolver_config};

use super::config::{GeneralConfig, Record, RunConfig};

pub struct Server {
    server: ServerFuture<CatalogRequestHandler>,
    catalog: Arc<RwLock<Catalog>>,
    general_config: GeneralConfig,
    udp_local_addr: Option<SocketAddr>,
}

struct CatalogRequestHandler {
    catalog: Arc<RwLock<Catalog>>,
}

impl CatalogRequestHandler {
    fn new(catalog: Arc<RwLock<Catalog>>) -> CatalogRequestHandler {
        // let system_conf = read_system_conf();
        // let recursor = match system_conf {
        //     Ok((conf, _)) => RecursorBuilder::default().build(conf),
        //     Err(_) => RecursorBuilder::default().build(get_default_resolver_config()),
        // }
        // // policy is security unware, this will never return an error
        // .unwrap();

        Self { catalog }
    }
}

#[async_trait::async_trait]
impl RequestHandler for CatalogRequestHandler {
    async fn handle_request<R: ResponseHandler>(
        &self,
        request: &Request,
        response_handle: R,
    ) -> ResponseInfo {
        self.catalog
            .read()
            .await
            .handle_request(request, response_handle)
            .await
    }
}

pub fn build_authority(domain: &str, records: &[Record]) -> Result<InMemoryAuthority> {
    let zone = rr::Name::from_str(domain)?;
    let mut authority = InMemoryAuthority::empty(zone, ZoneType::Primary, false);
    for record in records.iter() {
        let r = record.try_into()?;
        authority.upsert_mut(r, 0);
    }
    Ok(authority)
}

fn forward_config(
    config: ResolverConfig,
    options: ResolverOpts,
    excluded: &[std::net::IpAddr],
) -> ForwardConfig {
    let filter = |server: &&hickory_resolver::config::NameServerConfig| {
        !excluded.contains(&server.socket_addr.ip().to_canonical())
    };
    let mut servers = config
        .name_servers()
        .iter()
        .filter(filter)
        .cloned()
        .collect::<Vec<_>>();
    if servers.is_empty() {
        servers.extend(
            get_default_resolver_config()
                .name_servers()
                .iter()
                .filter(filter)
                .cloned(),
        );
    }
    ForwardConfig {
        name_servers: servers.into(),
        options: Some(options),
    }
}

impl Server {
    #[cfg(test)]
    pub fn new(config: RunConfig) -> Self {
        Self::try_new(config, None).unwrap()
    }

    pub(super) fn with_policy(
        config: RunConfig,
        plane: Arc<easytier_core::instance::CorePacketPlane>,
        context: easytier_core::socket::SocketContext,
    ) -> Result<Self> {
        Self::try_new(config, Some((plane, context)))
    }

    fn try_new(
        config: RunConfig,
        policy: Option<(
            Arc<easytier_core::instance::CorePacketPlane>,
            easytier_core::socket::SocketContext,
        )>,
    ) -> Result<Self> {
        let mut catalog = Catalog::new();
        for (domain, records) in config.zones().iter() {
            let zone = rr::Name::from_str(domain.as_str())?;
            let authroty = build_authority(domain, records)?;
            catalog.upsert(zone.clone().into(), vec![Arc::new(authroty)]);
        }

        // use forwarder authority for the root zone
        let system_conf =
            policy_forwarder_resolver_config(policy.as_ref().map(|(_, context)| context));
        let forward_config = forward_config(
            system_conf.0,
            system_conf.1,
            config.excluded_forward_nameservers(),
        );
        let auth: Arc<dyn AuthorityObject> = if let Some((plane, context)) = policy {
            plane.update_vpn_dns_servers(
                forward_config
                    .name_servers
                    .iter()
                    .map(|server| server.socket_addr.ip())
                    .collect(),
            );
            let provider =
                GenericConnector::new(super::policy_io::PolicyDnsIo::new(plane, context));
            Arc::new(
                ForwardAuthority::builder_with_config(forward_config, provider)
                    .build()
                    .map_err(anyhow::Error::msg)?,
            )
        } else {
            Arc::new(
                ForwardAuthority::builder_with_config(
                    forward_config,
                    TokioConnectionProvider::default(),
                )
                .build()
                .map_err(anyhow::Error::msg)?,
            )
        };

        catalog.upsert(rr::Name::from_str(".")?.into(), vec![auth]);

        let catalog = Arc::new(RwLock::new(catalog));
        let handler = CatalogRequestHandler::new(catalog.clone());
        let server = ServerFuture::new(handler);

        Ok(Self {
            server,
            catalog,
            general_config: config.general().clone(),
            udp_local_addr: None,
        })
    }

    #[cfg(test)]
    pub fn udp_local_addr(&self) -> Option<SocketAddr> {
        self.udp_local_addr
    }

    pub async fn register_udp_socket(&mut self, address: String) -> Result<SocketAddr> {
        let bind_addr = SocketAddr::from_str(&address)
            .with_context(|| format!("DNS Server failed to parse address {}", address))?;
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .with_context(|| {
            format!(
                "DNS Server failed to create UDP socket for address {}",
                address
            )
        })?;
        socket2::SockRef::from(&socket)
            .set_reuse_address(true)
            .with_context(|| {
                format!(
                    "DNS Server failed to set reuse address on socket {}",
                    address
                )
            })?;
        socket.bind(&bind_addr.into()).with_context(|| {
            format!("DNS Server failed to bind socket to address {}", bind_addr)
        })?;
        socket
            .set_nonblocking(true)
            .with_context(|| "DNS Server failed to set socket to non-blocking".to_string())?;
        let socket = UdpSocket::from_std(socket.into()).with_context(|| {
            format!(
                "DNS Server failed to convert socket to UdpSocket for address {}",
                address
            )
        })?;

        let local_addr = socket
            .local_addr()
            .with_context(|| "DNS Server failed to get local address".to_string())?;
        self.server.register_socket(socket);

        Ok(local_addr)
    }

    pub async fn run(&mut self) -> Result<()> {
        if let Some(address) = self.general_config.listen_tcp() {
            let tcp_listener = TcpListener::bind(address.clone())
                .await
                .with_context(|| format!("DNS Server failed to bind TCP address {}", address))?;
            self.server
                .register_listener(tcp_listener, Duration::from_secs(5));
        }

        if let Some(address) = self.general_config.listen_udp() {
            let local_addr = self.register_udp_socket(address.clone()).await?;
            self.udp_local_addr = Some(local_addr);
        };

        Ok(())
    }

    #[cfg(test)]
    pub async fn shutdown(&mut self) -> Result<()> {
        self.server.shutdown_gracefully().await?;
        Ok(())
    }

    pub async fn upsert(&self, name: LowerName, authority: Arc<dyn AuthorityObject>) {
        self.catalog.write().await.upsert(name, vec![authority]);
    }

    pub async fn read_catalog(&self) -> RwLockReadGuard<'_, Catalog> {
        self.catalog.read().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::dns_server::config::{
        GeneralConfigBuilder, RecordBuilder, RecordType, RunConfigBuilder,
    };
    use anyhow::Result;
    use hickory_client::client::{Client, ClientHandle};
    use hickory_proto::rr;
    use hickory_proto::runtime::TokioRuntimeProvider;
    use hickory_proto::udp::UdpClientStream;
    use maplit::hashmap;
    use std::time::Duration;

    #[test]
    fn policy_forwarder_configuration_retains_upstreams_and_survives_only_magic_dns() {
        use hickory_proto::xfer::Protocol;
        use hickory_resolver::config::NameServerConfig;
        let fake = "100.100.100.101".parse::<std::net::IpAddr>().unwrap();
        let mut resolver = ResolverConfig::new();
        resolver.add_name_server(NameServerConfig::new(
            "100.100.100.101:53".parse().unwrap(),
            Protocol::Udp,
        ));
        resolver.add_name_server(NameServerConfig::new(
            "[::ffff:100.100.100.101]:53".parse().unwrap(),
            Protocol::Udp,
        ));
        let fallback = forward_config(resolver.clone(), ResolverOpts::default(), &[fake]);
        assert_eq!(
            fallback
                .name_servers
                .iter()
                .map(|server| server.socket_addr)
                .collect::<Vec<_>>(),
            get_default_resolver_config()
                .name_servers()
                .iter()
                .map(|server| server.socket_addr)
                .collect::<Vec<_>>()
        );
        ForwardAuthority::builder_with_config(fallback, TokioConnectionProvider::default())
            .build()
            .unwrap();
        resolver.add_name_server(NameServerConfig::new(
            "192.168.1.1:53".parse().unwrap(),
            Protocol::Udp,
        ));
        let preserved = forward_config(resolver, ResolverOpts::default(), &[fake]);
        assert_eq!(
            preserved
                .name_servers
                .iter()
                .map(|server| server.socket_addr)
                .collect::<Vec<_>>(),
            ["192.168.1.1:53".parse::<SocketAddr>().unwrap()]
        );
    }

    #[tokio::test]
    async fn it_works() -> Result<()> {
        let mut server = Server::new(
            RunConfigBuilder::default()
                .general(GeneralConfigBuilder::default().build()?)
                .build()?,
        );
        server.run().await?;
        server.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn can_resolve_records() -> Result<()> {
        let configured_record = RecordBuilder::default()
            .rr_type(RecordType::A)
            .name("www.et.internal.".to_string())
            .value("123.123.123.123".to_string())
            .ttl(Duration::from_secs(60))
            .build()?;
        let configured_record2 = RecordBuilder::default()
            .rr_type(RecordType::A)
            .name("中文.et.internal.".to_string())
            .value("123.123.123.123".to_string())
            .ttl(Duration::from_secs(60))
            .build()?;
        let soa_record = RecordBuilder::default()
            .rr_type(RecordType::SOA)
            .name("et.internal.".to_string())
            .value(
                "ns.et.internal. hostmaster.et.internal. 2023101001 7200 3600 1209600 86400"
                    .to_string(),
            )
            .ttl(Duration::from_secs(60))
            .build()?;
        let config = RunConfigBuilder::default()
            .general(
                GeneralConfigBuilder::default()
                    .listen_udp("127.0.0.1:0")
                    .build()?,
            )
            .zones(hashmap! {
                "et.internal.".to_string() => vec![configured_record.clone(), soa_record.clone(), configured_record2.clone()],
            })
            .build()?;

        let mut server = Server::new(config);
        server.run().await?;

        let local_addr = server.udp_local_addr().unwrap();
        let stream = UdpClientStream::builder(local_addr, TokioRuntimeProvider::default()).build();
        let (mut client, background) = Client::connect(stream).await?;
        let background_task = tokio::spawn(background);
        let response = client
            .query(
                rr::Name::from_str("www.et.internal")?,
                rr::DNSClass::IN,
                rr::RecordType::A,
            )
            .await?;
        drop(background_task);

        println!("Response: {:?}", response);

        assert_eq!(response.answers().len(), 1);
        let expected_record: rr::Record = configured_record.try_into()?;
        assert_eq!(response.answers().first().unwrap(), &expected_record);

        server.shutdown().await?;
        Ok(())
    }
}
