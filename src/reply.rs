//! What replies start with, and the plain/HTML pair most of them are.
//!
//! The fleet's replies open with a status emoji. The constants name the
//! vocabulary for new texts; existing texts keep their wording.

use crate::format::html_escape;

/// Done, saved, confirmed.
pub const DONE: &str = "✅";
/// Tried and failed.
pub const FAILED: &str = "❌";
/// Not allowed for this sender.
pub const FORBIDDEN: &str = "⛔";
pub const WARNING: &str = "⚠️";
/// The bot did not understand the message.
pub const NOT_UNDERSTOOD: &str = "🤔";
/// Nothing there (no results, nothing pending).
pub const NOTHING: &str = "🤷";
pub const HINT: &str = "💡";
/// Points to a help command.
pub const HELP: &str = "❔";

/// A refusal for a command only admins may run. One of today's wordings
/// (garbage, quiz); bots that answer differently keep their text until the
/// fleet settles on one.
pub const NOT_ADMIN: &str = "⛔ Only admins can do that.";

/// A text as the plain body and its HTML: escaped, line breaks as `<br>`.
pub fn plain_and_html(plain: impl Into<String>) -> (String, String) {
    let plain = plain.into();
    let html = html_escape(&plain).replace('\n', "<br>");
    (plain, html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_is_escaped_with_line_breaks() {
        assert_eq!(
            plain_and_html("🩺 Status\nA & <b>"),
            (
                "🩺 Status\nA & <b>".to_owned(),
                "🩺 Status<br>A &amp; &lt;b&gt;".to_owned()
            )
        );
        assert_eq!(plain_and_html(NOT_ADMIN).1, NOT_ADMIN);
    }
}
