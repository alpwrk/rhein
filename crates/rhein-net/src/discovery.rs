use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use rhein_proto::Discovery;
use url::{Host, Url};

const DISCOVERY_LIMIT: usize = 256 * 1024;
const POINTER_LIMIT: usize = 512;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

/// Resolve an account domain to its live API and gateway, following a bounded number of
/// advertised account moves. A static website may delegate discovery through /telium.txt.
pub async fn discover(http: &reqwest::Client, domain: &str) -> Result<Discovery> {
    let start = normalise(domain);
    let mut name = start.clone();
    for _ in 0..4 {
        let mut doc = fetch(http, &name).await?;
        match doc.moved_to.as_deref() {
            Some(next) if !next.is_empty() && normalise(next) != name => name = normalise(next),
            _ => {
                if name != start {
                    doc.relocated_to = Some(name);
                }
                return Ok(doc);
            }
        }
    }
    bail!("{domain}: too many moved_to redirects")
}

/// Accepts a bare authority or an explicitly selected HTTP(S) origin.
pub fn normalise(input: &str) -> String {
    input.trim().trim_end_matches('/').to_owned()
}

/// Never silently downgrade public discovery to HTTP. The local development servers use
/// localhost / b.localhost, so retain the historical fallback for loopback hosts.
fn candidate_bases(name: &str) -> Vec<String> {
    if name.starts_with("http://") || name.starts_with("https://") {
        return vec![name.to_owned()];
    }
    let https = format!("https://{name}");
    if Url::parse(&https).is_ok_and(|url| is_loopback_host(&url)) {
        vec![https, format!("http://{name}")]
    } else {
        vec![https]
    }
}

fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(host)) => host == "localhost" || host.ends_with(".localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

async fn fetch(http: &reqwest::Client, name: &str) -> Result<Discovery> {
    let mut last = None;
    for base in candidate_bases(name) {
        match fetch_base(http, &base).await {
            Ok(doc) => return Ok(doc),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("{name}: unreachable")))
}

async fn read_capped(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|size| size > limit as u64) {
        bail!("Telium discovery response exceeds {limit} bytes");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("reading Telium discovery")? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            bail!("Telium discovery response exceeds {limit} bytes");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn reject_redirect(response: &reqwest::Response, requested: &str) -> Result<()> {
    // A caller can supply a custom reqwest client (e.g. Rhein's relay-proxied client).
    // Even if it follows redirects internally, never *trust* discovery fetched at a
    // different URL. In particular, the website's pointer is not an HTTP redirect.
    let expected = Url::parse(requested).context("invalid discovery URL")?;
    ensure!(
        response.url() == &expected,
        "{requested}: redirected discovery is not supported"
    );
    Ok(())
}

async fn fetch_base(http: &reqwest::Client, base: &str) -> Result<Discovery> {
    let base_url = Url::parse(base).with_context(|| format!("{base}: invalid server origin"))?;
    ensure!(
        base_url.host().is_some()
            && base_url.username().is_empty()
            && base_url.password().is_none()
            && base_url.path() == "/"
            && base_url.query().is_none()
            && base_url.fragment().is_none(),
        "{base}: expected a server origin without a path or credentials"
    );
    let direct_url = format!("{base}/.well-known/telium/server");
    let response = http
        .get(&direct_url)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("{direct_url}: unreachable"))?;
    reject_redirect(&response, &direct_url)?;

    // Prefer full discovery at the address the user entered, as before. A missing,
    // non-JSON, or unrecognisable document permits the website's text pointer.
    if response.status().is_success() {
        let body = read_capped(response, DISCOVERY_LIMIT).await?;
        if let Ok(mut doc) = serde_json::from_slice::<Discovery>(&body) {
            if !doc.server_name.is_empty() {
                if doc.api.is_empty() {
                    doc.api = base.to_owned();
                }
                doc.api = doc.api.trim_end_matches('/').to_owned();
                return Ok(doc);
            }
        }
    }

    // A pointer fetched over an ordinary HTTP website could be rewritten by a
    // network attacker. Explicit local HTTP is only useful for development.
    ensure!(
        base_url.scheme() == "https" || is_loopback_host(&base_url),
        "{base}: telium.txt delegation requires HTTPS"
    );
    let pointer_url = format!("{base}/telium.txt");
    let response = http
        .get(&pointer_url)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("{pointer_url}: unreachable"))?;
    reject_redirect(&response, &pointer_url)?;
    ensure!(
        response.status().is_success(),
        "{pointer_url}: HTTP {}",
        response.status()
    );
    ensure!(
        !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("text/html")),
        "{pointer_url}: an HTML page is not a telium.txt pointer"
    );
    let body = read_capped(response, POINTER_LIMIT).await?;
    let line = std::str::from_utf8(&body).context("telium.txt must be UTF-8")?;
    let target = parse_pointer(line, &base_url)?;

    // The file only selects a backend. Never recursively follow another pointer:
    // the backend must serve live discovery metadata under the fixed well-known path.
    let backend_url = format!("{}/.well-known/telium/server", target.origin().ascii_serialization());
    let response = http
        .get(&backend_url)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("{backend_url}: unreachable"))?;
    reject_redirect(&response, &backend_url)?;
    ensure!(
        response.status().is_success(),
        "{backend_url}: HTTP {}",
        response.status()
    );
    let bytes = read_capped(response, DISCOVERY_LIMIT).await?;
    let mut doc: Discovery = serde_json::from_slice(&bytes)
        .with_context(|| format!("{backend_url}: invalid discovery document"))?;
    validate_delegated(&doc, &base_url, &target)?;
    doc.api = doc.api.trim_end_matches('/').to_owned();
    Ok(doc)
}

/// A pointer is an HTTPS origin on the account domain or one of its subdomains.
/// It is never an arbitrary URL or an HTTP redirect.
fn parse_pointer(text: &str, identity: &Url) -> Result<Url> {
    ensure!(text.len() <= POINTER_LIMIT, "telium.txt exceeds {POINTER_LIMIT} bytes");
    let line = text.trim();
    ensure!(
        !line.is_empty() && !line.chars().any(char::is_whitespace),
        "telium.txt must contain exactly one hostname or HTTPS origin"
    );
    let raw = if line.contains("://") {
        line.to_owned()
    } else {
        format!("https://{line}")
    };
    let target = Url::parse(&raw).context("telium.txt contains an invalid URL")?;
    ensure!(
        target.scheme() == "https"
            && target.username().is_empty()
            && target.password().is_none()
            && target.port().is_none()
            && target.path() == "/"
            && target.query().is_none()
            && target.fragment().is_none(),
        "telium.txt must contain an HTTPS origin without credentials, a port, path or query"
    );

    let identity_host = identity.host_str().context("account domain has no hostname")?;
    let host = target.host_str().context("telium.txt has no hostname")?;
    ensure!(
        host.eq_ignore_ascii_case(identity_host)
            || host.to_ascii_lowercase().ends_with(&format!(".{}", identity_host.to_ascii_lowercase())),
        "telium.txt must point to the account domain or one of its subdomains"
    );
    Ok(target)
}

fn validate_delegated(doc: &Discovery, identity: &Url, target: &Url) -> Result<()> {
    let expected_name = identity.host_str().context("account domain has no hostname")?;
    ensure!(
        doc.server_name.eq_ignore_ascii_case(expected_name),
        "delegated Telium server does not identify itself as {expected_name}"
    );
    let advertised = Url::parse(&doc.api).context("delegated Telium server has no valid API URL")?;
    ensure!(
        advertised.origin() == target.origin()
            && advertised.username().is_empty()
            && advertised.password().is_none()
            && advertised.path() == "/"
            && advertised.query().is_none()
            && advertised.fragment().is_none(),
        "delegated Telium server advertises a different API address"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_domains_never_downgrade_to_http() {
        assert_eq!(candidate_bases("example.com"), vec!["https://example.com".to_owned()]);
        assert_eq!(
            candidate_bases("localhost:8448"),
            vec!["https://localhost:8448".to_owned(), "http://localhost:8448".to_owned()]
        );
        assert_eq!(
            candidate_bases("b.localhost:8449"),
            vec!["https://b.localhost:8449".to_owned(), "http://b.localhost:8449".to_owned()]
        );
        assert_eq!(candidate_bases("http://example.com"), vec!["http://example.com".to_owned()]);
    }

    #[test]
    fn accepts_same_domain_and_subdomain_pointers() {
        let origin = Url::parse("https://example.com/").unwrap();
        assert_eq!(
            parse_pointer("chat.example.com\n", &origin).unwrap().origin().ascii_serialization(),
            "https://chat.example.com"
        );
        assert_eq!(
            parse_pointer("https://example.com", &origin).unwrap().origin().ascii_serialization(),
            "https://example.com"
        );
        assert_eq!(
            parse_pointer("https://eu.chat.example.com", &origin).unwrap().origin().ascii_serialization(),
            "https://eu.chat.example.com"
        );
    }

    #[test]
    fn refuses_unsafe_pointers() {
        let origin = Url::parse("https://example.com/").unwrap();
        for pointer in [
            "",
            "https://attacker.com",
            "https://evil-example.com",
            "https://example.com.attacker.com",
            "http://chat.example.com",
            "https://chat.example.com:8448",
            "https://chat.example.com/path",
            "https://chat.example.com?x=1",
            "https://chat.example.com/#fragment",
            "https://user@chat.example.com",
            "https://chat.example.com@attacker.com",
            "chat.example.com\nother.example.com",
            "chat.example.com more.example.com",
            "ftp://chat.example.com",
        ] {
            assert!(parse_pointer(pointer, &origin).is_err(), "{pointer}");
        }
        assert!(parse_pointer(&"a".repeat(513), &origin).is_err());
    }

    #[test]
    fn delegated_document_must_confirm_original_identity_and_api() {
        let identity = Url::parse("https://example.com").unwrap();
        let target = parse_pointer("chat.example.com", &identity).unwrap();
        let mut doc = Discovery {
            server_name: "example.com".into(),
            api: "https://chat.example.com".into(),
            ..Discovery::default()
        };
        validate_delegated(&doc, &identity, &target).unwrap();
        doc.server_name = "victim.example".into();
        assert!(validate_delegated(&doc, &identity, &target).is_err());
        doc.server_name = "example.com".into();
        doc.api = "https://elsewhere.example.com".into();
        assert!(validate_delegated(&doc, &identity, &target).is_err());
        doc.api = "https://chat.example.com/unexpected-path".into();
        assert!(validate_delegated(&doc, &identity, &target).is_err());
    }

    fn serve(responses: Vec<(&'static str, &'static str)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let mut paths = Vec::new();
            for (status, body) in responses {
                let start = std::time::Instant::now();
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock
                            && start.elapsed() < Duration::from_secs(5) =>
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(err) => panic!("mock discovery request did not arrive: {err}"),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut buf = [0; 4096];
                let mut request = Vec::new();
                loop {
                    let count = stream.read(&mut buf).unwrap();
                    assert!(count > 0, "incomplete mock HTTP request");
                    request.extend_from_slice(&buf[..count]);
                    if request.windows(2).any(|pair| pair == b"\r\n") {
                        break;
                    }
                    assert!(request.len() < 8192, "oversized mock HTTP request");
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                paths.push(path);
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
            paths
        });
        (base, handle)
    }

    #[tokio::test]
    async fn existing_direct_discovery_skips_the_pointer() {
        let (base, handle) = serve(vec![(
            "200 OK",
            r#"{"server_name":"local.test","api":"http://localhost:8448","gateway":"ws://localhost:8448/gateway/v1"}"#,
        )]);
        let doc = discover(&reqwest::Client::new(), &base).await.unwrap();
        assert_eq!(doc.server_name, "local.test");
        assert_eq!(doc.api, "http://localhost:8448");
        assert_eq!(handle.join().unwrap(), ["/.well-known/telium/server"]);
    }

    #[tokio::test]
    async fn missing_document_tries_pointer_but_rejects_unrelated_host() {
        let (base, handle) = serve(vec![
            ("404 Not Found", "not found"),
            ("200 OK", "https://attacker.example"),
        ]);
        let result = discover(&reqwest::Client::new(), &base).await;
        assert!(result.is_err());
        assert_eq!(
            handle.join().unwrap(),
            ["/.well-known/telium/server", "/telium.txt"]
        );
    }
}
