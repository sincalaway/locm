use quick_xml::{events::Event, Reader};
use reqwest::{
    blocking::{Client, Response},
    header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, LOCATION},
    redirect::Policy,
    StatusCode,
};
use serde::Serialize;
use std::{
    io::Read,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    time::Duration,
};
use url::Url;

const MAX_QUERY_BYTES: usize = 256;
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_REDIRECTS: usize = 4;
const MAX_RESULTS: usize = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub summary: String,
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn ipv6_in_prefix(ip: Ipv6Addr, network: u128, prefix_len: u32) -> bool {
    let mask = u128::MAX << (128 - prefix_len);
    u128::from(ip) & mask == network & mask
}

fn public_ipv6(ip: Ipv6Addr) -> bool {
    let octets = ip.octets();
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return public_ipv4(mapped);
    }
    // Globally reachable unicast IPv6 is currently allocated from 2000::/3.
    (octets[0] & 0xe0) == 0x20
        && !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !ip.is_unicast_link_local()
        && !ip.is_unique_local()
        // IANA special-purpose, documentation, 6to4, and documentation-only prefixes.
        && !ipv6_in_prefix(ip, 0x2001_0000_0000_0000_0000_0000_0000_0000, 23)
        && !ipv6_in_prefix(ip, 0x2001_0db8_0000_0000_0000_0000_0000_0000, 32)
        && !ipv6_in_prefix(ip, 0x2002_0000_0000_0000_0000_0000_0000_0000, 16)
        && !ipv6_in_prefix(ip, 0x3fff_0000_0000_0000_0000_0000_0000_0000, 20)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_ipv4(ip),
        IpAddr::V6(ip) => public_ipv6(ip),
    }
}

fn validate_url(url: &Url) -> Result<(String, u16, Vec<SocketAddr>), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("网页读取只允许 HTTP 或 HTTPS 地址".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("网页地址不能包含用户名或密码".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "网页地址缺少主机名".to_string())?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
    {
        return Err("网页读取只允许可公开路由的互联网主机".to_string());
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "网页地址端口无效".to_string())?;
    if !((url.scheme() == "http" && port == 80) || (url.scheme() == "https" && port == 443)) {
        return Err("网页读取仅允许 HTTP 80 或 HTTPS 443 端口".to_string());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !public_ip(ip) {
            return Err("网页读取拒绝访问本机、私有或保留 IP 地址".to_string());
        }
    }
    let resolved: Vec<SocketAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| format!("无法解析网页主机：{error}"))?
        .collect();
    if resolved.is_empty() || resolved.iter().any(|address| !public_ip(address.ip())) {
        return Err("网页主机解析到了非公网地址，已拒绝访问".to_string());
    }
    Ok((host, port, resolved))
}

fn client_for(url: &Url) -> Result<Client, String> {
    let (host, _, addresses) = validate_url(url)?;
    Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(REQUEST_TIMEOUT)
        .redirect(Policy::none())
        .no_proxy()
        .resolve_to_addrs(&host, &addresses)
        .user_agent("LocM local Agent web reader")
        .build()
        .map_err(|error| format!("无法初始化安全网页客户端：{error}"))
}

fn fetch_bounded(url: &Url, accept: &str) -> Result<(Url, String, Vec<u8>), String> {
    let mut current = url.clone();
    for redirects in 0..=MAX_REDIRECTS {
        validate_url(&current)?;
        let client = client_for(&current)?;
        let response = client
            .get(current.clone())
            .header(ACCEPT, accept)
            .send()
            .map_err(|error| format!("网页请求失败：{error}"))?;
        if matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            if redirects == MAX_REDIRECTS {
                return Err("网页重定向次数超过安全上限".to_string());
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "网页重定向缺少有效 Location".to_string())?;
            current = current
                .join(location)
                .map_err(|error| format!("网页重定向地址无效：{error}"))?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("网页服务器返回 HTTP {}", response.status()));
        }
        return read_bounded_response(current, response);
    }
    Err("网页重定向次数超过安全上限".to_string())
}

fn read_bounded_response(url: Url, response: Response) -> Result<(Url, String, Vec<u8>), String> {
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err("网页内容超过 2 MiB 安全上限".to_string());
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_ascii_lowercase();
    if !(content_type.starts_with("text/html")
        || content_type.starts_with("application/xhtml+xml")
        || content_type.starts_with("text/plain")
        || content_type.starts_with("application/rss+xml")
        || content_type.starts_with("application/xml")
        || content_type.starts_with("text/xml"))
    {
        return Err("网页读取只接受 HTML、XML 或纯文本响应".to_string());
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取网页内容失败：{error}"))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err("网页内容超过 2 MiB 安全上限".to_string());
    }
    Ok((url, content_type, bytes))
}

pub fn web_search(query: &str) -> Result<Vec<SearchResult>, String> {
    let query = query.trim();
    if query.is_empty() || query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control) {
        return Err("网页搜索词不能为空、不能包含控制字符，且最多 256 字节".to_string());
    }
    let mut url = Url::parse("https://www.bing.com/search")
        .map_err(|error| format!("无法构造网页搜索地址：{error}"))?;
    url.query_pairs_mut()
        .append_pair("format", "rss")
        .append_pair("q", query);
    let (_, content_type, bytes) = fetch_bounded(
        &url,
        "application/rss+xml, application/xml;q=0.9, text/xml;q=0.8",
    )?;
    if !content_type.contains("xml") && !content_type.contains("rss") {
        return Err("网页搜索服务没有返回 XML/RSS 结果".to_string());
    }
    let xml = std::str::from_utf8(&bytes).map_err(|_| "网页搜索结果不是有效 UTF-8".to_string())?;
    parse_search_rss(xml)
}

fn append_search_field(current: &mut SearchResult, field: Option<&str>, text: &str) {
    match field {
        Some("title") => current.title.push_str(text),
        Some("url") => current.url.push_str(text),
        Some("summary") => current.summary.push_str(text),
        _ => {}
    }
}

fn parse_search_rss(xml: &str) -> Result<Vec<SearchResult>, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut results = Vec::new();
    let mut in_item = false;
    let mut field: Option<&'static str> = None;
    let mut current = SearchResult {
        title: String::new(),
        url: String::new(),
        summary: String::new(),
    };
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = event.local_name();
                match name.as_ref() {
                    "item" => {
                        in_item = true;
                        current = SearchResult {
                            title: String::new(),
                            url: String::new(),
                            summary: String::new(),
                        };
                    }
                    "title" if in_item => field = Some("title"),
                    "link" if in_item => field = Some("url"),
                    "description" if in_item => field = Some("summary"),
                    _ => {}
                }
            }
            Ok(Event::Empty(event)) if in_item && event.local_name().as_ref() == "link" => {
                for attribute in event.attributes().flatten() {
                    if attribute.key.local_name().as_ref() == "href" {
                        if let Ok(value) = quick_xml::escape::unescape(attribute.value.as_ref()) {
                            current.url = value.into_owned();
                        }
                    }
                }
            }
            Ok(Event::Text(event)) if in_item => {
                let text = event.xml_content(quick_xml::XmlVersion::Implicit1_0);
                append_search_field(&mut current, field, &text);
            }
            Ok(Event::GeneralRef(event)) if in_item => {
                let decoded = if let Some(character) = event
                    .resolve_char_ref()
                    .map_err(|error| format!("网页搜索 XML 字符引用无效：{error}"))?
                {
                    character.to_string()
                } else {
                    quick_xml::escape::resolve_predefined_entity(event.as_ref())
                        .unwrap_or("�")
                        .to_string()
                };
                append_search_field(&mut current, field, &decoded);
            }
            Ok(Event::CData(event)) if in_item => {
                let text = event.xml_content(quick_xml::XmlVersion::Implicit1_0);
                append_search_field(&mut current, field, &text);
            }
            Ok(Event::End(event)) => match event.local_name().as_ref() {
                "item" => {
                    if in_item && !current.title.trim().is_empty() && !current.url.trim().is_empty()
                    {
                        results.push(current.clone());
                    }
                    in_item = false;
                    field = None;
                    if results.len() >= MAX_RESULTS {
                        break;
                    }
                }
                "title" | "link" | "description" => field = None,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(format!("无法解析网页搜索 RSS：{error}")),
        }
    }
    for result in &mut results {
        result.title = strip_html(&result.title);
        result.summary = strip_html(&result.summary);
    }
    Ok(results)
}

pub fn web_fetch(raw_url: &str) -> Result<String, String> {
    if raw_url.trim().is_empty() || raw_url.len() > 2048 || raw_url.chars().any(char::is_control) {
        return Err("网页地址不能为空、不能包含控制字符，且最多 2048 字节".to_string());
    }
    let url = Url::parse(raw_url).map_err(|error| format!("网页地址无效：{error}"))?;
    let (final_url, content_type, bytes) =
        fetch_bounded(&url, "text/html, application/xhtml+xml, text/plain;q=0.9")?;
    let source = String::from_utf8_lossy(&bytes);
    let text = if content_type.starts_with("text/plain") {
        source.into_owned()
    } else {
        strip_html(&source)
    };
    let text = truncate_utf8(&text, MAX_OUTPUT_BYTES);
    Ok(format!(
        "最终地址：{final_url}\n内容类型：{content_type}\n\n{text}"
    ))
}

fn strip_html(source: &str) -> String {
    let mut output = String::with_capacity(source.len().min(MAX_OUTPUT_BYTES));
    let mut chars = source.chars().peekable();
    let mut skipping: Option<String> = None;
    while let Some(ch) = chars.next() {
        if ch == '<' {
            let mut tag = String::new();
            let mut closed = false;
            for next in chars.by_ref().take(512) {
                if next == '>' {
                    closed = true;
                    break;
                }
                tag.push(next);
            }
            if !closed {
                output.push('<');
                output.push_str(&tag);
                continue;
            }
            let trimmed = tag.trim();
            let closing = trimmed.starts_with('/');
            let name = trimmed
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches('/')
                .to_ascii_lowercase();
            if let Some(active) = skipping.as_deref() {
                if closing && name == active {
                    skipping = None;
                }
                continue;
            }
            if !closing && matches!(name.as_str(), "script" | "style" | "noscript" | "svg") {
                skipping = Some(name);
                continue;
            }
            if matches!(
                name.as_str(),
                "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "section" | "article"
            ) {
                output.push('\n');
            } else if matches!(name.as_str(), "td" | "th") {
                output.push('\t');
            }
            continue;
        }
        if skipping.is_some() {
            continue;
        }
        if ch == '&' {
            let mut entity = String::new();
            let mut terminated = false;
            for next in chars.by_ref().take(16) {
                if next == ';' {
                    terminated = true;
                    break;
                }
                if next.is_ascii_alphanumeric() || next == '#' {
                    entity.push(next);
                } else {
                    output.push('&');
                    output.push_str(&entity);
                    output.push(next);
                    break;
                }
            }
            if terminated {
                output.push(decode_entity(&entity));
            } else if !entity.is_empty() {
                output.push('&');
                output.push_str(&entity);
            }
        } else {
            output.push(ch);
        }
        if output.len() >= MAX_OUTPUT_BYTES * 2 {
            break;
        }
    }
    let mut compact = String::with_capacity(output.len().min(MAX_OUTPUT_BYTES));
    let mut previous_newline = false;
    let mut previous_space = false;
    for ch in output.chars() {
        if ch == '\n' || ch == '\r' {
            if !previous_newline {
                compact.push('\n');
            }
            previous_newline = true;
            previous_space = false;
        } else if ch.is_whitespace() {
            if !previous_space && !previous_newline {
                compact.push(' ');
            }
            previous_space = true;
        } else {
            compact.push(ch);
            previous_newline = false;
            previous_space = false;
        }
    }
    compact.trim().to_string()
}

fn decode_entity(entity: &str) -> char {
    match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" => '\'',
        "nbsp" => ' ',
        value if value.starts_with("#x") || value.starts_with("#X") => {
            u32::from_str_radix(&value[2..], 16)
                .ok()
                .and_then(char::from_u32)
                .unwrap_or('�')
        }
        value if value.starts_with('#') => value[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('�'),
        _ => '�',
    }
}

fn truncate_utf8(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…（内容已截断）", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::{parse_search_rss, public_ip, strip_html, validate_url, web_search, SearchResult};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use url::Url;

    #[test]
    fn rejects_private_reserved_and_local_addresses() {
        for ip in [
            "127.0.0.1",
            "10.2.3.4",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.2",
            "100.64.1.1",
            "192.0.0.1",
            "192.0.2.3",
            "192.88.99.1",
            "198.18.0.1",
        ] {
            assert!(
                !public_ip(ip.parse::<IpAddr>().unwrap()),
                "{ip} must not be public"
            );
        }
        assert!(!public_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!public_ip(IpAddr::V6("fd00::1".parse().unwrap())));
        assert!(!public_ip(IpAddr::V6("fe80::1".parse().unwrap())));
        assert!(!public_ip(IpAddr::V6("2001:db8::1".parse().unwrap())));
        assert!(!public_ip(IpAddr::V6("2002:c000:0201::1".parse().unwrap())));
        assert!(!public_ip(IpAddr::V6("3fff::1".parse().unwrap())));
        assert!(!public_ip(IpAddr::V6(
            "::ffff:192.168.1.1".parse().unwrap()
        )));
        assert!(public_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(public_ip(IpAddr::V6(
            "2606:4700:4700::1111".parse().unwrap()
        )));
    }

    #[test]
    fn rejects_unsupported_schemes_ports_and_credentials_before_network_access() {
        for value in [
            "file:///etc/passwd",
            "http://localhost/",
            "http://127.0.0.1/",
            "https://user:pass@example.com/",
            "https://example.com:8443/",
        ] {
            let url = Url::parse(value).unwrap();
            assert!(validate_url(&url).is_err(), "{value} should be rejected");
        }
    }

    #[test]
    fn parses_bounded_rss_results() {
        let xml = r#"<rss><channel><item><title>A &amp; B</title><link>https://example.com/a</link><description>Some &lt;b&gt;text&lt;/b&gt;</description></item></channel></rss>"#;
        assert_eq!(
            parse_search_rss(xml).unwrap(),
            vec![SearchResult {
                title: "A & B".into(),
                url: "https://example.com/a".into(),
                summary: "Some text".into(),
            }]
        );
        assert!(parse_search_rss("<broken").is_err());
    }

    #[test]
    fn strips_active_html_and_bounds_text() {
        assert_eq!(
            strip_html("<h1>Title</h1><script>bad()</script><p>A &amp; B</p>"),
            "Title\nA & B"
        );
        assert!(!strip_html("<style>.secret{}</style><p>safe</p>").contains("secret"));
    }

    #[test]
    fn search_validation_rejects_empty_oversized_and_control_queries() {
        assert!(web_search("").is_err());
        assert!(web_search(&"q".repeat(257)).is_err());
        assert!(web_search("hello\nworld").is_err());
    }
}
