//! The HTTP side: a GET with a size cap and errors phrased for the model.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, StatusCode, redirect};
use url::Url;

use crate::WebFetchConfig;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A fetched response body (possibly cut at the size cap).
#[derive(Debug)]
pub(crate) struct Fetched {
    pub final_url: Url,
    pub status: StatusCode,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    /// The download stopped at `max_bytes`.
    pub truncated: bool,
    /// Where a redirect to another site points; it was not followed.
    pub redirect_to: Option<Url>,
}

/// Builds the shared HTTP client from the config.
///
/// Redirects are followed only within the site the request started on: the permission
/// check approved that site, not wherever it redirects. Unless `allow_private_hosts` is
/// set, host names that resolve to private, loopback or link-local addresses are refused,
/// so a public name can't be pointed at the local network. A URL that names such an
/// address directly (an IP address or `localhost`) still works.
pub(crate) fn client(config: &WebFetchConfig) -> anyhow::Result<Client> {
    let max = config.max_redirects;
    let mut builder = Client::builder()
        .user_agent(config.user_agent.clone())
        .timeout(Duration::from_secs(config.timeout_secs.max(1)))
        .connect_timeout(Duration::from_secs(config.timeout_secs.clamp(1, 10)))
        .redirect(redirect::Policy::custom(move |attempt| {
            follow_same_site(attempt, max)
        }));
    if !config.allow_private_hosts {
        builder = builder.dns_resolver(Arc::new(PublicOnly));
    }
    Ok(builder.build()?)
}

fn follow_same_site(attempt: redirect::Attempt<'_>, max: usize) -> redirect::Action {
    if attempt.previous().len() > max {
        return attempt.error(TooManyRedirects(max));
    }
    match attempt.previous().first() {
        Some(first) if same_site(first, attempt.url()) => attempt.follow(),
        _ => attempt.stop(),
    }
}

/// Same origin, or the same host moving from http to https on the default ports.
pub(crate) fn same_site(from: &Url, to: &Url) -> bool {
    if from.origin() == to.origin() {
        return true;
    }
    from.scheme() == "http"
        && to.scheme() == "https"
        && from.host_str() == to.host_str()
        && from.port_or_known_default() == Some(80)
        && to.port_or_known_default() == Some(443)
}

#[derive(Debug)]
struct TooManyRedirects(usize);

impl std::fmt::Display for TooManyRedirects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "more than {} redirects", self.0)
    }
}

impl std::error::Error for TooManyRedirects {}

/// A host name resolved only to addresses web_fetch won't reach through a name.
#[derive(Debug)]
pub(crate) struct PrivateAddress {
    pub host: String,
    pub ip: Option<IpAddr>,
}

impl std::fmt::Display for PrivateAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ip {
            Some(ip) => write!(f, "{} resolves to the private address {ip}", self.host),
            None => write!(f, "{} resolves to no addresses", self.host),
        }
    }
}

impl std::error::Error for PrivateAddress {}

/// Resolves names to public addresses only (`localhost` excepted, since it names a
/// local address directly).
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = resolve_public(&host).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

pub(crate) async fn resolve_public(host: &str) -> Result<Vec<SocketAddr>, BoxError> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, 0)).await?.collect();
    let host_lc = host.trim_end_matches('.').to_ascii_lowercase();
    if host_lc == "localhost" || host_lc.ends_with(".localhost") {
        return Ok(addrs);
    }
    let public: Vec<SocketAddr> = addrs
        .iter()
        .copied()
        .filter(|a| is_public(a.ip()))
        .collect();
    if public.is_empty() {
        return Err(Box::new(PrivateAddress {
            host: host.to_string(),
            ip: addrs.first().map(SocketAddr::ip),
        }));
    }
    Ok(public)
}

/// Whether `ip` is a public unicast address.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || o[0] == 0
                // 100.64.0.0/10, carrier-grade NAT
                || (o[0] == 100 && (o[1] & 0xC0) == 64))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public(IpAddr::V4(v4)),
            None => {
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local())
            }
        },
    }
}

/// GETs `url`, reading at most `max_bytes` of the body. `Err` is a message for the model.
pub(crate) async fn get(
    client: &Client,
    url: &Url,
    max_bytes: usize,
    config: &WebFetchConfig,
) -> Result<Fetched, String> {
    let mut response = client
        .get(url.clone())
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,text/markdown,text/plain,application/json;q=0.9,*/*;q=0.5",
        )
        .send()
        .await
        .map_err(|e| describe_error(&e, url, config))?;
    let status = response.status();
    let final_url = response.url().clone();
    // A redirect the policy stopped: report it instead of reading the body.
    if status.is_redirection()
        && let Some(to) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|loc| final_url.join(loc).ok())
    {
        return Ok(Fetched {
            final_url,
            status,
            content_type: None,
            body: Vec::new(),
            truncated: false,
            redirect_to: Some(to),
        });
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| describe_error(&e, url, config))?
    {
        let room = max_bytes.saturating_sub(body.len());
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Fetched {
        final_url,
        status,
        content_type,
        body,
        truncated,
        redirect_to: None,
    })
}

/// Turns a transport error into a recoverable observation that says what to try.
fn describe_error(e: &reqwest::Error, url: &Url, config: &WebFetchConfig) -> String {
    let mut detail = e.to_string();
    let mut private = None;
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        detail.push_str(": ");
        detail.push_str(&s.to_string());
        if let Some(p) = s.downcast_ref::<PrivateAddress>() {
            private = Some(p.to_string());
        }
        source = s.source();
    }
    if let Some(why) = private {
        format!(
            "Not fetched: {why}. web_fetch reaches private and local addresses only when the \
             URL names them directly (an IP address or localhost), so a public-looking name \
             can't lead into the local network. If the user wants this host, ask them to set \
             plugins.tool-web-fetch.allow_private_hosts = true."
        )
    } else if e.is_timeout() {
        format!(
            "Fetching {url} timed out after {}s. The server may be slow or unreachable; \
             retry once, or try a different URL.",
            config.timeout_secs
        )
    } else if e.is_redirect() {
        format!(
            "Fetching {url} followed more than {} redirects. The URL may be a redirect \
             loop; try the final address directly. ({detail})",
            config.max_redirects
        )
    } else if e.is_connect() {
        format!(
            "Could not connect to {} (DNS lookup or connection failed). Check the host \
             name for typos; the site may be down or unreachable from here. ({detail})",
            url.host_str().unwrap_or("the host")
        )
    } else {
        format!("Fetching {url} failed: {detail}. Check the URL and try again.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn same_site_rules() {
        assert!(same_site(&url("https://a.dev/x"), &url("https://a.dev/y")));
        assert!(same_site(&url("http://a.dev/"), &url("https://a.dev/")));
        assert!(!same_site(&url("https://a.dev/"), &url("http://a.dev/")));
        assert!(!same_site(
            &url("https://a.dev/"),
            &url("https://www.a.dev/")
        ));
        assert!(!same_site(
            &url("https://a.dev/"),
            &url("https://a.dev:8443/")
        ));
        assert!(!same_site(
            &url("https://a.dev/"),
            &url("http://169.254.169.254/latest")
        ));
    }

    #[test]
    fn public_and_private_addresses() {
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn names_must_resolve_to_public_addresses() {
        let err = resolve_public("127.0.0.1").await.unwrap_err();
        assert!(err.downcast_ref::<PrivateAddress>().is_some(), "{err}");
        // `localhost` names a local address directly.
        assert!(!resolve_public("localhost").await.unwrap().is_empty());
    }
}
