//! URL disguise parameters and the HTTP Upgrade request target.

use std::{net::Ipv6Addr, time::Duration};

use crate::tunnel::insecure_tls::sni_from_url;
use crate::tunnel::traffic_disguise::TrafficStyle;

const DEFAULT_BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const DEFAULT_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9,en;q=0.8";
const DISGUISE_QUERY_KEYS: [&str; 9] = [
    "sni",
    "host",
    "path",
    "ua",
    "accept_language",
    "accept",
    "cache_control",
    "padding",
    "traffic_disguise",
];

/// Hard bounds for the `padding=<interval_ms>,<size>` query parameter. The
/// URL is attacker-controllable (it can originate from a peer-broadcast
/// listener), so both values must be clamped before use: an unbounded size
/// would drive periodic oversized allocations and base64 encodings, and a
/// near-zero interval would flood the link with padding frames.
const MIN_PADDING_INTERVAL: Duration = Duration::from_millis(100);
const MAX_PADDING_INTERVAL: Duration = Duration::from_secs(60);
const MAX_PADDING_SIZE: usize = 4096;
const DEFAULT_PADDING_SIZE: usize = 128;

/// Parse the independent HTTP Host value without allowing URL components or
/// silently discarding an explicit port. Bare IPv6 addresses retain the old
/// hostname-only behavior; an IPv6 authority with a port must use brackets.
fn request_authority(value: &str, inherited_port: Option<u16>) -> anyhow::Result<String> {
    let invalid = || anyhow::anyhow!("invalid disguise host {value:?}");
    anyhow::ensure!(
        !value.is_empty()
            && !value.chars().any(|character| {
                character.is_whitespace()
                    || character.is_control()
                    || matches!(character, '@' | '/' | '\\' | '?' | '#')
            }),
        "invalid disguise host {value:?}"
    );
    let port = |value: &str| -> anyhow::Result<u16> {
        anyhow::ensure!(
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
            "invalid disguise host port {value:?}"
        );
        value.parse().map_err(|_| invalid())
    };
    let (host, explicit_port) = if let Ok(address) = value.parse::<Ipv6Addr>() {
        (format!("[{address}]"), None)
    } else if value.starts_with('[') {
        let closing = value.find(']').ok_or_else(invalid)?;
        let (host, remainder) = value.split_at(closing + 1);
        let explicit_port = if remainder.is_empty() {
            None
        } else {
            Some(port(remainder.strip_prefix(':').ok_or_else(invalid)?)?)
        };
        (host.to_owned(), explicit_port)
    } else {
        match value.split_once(':') {
            Some((host, value)) => (host.to_owned(), Some(port(value)?)),
            None => (value.to_owned(), None),
        }
    };
    let host = url::Host::parse(&host).map_err(|_| invalid())?;
    let authority = match explicit_port.or(inherited_port) {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    // Keep the same authority syntax that the WebSocket request builder uses.
    let _: http::uri::Authority = authority.parse().map_err(|_| invalid())?;
    Ok(authority)
}

/// Obfuscation parameters for ws/wss tunnels, parsed from URL query params:
/// - `sni`: TLS SNI (and usually the Host header) presented to middleboxes
/// - `host`: Host authority (hostname/IP, optionally with a port), defaults to
///   the SNI or URL host; a hostname alone retains the URL's port
/// - `path`: request path, e.g. `/api/v1/query` to mimic an API endpoint
/// - `ua`: User-Agent header, defaults to a Chrome UA
/// - `accept_language`: Accept-Language header
/// - `accept` / `cache_control`: override the traffic style's HTTP headers
/// - `traffic_disguise`: speedtest/video defaults for an otherwise root path
/// - `padding=<interval_ms>[,<size>]`: send negotiated WebSocket text padding so
///   the flow looks like an interactive HTTPS session instead of a steady pipe
#[derive(Debug, Clone, Default)]
pub struct WssDisguise {
    enabled: bool,
    pub sni: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub user_agent: Option<String>,
    pub accept_language: Option<String>,
    pub accept: Option<String>,
    pub cache_control: Option<String>,
    traffic_style: Option<TrafficStyle>,
    pub padding_interval: Option<Duration>,
    pub padding_size: usize,
}

impl WssDisguise {
    pub fn from_url(url: &url::Url) -> Self {
        let mut disguise = WssDisguise {
            padding_size: DEFAULT_PADDING_SIZE,
            ..Default::default()
        };
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "traffic_disguise" => {
                    disguise.enabled = true;
                    disguise.traffic_style = Some(TrafficStyle::from_url(url));
                }
                // An empty `sni`/`host` value means "not set": using it
                // verbatim would break the TLS SNI and the Host header.
                "sni" if !value.is_empty() => {
                    disguise.enabled = true;
                    disguise.sni = Some(value.into_owned());
                }
                "host" if !value.is_empty() => {
                    disguise.enabled = true;
                    disguise.host = Some(value.into_owned());
                }
                "path" => {
                    disguise.enabled = true;
                    disguise.path = Some(value.into_owned());
                }
                "ua" => {
                    disguise.enabled = true;
                    disguise.user_agent = Some(value.into_owned());
                }
                "accept_language" => {
                    disguise.enabled = true;
                    disguise.accept_language = Some(value.into_owned());
                }
                "accept" => {
                    disguise.enabled = true;
                    disguise.accept = Some(value.into_owned());
                }
                "cache_control" => {
                    disguise.enabled = true;
                    disguise.cache_control = Some(value.into_owned());
                }
                "padding" => {
                    disguise.enabled = true;
                    let mut parts = value.split(',');
                    let interval_ms = parts.next().unwrap_or_default().parse::<u64>().ok();
                    if let Some(interval) = interval_ms
                        .filter(|ms| *ms > 0)
                        .map(Duration::from_millis)
                        .map(|interval| interval.clamp(MIN_PADDING_INTERVAL, MAX_PADDING_INTERVAL))
                    {
                        disguise.padding_interval = Some(interval);
                    } else if !value.is_empty() {
                        // Values like `padding=true` silently disable padding
                        // otherwise; the parameter is `<interval_ms>[,<size>]`.
                        tracing::warn!(%value, "ignoring malformed ws padding parameter; expected <interval_ms>[,<size>]");
                    }
                    if let Some(size) = parts.next().and_then(|size| size.parse::<usize>().ok())
                        && size > 0
                    {
                        disguise.padding_size = size.min(MAX_PADDING_SIZE);
                    }
                }
                _ => {}
            }
        }
        disguise
    }

    pub fn user_agent(&self) -> String {
        self.user_agent
            .clone()
            .unwrap_or_else(|| DEFAULT_BROWSER_UA.to_owned())
    }

    pub fn accept_language(&self) -> String {
        self.accept_language
            .clone()
            .unwrap_or_else(|| DEFAULT_ACCEPT_LANGUAGE.to_owned())
    }

    pub fn accept(&self) -> &str {
        self.accept.as_deref().unwrap_or(match self.traffic_style {
            Some(TrafficStyle::Speedtest) => "application/octet-stream,*/*;q=0.8",
            Some(TrafficStyle::Video) => "video/*,application/octet-stream;q=0.9,*/*;q=0.8",
            None => "*/*",
        })
    }

    pub fn cache_control(&self) -> Option<&str> {
        self.cache_control.as_deref().or(match self.traffic_style {
            Some(TrafficStyle::Speedtest) => Some("no-cache, no-store"),
            Some(TrafficStyle::Video) => Some("max-age=0"),
            None => None,
        })
    }

    /// SNI presented to the TLS layer. Defaults to the URL host, which keeps
    /// the official behavior when no `sni` param is configured. IP-literal
    /// hosts are stringified explicitly: `url.domain()` returns None for
    /// them, which used to degrade the SNI (and the Host header built from
    /// it) to "localhost". An explicitly configured but empty `sni` param is
    /// ignored, matching the http3 side.
    pub fn sni(&self, url: &url::Url) -> String {
        sni_from_url(url, self.sni.as_deref())
    }

    /// Builds the HTTP request target (scheme/host/path/query) for the
    /// WebSocket upgrade while stripping the disguise parameters themselves.
    pub fn request_uri(&self, url: &url::Url) -> anyhow::Result<String> {
        let host = self.host.clone().unwrap_or_else(|| self.sni(url));
        let authority = request_authority(&host, url.port())?;
        let path = self.path.clone().unwrap_or_else(|| {
            if url.path().is_empty() || url.path() == "/" {
                match self.traffic_style {
                    Some(TrafficStyle::Speedtest) => "/speedtest/download".to_owned(),
                    Some(TrafficStyle::Video) => "/video/live/stream".to_owned(),
                    None => "/".to_owned(),
                }
            } else {
                url.path().to_owned()
            }
        });
        let query: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| !DISGUISE_QUERY_KEYS.contains(&key.as_ref()))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let mut request_url = url.clone();
        request_url.set_path(&path);
        request_url.query_pairs_mut().clear();
        if !query.is_empty() {
            request_url.query_pairs_mut().extend_pairs(query);
        }
        // Replacing only the authority keeps the original dial URL and SNI
        // separate. Building this string after URL normalization also retains
        // an explicit default port (for example `host=front.example:443`).
        Ok(format!(
            "{}{}{}",
            &request_url[..url::Position::BeforeHost],
            authority,
            &request_url[url::Position::AfterPort..],
        ))
    }

    pub fn padding_enabled(&self) -> bool {
        self.padding_interval.is_some()
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traffic_styles_supply_distinct_default_request_templates() {
        for (style, path, accept, cache_control) in [
            (
                "speedtest",
                "/speedtest/download",
                "application/octet-stream,*/*;q=0.8",
                "no-cache, no-store",
            ),
            (
                "video",
                "/video/live/stream",
                "video/*,application/octet-stream;q=0.9,*/*;q=0.8",
                "max-age=0",
            ),
        ] {
            let url: url::Url = format!("wss://host.example/?traffic_disguise={style}&keep=1")
                .parse()
                .unwrap();
            let disguise = WssDisguise::from_url(&url);
            let uri: http::Uri = disguise.request_uri(&url).unwrap().parse().unwrap();
            assert_eq!(
                uri.path_and_query().unwrap().as_str(),
                format!("{path}?keep=1")
            );
            assert_eq!(disguise.accept(), accept);
            assert_eq!(disguise.cache_control(), Some(cache_control));
        }
    }

    #[test]
    fn explicit_paths_and_headers_override_traffic_style_defaults() {
        let mut url: url::Url = "wss://host.example/custom?traffic_disguise=video&keep=1"
            .parse()
            .unwrap();
        let disguise = WssDisguise::from_url(&url);
        let uri: http::Uri = disguise.request_uri(&url).unwrap().parse().unwrap();
        assert_eq!(uri.path_and_query().unwrap().as_str(), "/custom?keep=1");

        url.query_pairs_mut()
            .append_pair("path", "/explicit")
            .append_pair("ua", "custom player")
            .append_pair("accept", "application/custom")
            .append_pair("cache_control", "no-cache");
        let disguise = WssDisguise::from_url(&url);
        let uri: http::Uri = disguise.request_uri(&url).unwrap().parse().unwrap();
        assert_eq!(uri.path_and_query().unwrap().as_str(), "/explicit?keep=1");
        assert_eq!(disguise.user_agent(), "custom player");
        assert_eq!(disguise.accept(), "application/custom");
        assert_eq!(disguise.cache_control(), Some("no-cache"));
    }

    #[test]
    fn host_authority_preserves_explicit_or_inherited_port() {
        for (host, expected) in [
            ("front.example", "front.example:8443"),
            ("front.example:443", "front.example:443"),
            ("front.example:9443", "front.example:9443"),
            ("192.0.2.2", "192.0.2.2:8443"),
            ("192.0.2.2:8080", "192.0.2.2:8080"),
            ("[2001:db8::2]", "[2001:db8::2]:8443"),
            ("[2001:db8::2]:443", "[2001:db8::2]:443"),
            ("2001:db8::2", "[2001:db8::2]:8443"),
            ("[::1]:65535", "[::1]:65535"),
        ] {
            let mut url: url::Url = "wss://192.0.2.1:8443/original?keep=1".parse().unwrap();
            url.query_pairs_mut()
                .append_pair("host", host)
                .append_pair("sni", "tls.example")
                .append_pair("path", "/upgrade");
            let original = url.clone();
            let disguise = WssDisguise::from_url(&url);
            let uri: http::Uri = disguise.request_uri(&url).unwrap().parse().unwrap();
            assert_eq!(uri.authority().unwrap().as_str(), expected, "{host}");
            assert_eq!(uri.path_and_query().unwrap().as_str(), "/upgrade?keep=1");
            assert_eq!(disguise.sni(&url), "tls.example", "{host}");
            assert_eq!(url, original, "Host must not rewrite the dial URL");
        }
    }

    #[test]
    fn host_authority_without_a_port_keeps_the_default_port_implicit() {
        let mut url: url::Url = "wss://192.0.2.1/ws".parse().unwrap();
        url.query_pairs_mut().append_pair("host", "front.example");
        let uri: http::Uri = WssDisguise::from_url(&url)
            .request_uri(&url)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(uri.authority().unwrap().as_str(), "front.example");
    }

    #[test]
    fn host_authority_rejects_url_components_and_invalid_ports() {
        let url: url::Url = "wss://192.0.2.1:8443/ws".parse().unwrap();
        for host in [
            "",
            "front.example:",
            "front.example:abc",
            "front.example:-1",
            "front.example:+443",
            "front.example:65536",
            "front.example:443:80",
            "front.example/other",
            "front.example?query=1",
            "front.example#fragment",
            "user@front.example",
            "user:password@front.example",
            "https://front.example",
            "front.example\\other",
            "front example",
            "front.example\r\nX-Injected: 1",
            "front.example%2fother",
            "[::1",
            "[::1]:",
            "[::1]:wrong",
            "[::1]suffix",
            "[not-an-ip]:443",
        ] {
            let disguise = WssDisguise {
                host: Some(host.to_owned()),
                ..Default::default()
            };
            assert!(disguise.request_uri(&url).is_err(), "{host:?}");
        }
    }

    #[test]
    fn wss_disguise_sni_handles_ip_literal_hosts() {
        // url.domain() is None for IP literals; the SNI (and the Host header
        // built from it) must not degrade to "localhost" for direct-IP peers.
        let ipv4: url::Url = "wss://192.0.2.1:443/ws".parse().unwrap();
        assert_eq!(WssDisguise::from_url(&ipv4).sni(&ipv4), "192.0.2.1");

        let ipv6: url::Url = "wss://[2001:db8::1]:443/ws".parse().unwrap();
        assert_eq!(WssDisguise::from_url(&ipv6).sni(&ipv6), "2001:db8::1");
    }

    #[test]
    fn wss_disguise_empty_sni_param_falls_back_to_host() {
        let url: url::Url = "wss://www.example.com:443?sni=".parse().unwrap();
        let sni = WssDisguise::from_url(&url).sni(&url);
        assert_eq!(sni, "www.example.com");
        assert!(rustls::pki_types::ServerName::try_from(sni).is_ok());
    }

    #[test]
    fn wss_disguise_sni_param_overrides_host() {
        let url: url::Url = "wss://origin.example.com:443?sni=www.example.com"
            .parse()
            .unwrap();
        assert_eq!(WssDisguise::from_url(&url).sni(&url), "www.example.com");
    }

    #[test]
    fn wss_disguise_empty_host_param_is_ignored() {
        let url: url::Url = "wss://www.example.com:443?host=".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert!(disguise.host.is_none());
        assert_eq!(disguise.sni(&url), "www.example.com");
    }

    #[test]
    fn wss_disguise_request_uri_keeps_ip_literal_host() {
        let url: url::Url = "wss://192.0.2.1:443/ws?sni=".parse().unwrap();
        let uri = WssDisguise::from_url(&url).request_uri(&url).unwrap();
        // The disguise params must be stripped and the IP host preserved
        // (Url serialization may omit the wss default port).
        let parsed: url::Url = uri.parse().unwrap();
        assert_eq!(parsed.scheme(), "wss");
        assert_eq!(parsed.host_str(), Some("192.0.2.1"));
        assert_eq!(parsed.path(), "/ws");
        assert!(parsed.query().unwrap_or("").is_empty());
    }

    #[test]
    fn wss_disguise_request_uri_keeps_ipv6_literal_host() {
        // `Url::set_host` rejects a bare IPv6 literal when the first colon is
        // at index 0 and truncates one at the first colon otherwise, so both
        // loopback-style and global addresses must stay intact.
        for raw in [
            "wss://[::1]:443/ws?ua=x",
            "wss://[2001:db8::1]:8443/ws?padding=100,64",
        ] {
            let url: url::Url = raw.parse().unwrap();
            let uri = WssDisguise::from_url(&url).request_uri(&url).unwrap();
            let parsed: url::Url = uri.parse().unwrap();
            assert_eq!(parsed.scheme(), "wss", "{raw}");
            assert_eq!(
                parsed.host(),
                url.host(),
                "IPv6 literal host must round-trip unchanged: {raw}"
            );
            assert_eq!(parsed.path(), "/ws", "{raw}");
            assert!(parsed.query().unwrap_or("").is_empty(), "{raw}");
        }
    }

    #[test]
    fn wss_disguise_request_uri_brackets_bare_ipv6_host_param() {
        let url: url::Url = "wss://www.example.com:443/ws?host=fe80::1&ua=x"
            .parse()
            .unwrap();
        let uri = WssDisguise::from_url(&url).request_uri(&url).unwrap();
        let parsed: url::Url = uri.parse().unwrap();
        assert_eq!(parsed.host_str(), Some("[fe80::1]"));
    }

    #[test]
    fn wss_disguise_request_uri_applies_path_host_and_strips_params() {
        let url: url::Url =
            "wss://real.example.com:8443/token?sni=www.cloudflare.com&host=api.cloudflare.com&path=/api/v1/query&ua=Mozilla/5.0&padding=2000,256&keep=1"
                .parse()
                .unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert!(disguise.enabled());
        assert_eq!(disguise.sni(&url), "www.cloudflare.com");
        assert_eq!(disguise.user_agent(), "Mozilla/5.0");
        assert_eq!(
            disguise.padding_interval,
            Some(std::time::Duration::from_millis(2000))
        );
        assert_eq!(disguise.padding_size, 256);

        let request_uri = disguise.request_uri(&url).unwrap();
        let parsed: url::Url = request_uri.parse().unwrap();
        assert_eq!(parsed.scheme(), "wss");
        assert_eq!(parsed.host_str(), Some("api.cloudflare.com"));
        assert_eq!(parsed.path(), "/api/v1/query");
        let params: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(params, vec![("keep".to_string(), "1".to_string())]);
        assert!(!request_uri.contains("sni="));
        assert!(!request_uri.contains("ua="));
        assert!(!request_uri.contains("padding="));
    }

    #[test]
    fn wss_disguise_defaults_preserve_official_behavior() {
        let url: url::Url = "wss://host.example.com:443".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert!(!disguise.enabled());
        assert_eq!(disguise.sni(&url), "host.example.com");
        assert!(!disguise.padding_enabled());
        let request_uri = disguise.request_uri(&url).unwrap();
        let parsed: url::Url = request_uri.parse().unwrap();
        assert_eq!(parsed.path(), "/");
        assert_eq!(parsed.host_str(), Some("host.example.com"));
    }

    #[test]
    fn wss_disguise_padding_params_are_clamped_to_safe_bounds() {
        // Padding URLs can come from peer-broadcast listener info, so a
        // malicious peer could advertise padding=1,4000000000 to make this
        // node allocate huge buffers every millisecond. Both values must be
        // clamped to sane bounds.
        let url: url::Url = "wss://h/?padding=1,4000000000".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert_eq!(disguise.padding_interval, Some(MIN_PADDING_INTERVAL));
        assert_eq!(disguise.padding_size, MAX_PADDING_SIZE);

        // An absurdly large interval is harmless but still clamped.
        let url: url::Url = "wss://h/?padding=86400000".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert_eq!(disguise.padding_interval, Some(MAX_PADDING_INTERVAL));
        assert_eq!(disguise.padding_size, DEFAULT_PADDING_SIZE);

        // Values inside the bounds pass through unchanged.
        let url: url::Url = "wss://h/?padding=250,512".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert_eq!(disguise.padding_interval, Some(Duration::from_millis(250)));
        assert_eq!(disguise.padding_size, 512);

        // Zero interval still disables padding entirely.
        let url: url::Url = "wss://h/?padding=0,512".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert!(!disguise.padding_enabled());
    }
}
