//! HTTP side of `web_fetch`: a GET with manual redirect handling, a
//! destination check on every hop, a bounded body and errors an agent can act
//! on.

use std::collections::HashMap;
use std::error::Error as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use tokio::time::{timeout_at, Instant};
use url::{Host, Url};

use super::decode;

/// Same ceiling as opencode's webfetch; larger bodies are rejected unread.
pub(super) const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;
/// Error pages are read only far enough for a useful excerpt.
const ERROR_BODY_BYTES: usize = 64 * 1024;
const ERROR_EXCERPT_CHARS: usize = 600;
const DNS_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Honest but browser-compatible identification (the opencode approach): a
/// spoofed Chrome string without a matching TLS fingerprint is exactly what
/// bot filters look for.
fn default_user_agent() -> String {
    format!(
        "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; Locus-WebFetch/{}; +https://unity.farlocus.com",
        env!("CARGO_PKG_VERSION")
    )
}
/// Retried once when Cloudflare answers with a challenge page.
const CHALLENGE_RETRY_USER_AGENT: &str = "Locus";

pub(super) struct FetchOptions {
    pub client: reqwest::Client,
    pub accept: &'static str,
    pub deadline: Instant,
    pub timeout: Duration,
    /// Tests reach a local server; the tool never sets this.
    pub allow_private: bool,
}

pub(super) struct FetchedResponse {
    pub final_url: Url,
    pub content_type: String,
    pub body: Vec<u8>,
    pub notes: Vec<String>,
}

/// Client for web_fetch: proxy settings from the app, redirects handled here.
pub(super) fn build_client() -> Result<reqwest::Client, String> {
    crate::network::reqwest_client(
        crate::network::ReqwestClientOptions::new()
            .connect_timeout(CONNECT_TIMEOUT)
            .gzip(true)
            .deflate(true)
            .follow_redirects(false),
    )
}

pub(super) async fn fetch(url: &Url, options: &FetchOptions) -> Result<FetchedResponse, String> {
    let mut current = url.clone();
    let mut cookies: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut redirects = 0usize;
    let mut user_agent = default_user_agent();
    let mut challenge_retried = false;
    let mut notes = Vec::new();

    let response = loop {
        check_destination(&current, options.allow_private).await?;
        let mut request = options
            .client
            .get(current.as_str())
            .header(reqwest::header::USER_AGENT, user_agent.as_str())
            .header(reqwest::header::ACCEPT, options.accept)
            .header(
                reqwest::header::ACCEPT_LANGUAGE,
                "en-US,en;q=0.9,zh-CN;q=0.8,*;q=0.5",
            );
        if let Some(cookie) = cookie_header(&cookies, &current) {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        let response = match timeout_at(options.deadline, request.send()).await {
            Err(_) => return Err(timeout_message(&current, options.timeout)),
            Ok(Err(error)) => {
                return Err(describe_request_error(&error, &current, options.timeout))
            }
            Ok(Ok(response)) => response,
        };
        store_cookies(&mut cookies, &current, response.headers());

        let status = response.status();
        if status.is_redirection() {
            if let Some(location) = header_str(response.headers(), reqwest::header::LOCATION) {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(format!(
                        "Too many redirects (more than {MAX_REDIRECTS}) while fetching {url}; the site may be stuck in a redirect loop."
                    ));
                }
                current = current.join(location.trim()).map_err(|error| {
                    format!("{current} redirected to an invalid location {location:?}: {error}")
                })?;
                continue;
            }
        }
        if is_cloudflare_challenge(status, response.headers()) && !challenge_retried {
            challenge_retried = true;
            user_agent = CHALLENGE_RETRY_USER_AGENT.to_string();
            notes.push(
                "Cloudflare answered with a bot challenge; retried with a plain user agent."
                    .to_string(),
            );
            continue;
        }
        break response;
    };

    let status = response.status();
    if !status.is_success() {
        return Err(http_error(response, &current, options).await);
    }

    let content_type = header_str(response.headers(), reqwest::header::CONTENT_TYPE)
        .unwrap_or("")
        .to_string();
    let declared_length = response.content_length();
    if let decode::MimeClass::Unsupported(description) =
        decode::classify_mime(&decode::mime_type(&content_type))
    {
        return Err(unsupported_message(
            &current,
            description,
            &decode::mime_type(&content_type),
            declared_length,
        ));
    }
    if declared_length.is_some_and(|length| length > MAX_BODY_BYTES as u64) {
        return Err(too_large_message(&current, declared_length));
    }

    let body = read_body(response, MAX_BODY_BYTES, options)
        .await
        .map_err(|error| match error {
            BodyError::TooLarge => too_large_message(&current, declared_length),
            BodyError::Timeout => timeout_message(&current, options.timeout),
            BodyError::Read(error) => describe_request_error(&error, &current, options.timeout),
        })?;

    Ok(FetchedResponse {
        final_url: current,
        content_type,
        body,
        notes,
    })
}

fn header_str(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

// ─── Body ───────────────────────────────────────────────────────────────────

enum BodyError {
    TooLarge,
    Timeout,
    Read(reqwest::Error),
}

async fn read_body(
    mut response: reqwest::Response,
    limit: usize,
    options: &FetchOptions,
) -> Result<Vec<u8>, BodyError> {
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .map_or(64 * 1024, |length| length.min(limit as u64) as usize),
    );
    loop {
        let chunk = match timeout_at(options.deadline, response.chunk()).await {
            Err(_) => return Err(BodyError::Timeout),
            Ok(Err(error)) => return Err(BodyError::Read(error)),
            Ok(Ok(None)) => return Ok(body),
            Ok(Ok(Some(chunk))) => chunk,
        };
        if body.len() + chunk.len() > limit {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
}

/// Like `read_body`, but keeps the first `limit` bytes instead of failing.
async fn read_body_prefix(
    mut response: reqwest::Response,
    limit: usize,
    deadline: Instant,
) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < limit {
        match timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let take = chunk.len().min(limit - body.len());
                body.extend_from_slice(&chunk[..take]);
            }
            _ => break,
        }
    }
    body
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

fn too_large_message(url: &Url, declared: Option<u64>) -> String {
    let size = declared.map_or(String::new(), |bytes| format!(" ({})", format_size(bytes)));
    format!(
        "{url} is larger than the {} web_fetch limit{size}. Download it with bash or python to process it.",
        format_size(MAX_BODY_BYTES as u64)
    )
}

pub(super) fn unsupported_message(
    url: &Url,
    description: &str,
    mime: &str,
    length: Option<u64>,
) -> String {
    let mut details = Vec::new();
    if !mime.is_empty() {
        details.push(mime.to_string());
    }
    if let Some(bytes) = length {
        details.push(format_size(bytes));
    }
    let detail = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    format!(
        "{url} returned {description}{detail}. web_fetch only returns text; download the file with bash or python to process it."
    )
}

// ─── Errors ─────────────────────────────────────────────────────────────────

fn timeout_message(url: &Url, timeout: Duration) -> String {
    format!(
        "Timed out after {}s fetching {url}. The site may be slow or unreachable; retry with a larger timeout (max 120) or use another source.",
        timeout.as_secs()
    )
}

/// The full cause chain: reqwest's own message ("error sending request for
/// url") hides whether DNS, TLS, the proxy or the connection failed.
fn describe_request_error(error: &reqwest::Error, url: &Url, timeout: Duration) -> String {
    if error.is_timeout() {
        return timeout_message(url, timeout);
    }
    let mut causes: Vec<String> = Vec::new();
    let mut io_kind = None;
    let mut os_code = None;
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            io_kind.get_or_insert(io.kind());
            if let Some(code) = io.raw_os_error() {
                os_code.get_or_insert(code);
            }
        }
        let text = cause.to_string();
        if !text.is_empty() && !causes.iter().any(|seen| seen.contains(&text)) {
            causes.push(text);
        }
        source = cause.source();
    }
    let detail = if causes.is_empty() {
        error.to_string()
    } else {
        causes.join(": ")
    };
    // OS messages are localized, so classify by error kind and code first;
    // the English keywords come from hyper and the TLS libraries.
    let lower = detail.to_ascii_lowercase();
    // Windows SChannel failures surface as SEC_E_* HRESULTs (0x8009xxxx).
    let schannel = os_code.is_some_and(|code| (code as u32) & 0xFFFF_0000 == 0x8009_0000);
    let kind = if lower.contains("dns error")
        || lower.contains("failed to lookup")
        || lower.contains("no such host")
    {
        "DNS lookup failed"
    } else if lower.contains("certificate") {
        "the TLS certificate was rejected"
    } else if schannel
        || lower.contains("tls")
        || lower.contains("ssl")
        || lower.contains("handshake")
    {
        "the TLS handshake failed"
    } else {
        match io_kind {
            Some(std::io::ErrorKind::ConnectionRefused) => "the connection was refused",
            Some(std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => {
                "the connection was reset"
            }
            Some(std::io::ErrorKind::TimedOut) => "connecting timed out",
            _ if lower.contains("refused") => "the connection was refused",
            _ if lower.contains("proxy") || lower.contains("tunnel") => {
                "the proxy could not reach the site"
            }
            _ if error.is_connect() => "the connection failed",
            _ if error.is_body() || error.is_decode() => "reading the response failed",
            _ => "the request failed",
        }
    };
    format!("Failed to fetch {url}: {kind} ({detail}).")
}

fn is_cloudflare_challenge(status: reqwest::StatusCode, headers: &HeaderMap) -> bool {
    matches!(status.as_u16(), 403 | 503)
        && header_str(
            headers,
            reqwest::header::HeaderName::from_static("cf-mitigated"),
        )
        .is_some_and(|value| value.eq_ignore_ascii_case("challenge"))
}

fn looks_like_bot_wall(headers: &HeaderMap, excerpt: &str) -> bool {
    let cloudflare = header_str(headers, reqwest::header::SERVER)
        .is_some_and(|server| server.to_ascii_lowercase().contains("cloudflare"))
        || headers.contains_key("cf-mitigated");
    let lower = excerpt.to_ascii_lowercase();
    (cloudflare
        && (lower.contains("just a moment")
            || lower.contains("attention required")
            || lower.contains("checking your browser")))
        || lower.contains("cf-chl")
        || lower.contains("captcha")
        || lower.contains("verify you are human")
}

async fn http_error(response: reqwest::Response, url: &Url, options: &FetchOptions) -> String {
    let status = response.status();
    let headers = response.headers().clone();
    let content_type = header_str(&headers, reqwest::header::CONTENT_TYPE)
        .unwrap_or("")
        .to_string();
    let retry_after = header_str(&headers, reqwest::header::RETRY_AFTER).map(str::to_string);
    let body = read_body_prefix(response, ERROR_BODY_BYTES, options.deadline).await;
    let (text, is_html) = decode::decode_lossy(&body, &content_type, url.host_str());
    let mut excerpt = if is_html {
        super::html::render(&text, url, Some(super::html::Style::Text)).content
    } else {
        text
    };
    excerpt = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
    if excerpt.chars().count() > ERROR_EXCERPT_CHARS {
        excerpt = excerpt
            .chars()
            .take(ERROR_EXCERPT_CHARS)
            .collect::<String>()
            + "…";
    }

    let code = status.as_u16();
    let hint = if (code == 401 || code == 403 || code == 503)
        && looks_like_bot_wall(&headers, &excerpt)
    {
        "The site shows a bot check (e.g. a Cloudflare challenge) that needs a real browser. web_fetch cannot pass it; use another source instead of retrying.".to_string()
    } else {
        match code {
            401 => "The page requires authentication.".to_string(),
            403 => "Access is forbidden: the site may require login, block automated clients, or restrict this region. Retrying the same URL will not help.".to_string(),
            404 | 410 => "The page does not exist. Check the URL or look for the current link.".to_string(),
            429 => match retry_after {
                Some(after) => format!("Rate limited; the server asks to retry after {after}."),
                None => "Rate limited; wait before retrying.".to_string(),
            },
            500..=599 => "The server failed; retrying later may work.".to_string(),
            _ => String::new(),
        }
    };

    let reason = status
        .canonical_reason()
        .map_or(String::new(), |reason| format!(" {reason}"));
    let mut message = format!("HTTP {code}{reason} from {url}.");
    if !hint.is_empty() {
        message.push(' ');
        message.push_str(&hint);
    }
    if !excerpt.is_empty() {
        message.push_str("\n\nResponse body excerpt:\n");
        message.push_str(&excerpt);
    }
    message
}

// ─── Cookies ────────────────────────────────────────────────────────────────

/// Host-scoped cookies kept for the redirect chain of one fetch: sites that
/// set a cookie and redirect would otherwise loop.
fn store_cookies(jar: &mut HashMap<String, Vec<(String, String)>>, url: &Url, headers: &HeaderMap) {
    let Some(host) = url.host_str() else {
        return;
    };
    for value in headers.get_all(reqwest::header::SET_COOKIE) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        let Some((name, cookie_value)) = value
            .split(';')
            .next()
            .and_then(|pair| pair.split_once('='))
        else {
            continue;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let cookies = jar.entry(host.to_ascii_lowercase()).or_default();
        cookies.retain(|(existing, _)| *existing != name);
        cookies.push((name, cookie_value.trim().to_string()));
    }
}

fn cookie_header(jar: &HashMap<String, Vec<(String, String)>>, url: &Url) -> Option<HeaderValue> {
    let cookies = jar.get(&url.host_str()?.to_ascii_lowercase())?;
    let header = cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    HeaderValue::from_str(&header).ok()
}

// ─── Destination check ──────────────────────────────────────────────────────

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || first == 0
        // Carrier-grade NAT (100.64.0.0/10).
        || (first == 100 && (64..128).contains(&second))
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(mapped);
    }
    let first = ip.segments()[0];
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Unique local (fc00::/7) and link-local (fe80::/10).
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
}

pub(super) fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_blocked_ipv4(ip),
        IpAddr::V6(ip) => is_blocked_ipv6(ip),
    }
}

fn is_local_host_name(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost"
        || [".localhost", ".local", ".internal", ".home.arpa", ".lan"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
}

fn private_destination_message(url: &Url, address: Option<IpAddr>) -> String {
    let resolved = address.map_or(String::new(), |ip| format!(" ({ip})"));
    format!(
        "web_fetch only fetches public internet addresses; {url} points to a local or private network{resolved}. Use bash or python (e.g. curl) for local and intranet URLs."
    )
}

/// Rejects loopback, private, link-local and similar destinations, checked
/// again after every redirect. A DNS name that resolves to such an address is
/// rejected too; when resolution fails here the request still goes ahead, as
/// a configured proxy resolves names on its side.
pub(super) async fn check_destination(url: &Url, allow_private: bool) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "Only http:// and https:// URLs are supported (got {url})."
        ));
    }
    if allow_private {
        return Ok(());
    }
    match url.host() {
        None => Err(format!("{url} has no host.")),
        Some(Host::Ipv4(ip)) if is_blocked_ipv4(ip) => Err(private_destination_message(url, None)),
        Some(Host::Ipv6(ip)) if is_blocked_ipv6(ip) => Err(private_destination_message(url, None)),
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => Ok(()),
        Some(Host::Domain(domain)) => {
            if is_local_host_name(domain) {
                return Err(private_destination_message(url, None));
            }
            let port = url.port_or_known_default().unwrap_or(80);
            let lookup = tokio::net::lookup_host((domain, port));
            if let Ok(Ok(addresses)) = tokio::time::timeout(DNS_CHECK_TIMEOUT, lookup).await {
                if let Some(address) = addresses
                    .map(|address| address.ip())
                    .find(|ip| is_blocked_ip(*ip))
                {
                    return Err(private_destination_message(url, Some(address)));
                }
            }
            Ok(())
        }
    }
}
