use clap::{App as Argparse, AppSettings as ArgParseSettings, Arg};
use matrix_sdk::ruma::{EventId, OwnedEventId};
use regex::Regex;
use weechat::{
    buffer::Buffer,
    hooks::{Command, CommandCallback, CommandSettings},
    Args, Prefix, Weechat,
};

use super::parse_and_run;
use crate::Servers;

const EVENT_ID_TAG_PREFIX: &str = "matrix_id_";

/// The pattern of a `/pattern/` or `/pattern/replacement/` argument.
enum Pattern<'a> {
    Substring(&'a str),
    Regex(Regex),
}

impl<'a> Pattern<'a> {
    fn new(pattern: &'a str, regex: bool) -> Result<Self, String> {
        if regex {
            Regex::new(pattern).map(Self::Regex).map_err(|error| {
                format!("Invalid pattern /{pattern}/: {error}")
            })
        } else {
            Ok(Self::Substring(pattern))
        }
    }

    fn is_match(&self, message: &str) -> bool {
        match self {
            Self::Substring(pattern) => message.contains(pattern),
            Self::Regex(regex) => regex.is_match(message),
        }
    }

    fn replace_all(&self, message: &str, replacement: &str) -> String {
        match self {
            Self::Substring(pattern) => message.replace(pattern, replacement),
            Self::Regex(regex) => {
                regex.replace_all(message, replacement).into_owned()
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PatternEdit<'a> {
    pattern: &'a str,
    message: &'a str,
}

pub struct EditCommand {
    servers: Servers,
}

impl EditCommand {
    pub fn create(servers: &Servers) -> Result<Command, ()> {
        let settings = CommandSettings::new("edit")
            .description("Edit a Matrix event in the current room.")
            .add_argument("[event-id|index|/pattern/] <message>")
            .arguments_description(
                "event-id: The Matrix event ID to edit.
    index: 1-based recent unredacted event index. 1, 0, or -1 select the latest \
           event; 2 or -2 select the event before that.
  pattern: A case-sensitive substring enclosed in slashes, or a regular \
           expression with matrix-rust.input.edit_regex on. The most recent \
           unredacted message matching it is selected.
  message: New plain-text message body. The shorthand /pattern/replacement/ \
           selects the most recent matching message and replaces that text; \
           an empty replacement deletes it. Typing s/pattern/replacement/ as \
           a message does the same (matrix-rust.input.sed_edit).",
            );

        Command::new(
            settings,
            EditCommand {
                servers: servers.clone(),
            },
        )
    }

    fn latest_event_id(buffer: &Buffer) -> Option<OwnedEventId> {
        Self::event_id_at_index(buffer, 1)
    }

    fn event_id_at_index(
        buffer: &Buffer,
        index: usize,
    ) -> Option<OwnedEventId> {
        buffer
            .lines()
            .rev()
            .filter_map(|line| {
                let tags = line.tags();

                if tags.iter().any(|tag| tag.as_ref() == "matrix_redacted") {
                    return None;
                }

                tags.iter()
                    .find_map(|tag| {
                        tag.as_ref().strip_prefix(EVENT_ID_TAG_PREFIX)
                    })
                    .and_then(|event_id| EventId::parse(event_id).ok())
            })
            .nth(index.saturating_sub(1))
    }

    fn parse_index(argument: &str) -> Option<usize> {
        let index = argument.parse::<isize>().ok()?;

        match index {
            0 | -1 => Some(1),
            n if n > 0 => Some(n as usize),
            n => n.checked_abs().map(|n| n as usize),
        }
    }

    fn parse_pattern(argument: &str) -> Result<Option<&str>, String> {
        let Some(pattern) = argument
            .strip_prefix('/')
            .and_then(|argument| argument.strip_suffix('/'))
        else {
            return Ok(None);
        };

        if pattern.is_empty() {
            Err("The edit pattern cannot be empty.".to_owned())
        } else {
            Ok(Some(pattern))
        }
    }

    fn parse_pattern_edit(
        argument: &str,
    ) -> Result<Option<PatternEdit<'_>>, String> {
        let Some(argument) = argument.strip_prefix('/') else {
            return Ok(None);
        };
        let Some((pattern, replacement)) = argument.split_once('/') else {
            return Ok(None);
        };
        let Some(replacement) = replacement.strip_suffix('/') else {
            return Ok(None);
        };

        if pattern.is_empty() {
            Err("The edit pattern cannot be empty.".to_owned())
        } else {
            Ok(Some(PatternEdit {
                pattern,
                message: replacement,
            }))
        }
    }

    fn event_id_matching_pattern(
        buffer: &Buffer,
        pattern: &Pattern<'_>,
    ) -> Option<OwnedEventId> {
        Self::event_matching_pattern(buffer, pattern)
            .map(|(event_id, _)| event_id)
    }

    fn event_matching_pattern(
        buffer: &Buffer,
        pattern: &Pattern<'_>,
    ) -> Option<(OwnedEventId, String)> {
        buffer.lines().rev().find_map(|line| {
            let tags = line.tags();
            let message = Weechat::remove_color(&line.message());

            if tags.iter().any(|tag| tag.as_ref() == "matrix_redacted")
                || !pattern.is_match(&message)
            {
                return None;
            }

            tags.iter()
                .find_map(|tag| tag.as_ref().strip_prefix(EVENT_ID_TAG_PREFIX))
                .and_then(|event_id| EventId::parse(event_id).ok())
                .map(|event_id| (event_id, message))
        })
    }

    fn apply_pattern_edit(
        message: &str,
        pattern: &Pattern<'_>,
        edit: &PatternEdit<'_>,
    ) -> String {
        pattern.replace_all(message, edit.message)
    }

    pub(crate) fn parse_arguments(
        buffer: &Buffer,
        arguments: Option<Vec<&str>>,
        regex: bool,
    ) -> Result<(OwnedEventId, String), String> {
        let Some(arguments) =
            arguments.filter(|arguments| !arguments.is_empty())
        else {
            return Err(
                "Usage: /edit [event-id|index|/pattern/] <message>".to_owned()
            );
        };

        let Some((first, rest)) = arguments.split_first() else {
            unreachable!("empty arguments were filtered above");
        };

        if first.starts_with('$') && EventId::parse(*first).is_err() {
            return Err(format!("Invalid Matrix event ID: {}", first));
        }

        let (event_id, message) = if let Ok(event_id) = EventId::parse(*first) {
            (event_id, rest.join(" "))
        } else if let Some(index) = Self::parse_index(first) {
            let event_id =
                Self::event_id_at_index(buffer, index).ok_or_else(|| {
                    format!("No Matrix event found at edit index {}.", first)
                })?;

            (event_id, rest.join(" "))
        } else if rest.is_empty() {
            if let Some(pattern_edit) = Self::parse_pattern_edit(first)? {
                let pattern = Pattern::new(pattern_edit.pattern, regex)?;
                let (event_id, original_message) =
                    Self::event_matching_pattern(buffer, &pattern).ok_or_else(
                        || {
                            format!(
                                "No unredacted Matrix event matches /{}/.",
                                pattern_edit.pattern
                            )
                        },
                    )?;

                (
                    event_id,
                    Self::apply_pattern_edit(
                        &original_message,
                        &pattern,
                        &pattern_edit,
                    ),
                )
            } else if let Some(pattern) = Self::parse_pattern(first)? {
                return Err(format!(
                    "Edit message cannot be empty for /{}/.",
                    pattern
                ));
            } else {
                let event_id =
                    Self::latest_event_id(buffer).ok_or_else(|| {
                        "No Matrix event found to edit.".to_owned()
                    })?;

                (event_id, arguments.join(" "))
            }
        } else if let Some(pattern) = Self::parse_pattern(first)? {
            let event_id = Self::event_id_matching_pattern(
                buffer,
                &Pattern::new(pattern, regex)?,
            )
            .ok_or_else(|| {
                format!("No unredacted Matrix event matches /{}/.", pattern)
            })?;

            (event_id, rest.join(" "))
        } else {
            let event_id = Self::latest_event_id(buffer)
                .ok_or_else(|| "No Matrix event found to edit.".to_owned())?;

            (event_id, arguments.join(" "))
        };

        if message.is_empty() {
            Err("Edit message cannot be empty.".to_owned())
        } else {
            Ok((event_id, message))
        }
    }

    fn parser() -> Argparse<'static, 'static> {
        Argparse::new("edit")
            .settings(&[
                ArgParseSettings::AllowLeadingHyphen,
                ArgParseSettings::DisableHelpFlags,
                ArgParseSettings::DisableVersion,
            ])
            .arg(Arg::with_name("arguments").multiple(true))
    }

    fn edit_regex(&self, buffer: &Buffer) -> bool {
        self.servers
            .find_room(buffer)
            .is_some_and(|room| room.edit_regex())
    }

    fn edit(&self, buffer: &Buffer, event_id: OwnedEventId, message: String) {
        if let Some(room) = self.servers.find_room(buffer) {
            Weechat::spawn(
                async move { room.send_edit(event_id, message).await },
            )
            .detach();
        } else {
            Weechat::print(
                "The /edit command needs to be run in a Matrix room buffer.",
            );
        }
    }
}

impl CommandCallback for EditCommand {
    fn callback(&mut self, _: &Weechat, buffer: &Buffer, arguments: Args) {
        parse_and_run(Self::parser(), arguments, |args| {
            match Self::parse_arguments(
                buffer,
                args.values_of("arguments").map(|v| v.collect()),
                self.edit_regex(buffer),
            ) {
                Ok((event_id, message)) => self.edit(buffer, event_id, message),
                Err(error) => buffer.print(&format!(
                    "{}matrix: {}",
                    Weechat::prefix(Prefix::Error),
                    error
                )),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{EditCommand, Pattern, PatternEdit};

    #[test]
    fn edit_index_accepts_recent_event_forms() {
        assert_eq!(Some(1), EditCommand::parse_index("1"));
        assert_eq!(Some(1), EditCommand::parse_index("0"));
        assert_eq!(Some(1), EditCommand::parse_index("-1"));
        assert_eq!(Some(2), EditCommand::parse_index("2"));
        assert_eq!(Some(2), EditCommand::parse_index("-2"));
        assert_eq!(None, EditCommand::parse_index("abc"));
    }

    #[test]
    fn edit_pattern_uses_slash_delimiters() {
        assert_eq!(Ok(Some("needle")), EditCommand::parse_pattern("/needle/"));
        assert_eq!(Ok(None), EditCommand::parse_pattern("needle"));
        assert_eq!(Ok(None), EditCommand::parse_pattern("/needle"));
    }

    #[test]
    fn edit_pattern_rejects_empty_match() {
        assert_eq!(
            Err("The edit pattern cannot be empty.".to_owned()),
            EditCommand::parse_pattern("//")
        );
    }

    #[test]
    fn edit_pattern_replacement_uses_sed_like_delimiters() {
        assert_eq!(
            Ok(Some(PatternEdit {
                pattern: "teh",
                message: "the",
            })),
            EditCommand::parse_pattern_edit("/teh/the/")
        );
        assert_eq!(Ok(None), EditCommand::parse_pattern_edit("teh/the/"));
        assert_eq!(Ok(None), EditCommand::parse_pattern_edit("/teh/the"));
    }

    #[test]
    fn edit_pattern_replacement_rejects_empty_pattern() {
        assert_eq!(
            Err("The edit pattern cannot be empty.".to_owned()),
            EditCommand::parse_pattern_edit("//the/")
        );
    }

    #[test]
    fn edit_pattern_with_empty_replacement_deletes_text() {
        let edit = EditCommand::parse_pattern_edit("/ mako//")
            .unwrap()
            .expect("pattern edit");
        assert_eq!(
            PatternEdit {
                pattern: " mako",
                message: "",
            },
            edit
        );
        assert_eq!(
            "hi there",
            EditCommand::apply_pattern_edit(
                "hi mako there",
                &Pattern::Substring(edit.pattern),
                &edit
            )
        );
    }

    #[test]
    fn edit_pattern_replacement_updates_matching_message_text() {
        let edit = PatternEdit {
            pattern: "teh",
            message: "the",
        };

        assert_eq!(
            "the cat ate the kibble",
            EditCommand::apply_pattern_edit(
                "teh cat ate teh kibble",
                &Pattern::Substring(edit.pattern),
                &edit
            )
        );
    }

    #[test]
    fn regex_patterns_match_and_replace_with_groups() {
        let edit = PatternEdit {
            pattern: r"(\w+) cat",
            message: "$1 dog",
        };
        let pattern = Pattern::new(edit.pattern, true).unwrap();

        assert!(pattern.is_match("teh cat ate"));
        assert!(!pattern.is_match("cat"));
        assert_eq!(
            "teh dog ate teh kibble",
            EditCommand::apply_pattern_edit(
                "teh cat ate teh kibble",
                &pattern,
                &edit
            )
        );
        assert!(Pattern::new("(", true).is_err());
        assert!(Pattern::new("(", false).unwrap().is_match("a ( b"));
    }

    #[test]
    fn edit_pattern_matches_message_substrings() {
        let pattern = Pattern::Substring("/edit");
        assert!(pattern.is_match("Something like /edit /pattern/replacement/"));
        assert!(!pattern.is_match("Something like /reply /pattern/"));
    }

    #[test]
    fn command_parser_accepts_negative_edit_indexes() {
        let matches = EditCommand::parser()
            .get_matches_from_safe(vec!["edit", "-2", "message"])
            .unwrap();
        let arguments: Vec<_> =
            matches.values_of("arguments").unwrap().collect();

        assert_eq!(vec!["-2", "message"], arguments);
    }

    #[test]
    fn command_parser_accepts_pattern_replacement() {
        let matches = EditCommand::parser()
            .get_matches_from_safe(vec!["edit", "/teh/the/"])
            .unwrap();
        let arguments: Vec<_> =
            matches.values_of("arguments").unwrap().collect();

        assert_eq!(vec!["/teh/the/"], arguments);
    }
}
