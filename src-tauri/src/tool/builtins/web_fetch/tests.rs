use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use super::decode::{self, BodyKind, EncodingSource};
use super::html::{self, Style};
use super::http;
use super::{execute, parse_request, render_output, Document};

// ─── Local HTTP server ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct RequestHead {
    path: String,
    headers: HashMap<String, String>,
}

impl RequestHead {
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map_or("", String::as_str)
    }
}

type Handler = Arc<dyn Fn(&RequestHead) -> Vec<u8> + Send + Sync>;

struct Route {
    path: &'static str,
    delay: Duration,
    handler: Handler,
}

fn route(
    path: &'static str,
    handler: impl Fn(&RequestHead) -> Vec<u8> + Send + Sync + 'static,
) -> Route {
    Route {
        path,
        delay: Duration::ZERO,
        handler: Arc::new(handler),
    }
}

struct TestServer {
    base: Url,
    seen: Arc<Mutex<Vec<RequestHead>>>,
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        self.base.join(path).unwrap().to_string()
    }

    fn requests_to(&self, path: &str) -> Vec<RequestHead> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|head| head.path == path)
            .cloned()
            .collect()
    }
}

fn parse_head(raw: &[u8]) -> RequestHead {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let path = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    RequestHead { path, headers }
}

async fn serve(routes: Vec<Route>) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let routes: Arc<HashMap<&'static str, (Duration, Handler)>> = Arc::new(
        routes
            .into_iter()
            .map(|route| (route.path, (route.delay, route.handler)))
            .collect(),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server_seen = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let routes = routes.clone();
            let seen = server_seen.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut chunk = [0u8; 4096];
                while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => raw.extend_from_slice(&chunk[..read]),
                    }
                }
                let head = parse_head(&raw);
                seen.lock().unwrap().push(head.clone());
                let reply = match routes.get(head.path.as_str()) {
                    Some((delay, handler)) => {
                        tokio::time::sleep(*delay).await;
                        handler(&head)
                    }
                    None => response(404, &[], b"no route"),
                };
                let _ = socket.write_all(&reply).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    TestServer { base, seen }
}

fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn html_response(body: &str) -> Vec<u8> {
    response(
        200,
        &[("Content-Type", "text/html; charset=utf-8")],
        body.as_bytes(),
    )
}

fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn fetch(url: &str, extra: serde_json::Value) -> Result<String, String> {
    let mut args = json!({ "url": url });
    if let (Some(args), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
        args.extend(extra.clone());
    }
    execute(parse_request(&args).unwrap(), Some(test_client()), true).await
}

fn long_paragraph(words: usize) -> String {
    vec!["content"; words].join(" ")
}

// ─── HTTP behaviour ─────────────────────────────────────────────────────────

#[tokio::test]
async fn follows_redirects_with_cookies_and_reports_the_final_url() {
    let server = serve(vec![
        route("/start", |_| {
            response(
                302,
                &[("Location", "/final"), ("Set-Cookie", "session=abc; Path=/; HttpOnly")],
                b"",
            )
        }),
        route("/final", |head| {
            if head.header("cookie").contains("session=abc") {
                html_response("<html><head><title>Final page</title></head><body><h1>Done</h1><p>Arrived.</p></body></html>")
            } else {
                response(403, &[], b"cookie missing")
            }
        }),
    ])
    .await;

    let output = fetch(&server.url("/start"), json!({})).await.unwrap();

    assert!(
        output.starts_with(&format!("URL: {}\n", server.url("/final"))),
        "{output}"
    );
    assert!(output.contains(&format!("Redirected from: {}\n", server.url("/start"))));
    assert!(output.contains("Title: Final page\n"));
    assert!(output.contains("# Done\n\nArrived."), "{output}");
    let agent = server.requests_to("/final")[0]
        .header("user-agent")
        .to_string();
    assert!(agent.contains("Locus-WebFetch/"), "{agent}");
}

#[tokio::test]
async fn private_destinations_are_rejected_before_connecting() {
    for url in [
        "http://127.0.0.1:8080/",
        "http://localhost/",
        "http://dev.localhost/",
        "http://[::1]/",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.1.2.3/",
        "http://172.16.0.1/",
        "http://192.168.1.1/",
        "http://100.64.0.1/",
        "http://0.0.0.0/",
        "http://[fd00::1]/",
        "http://[fe80::1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://metadata.google.internal/",
        "http://printer.local/",
    ] {
        let error = http::check_destination(&Url::parse(url).unwrap(), false)
            .await
            .expect_err(url);
        assert!(error.contains("public internet"), "{url}: {error}");
    }
    for url in ["http://8.8.8.8/", "https://[2606:4700:4700::1111]/"] {
        http::check_destination(&Url::parse(url).unwrap(), false)
            .await
            .unwrap_or_else(|error| panic!("{url}: {error}"));
    }
    let error = http::check_destination(&Url::parse("ftp://example.com/").unwrap(), false)
        .await
        .unwrap_err();
    assert!(error.contains("http://"), "{error}");

    let server = serve(vec![route("/", |_| html_response("<p>secret</p>"))]).await;
    let args = json!({ "url": server.url("/") });
    let error = execute(parse_request(&args).unwrap(), Some(test_client()), false)
        .await
        .unwrap_err();
    assert!(error.contains("public internet"), "{error}");
    assert!(server.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn oversized_and_binary_responses_are_rejected() {
    let server = serve(vec![
        route("/declared", |_| {
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 104857600\r\nConnection: close\r\n\r\n".to_vec()
        }),
        route("/streamed", |_| {
            let mut reply =
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n".to_vec();
            reply.extend(std::iter::repeat_n(b'a', http::MAX_BODY_BYTES + 1024));
            reply
        }),
        route("/image", |_| response(200, &[("Content-Type", "image/png")], b"\x89PNG\r\n\x1a\nxx")),
        route("/octet-pdf", |_| {
            response(
                200,
                &[("Content-Type", "application/octet-stream")],
                b"%PDF-1.7\n%binary",
            )
        }),
    ])
    .await;

    let declared = fetch(&server.url("/declared"), json!({}))
        .await
        .unwrap_err();
    assert!(declared.contains("larger than the 5.0 MiB"), "{declared}");
    assert!(declared.contains("100.0 MiB"), "{declared}");
    let streamed = fetch(&server.url("/streamed"), json!({}))
        .await
        .unwrap_err();
    assert!(streamed.contains("larger than the 5.0 MiB"), "{streamed}");
    let image = fetch(&server.url("/image"), json!({})).await.unwrap_err();
    assert!(image.contains("returned an image (image/png"), "{image}");
    let pdf = fetch(&server.url("/octet-pdf"), json!({}))
        .await
        .unwrap_err();
    assert!(pdf.contains("returned a PDF document"), "{pdf}");
}

#[tokio::test]
async fn http_errors_carry_a_hint_and_a_body_excerpt() {
    let server = serve(vec![
        route("/missing", |_| {
            response(
                404,
                &[("Content-Type", "text/html")],
                b"<html><body><nav>Home</nav><h1>Not here</h1><p>Try the archive.</p></body></html>",
            )
        }),
        route("/limited", |_| {
            response(
                429,
                &[("Content-Type", "application/json"), ("Retry-After", "60")],
                br#"{"message":"API rate limit exceeded"}"#,
            )
        }),
        route("/challenge", |_| {
            response(
                403,
                &[("Server", "cloudflare"), ("cf-mitigated", "challenge"), ("Content-Type", "text/html")],
                b"<html><head><title>Just a moment...</title></head><body>Checking your browser</body></html>",
            )
        }),
    ])
    .await;

    let missing = fetch(&server.url("/missing"), json!({})).await.unwrap_err();
    assert!(missing.starts_with("HTTP 404 Not Found from"), "{missing}");
    assert!(missing.contains("does not exist"), "{missing}");
    assert!(missing.contains("Not here Try the archive."), "{missing}");
    assert!(!missing.contains("Home"), "{missing}");

    let limited = fetch(&server.url("/limited"), json!({})).await.unwrap_err();
    assert!(limited.contains("retry after 60"), "{limited}");
    assert!(limited.contains("API rate limit exceeded"), "{limited}");

    let challenge = fetch(&server.url("/challenge"), json!({}))
        .await
        .unwrap_err();
    assert!(challenge.contains("bot check"), "{challenge}");
    let attempts = server.requests_to("/challenge");
    assert_eq!(attempts.len(), 2, "one retry with a plain user agent");
    assert_eq!(attempts[1].header("user-agent"), "Locus");
}

#[tokio::test]
async fn cloudflare_challenge_is_retried_with_a_plain_user_agent() {
    let server = serve(vec![route("/guarded", |head| {
        if head.header("user-agent") == "Locus" {
            html_response("<html><body><p>Welcome in.</p></body></html>")
        } else {
            response(403, &[("cf-mitigated", "challenge")], b"challenge")
        }
    })])
    .await;

    let output = fetch(&server.url("/guarded"), json!({})).await.unwrap();
    assert!(output.contains("Welcome in."), "{output}");
    assert!(
        output.contains("Note: Cloudflare answered with a bot challenge"),
        "{output}"
    );
}

#[tokio::test]
async fn timeouts_and_refused_connections_name_the_cause() {
    let server = serve(vec![Route {
        path: "/slow",
        delay: Duration::from_secs(3),
        handler: Arc::new(|_| html_response("<p>late</p>")),
    }])
    .await;
    let slow = fetch(&server.url("/slow"), json!({ "timeout": 1 }))
        .await
        .unwrap_err();
    assert!(slow.starts_with("Timed out after 1s"), "{slow}");

    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let refused = fetch(&format!("http://127.0.0.1:{port}/"), json!({}))
        .await
        .unwrap_err();
    assert!(refused.contains("the connection was refused"), "{refused}");
}

#[tokio::test]
async fn meta_refresh_shells_are_followed() {
    let server = serve(vec![
        route("/shell", |_| {
            html_response(
                r#"<html><head><meta http-equiv="Refresh" content="0; URL='/real'"></head><body>Redirecting...</body></html>"#,
            )
        }),
        route("/real", |_| html_response("<html><body><p>Real content.</p></body></html>")),
    ])
    .await;

    let output = fetch(&server.url("/shell"), json!({})).await.unwrap();
    assert!(
        output.starts_with(&format!("URL: {}\n", server.url("/real"))),
        "{output}"
    );
    assert!(
        output.contains("Note: Followed a <meta refresh> redirect"),
        "{output}"
    );
    assert!(output.contains("Real content."), "{output}");
}

#[tokio::test]
async fn legacy_charset_pages_are_decoded_end_to_end() {
    let (gbk, _, _) = encoding_rs::GBK.encode(
        "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=gb2312\"><title>茶馆</title></head><body><p>第一幕：裕泰大茶馆。</p></body></html>",
    );
    let body = gbk.into_owned();
    let server = serve(vec![route("/gbk", move |_| {
        response(200, &[("Content-Type", "text/html")], &body)
    })])
    .await;

    let output = fetch(&server.url("/gbk"), json!({})).await.unwrap();
    assert!(output.contains("Title: 茶馆"), "{output}");
    assert!(output.contains("第一幕：裕泰大茶馆。"), "{output}");
    assert!(
        output.contains("decoded as GBK (declared in the document)"),
        "{output}"
    );
}

#[tokio::test]
async fn continuation_reads_page_by_characters_from_the_cache() {
    let lines: Vec<String> = (0..400).map(|index| format!("第{index}行内容")).collect();
    let text = lines.join("\n");
    let server = serve(vec![route("/long.txt", move |_| {
        response(
            200,
            &[("Content-Type", "text/plain; charset=utf-8")],
            text.as_bytes(),
        )
    })])
    .await;
    let url = server.url("/long.txt");

    let first = fetch(&url, json!({ "maxChars": 1000 })).await.unwrap();
    let next_offset: usize = first
        .split("offset=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .expect("continuation offset");
    assert!(first.contains("第0行内容"), "{first}");
    assert!((800..=1000).contains(&next_offset), "{next_offset}");

    let second = fetch(&url, json!({ "maxChars": 1000, "offset": next_offset }))
        .await
        .unwrap();
    assert!(second.contains("Cached: fetched"), "{second}");
    assert!(
        second.contains(&format!("Showing characters {next_offset}-")),
        "{second}"
    );
    assert_eq!(
        server.requests_to("/long.txt").len(),
        1,
        "served from the cache"
    );

    let past_end = fetch(&url, json!({ "offset": 1_000_000 }))
        .await
        .unwrap_err();
    assert!(past_end.contains("past the end"), "{past_end}");
}

// ─── Arguments and output ───────────────────────────────────────────────────

#[test]
fn arguments_are_validated_and_clamped() {
    assert!(parse_request(&json!({})).is_err());
    let scheme = parse_request(&json!({ "url": "example.com/page" }))
        .err()
        .unwrap();
    assert!(scheme.contains("http://"), "{scheme}");
    assert!(parse_request(&json!({ "url": "https://a.example", "format": "pdf" })).is_err());

    let low =
        parse_request(&json!({ "url": "https://a.example", "timeout": 0, "maxChars": 0 })).unwrap();
    assert_eq!(low.timeout, Duration::from_secs(1));
    assert_eq!(low.max_chars, 1);
    let high = parse_request(
        &json!({ "url": "https://a.example", "timeout": 999, "maxChars": 9_999_999 }),
    )
    .unwrap();
    assert_eq!(high.timeout, Duration::from_secs(120));
    assert_eq!(high.max_chars, 200_000);
    let legacy = parse_request(&json!({ "url": "https://a.example", "max_chars": 10 })).unwrap();
    assert_eq!(legacy.max_chars, 10);
}

fn document(content: &str) -> Document {
    let url = Url::parse("https://example.com/doc").unwrap();
    Document {
        requested_url: url.clone(),
        final_url: url,
        title: Some("Doc".to_string()),
        content_type: "text/plain".to_string(),
        encoding_note: None,
        notes: Vec::new(),
        content: content.to_string(),
        char_count: content.chars().count(),
    }
}

#[test]
fn output_counts_characters_not_bytes() {
    let doc = document(&"中".repeat(30));
    let output = render_output(&doc, 0, 10, None).unwrap();
    assert!(
        output.contains(&format!("\n\n{}\n\n", "中".repeat(10))),
        "{output}"
    );
    assert!(
        output.contains("[Showing characters 0-10 of 30."),
        "{output}"
    );

    let last = render_output(&doc, 20, 10, None).unwrap();
    assert!(
        last.ends_with("[Showing characters 20-30 of 30; end of content.]"),
        "{last}"
    );

    let whole = render_output(&document("short"), 0, 100, None).unwrap();
    assert_eq!(
        whole,
        "URL: https://example.com/doc\nTitle: Doc\nContent-Type: text/plain\n\nshort"
    );
}

#[test]
fn output_cuts_at_a_line_break_near_the_limit() {
    let doc = document(&format!("{}\n{}", "a".repeat(95), "b".repeat(50)));
    let output = render_output(&doc, 0, 100, None).unwrap();
    assert!(
        output.contains(&format!(
            "{}\n\n[Showing characters 0-96 of 146.",
            "a".repeat(95)
        )),
        "{output}"
    );
}

// ─── Decoding ───────────────────────────────────────────────────────────────

#[test]
fn charset_comes_from_header_document_or_detection() {
    let chinese = "这是一段用于检测编码的中文文本，内容来自一个没有声明字符集的旧网站。".repeat(4);

    let (gbk, _, _) = encoding_rs::GBK.encode(&chinese);
    let detected = decode::decode(&gbk, "text/plain", Some("www.example.cn")).unwrap();
    assert_eq!(detected.text, chinese);
    assert_eq!(detected.encoding, encoding_rs::GBK);
    assert_eq!(detected.encoding_source, EncodingSource::Detected);

    let (big5, _, _) = encoding_rs::BIG5.encode("<meta charset=big5><p>繁體中文</p>");
    let declared = decode::decode(&big5, "text/html", None).unwrap();
    assert!(declared.text.contains("繁體中文"));
    assert_eq!(declared.kind, BodyKind::Html);
    assert_eq!(declared.encoding_source, EncodingSource::Document);

    let (latin1, _, _) = encoding_rs::WINDOWS_1252.encode("café");
    let header = decode::decode(&latin1, "text/plain; charset=\"ISO-8859-1\"", None).unwrap();
    assert_eq!(header.text, "café");
    assert_eq!(header.encoding_source, EncodingSource::Header);

    // A wrong declaration on a valid UTF-8 body falls back to UTF-8.
    let mislabeled =
        decode::decode(chinese.as_bytes(), "text/plain; charset=shift_jis", None).unwrap();
    assert_eq!(mislabeled.text, chinese);
    assert_eq!(mislabeled.encoding_note(), None);
}

#[test]
fn generic_types_are_sniffed() {
    assert_eq!(
        decode::decode(b"\x89PNG\r\n\x1a\n....", "application/octet-stream", None).unwrap_err(),
        "an image"
    );
    assert_eq!(
        decode::decode(b"ab\0cd", "", None).unwrap_err(),
        "binary content"
    );
    let html = decode::decode(b"<!DOCTYPE html><html><body>x</body></html>", "", None).unwrap();
    assert_eq!(html.kind, BodyKind::Html);
    let text = decode::decode(b"plain notes", "application/octet-stream", None).unwrap();
    assert_eq!(text.kind, BodyKind::Text);
    assert_eq!(
        decode::decode(b"{}", "application/vnd.api+json", None)
            .unwrap()
            .kind,
        BodyKind::Text
    );
    assert_eq!(
        decode::decode(b"x", "video/mp4", None).unwrap_err(),
        "a video"
    );
}

// ─── HTML rendering ─────────────────────────────────────────────────────────

fn markdown(html: &str) -> String {
    let url = Url::parse("https://example.com/guide/page").unwrap();
    html::render(html, &url, Some(Style::Markdown)).content
}

#[test]
fn deeply_nested_markup_renders_on_a_worker_sized_stack() {
    // html2md's recursive walk overflowed a 2 MiB stack at ~5000 levels.
    let depth = 50_000;
    let page = format!(
        "<html><body><p>{}deep text{}</p></body></html>",
        "<span>".repeat(depth),
        "</span>".repeat(depth)
    );
    let rendered = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let url = Url::parse("https://example.com/").unwrap();
            html::render(&page, &url, Some(Style::Markdown))
        })
        .unwrap()
        .join()
        .expect("rendering must not overflow the stack");
    assert!(!rendered.simplified);
    assert_eq!(rendered.content, "deep text");
}

#[test]
fn pathological_container_nesting_falls_back_to_plain_text() {
    let url = Url::parse("https://example.com/").unwrap();
    let depth = 10_000;
    let page = format!(
        "<html><head><title>x</title><script>if (a < b) {{ }}</script></head><body>{}<h1>Deep</h1><p>a &amp; b&#33;</p><!-- <div> -->{}</body></html>",
        "<div>".repeat(depth),
        "</div>".repeat(depth)
    );
    let started = std::time::Instant::now();
    let rendered = html::render(&page, &url, Some(Style::Markdown));
    assert!(rendered.simplified);
    assert_eq!(rendered.content, "Deep\n\na & b!");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    let siblings = "<div>item</div>".repeat(depth);
    let rendered = html::render(&format!("<body>{siblings}</body>"), &url, Some(Style::Text));
    assert!(!rendered.simplified);
    assert_eq!(rendered.content.lines().count(), depth);
}

#[test]
fn page_noise_is_removed_and_links_are_absolute() {
    let page = r##"<html><head><title>T</title><style>.x{}</style><script>var x=1</script></head><body>
        <header><a href="/">Site logo</a></header>
        <nav><a href="/a">Nav link</a></nav>
        <div class="cookie-banner">We use cookies</div>
        <noscript>Enable JavaScript to continue</noscript>
        <svg><title>icon</title></svg>
        <div hidden>Hidden text</div><span aria-hidden="true">Decor</span>
        <div style="display: none">Invisible</div>
        <form><input value="Search"><button>Go</button><label>Label kept</label></form>
        <h1>Guide</h1>
        <p>See <a href="docs">the docs</a>, <a href="https://x.example/">https://x.example/</a>
           and <a href="javascript:void(0)">script link</a> or <a href="#top">top</a>.</p>
        <p><img src="/img/d.png" alt="Diagram"><img src="data:image/png;base64,AAAA" alt="Inline">
           <img src="/img/spacer.gif"></p>
        <p><a href="/File:big.png"><img src="/thumb.png" alt="Big [v2]"></a></p>
        <footer>Copyright</footer>
    </body></html>"##;
    let output = markdown(page);

    for noise in [
        "Site logo",
        "Nav link",
        "cookies",
        "JavaScript",
        "icon",
        "Hidden text",
        "Decor",
        "Invisible",
        "Search",
        "Go",
        "Copyright",
        "var x",
        ".x{",
        "data:",
        "spacer",
    ] {
        assert!(!output.contains(noise), "{noise:?} leaked into:\n{output}");
    }
    assert!(output.contains("Label kept"), "{output}");
    assert!(output.contains("# Guide"), "{output}");
    assert!(
        output.contains("[the docs](https://example.com/guide/docs)"),
        "{output}"
    );
    assert!(output.contains(", https://x.example/"), "{output}");
    assert!(output.contains("and script link or top."), "{output}");
    assert!(
        output.contains("![Diagram](https://example.com/img/d.png)"),
        "{output}"
    );
    assert!(
        output.contains(
            "[![Big \\[v2\\]](https://example.com/thumb.png)](https://example.com/File:big.png)"
        ),
        "{output}"
    );
}

#[test]
fn main_content_is_used_only_when_it_holds_the_text() {
    let url = Url::parse("https://example.com/").unwrap();
    let dominant = format!(
        "<body><div>Intro blurb</div><main><p>{}</p></main></body>",
        long_paragraph(120)
    );
    let page = html::render(&dominant, &url, Some(Style::Markdown));
    assert_eq!(page.content_root, Some("main"));
    assert!(!page.content.contains("Intro blurb"));

    let minor = format!(
        "<body><div><p>{}</p></div><main><p>{}</p></main></body>",
        long_paragraph(400),
        long_paragraph(40)
    );
    let page = html::render(&minor, &url, Some(Style::Markdown));
    assert_eq!(page.content_root, None);

    let listing = format!(
        "<body><article><p>{}</p></article><article><p>{}</p></article></body>",
        long_paragraph(60),
        long_paragraph(60)
    );
    assert_eq!(
        html::render(&listing, &url, Some(Style::Markdown)).content_root,
        None
    );
}

#[test]
fn block_structure_becomes_markdown() {
    let page = r#"<body>
        <h2>Install <a href="/x">now</a></h2>
        <ol><li>First</li><li>Second<ul><li>Nested</li></ul></li></ol>
        <blockquote><p>Quoted line</p><p>Second quote</p></blockquote>
        <pre><code class="language-rust">fn main() {
    println!("```");
}</code></pre>
        <p>Use <code>cargo test</code> with <strong>care</strong> and <em>style</em>.<br>Next line</p>
        <table><caption>Settings</caption>
          <tr><th>Name</th><th>Value</th></tr>
          <tr><td>a | b</td><td>1</td></tr>
          <tr><td colspan="2">wide</td></tr>
        </table>
        <hr>
        <p>After</p>
    </body>"#;
    let output = markdown(page);
    let expected = [
        "## Install [now](https://example.com/x)",
        "1. First\n2. Second\n   - Nested",
        "> Quoted line\n>\n> Second quote",
        "````rust\nfn main() {\n    println!(\"```\");\n}\n````",
        "Use `cargo test` with **care** and *style*.\nNext line",
        "Settings\n| Name | Value |\n| --- | --- |\n| a \\| b | 1 |\n| wide |  |",
        "---\n\nAfter",
    ];
    for fragment in expected {
        assert!(
            output.contains(fragment),
            "missing {fragment:?} in:\n{output}"
        );
    }
    assert!(!output.contains("\n\n\n"), "{output}");
}

#[test]
fn text_style_keeps_blocks_and_decodes_entities_once() {
    let url = Url::parse("https://example.com/").unwrap();
    let page = "<body><p>First &amp;lt; para</p><p>Second&#20013;&#x6587;</p><ul><li>One</li><li>Two</li></ul><table><tr><td>a</td><td>b</td></tr></table></body>";
    let output = html::render(page, &url, Some(Style::Text)).content;
    assert_eq!(
        output,
        "First &lt; para\n\nSecond中文\n\n- One\n- Two\n\na | b"
    );
}

#[test]
fn page_metadata_reports_refresh_and_script_shells() {
    let url = Url::parse("https://example.com/dir/").unwrap();
    let shell = r#"<html><head><title> App </title><meta http-equiv="refresh" content="5;url=next.html"><script src="app.js"></script></head>
        <body><div id="root"></div><noscript>You need to enable JavaScript to run this app.</noscript></body></html>"#;
    let page = html::render(shell, &url, None);
    assert_eq!(page.title.as_deref(), Some("App"));
    assert_eq!(
        page.refresh,
        Some((5, Url::parse("https://example.com/dir/next.html").unwrap()))
    );
    assert!(page.needs_javascript);
    assert!(page.content.is_empty());

    let article = format!("<body><p>{}</p></body>", long_paragraph(100));
    assert!(!html::render(&article, &url, Some(Style::Markdown)).needs_javascript);
}

/// Live samples from sessions where the old implementation failed. Run with
/// `cargo test --lib -- --ignored web_fetch_live_samples --nocapture`.
#[tokio::test]
#[ignore = "network"]
async fn web_fetch_live_samples() {
    let samples = [
        (
            "https://www.kanunu8.com/book3/6942/",
            json!({ "maxChars": 600 }),
        ),
        (
            "http://www.ibiblio.org/chinese-text/novel/rural/Golden_Time",
            json!({ "maxChars": 400 }),
        ),
        (
            "https://sekiroshadowsdietwice.wiki.fextralife.com/Posture",
            json!({ "maxChars": 1200 }),
        ),
        (
            "https://zh.wikisource.org/wiki/孔乙己",
            json!({ "maxChars": 800 }),
        ),
        (
            "https://docs.rs/serde/latest/serde/",
            json!({ "maxChars": 800 }),
        ),
        (
            "https://www.zhaobenshu.com/52_52793/28557314.html",
            json!({}),
        ),
        (
            "https://sekiro.fandom.com/wiki/Genichiro_Ashina",
            json!({ "maxChars": 600 }),
        ),
        (
            "https://www.google.com/search?q=locus",
            json!({ "maxChars": 400 }),
        ),
        ("https://arxiv.org/pdf/1706.03762", json!({})),
        (
            "https://raw.githubusercontent.com/rust-lang/rust/master/README.md",
            json!({ "maxChars": 300 }),
        ),
        ("http://localhost:1/", json!({})),
    ];
    for (url, extra) in samples {
        let mut args = json!({ "url": url });
        if let (Some(args), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
            args.extend(extra.clone());
        }
        let started = std::time::Instant::now();
        let result = execute(parse_request(&args).unwrap(), None, false).await;
        let (label, text) = match &result {
            Ok(text) => ("OK", text),
            Err(text) => ("ERR", text),
        };
        std::eprintln!(
            "\n===== {label} {url} ({} ms, {} chars)\n{}",
            started.elapsed().as_millis(),
            text.chars().count(),
            text.chars().take(1400).collect::<String>()
        );
    }
}
