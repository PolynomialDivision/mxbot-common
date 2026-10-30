//! Chat commands from a static table: parsing, help, and who may run them.
//!
//! A bot describes its commands once, as a `const` [`Commands`] table of
//! [`Spec`]s, and keeps its own typed command enum and handlers.
//! [`Commands::parse`] says which spec a message invokes; [`Commands::help`]
//! writes the help from the same table, so the two cannot drift apart.
//!
//! Two shapes:
//!   * a family, `!garbage <verb> …` ([`Commands::family`]): a spec named `""`
//!     is the family word alone, and an unknown verb is
//!     [`Parsed::Unknown`], for the bot to answer;
//!   * flat heads, `!today`, `!week` ([`Commands::flat`]): an unknown head is
//!     [`Parsed::NotOurs`]. Bots share rooms, and one bot's command is unknown
//!     to the next, so only a bot's own family answers what it does not know.
//!
//! Head and verb are compared ASCII case-insensitively unless the table says
//! [`Commands::case_sensitive`]. What follows the verb is left to the bot
//! ([`Invocation::rest`]).

use crate::format;

/// Who may run a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Everyone,
    /// Admins; others are refused ([`Parsed::Refused`]), for the bot to answer.
    Admin,
    /// Admins; others are ignored ([`Parsed::NotOurs`]), as if the command did
    /// not exist.
    AdminSilent,
}

/// How heads and verbs are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// `!Garbage Next` is `!garbage next` (ASCII letters only).
    Insensitive,
    /// Only the spelling in the table counts.
    Sensitive,
}

impl Case {
    fn eq(self, written: &str, name: &str) -> bool {
        match self {
            Case::Insensitive => written.eq_ignore_ascii_case(name),
            Case::Sensitive => written == name,
        }
    }
}

/// One command: its name, the words that mean the same, and its help line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    /// The verb in a family (`"next"` for `!garbage next`, `""` for `!garbage`
    /// alone), or the head without `!` (`"today"`).
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// Shown after the command in the help: `"[hours]"`, `"<topic>"`.
    pub usage: &'static str,
    pub emoji: &'static str,
    pub help: &'static str,
    pub access: Access,
    /// Runs, but is not listed in the help.
    pub hidden: bool,
}

impl Spec {
    pub const fn new(name: &'static str, emoji: &'static str, help: &'static str) -> Self {
        Self {
            name,
            aliases: &[],
            usage: "",
            emoji,
            help,
            access: Access::Everyone,
            hidden: false,
        }
    }

    pub const fn aliases(self, aliases: &'static [&'static str]) -> Self {
        Self { aliases, ..self }
    }

    pub const fn usage(self, usage: &'static str) -> Self {
        Self { usage, ..self }
    }

    pub const fn admin(self) -> Self {
        Self {
            access: Access::Admin,
            ..self
        }
    }

    pub const fn admin_silent(self) -> Self {
        Self {
            access: Access::AdminSilent,
            ..self
        }
    }

    pub const fn hidden(self) -> Self {
        Self {
            hidden: true,
            ..self
        }
    }

    fn answers_to(&self, word: &str, case: Case) -> bool {
        case.eq(word, self.name) || self.aliases.iter().any(|alias| case.eq(word, alias))
    }
}

/// What a message is, for one bot's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parsed<'a> {
    /// A command of the table that the sender may run.
    Run(Invocation<'a>),
    /// A command for admins ([`Access::Admin`]) from someone else.
    Refused(&'static Spec),
    /// The bot's family with a verb it does not know (`""`: the family word
    /// alone, and the table has no spec for that), as written.
    Unknown(&'a str),
    /// Not a command of this bot: stay silent.
    NotOurs,
}

/// A recognised command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invocation<'a> {
    pub spec: &'static Spec,
    /// The verb or head as written (an alias, other case), without `!`.
    pub word: &'a str,
    /// Everything after it, trimmed; `""` when nothing follows.
    pub rest: &'a str,
}

impl<'a> Invocation<'a> {
    /// Whether this runs the spec named `name`.
    pub fn is(&self, name: &str) -> bool {
        self.spec.name == name
    }

    /// The words after the command.
    pub fn args(&self) -> std::str::SplitWhitespace<'a> {
        self.rest.split_whitespace()
    }
}

/// One bot's command table.
#[derive(Clone, Copy, Debug)]
pub struct Commands {
    family: Option<&'static str>,
    specs: &'static [Spec],
    case: Case,
    strip_reply_fallback: bool,
    separator: &'static str,
}

impl Commands {
    /// `!<family> <verb> …`, the family word without `!`.
    pub const fn family(family: &'static str, specs: &'static [Spec]) -> Self {
        Self::new(Some(family), specs)
    }

    /// `!<head> …` for each spec.
    pub const fn flat(specs: &'static [Spec]) -> Self {
        Self::new(None, specs)
    }

    const fn new(family: Option<&'static str>, specs: &'static [Spec]) -> Self {
        Self {
            family,
            specs,
            case: Case::Insensitive,
            strip_reply_fallback: true,
            separator: ": ",
        }
    }

    /// Heads and verbs only in the table's spelling.
    pub const fn case_sensitive(self) -> Self {
        Self {
            case: Case::Sensitive,
            ..self
        }
    }

    /// Parse the body as it is: a reply's quoted fallback (`> <@a:b> …` at the
    /// top) is not removed first, so such a reply is no command.
    pub const fn raw_body(self) -> Self {
        Self {
            strip_reply_fallback: false,
            ..self
        }
    }

    /// Between a command and its description in the help; `": "` by default.
    pub const fn separator(self, separator: &'static str) -> Self {
        Self { separator, ..self }
    }

    pub fn specs(&self) -> &'static [Spec] {
        self.specs
    }

    /// The spec a verb (in a family) or head (without `!`) names.
    pub fn find(&self, word: &str) -> Option<&'static Spec> {
        self.specs
            .iter()
            .find(|spec| spec.answers_to(word, self.case))
    }

    /// What `body` invokes, without looking at who sent it: for bots that
    /// check access themselves. [`Parsed::Refused`] never comes from here.
    pub fn parse<'a>(&self, body: &'a str) -> Parsed<'a> {
        self.parse_as(body, || true)
    }

    /// What `body` invokes for a sender; `is_admin` is asked only for a
    /// command that is not for everyone.
    pub fn parse_as<'a>(&self, body: &'a str, is_admin: impl FnOnce() -> bool) -> Parsed<'a> {
        let body = if self.strip_reply_fallback {
            format::strip_reply_fallback(body)
        } else {
            body
        };
        let (head, rest) = split_word(body.trim());
        let Some(head) = head.strip_prefix('!') else {
            return Parsed::NotOurs;
        };
        let (word, rest) = match self.family {
            Some(family) if self.case.eq(head, family) => split_word(rest),
            Some(_) => return Parsed::NotOurs,
            None if head.is_empty() => return Parsed::NotOurs,
            None => (head, rest),
        };
        let Some(spec) = self.find(word) else {
            return match self.family {
                Some(_) => Parsed::Unknown(word),
                None => Parsed::NotOurs,
            };
        };
        if spec.access == Access::Everyone || is_admin() {
            Parsed::Run(Invocation { spec, word, rest })
        } else if spec.access == Access::AdminSilent {
            Parsed::NotOurs
        } else {
            Parsed::Refused(spec)
        }
    }

    /// How a spec is written: `!garbage next`, `!garbage`, `!today`.
    pub fn command(&self, spec: &Spec) -> String {
        match (self.family, spec.name) {
            (Some(family), "") => format!("!{family}"),
            (Some(family), name) => format!("!{family} {name}"),
            (None, name) => format!("!{name}"),
        }
    }

    /// `emoji !command usage: help`.
    pub fn help_line(&self, spec: &Spec) -> String {
        let mut line = String::new();
        if !spec.emoji.is_empty() {
            line.push_str(spec.emoji);
            line.push(' ');
        }
        line.push_str(&self.command(spec));
        if !spec.usage.is_empty() {
            line.push(' ');
            line.push_str(spec.usage);
        }
        line.push_str(self.separator);
        line.push_str(spec.help);
        line
    }

    /// `title`, then a line for each spec that is not hidden, in table order.
    pub fn help(&self, title: &str) -> String {
        let lines = self.lines(|_| true);
        format!("{title}\n{lines}")
    }

    /// The lines of the admin commands, for `BotBuilder::admin_help`.
    pub fn admin_help(&self) -> String {
        self.lines(|spec| spec.access != Access::Everyone)
    }

    fn lines(&self, wanted: impl Fn(&Spec) -> bool) -> String {
        self.specs
            .iter()
            .filter(|spec| !spec.hidden && wanted(spec))
            .map(|spec| self.help_line(spec))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The first word and the rest after it, trimmed at the front.
fn split_word(text: &str) -> (&str, &str) {
    match text.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim_start()),
        None => (text, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY: Commands = Commands::family(
        "garbage",
        &[
            Spec::new("", "📋", "next per type"),
            Spec::new("next", "⏭️", "next pickup").aliases(&["n"]),
            Spec::new("refresh", "🔄", "reload").admin(),
            Spec::new("debug", "🐞", "internals").admin_silent(),
            Spec::new("help", "", "this help").aliases(&["?"]).hidden(),
        ],
    );

    const FLAT: Commands = Commands::flat(&[
        Spec::new("today", "☀️", "today's events"),
        Spec::new("source-stats", "📊", "sources")
            .usage("[hours]")
            .admin(),
    ])
    .case_sensitive()
    .separator(" · ");

    fn run<'a>(table: &Commands, body: &'a str) -> Option<(&'static str, &'a str)> {
        match table.parse(body) {
            Parsed::Run(inv) => Some((inv.spec.name, inv.rest)),
            _ => None,
        }
    }

    #[test]
    fn family_verbs_aliases_and_rest() {
        assert_eq!(run(&FAMILY, "!garbage"), Some(("", "")));
        assert_eq!(run(&FAMILY, "  !Garbage  "), Some(("", "")));
        assert_eq!(run(&FAMILY, "!garbage NEXT"), Some(("next", "")));
        assert_eq!(run(&FAMILY, "!garbage n  a  b "), Some(("next", "a  b")));
        assert_eq!(run(&FAMILY, "!garbage\tnext"), Some(("next", "")));
        assert_eq!(run(&FAMILY, "!garbage ?"), Some(("help", "")));
        let Parsed::Run(inv) = FAMILY.parse("!GARBAGE N x y") else {
            panic!("not run");
        };
        assert_eq!((inv.word, inv.is("next")), ("N", true));
        assert_eq!(inv.args().collect::<Vec<_>>(), ["x", "y"]);
    }

    #[test]
    fn unknown_only_inside_the_family() {
        assert_eq!(FAMILY.parse("!garbage Bogus x"), Parsed::Unknown("Bogus"));
        assert_eq!(FAMILY.parse("!garbagefoo"), Parsed::NotOurs);
        assert_eq!(FAMILY.parse("garbage next"), Parsed::NotOurs);
        assert_eq!(FAMILY.parse("!next"), Parsed::NotOurs);
        assert_eq!(FAMILY.parse(""), Parsed::NotOurs);
        assert_eq!(FLAT.parse("!tomorrow"), Parsed::NotOurs);
        assert_eq!(FLAT.parse("!"), Parsed::NotOurs);
        const BARE: Commands = Commands::family("x", &[Spec::new("a", "", "")]);
        assert_eq!(BARE.parse("!x"), Parsed::Unknown(""));
    }

    #[test]
    fn case_sensitive_tables_take_only_their_spelling() {
        assert_eq!(run(&FLAT, "!today"), Some(("today", "")));
        assert_eq!(run(&FLAT, "!Today"), None);
        assert_eq!(run(&FLAT, "!source-stats 24"), Some(("source-stats", "24")));
        assert_eq!(FLAT.parse("!source-statsX"), Parsed::NotOurs);
    }

    #[test]
    fn access_asks_for_admins_only_when_needed() {
        let refresh = FAMILY.find("refresh").unwrap();
        assert_eq!(
            FAMILY.parse_as("!garbage refresh", || false),
            Parsed::Refused(refresh)
        );
        assert_eq!(FAMILY.parse_as("!garbage debug", || false), Parsed::NotOurs);
        assert!(matches!(
            FAMILY.parse_as("!garbage debug", || true),
            Parsed::Run(_)
        ));
        let next = FAMILY.parse_as("!garbage next", || panic!("asked for a public command"));
        assert!(matches!(next, Parsed::Run(_)));
        // `parse` leaves access to the bot.
        assert!(matches!(FAMILY.parse("!garbage refresh"), Parsed::Run(_)));
    }

    #[test]
    fn reply_fallback_is_stripped_unless_the_table_says_raw() {
        let reply = "> <@a:x> what bins?\n> more\n\n!garbage next";
        assert_eq!(run(&FAMILY, reply), Some(("next", "")));
        assert_eq!(FAMILY.raw_body().parse(reply), Parsed::NotOurs);
    }

    #[test]
    fn help_lists_visible_specs_in_order() {
        assert_eq!(
            FAMILY.help("🗑️ Garbage commands"),
            "🗑️ Garbage commands\n\
             📋 !garbage: next per type\n\
             ⏭️ !garbage next: next pickup\n\
             🔄 !garbage refresh: reload\n\
             🐞 !garbage debug: internals"
        );
        assert_eq!(
            FAMILY.admin_help(),
            "🔄 !garbage refresh: reload\n🐞 !garbage debug: internals"
        );
        assert_eq!(
            FLAT.help("**Calendar**\n"),
            "**Calendar**\n\n☀️ !today · today's events\n📊 !source-stats [hours] · sources"
        );
        const PLAIN: Commands = Commands::flat(&[Spec::new("x", "", "does x")]);
        assert_eq!(PLAIN.help_line(&PLAIN.specs()[0]), "!x: does x");
    }
}
