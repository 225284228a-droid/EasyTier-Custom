use std::net::IpAddr;

use chrono::{DateTime, Utc};
use reqwest::{Response, StatusCode, header};
use serde::Deserialize;

use super::CityLocation;

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const DEFAULT_RATE_LIMIT_SECONDS: i64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug)]
pub(super) enum Provider {
    GeoJs,
    IpWhoIs,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ProviderError {
    #[error("geolocation provider is rate limited until {retry_at}")]
    RateLimited { retry_at: i64 },
    #[error("geolocation provider is unavailable")]
    Unavailable,
}

impl Provider {
    pub(super) fn source(self) -> &'static str {
        match self {
            Self::GeoJs => "geojs",
            Self::IpWhoIs => "ipwho.is",
        }
    }

    fn endpoint(self, ip: IpAddr) -> String {
        let ip = ip.to_canonical();
        match self {
            Self::GeoJs => format!("https://get.geojs.io/v1/ip/geo/{ip}.json"),
            Self::IpWhoIs => format!(
                "https://ipwho.is/{ip}?fields=ip,success,message,country,city,region,latitude,longitude"
            ),
        }
    }

    pub(super) async fn fetch(
        self,
        client: &reqwest::Client,
        ip: IpAddr,
    ) -> Result<CityLocation, ProviderError> {
        let response = client
            .get(self.endpoint(ip))
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        self.parse_response(response, ip).await
    }

    #[cfg(test)]
    pub(super) async fn fetch_at(
        self,
        client: &reqwest::Client,
        ip: IpAddr,
        url: url::Url,
    ) -> Result<CityLocation, ProviderError> {
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        self.parse_response(response, ip).await
    }

    async fn parse_response(
        self,
        mut response: Response,
        ip: IpAddr,
    ) -> Result<CityLocation, ProviderError> {
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited {
                retry_at: retry_at(response.headers(), Utc::now().timestamp()),
            });
        }
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ProviderError::Unavailable);
        }

        // Bound streamed bodies too; Content-Length is optional and untrusted.
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ProviderError::Unavailable)?
        {
            if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err(ProviderError::Unavailable);
            }
            body.extend_from_slice(&chunk);
        }
        self.parse_body(&body, ip)
    }

    fn parse_body(self, body: &[u8], ip: IpAddr) -> Result<CityLocation, ProviderError> {
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(ProviderError::Unavailable);
        }
        let location = match self {
            Self::GeoJs => {
                let response: GeoJsResponse =
                    serde_json::from_slice(body).map_err(|_| ProviderError::Unavailable)?;
                ensure_matching_ip(&response.ip, ip)?;
                CityLocation {
                    country: required_place(&response.country)?,
                    city: required_place(&response.city)?,
                    region: response.region.as_deref().and_then(optional_place),
                    latitude: response
                        .latitude
                        .trim()
                        .parse()
                        .map_err(|_| ProviderError::Unavailable)?,
                    longitude: response
                        .longitude
                        .trim()
                        .parse()
                        .map_err(|_| ProviderError::Unavailable)?,
                    accuracy_radius_km: response.accuracy,
                }
            }
            Self::IpWhoIs => {
                let response: IpWhoIsResponse =
                    serde_json::from_slice(body).map_err(|_| ProviderError::Unavailable)?;
                if !response.success {
                    return Err(ProviderError::Unavailable);
                }
                ensure_matching_ip(&response.ip, ip)?;
                CityLocation {
                    country: required_place(&response.country)?,
                    city: required_place(&response.city)?,
                    region: response.region.as_deref().and_then(optional_place),
                    latitude: response.latitude,
                    longitude: response.longitude,
                    accuracy_radius_km: None,
                }
            }
        };
        if !location.latitude.is_finite()
            || !(-90.0..=90.0).contains(&location.latitude)
            || !location.longitude.is_finite()
            || !(-180.0..=180.0).contains(&location.longitude)
            || location
                .accuracy_radius_km
                .is_some_and(|radius| !radius.is_finite() || radius < 0.0)
        {
            return Err(ProviderError::Unavailable);
        }
        Ok(location)
    }
}

#[derive(Deserialize)]
struct GeoJsResponse {
    ip: String,
    country: String,
    city: String,
    region: Option<String>,
    latitude: String,
    longitude: String,
    accuracy: Option<f64>,
}

#[derive(Deserialize)]
struct IpWhoIsResponse {
    ip: String,
    success: bool,
    country: String,
    city: String,
    region: Option<String>,
    latitude: f64,
    longitude: f64,
}

fn ensure_matching_ip(value: &str, expected: IpAddr) -> Result<(), ProviderError> {
    let actual: IpAddr = value
        .trim()
        .parse()
        .map_err(|_| ProviderError::Unavailable)?;
    if actual.to_canonical() != expected.to_canonical() {
        return Err(ProviderError::Unavailable);
    }
    Ok(())
}

fn required_place(value: &str) -> Result<String, ProviderError> {
    optional_place(value).ok_or(ProviderError::Unavailable)
}

fn optional_place(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && !value.eq_ignore_ascii_case("unknown")
        && value.chars().count() <= 128
        && !value.chars().any(char::is_control))
    .then(|| value.to_owned())
}

fn retry_at(headers: &header::HeaderMap, now: i64) -> i64 {
    let fallback = now.saturating_add(DEFAULT_RATE_LIMIT_SECONDS);
    let Some(value) = headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    else {
        return fallback;
    };
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        if let Ok(seconds) = value.parse::<u64>() {
            return now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX));
        }
    }
    DateTime::parse_from_rfc2822(value)
        .map(|date| date.timestamp().max(now))
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{Router, body::Body, response::Response as AxumResponse, routing::get};
    use serde_json::{Value, json};
    use tokio_util::task::AbortOnDropHandle;

    use super::*;

    fn ip() -> IpAddr {
        "1.1.1.1".parse().unwrap()
    }

    fn geojs_body() -> Value {
        json!({
            "ip": "1.1.1.1",
            "country": " Australia ",
            "city": " Sydney ",
            "region": " New South Wales ",
            "latitude": "-33.8688",
            "longitude": "151.2093",
            "accuracy": 20
        })
    }

    fn ipwhois_body() -> Value {
        json!({
            "ip": "1.1.1.1",
            "success": true,
            "country": "Australia",
            "city": "Sydney",
            "region": "New South Wales",
            "latitude": -33.8688,
            "longitude": 151.2093
        })
    }

    fn parse(provider: Provider, body: Value) -> Result<CityLocation, ProviderError> {
        provider.parse_body(&serde_json::to_vec(&body).unwrap(), ip())
    }

    fn assert_unavailable(result: Result<CityLocation, ProviderError>) {
        assert!(matches!(result, Err(ProviderError::Unavailable)));
    }

    fn oversized_geojs_body() -> String {
        let mut bytes = serde_json::to_vec(&geojs_body()).unwrap();
        bytes.resize(MAX_RESPONSE_BYTES + 1, b' ');
        String::from_utf8(bytes).unwrap()
    }

    async fn mock(
        status: StatusCode,
        retry_after: Option<&str>,
        body: String,
    ) -> (url::Url, AbortOnDropHandle<()>) {
        let retry_after = retry_after.map(str::to_owned);
        let app = Router::new().route(
            "/",
            get(move || {
                let body = body.clone();
                let retry_after = retry_after.clone();
                async move {
                    let mut response = AxumResponse::builder().status(status);
                    if let Some(value) = retry_after {
                        response = response.header(header::RETRY_AFTER, value);
                    }
                    response.body(Body::from(body)).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        (format!("http://{address}/").parse().unwrap(), task)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap()
    }

    #[test]
    fn fixed_endpoints_always_include_requested_ip() {
        assert_eq!(Provider::GeoJs.source(), "geojs");
        assert_eq!(Provider::IpWhoIs.source(), "ipwho.is");
        assert_eq!(
            Provider::GeoJs.endpoint(ip()),
            "https://get.geojs.io/v1/ip/geo/1.1.1.1.json"
        );
        assert_eq!(
            Provider::IpWhoIs.endpoint(ip()),
            "https://ipwho.is/1.1.1.1?fields=ip,success,message,country,city,region,latitude,longitude"
        );
        let ipv6 = "2606:4700:4700::1111".parse().unwrap();
        assert_eq!(
            Provider::GeoJs.endpoint(ipv6),
            "https://get.geojs.io/v1/ip/geo/2606:4700:4700::1111.json"
        );
        assert_eq!(
            Provider::IpWhoIs.endpoint(ipv6),
            "https://ipwho.is/2606:4700:4700::1111?fields=ip,success,message,country,city,region,latitude,longitude"
        );
    }

    #[test]
    fn parses_both_providers_without_inventing_accuracy() {
        let expected = CityLocation {
            country: "Australia".to_owned(),
            city: "Sydney".to_owned(),
            region: Some("New South Wales".to_owned()),
            latitude: -33.8688,
            longitude: 151.2093,
            accuracy_radius_km: Some(20.0),
        };
        assert_eq!(parse(Provider::GeoJs, geojs_body()).unwrap(), expected);
        assert_eq!(
            parse(Provider::IpWhoIs, ipwhois_body()).unwrap(),
            CityLocation {
                accuracy_radius_km: None,
                ..expected
            }
        );
    }

    #[test]
    fn normalizes_ipv6_and_ipv4_mapped_ip_echoes() {
        let ipv6 = "2606:4700:4700::1111".parse().unwrap();
        for (provider, mut body) in [
            (Provider::GeoJs, geojs_body()),
            (Provider::IpWhoIs, ipwhois_body()),
        ] {
            body["ip"] = json!("2606:4700:4700:0000:0000:0000:0000:1111");
            assert!(
                provider
                    .parse_body(&serde_json::to_vec(&body).unwrap(), ipv6)
                    .is_ok()
            );
            body["ip"] = json!("::ffff:1.1.1.1");
            assert!(parse(provider, body).is_ok());
        }
    }

    #[test]
    fn rejects_missing_malformed_or_mismatched_ip() {
        for (provider, body) in [
            (Provider::GeoJs, geojs_body()),
            (Provider::IpWhoIs, ipwhois_body()),
        ] {
            for value in [json!("223.5.5.5"), json!("invalid"), json!(""), Value::Null] {
                let mut invalid = body.clone();
                invalid["ip"] = value;
                assert_unavailable(parse(provider, invalid));
            }
            let mut missing = body;
            missing.as_object_mut().unwrap().remove("ip");
            assert_unavailable(parse(provider, missing));
        }
    }

    #[test]
    fn requires_a_real_country_and_city() {
        for (provider, body) in [
            (Provider::GeoJs, geojs_body()),
            (Provider::IpWhoIs, ipwhois_body()),
        ] {
            for field in ["city", "country"] {
                for value in [json!(""), json!("   "), json!(" Unknown "), Value::Null] {
                    let mut invalid = body.clone();
                    invalid[field] = value;
                    assert_unavailable(parse(provider, invalid));
                }
                let mut missing = body.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert_unavailable(parse(provider, missing));
            }
        }
    }

    #[test]
    fn optional_region_is_not_invented() {
        for (provider, mut body) in [
            (Provider::GeoJs, geojs_body()),
            (Provider::IpWhoIs, ipwhois_body()),
        ] {
            body.as_object_mut().unwrap().remove("region");
            assert_eq!(parse(provider, body.clone()).unwrap().region, None);
            body["region"] = json!(" unknown ");
            assert_eq!(parse(provider, body).unwrap().region, None);
        }
    }

    #[test]
    fn rejects_invalid_geojs_coordinates_and_accuracy() {
        for (field, value) in [
            ("latitude", json!("90.0001")),
            ("latitude", json!("-90.0001")),
            ("latitude", json!("NaN")),
            ("latitude", json!("inf")),
            ("latitude", json!("invalid")),
            ("longitude", json!("180.0001")),
            ("longitude", json!("-180.0001")),
            ("longitude", json!("-inf")),
            ("longitude", Value::Null),
            ("accuracy", json!(-1)),
        ] {
            let mut body = geojs_body();
            body[field] = value;
            assert_unavailable(parse(Provider::GeoJs, body));
        }
    }

    #[test]
    fn rejects_invalid_ipwhois_coordinates_and_failed_status() {
        for (field, value) in [
            ("latitude", json!(90.0001)),
            ("latitude", json!(-90.0001)),
            ("latitude", json!("NaN")),
            ("longitude", json!(180.0001)),
            ("longitude", json!(-180.0001)),
            ("longitude", Value::Null),
            ("success", json!(false)),
            ("success", Value::Null),
        ] {
            let mut body = ipwhois_body();
            body[field] = value;
            assert_unavailable(parse(Provider::IpWhoIs, body));
        }
    }

    #[test]
    fn accepts_coordinate_boundaries_and_missing_accuracy() {
        let mut geojs = geojs_body();
        geojs["latitude"] = json!("-90");
        geojs["longitude"] = json!("180");
        geojs.as_object_mut().unwrap().remove("accuracy");
        let location = parse(Provider::GeoJs, geojs).unwrap();
        assert_eq!(location.latitude, -90.0);
        assert_eq!(location.longitude, 180.0);
        assert_eq!(location.accuracy_radius_km, None);
        let mut ipwhois = ipwhois_body();
        ipwhois["latitude"] = json!(90);
        ipwhois["longitude"] = json!(-180);
        assert!(parse(Provider::IpWhoIs, ipwhois).is_ok());
    }

    #[test]
    fn rejects_oversized_or_malformed_json() {
        for (provider, body) in [
            (Provider::GeoJs, geojs_body()),
            (Provider::IpWhoIs, ipwhois_body()),
        ] {
            let mut bytes = serde_json::to_vec(&body).unwrap();
            bytes.resize(MAX_RESPONSE_BYTES + 1, b' ');
            assert_unavailable(provider.parse_body(&bytes, ip()));
            assert_unavailable(provider.parse_body(b"not json", ip()));
            assert_unavailable(provider.parse_body(b"[]", ip()));
        }
    }

    #[test]
    fn accepts_json_at_the_exact_size_limit() {
        for (provider, body) in [
            (Provider::GeoJs, geojs_body()),
            (Provider::IpWhoIs, ipwhois_body()),
        ] {
            let mut bytes = serde_json::to_vec(&body).unwrap();
            bytes.resize(MAX_RESPONSE_BYTES, b' ');
            assert!(provider.parse_body(&bytes, ip()).is_ok());
        }
    }

    #[test]
    fn retry_after_supports_seconds_dates_and_default() {
        let now = 1_791_244_800;
        let mut headers = header::HeaderMap::new();
        assert_eq!(retry_at(&headers, now), now + DEFAULT_RATE_LIMIT_SECONDS);
        for (value, expected) in [
            ("120", now + 120),
            ("0", now),
            (" 120 ", now + 120),
            ("invalid", now + DEFAULT_RATE_LIMIT_SECONDS),
            ("-1", now + DEFAULT_RATE_LIMIT_SECONDS),
            ("", now + DEFAULT_RATE_LIMIT_SECONDS),
            ("18446744073709551615", i64::MAX),
        ] {
            headers.insert(header::RETRY_AFTER, value.parse().unwrap());
            assert_eq!(retry_at(&headers, now), expected);
        }
        let future = DateTime::from_timestamp(now + 2 * DEFAULT_RATE_LIMIT_SECONDS, 0).unwrap();
        headers.insert(header::RETRY_AFTER, future.to_rfc2822().parse().unwrap());
        assert_eq!(retry_at(&headers, now), future.timestamp());
        let past = DateTime::from_timestamp(now - 60, 0).unwrap();
        headers.insert(header::RETRY_AFTER, past.to_rfc2822().parse().unwrap());
        assert_eq!(retry_at(&headers, now), now);
        headers.insert(
            header::RETRY_AFTER,
            header::HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        assert_eq!(retry_at(&headers, now), now + DEFAULT_RATE_LIMIT_SECONDS);
    }

    #[tokio::test]
    async fn fetches_from_local_mock_for_both_providers() {
        let client = client();
        for (provider, body) in [
            (Provider::GeoJs, geojs_body()),
            (Provider::IpWhoIs, ipwhois_body()),
        ] {
            let (url, _server) = mock(StatusCode::OK, None, body.to_string()).await;
            let location = provider.fetch_at(&client, ip(), url).await.unwrap();
            assert_eq!(location.city, "Sydney");
        }
    }

    #[tokio::test]
    async fn http_failures_and_oversized_bodies_are_unavailable() {
        let client = client();
        for (status, body) in [
            (StatusCode::INTERNAL_SERVER_ERROR, geojs_body().to_string()),
            (StatusCode::FOUND, geojs_body().to_string()),
            (StatusCode::NO_CONTENT, String::new()),
            (StatusCode::OK, oversized_geojs_body()),
            (StatusCode::OK, r#"{"success":false}"#.to_owned()),
        ] {
            let (url, _server) = mock(status, None, body).await;
            assert_unavailable(Provider::GeoJs.fetch_at(&client, ip(), url.clone()).await);
            assert_unavailable(Provider::IpWhoIs.fetch_at(&client, ip(), url).await);
        }
    }

    #[tokio::test]
    async fn rejects_oversized_chunked_body_without_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let _server = AbortOnDropHandle::new(tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            socket.read(&mut request).await.unwrap();
            let body = oversized_geojs_body();
            let response = format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }));
        let url = format!("http://{address}/").parse().unwrap();
        assert_unavailable(Provider::GeoJs.fetch_at(&client(), ip(), url).await);
    }

    #[tokio::test]
    async fn http_rate_limits_preserve_retry_after() {
        let client = client();
        for delay in [None, Some("120")] {
            let (url, _server) = mock(StatusCode::TOO_MANY_REQUESTS, delay, String::new()).await;
            let before = Utc::now().timestamp();
            let expected_delay = if delay.is_some() {
                120
            } else {
                DEFAULT_RATE_LIMIT_SECONDS
            };
            match Provider::GeoJs.fetch_at(&client, ip(), url).await {
                Err(ProviderError::RateLimited { retry_at }) => {
                    assert!(retry_at >= before + expected_delay);
                    assert!(retry_at <= Utc::now().timestamp() + expected_delay);
                }
                result => panic!("expected rate limit, got {result:?}"),
            }
        }
        let future =
            DateTime::from_timestamp(Utc::now().timestamp() + 2 * DEFAULT_RATE_LIMIT_SECONDS, 0)
                .unwrap();
        let date = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let (url, _server) = mock(StatusCode::TOO_MANY_REQUESTS, Some(&date), String::new()).await;
        match Provider::IpWhoIs.fetch_at(&client, ip(), url).await {
            Err(ProviderError::RateLimited { retry_at }) => {
                assert_eq!(retry_at, future.timestamp());
            }
            result => panic!("expected rate limit, got {result:?}"),
        }
    }

    #[test]
    fn errors_never_include_the_requested_ip_or_response_body() {
        let error = parse(Provider::GeoJs, json!({"ip": "1.1.1.1"})).unwrap_err();
        assert_eq!(error.to_string(), "geolocation provider is unavailable");
        assert_eq!(format!("{error:?}"), "Unavailable");
    }
}
