//! The language of a website's pages: German, English or Ukrainian, as the house's other
//! pages choose it, and the pages' texts as one table of (key, German, English, Ukrainian).
//!
//! A website keeps its own table and its own `t(lang, key)` over [`text`] and [`fill`], and
//! tests in its own crate that every key it uses is in its table.

use http::{header, HeaderMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    De,
    En,
    Uk,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::De, Lang::En, Lang::Uk];

    /// A language tag's language: "de-AT" is German, "uk" and "ua" Ukrainian.
    pub fn from_tag(tag: &str) -> Option<Lang> {
        let primary = tag.trim().split(['-', '_']).next()?.to_ascii_lowercase();
        match primary.as_str() {
            "de" => Some(Lang::De),
            "en" => Some(Lang::En),
            "uk" | "ua" => Some(Lang::Uk),
            _ => None,
        }
    }

    /// For `<html lang>` and `?lang=`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::De => "de",
            Lang::En => "en",
            Lang::Uk => "uk",
        }
    }
}

/// A page's language: `?lang=` in `query` (a page's own switcher; `None` for a website
/// without one), the house-wide `house_lang` cookie the start page sets, the browser's
/// Accept-Language (the highest q, the earlier one among equals; every header line), else
/// `default`. What none of them names in a known language is passed over.
pub fn pick(headers: &HeaderMap, query: Option<&str>, default: Lang) -> Lang {
    if let Some(lang) = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| pair.strip_prefix("lang="))
        .find_map(Lang::from_tag)
    {
        return lang;
    }
    let cookie = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().strip_prefix("house_lang="))
        .find_map(Lang::from_tag);
    if let Some(lang) = cookie {
        return lang;
    }
    let mut wanted: Vec<(f32, usize, Lang)> = headers
        .get_all(header::ACCEPT_LANGUAGE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .enumerate()
        .filter_map(|(i, item)| {
            let mut parts = item.split(';');
            let lang = Lang::from_tag(parts.next()?)?;
            let q = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .find_map(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            (q > 0.0).then_some((q, i, lang))
        })
        .collect();
    // Highest q first; the earlier one among equals.
    wanted.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    wanted.first().map_or(default, |w| w.2)
}

/// A website's texts: (key, German, English, Ukrainian).
pub type Texts = [(&'static str, &'static str, &'static str, &'static str)];

/// The text for `key` in `table`; empty when there is none (and a panic in a debug build:
/// each website tests that every key it uses is there).
pub fn text(table: &'static Texts, lang: Lang, key: &str) -> &'static str {
    match table.iter().find(|(k, ..)| *k == key) {
        Some((_, de, en, uk)) => match lang {
            Lang::De => de,
            Lang::En => en,
            Lang::Uk => uk,
        },
        None => {
            debug_assert!(false, "no text for {key:?}");
            ""
        }
    }
}

/// [`text`] with each `{}` replaced by the next of `args` (missing ones: empty).
pub fn fill(table: &'static Texts, lang: Lang, key: &str, args: &[&str]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut parts = text(table, lang, key).split("{}");
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    for part in parts {
        out.push_str(args.next().copied().unwrap_or(""));
        out.push_str(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use http::{HeaderName, HeaderValue};

    use super::*;

    fn headers(pairs: &[(HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.append(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        h
    }

    fn browser(value: &str) -> HeaderMap {
        headers(&[(header::ACCEPT_LANGUAGE, value)])
    }

    #[test]
    fn tags_and_codes() {
        assert_eq!(Lang::from_tag(" de-AT"), Some(Lang::De));
        assert_eq!(Lang::from_tag("EN_gb"), Some(Lang::En));
        assert_eq!(Lang::from_tag("ua"), Some(Lang::Uk));
        assert_eq!(Lang::from_tag("ru"), None);
        assert_eq!(Lang::from_tag(""), None);
        for lang in Lang::ALL {
            assert_eq!(Lang::from_tag(lang.code()), Some(lang));
        }
    }

    #[test]
    fn switcher_then_cookie_then_browser_then_the_default() {
        let none = HeaderMap::new();
        assert_eq!(pick(&none, None, Lang::De), Lang::De);
        assert_eq!(pick(&none, None, Lang::En), Lang::En);
        let cookie = headers(&[(header::COOKIE, "theme=dark; house_lang=uk")]);
        assert_eq!(pick(&cookie, None, Lang::De), Lang::Uk);
        assert_eq!(pick(&cookie, Some("weeks=2&lang=en"), Lang::De), Lang::En);
        assert_eq!(pick(&cookie, Some("lang=fr"), Lang::De), Lang::Uk);
        // An unknown cookie value: the browser decides.
        let odd = headers(&[
            (header::COOKIE, "house_lang=xx"),
            (header::ACCEPT_LANGUAGE, "en"),
        ]);
        assert_eq!(pick(&odd, None, Lang::De), Lang::En);
        let both = headers(&[
            (header::COOKIE, "other=1; house_lang=de"),
            (header::ACCEPT_LANGUAGE, "en"),
        ]);
        assert_eq!(pick(&both, None, Lang::En), Lang::De);
    }

    #[test]
    fn the_browser_by_weight_then_order() {
        let de = |value: &str| pick(&browser(value), None, Lang::De);
        assert_eq!(de("fr-FR, en-GB;q=0.8, de;q=0.7"), Lang::En);
        assert_eq!(de("uk-UA;q=0.9, de-DE"), Lang::De);
        assert_eq!(de("uk-UA, ru;q=0.8"), Lang::Uk);
        assert_eq!(de("en-GB,en;q=0.9"), Lang::En);
        assert_eq!(de("fr-FR,uk;q=0.8,en;q=0.5"), Lang::Uk);
        assert_eq!(de("en;q=0.3, de;q=0.7"), Lang::De);
        assert_eq!(de("en;q=0, fr"), Lang::De);
        assert_eq!(de("fr, it"), Lang::De);
        assert_eq!(pick(&browser("fr, it"), None, Lang::En), Lang::En);
        // Equal weights: the first named.
        assert_eq!(de("uk;q=0.5, en;q=0.5"), Lang::Uk);
        // Every header line counts, as one list.
        let two = headers(&[
            (header::ACCEPT_LANGUAGE, "fr"),
            (header::ACCEPT_LANGUAGE, "uk"),
        ]);
        assert_eq!(pick(&two, None, Lang::En), Lang::Uk);
    }

    const TEXTS: &Texts = &[
        (
            "page.title",
            "Putzplan",
            "Cleaning plan",
            "Графік прибирання",
        ),
        ("rel.in_days", "in {} Tagen", "in {} days", "через {} днів"),
        ("two", "{} und {}", "{} and {}", "{} і {}"),
    ];

    #[test]
    fn texts_by_key_with_their_placeholders_filled_in_order() {
        assert_eq!(text(TEXTS, Lang::De, "page.title"), "Putzplan");
        assert_eq!(text(TEXTS, Lang::Uk, "page.title"), "Графік прибирання");
        assert_eq!(fill(TEXTS, Lang::En, "rel.in_days", &["3"]), "in 3 days");
        assert_eq!(fill(TEXTS, Lang::De, "two", &["a", "b"]), "a und b");
        assert_eq!(fill(TEXTS, Lang::Uk, "two", &["a"]), "a і ");
        assert_eq!(fill(TEXTS, Lang::En, "page.title", &["x"]), "Cleaning plan");
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "no text"))]
    fn a_missing_key_is_empty_and_a_bug() {
        assert_eq!(text(TEXTS, Lang::En, "no.such"), "");
    }
}
