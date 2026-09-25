//! Message formatting with Matrix mention pills.
//!
//! [`mentionify`] turns lightweight markup into a text message:
//!   * `@user:server`  → a matrix.to pill, *and* an `m.mentions` entry
//!   * `**bold**`      → `<strong>`
//!   * `~~strike~~`    → `<del>`
//!   * `[label](url)`  → `<a href="url">label</a>`
//!   * `\n`            → `<br>`
//!
//! Everything else is HTML-escaped. Plain text without markup stays a plain
//! message.
//!
//! `m.mentions`, not the HTML pill, is what clients and homeservers use for
//! notifications. With the fleet's SDK fork it is also copied into the
//! cleartext wrapper of encrypted events, so mentions notify in encrypted
//! rooms too.

use std::collections::{BTreeSet, HashMap};

use matrix_sdk::{
    ruma::{
        events::{room::message::RoomMessageEventContent, Mentions},
        OwnedUserId, UserId,
    },
    Room,
};

/// Build a message, labelling pills with the MXID localpart.
pub fn mentionify(text: &str) -> RoomMessageEventContent {
    build(text, |token| default_label(token).to_owned())
}

/// Like [`mentionify`], but label pills with `names[mxid]` when present. The
/// plain body shows the same labels.
pub fn mentionify_with_names(
    text: &str,
    names: &HashMap<String, String>,
) -> RoomMessageEventContent {
    build(text, |token| {
        names
            .get(token)
            .cloned()
            .unwrap_or_else(|| default_label(token).to_owned())
    })
}

/// Like [`mentionify_with_names`], with display names looked up in `room`.
pub async fn mentionify_rich(text: &str, room: &Room) -> RoomMessageEventContent {
    let mxids = extract_mxids(text);
    if mxids.is_empty() {
        return mentionify(text);
    }
    let refs: Vec<&str> = mxids.iter().map(String::as_str).collect();
    let names = fetch_names(room, &refs).await;
    mentionify_with_names(text, &names)
}

/// A message mentioning exactly `user_id`, as `{prefix}<pill>{suffix}`.
pub fn mention_user(
    user_id: &UserId,
    label: &str,
    prefix: &str,
    suffix: &str,
) -> RoomMessageEventContent {
    let plain = format!("{prefix}{label}{suffix}");
    let html = format!(
        "{}{}{}",
        html_escape(prefix),
        user_pill(user_id, label),
        html_escape(suffix)
    );
    RoomMessageEventContent::text_html(plain, html)
        .add_mentions(Mentions::with_user_ids([user_id.to_owned()]))
}

/// HTML for a single user pill.
pub fn user_pill(user_id: &UserId, label: &str) -> String {
    format!(
        r#"<a href="https://matrix.to/#/{}">{}</a>"#,
        html_escape(user_id.as_str()),
        html_escape(label)
    )
}

/// All `@localpart:server` tokens in `text`, deduplicated, in order.
pub fn extract_mxids(text: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        if text.as_bytes()[pos] == b'@' {
            let token = mxid_token_at(text, pos);
            if is_mxid_token(token) && !result.iter().any(|seen| seen == token) {
                result.push(token.to_owned());
            }
            pos += token.len().max(1);
        } else {
            pos += text[pos..].chars().next().map_or(1, char::len_utf8);
        }
    }
    result
}

/// Display names for `user_ids` from room state (localpart when unset).
pub async fn fetch_names(room: &Room, user_ids: &[&str]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for &raw in user_ids {
        let Ok(user_id) = OwnedUserId::try_from(raw) else {
            continue;
        };
        if let Ok(Some(member)) = room.get_member(&user_id).await {
            let name = member
                .display_name()
                .and_then(sanitize_display_name)
                .unwrap_or_else(|| member.user_id().localpart().to_owned());
            names.insert(raw.to_owned(), name);
        }
    }
    names
}

/// Collapse control characters (e.g. embedded newlines) in a display name
/// and trim it; `None` when nothing is left.
pub fn sanitize_display_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Escape `&`, `<`, `>`, `"` and `'` for HTML content and attributes.
pub fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    push_escaped(&mut out, value);
    out
}

fn push_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

fn default_label(token: &str) -> &str {
    token
        .split(':')
        .next()
        .unwrap_or("")
        .trim_start_matches('@')
}

fn mxid_token_at(text: &str, pos: usize) -> &str {
    let len = text[pos..]
        .find(|c: char| {
            c.is_whitespace() || matches!(c, ',' | '!' | '?' | '*' | ')' | ']' | '"' | '\'')
        })
        .unwrap_or(text.len() - pos);
    &text[pos..pos + len]
}

fn is_mxid_token(token: &str) -> bool {
    token.len() > 4 && token.contains(':')
}

/// `[label](url)` starting at `pos`: returns (label, url, end position).
fn markdown_link_at(text: &str, pos: usize) -> Option<(&str, &str, usize)> {
    let label_end = pos + 1 + text[pos + 1..].find(']')?;
    if text.as_bytes().get(label_end + 1) != Some(&b'(') {
        return None;
    }
    let url_start = label_end + 2;
    let url_end = url_start + text[url_start..].find(')')?;
    Some((
        &text[pos + 1..label_end],
        &text[url_start..url_end],
        url_end + 1,
    ))
}

fn build(text: &str, label_for: impl Fn(&str) -> String) -> RoomMessageEventContent {
    let bytes = text.as_bytes();
    let mut plain = String::with_capacity(text.len());
    let mut html = String::with_capacity(text.len() * 2);
    let mut pos = 0;
    let mut formatted = false;
    let mut in_bold = false;
    let mut in_strike = false;
    let mut mentioned: BTreeSet<OwnedUserId> = BTreeSet::new();

    while pos < text.len() {
        if bytes[pos] == b'*' && bytes.get(pos + 1) == Some(&b'*') {
            html.push_str(if in_bold { "</strong>" } else { "<strong>" });
            in_bold = !in_bold;
            formatted = true;
            pos += 2;
            continue;
        }

        if bytes[pos] == b'~' && bytes.get(pos + 1) == Some(&b'~') {
            html.push_str(if in_strike { "</del>" } else { "<del>" });
            in_strike = !in_strike;
            formatted = true;
            pos += 2;
            continue;
        }

        if bytes[pos] == b'[' {
            if let Some((label, url, end)) = markdown_link_at(text, pos) {
                plain.push_str(label);
                html.push_str(r#"<a href=""#);
                push_escaped(&mut html, url);
                html.push_str(r#"">"#);
                push_escaped(&mut html, label);
                html.push_str("</a>");
                formatted = true;
                pos = end;
                continue;
            }
        }

        if bytes[pos] == b'@' {
            let token = mxid_token_at(text, pos);
            if is_mxid_token(token) {
                let label = label_for(token);
                plain.push_str(&label);
                html.push_str(r#"<a href="https://matrix.to/#/"#);
                push_escaped(&mut html, token);
                html.push_str(r#"">"#);
                push_escaped(&mut html, &label);
                html.push_str("</a>");
                formatted = true;
                if let Ok(user_id) = OwnedUserId::try_from(token) {
                    mentioned.insert(user_id);
                }
                pos += token.len();
                continue;
            }
        }

        let ch = text[pos..]
            .chars()
            .next()
            .expect("pos is on a char boundary");
        plain.push(ch);
        if ch == '\n' {
            html.push_str("<br>");
            formatted = true;
        } else {
            push_escaped(&mut html, ch.encode_utf8(&mut [0; 4]));
        }
        pos += ch.len_utf8();
    }

    // Close unbalanced markers (shouldn't happen with well-formed input).
    if in_bold {
        html.push_str("</strong>");
    }
    if in_strike {
        html.push_str("</del>");
    }

    let content = if formatted {
        RoomMessageEventContent::text_html(plain, html)
    } else {
        RoomMessageEventContent::text_plain(text)
    };
    if mentioned.is_empty() {
        content
    } else {
        content.add_mentions(Mentions::with_user_ids(mentioned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{events::room::message::MessageType, user_id};

    fn bodies(c: &RoomMessageEventContent) -> (String, Option<String>) {
        match &c.msgtype {
            MessageType::Text(t) => (t.body.clone(), t.formatted.as_ref().map(|f| f.body.clone())),
            _ => panic!("unexpected msgtype"),
        }
    }

    #[test]
    fn mxid_becomes_pill_and_mention() {
        let c = mentionify("Hello @alice:example.org!");
        let (plain, html) = bodies(&c);
        let html = html.unwrap();
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
        assert!(html.contains(">alice<"));
        assert_eq!(plain, "Hello alice!");
        let mentions = c.mentions.expect("m.mentions must be set");
        assert!(mentions.user_ids.contains(user_id!("@alice:example.org")));
        assert!(!mentions.room);
    }

    #[test]
    fn plain_text_stays_plain_without_mentions() {
        let c = mentionify("no mentions here");
        let (_, html) = bodies(&c);
        assert!(html.is_none());
        assert!(c.mentions.is_none());
    }

    #[test]
    fn markup_is_rendered_and_stripped_from_plain() {
        let c = mentionify("**Floor 1** ~~Sports~~ [Map](https://e.org/?a=1&b=2)\nnext");
        let (plain, html) = bodies(&c);
        let html = html.unwrap();
        assert!(html.contains("<strong>Floor 1</strong>"));
        assert!(html.contains("<del>Sports</del>"));
        assert!(html.contains(r#"<a href="https://e.org/?a=1&amp;b=2">Map</a>"#));
        assert!(html.contains("<br>"));
        assert_eq!(plain, "Floor 1 Sports Map\nnext");
    }

    #[test]
    fn html_is_escaped_including_display_names() {
        let names = HashMap::from([("@a:x.org".to_owned(), "<b>A</b>".to_owned())]);
        let c = mentionify_with_names("x < y & @a:x.org", &names);
        let (plain, html) = bodies(&c);
        let html = html.unwrap();
        assert!(html.contains("x &lt; y &amp; "));
        assert!(html.contains("&lt;b&gt;A&lt;/b&gt;"));
        assert_eq!(plain, "x < y & <b>A</b>");
    }

    #[test]
    fn every_pill_is_mentioned_but_invalid_tokens_are_not() {
        let c = mentionify("@a:x.org and @b:y.org");
        assert_eq!(c.mentions.unwrap().user_ids.len(), 2);
        let c = mentionify("@alice:");
        assert!(c.mentions.is_none());
    }

    #[test]
    fn extract_mxids_deduplicates() {
        assert_eq!(
            extract_mxids("@a:x.org, @b:y.org and @a:x.org!"),
            vec!["@a:x.org".to_owned(), "@b:y.org".to_owned()]
        );
    }

    #[test]
    fn mention_user_builds_single_mention() {
        let c = mention_user(user_id!("@a:x.org"), "Alice", "Hi ", "!");
        let (plain, html) = bodies(&c);
        assert_eq!(plain, "Hi Alice!");
        assert!(html.unwrap().contains(">Alice</a>!"));
        assert_eq!(c.mentions.unwrap().user_ids.len(), 1);
    }

    #[test]
    fn sanitize_display_name_cleans_control_chars() {
        assert_eq!(sanitize_display_name(" A\nB ").as_deref(), Some("A B"));
        assert_eq!(sanitize_display_name(" \n "), None);
    }
}
