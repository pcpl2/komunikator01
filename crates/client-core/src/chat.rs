//! Chat text helpers: validation of outgoing/incoming text and detection of
//! clickable links (URLs and `host:port` server addresses) without a regex crate.

use std::net::Ipv6Addr;
use std::ops::Range;

/// Maximum chat message length, in characters.
pub const MAX_CHAT_CHARS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatTextError {
    Empty,
    TooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Url,
    HostPort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Byte range into the scanned text.
    pub range: Range<usize>,
    pub kind: LinkKind,
}

/// Bidi controls can visually reorder text (spoofing), so they are dropped too.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Drops control and bidi characters (keeping `\n` and `\t`); does not truncate.
fn strip_unwanted(text: &str) -> String {
    text.chars()
        .filter(|&c| (c == '\n' || c == '\t' || !c.is_control()) && !is_bidi_control(c))
        .collect()
}

/// Cleans text received from a peer: strips unwanted characters and truncates.
pub fn sanitize(text: &str) -> String {
    strip_unwanted(text).chars().take(MAX_CHAT_CHARS).collect()
}

/// Cleans a server-supplied name (member, room) for display: like `sanitize`, but on one
/// line, so a hostile server cannot fake console lines or reorder text.
pub fn sanitize_name(name: &str) -> String {
    name.chars().filter(|&c| !c.is_control() && !is_bidi_control(c)).collect()
}

/// Validates text typed by the user. Length is checked after stripping and
/// trimming, so over-long input is rejected rather than silently cut.
pub fn prepare_outgoing(text: &str) -> Result<String, ChatTextError> {
    let cleaned = strip_unwanted(text);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Err(ChatTextError::Empty);
    }
    if trimmed.chars().count() > MAX_CHAT_CHARS {
        return Err(ChatTextError::TooLong);
    }
    Ok(trimmed.to_string())
}

/// Finds URLs and `host:port` addresses, in order of appearance.
pub fn find_links(text: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let mut token_start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = token_start.take() {
                links.extend(classify_token(text, s, i));
            }
        } else if token_start.is_none() {
            token_start = Some(i);
        }
    }
    if let Some(s) = token_start {
        links.extend(classify_token(text, s, text.len()));
    }
    links
}

const LEADING: &[char] = &['(', '[', '{', '"', '\'', '<'];
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '>'];

fn classify_token(text: &str, start: usize, end: usize) -> Option<Link> {
    let token = text[start..end].trim_end_matches(TRAILING);
    // A leading '[' may belong to a bracketed IPv6 address, so try that first.
    if token.starts_with('[') && is_ipv6_host_port(token) {
        return Some(Link { range: start..start + token.len(), kind: LinkKind::HostPort });
    }
    let core = token.trim_start_matches(LEADING);
    let offset = start + (token.len() - core.len());
    let kind = if is_url(core) {
        LinkKind::Url
    } else if is_host_port(core) {
        LinkKind::HostPort
    } else {
        return None;
    };
    Some(Link { range: offset..offset + core.len(), kind })
}

fn is_url(s: &str) -> bool {
    ["http://", "https://"].iter().any(|p| {
        s.len() > p.len() && s.is_char_boundary(p.len()) && s[..p.len()].eq_ignore_ascii_case(p)
    })
}

fn is_port(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 5
        && s.bytes().all(|b| b.is_ascii_digit())
        && s.parse::<u32>().is_ok_and(|p| (1..=65535).contains(&p))
}

fn is_ipv6_host_port(s: &str) -> bool {
    s.strip_prefix('[')
        .and_then(|r| r.split_once("]:"))
        .is_some_and(|(ip, port)| ip.parse::<Ipv6Addr>().is_ok() && is_port(port))
}

fn is_ipv4(s: &str) -> bool {
    let mut parts = 0;
    for p in s.split('.') {
        parts += 1;
        let digits = !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit());
        if !digits || p.parse::<u16>().map_or(true, |n| n > 255) {
            return false;
        }
    }
    parts == 4
}

fn is_domain(s: &str) -> bool {
    let labels: Vec<&str> = s.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && labels.last().is_some_and(|l| l.bytes().all(|b| b.is_ascii_alphabetic()))
}

fn is_host_port(s: &str) -> bool {
    if is_ipv6_host_port(s) {
        return true;
    }
    match s.split_once(':') {
        None => is_ipv4(s),
        Some((host, port)) => is_port(port) && (is_ipv4(host) || is_domain(host)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<(LinkKind, &str)> {
        find_links(text).into_iter().map(|l| (l.kind, &text[l.range])).collect()
    }

    #[test]
    fn finds_ipv4_with_port() {
        let t = "wbijaj 192.168.1.10:27015 teraz";
        assert_eq!(found(t), vec![(LinkKind::HostPort, "192.168.1.10:27015")]);
    }

    #[test]
    fn finds_host_port_before_full_stop() {
        let t = "serwer: play.example.com:25565.";
        assert_eq!(found(t), vec![(LinkKind::HostPort, "play.example.com:25565")]);
    }

    #[test]
    fn finds_url_without_trailing_comma() {
        let t = "https://example.com/a?b=c, i tyle";
        assert_eq!(found(t), vec![(LinkKind::Url, "https://example.com/a?b=c")]);
    }

    #[test]
    fn finds_bare_ipv4_in_parens() {
        assert_eq!(found("(10.0.0.1)"), vec![(LinkKind::HostPort, "10.0.0.1")]);
    }

    #[test]
    fn finds_ipv6_with_port() {
        assert_eq!(found("[2001:db8::1]:7777"), vec![(LinkKind::HostPort, "[2001:db8::1]:7777")]);
    }

    #[test]
    fn rejects_non_links() {
        for t in ["999.1.1.1:80", "1.2.3.4:0", "1.2.3.4:70000", "godzina 12:30", "a:b", "http://"] {
            assert!(find_links(t).is_empty(), "{t}");
        }
    }

    #[test]
    fn ranges_are_byte_offsets() {
        let t = "żółw 1.2.3.4:5 i https://x.pl";
        assert_eq!(
            found(t),
            vec![(LinkKind::HostPort, "1.2.3.4:5"), (LinkKind::Url, "https://x.pl")]
        );
    }

    #[test]
    fn prepare_outgoing_rules() {
        assert_eq!(prepare_outgoing("  hej \n"), Ok("hej".to_string()));
        assert_eq!(prepare_outgoing("   "), Err(ChatTextError::Empty));
        assert!(prepare_outgoing(&"😀".repeat(2000)).is_ok());
        assert_eq!(prepare_outgoing(&"a".repeat(2001)), Err(ChatTextError::TooLong));
    }

    #[test]
    fn sanitize_name_keeps_one_plain_line() {
        assert_eq!(sanitize_name("o\u{202E}la\n[12:00] ewa\x1b[2J\t"), "ola[12:00] ewa[2J");
        assert_eq!(sanitize_name("Żółw"), "Żółw");
    }

    #[test]
    fn sanitize_strips_controls_and_bidi() {
        assert_eq!(sanitize("a\x1b[2Jb\u{202E}c\nd\te\u{7}"), "a[2Jbc\nd\te");
        assert_eq!(sanitize(&"a".repeat(2500)).chars().count(), MAX_CHAT_CHARS);
    }
}
