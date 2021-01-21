use std::{convert::TryFrom, fmt::Debug, io, sync::Arc};

use futures::executor::block_on;
use serde::Serialize;

use atty::Stream;
use clap::{App as Argparse, AppSettings as ArgParseSettings, Arg, ArgMatches, SubCommand};
use rustyline::{
    completion::{Completer, Pair},
    error::ReadlineError,
    highlight::{Highlighter, MatchingBracketHighlighter},
    hint::{Hinter, HistoryHinter},
    validate::{MatchingBracketValidator, Validator},
    CompletionType, Config, Context, EditMode, Editor, OutputStreamType,
};
use rustyline_derive::Helper;

use syntect::{
    dumps::from_binary,
    easy::HighlightLines,
    highlighting::{Style, ThemeSet},
    parsing::SyntaxSet,
    util::{as_24_bit_terminal_escaped, LinesWithEndings},
};

use matrix_sdk_base::{
    events::EventType,
    identifiers::{RoomId, UserId},
    RoomInfo, Store,
};

#[derive(Clone)]
struct Inspector {
    store: Store,
    printer: Printer,
}

#[derive(Helper)]
struct InspectorHelper {
    store: Store,
    _highlighter: MatchingBracketHighlighter,
    _validator: MatchingBracketValidator,
    _hinter: HistoryHinter,
}

impl InspectorHelper {
    const EVENT_TYPES: &'static [&'static str] = &[
        "m.room.aliases",
        "m.room.avatar",
        "m.room.canonical_alias",
        "m.room.create",
        "m.room.encryption",
        "m.room.guest_access",
        "m.room.history_visibility",
        "m.room.join_rules",
        "m.room.name",
        "m.room.power_levels",
        "m.room.tombstone",
        "m.room.topic",
    ];

    fn new(store: Store) -> Self {
        Self {
            store,
            _highlighter: MatchingBracketHighlighter::new(),
            _validator: MatchingBracketValidator::new(),
            _hinter: HistoryHinter {},
        }
    }

    fn complete_event_types(&self, arg: Option<&&str>) -> Vec<Pair> {
        Self::EVENT_TYPES
            .iter()
            .map(|t| Pair {
                display: t.to_string(),
                replacement: format!("{} ", t),
            })
            .filter(|r| {
                if let Some(arg) = arg {
                    r.replacement.starts_with(arg)
                } else {
                    true
                }
            })
            .collect()
    }

    fn complete_rooms(&self, arg: Option<&&str>) -> Vec<Pair> {
        let rooms: Vec<RoomInfo> = block_on(async { self.store.get_room_infos().await.unwrap() });

        rooms
            .into_iter()
            .map(|r| Pair {
                display: r.room_id.to_string(),
                replacement: format!("{} ", r.room_id.to_string()),
            })
            .filter(|r| {
                if let Some(arg) = arg {
                    r.replacement.starts_with(arg)
                } else {
                    true
                }
            })
            .collect()
    }
}

impl Completer for InspectorHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &Context<'_>,
    ) -> Result<(usize, Vec<Pair>), ReadlineError> {
        let args: Vec<&str> = line.split_ascii_whitespace().collect();

        let commands = vec![
            ("get-state", "get a state event in the given room"),
            (
                "get-profiles",
                "get all the stored profiles in the given room",
            ),
            ("list-rooms", "list all rooms"),
            (
                "get-members",
                "get all the membership events in the given room",
            ),
        ]
        .iter()
        .map(|(r, d)| Pair {
            display: format!("{} ({})", r, d),
            replacement: format!("{} ", r.to_string()),
        })
        .collect();

        if args.is_empty() {
            Ok((pos, commands))
        } else if args.len() == 1 {
            if (args[0] == "get-state" || args[0] == "get-members" || args[0] == "get-profiles")
                && line.ends_with(' ')
            {
                Ok((args[0].len() + 1, self.complete_rooms(args.get(1))))
            } else {
                Ok((
                    0,
                    commands
                        .into_iter()
                        .filter(|c| c.replacement.starts_with(args[0]))
                        .collect(),
                ))
            }
        } else if args.len() == 2 {
            if args[0] == "get-state" {
                if line.ends_with(' ') {
                    Ok((
                        args[0].len() + args[1].len() + 2,
                        self.complete_event_types(args.get(2)),
                    ))
                } else {
                    Ok((args[0].len() + 1, self.complete_rooms(args.get(1))))
                }
            } else if args[0] == "get-members" || args[0] == "get-profiles" {
                Ok((args[0].len() + 1, self.complete_rooms(args.get(1))))
            } else {
                Ok((pos, vec![]))
            }
        } else if args.len() == 3 {
            if args[0] == "get-state" {
                Ok((
                    args[0].len() + args[1].len() + 2,
                    self.complete_event_types(args.get(2)),
                ))
            } else {
                Ok((pos, vec![]))
            }
        } else {
            Ok((pos, vec![]))
        }
    }
}

impl Hinter for InspectorHelper {
    type Hint = String;
}

impl Highlighter for InspectorHelper {}

impl Validator for InspectorHelper {}

#[derive(Clone, Debug)]
struct Printer {
    ps: Arc<SyntaxSet>,
    ts: Arc<ThemeSet>,
    json: bool,
    color: bool,
}

impl Printer {
    fn new(json: bool, color: bool) -> Self {
        let syntax_set: SyntaxSet = from_binary(include_bytes!("./syntaxes.bin"));
        let themes: ThemeSet = from_binary(include_bytes!("./themes.bin"));

        Self {
            ps: syntax_set.into(),
            ts: themes.into(),
            json,
            color,
        }
    }

    fn pretty_print_struct<T: Debug + Serialize>(&self, data: &T) {
        let data = if self.json {
            serde_json::to_string_pretty(data).expect("Can't serialize struct")
        } else {
            format!("{:#?}", data)
        };

        let syntax = if self.json {
            self.ps
                .find_syntax_by_extension("rs")
                .expect("Can't find rust syntax extension")
        } else {
            self.ps
                .find_syntax_by_extension("json")
                .expect("Can't find json syntax extension")
        };

        if self.color {
            let mut h = HighlightLines::new(syntax, &self.ts.themes["Forest Night"]);

            for line in LinesWithEndings::from(&data) {
                let ranges: Vec<(Style, &str)> = h.highlight(line, &self.ps);
                let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                print!("{}", escaped);
            }

            // Clear the formatting
            println!("\x1b[0m");
        } else {
            println!("{}", data);
        }
    }
}

impl Inspector {
    fn new(database_path: &str, json: bool, color: bool) -> Self {
        let printer = Printer::new(json, color);
        let store = Store::open_default(database_path, None).unwrap();

        Self { store, printer }
    }

    async fn run(&self, matches: ArgMatches<'_>) {
        match matches.subcommand() {
            ("get-profiles", args) => {
                let args = args.expect("No args provided for get-state");
                let room_id = RoomId::try_from(args.value_of("room-id").unwrap()).unwrap();

                self.get_profiles(room_id).await;
            }

            ("get-members", args) => {
                let args = args.expect("No args provided for get-state");
                let room_id = RoomId::try_from(args.value_of("room-id").unwrap()).unwrap();

                self.get_members(room_id).await;
            }
            ("list-rooms", _) => self.list_rooms().await,
            ("get-state", args) => {
                let args = args.expect("No args provided for get-state");
                let room_id = RoomId::try_from(args.value_of("room-id").unwrap()).unwrap();
                let event_type = EventType::try_from(args.value_of("event-type").unwrap()).unwrap();
                self.get_state(room_id, event_type).await;
            }
            _ => unreachable!(),
        }
    }

    async fn list_rooms(&self) {
        let rooms: Vec<RoomInfo> = self.store.get_room_infos().await.unwrap();
        self.printer.pretty_print_struct(&rooms);
    }

    async fn get_profiles(&self, room_id: RoomId) {
        let joined: Vec<UserId> = self.store.get_joined_user_ids(&room_id).await.unwrap();

        for member in joined {
            let event = self.store.get_profile(&room_id, &member).await.unwrap();
            self.printer.pretty_print_struct(&event);
        }
    }

    async fn get_members(&self, room_id: RoomId) {
        let joined: Vec<UserId> = self.store.get_joined_user_ids(&room_id).await.unwrap();

        for member in joined {
            let event = self
                .store
                .get_member_event(&room_id, &member)
                .await
                .unwrap();
            self.printer.pretty_print_struct(&event);
        }
    }

    async fn get_state(&self, room_id: RoomId, event_type: EventType) {
        self.printer.pretty_print_struct(
            &self
                .store
                .get_state_event(&room_id, event_type, "")
                .await
                .unwrap(),
        );
    }

    fn subcommands() -> Vec<Argparse<'static, 'static>> {
        vec![
            SubCommand::with_name("list-rooms"),
            SubCommand::with_name("get-members").arg(
                Arg::with_name("room-id").required(true).validator(|r| {
                    RoomId::try_from(r)
                        .map(|_| ())
                        .map_err(|_| "Invalid room id given".to_owned())
                }),
            ),
            SubCommand::with_name("get-profiles").arg(
                Arg::with_name("room-id").required(true).validator(|r| {
                    RoomId::try_from(r)
                        .map(|_| ())
                        .map_err(|_| "Invalid room id given".to_owned())
                }),
            ),
            SubCommand::with_name("get-state")
                .arg(Arg::with_name("room-id").required(true).validator(|r| {
                    RoomId::try_from(r)
                        .map(|_| ())
                        .map_err(|_| "Invalid room id given".to_owned())
                }))
                .arg(Arg::with_name("event-type").required(true).validator(|e| {
                    EventType::try_from(e)
                        .map(|_| ())
                        .map_err(|_| "Invalid event type".to_string())
                })),
        ]
    }

    async fn parse_and_run(&self, input: &str) {
        let argparse = Argparse::new("state-inspector")
            .global_setting(ArgParseSettings::DisableHelpFlags)
            .global_setting(ArgParseSettings::DisableVersion)
            .global_setting(ArgParseSettings::VersionlessSubcommands)
            .global_setting(ArgParseSettings::NoBinaryName)
            .setting(ArgParseSettings::SubcommandRequiredElseHelp)
            .subcommands(Inspector::subcommands());

        match argparse.get_matches_from_safe(input.split_ascii_whitespace()) {
            Ok(m) => {
                self.run(m).await;
            }
            Err(e) => {
                println!("{}", e);
            }
        }
    }
}

fn main() -> io::Result<()> {
    let argparse = Argparse::new("state-inspector")
        .global_setting(ArgParseSettings::DisableVersion)
        .global_setting(ArgParseSettings::VersionlessSubcommands)
        .arg(Arg::with_name("database").required(true))
        .arg(
            Arg::with_name("json")
                .long("json")
                .help("set the output to raw json instead of Rust structs")
                .global(true)
                .takes_value(false),
        )
        .subcommands(Inspector::subcommands());

    let matches = argparse.get_matches();

    let database_path = matches.args.get("database").expect("No database path");
    let json = matches.is_present("json");
    let color = atty::is(Stream::Stdout);

    let inspector = Inspector::new(&database_path.vals[0].to_string_lossy(), json, color);

    if matches.subcommand.is_none() {
        let config = Config::builder()
            .history_ignore_space(true)
            .completion_type(CompletionType::List)
            .edit_mode(EditMode::Emacs)
            .output_stream(OutputStreamType::Stdout)
            .build();

        let helper = InspectorHelper::new(inspector.store.clone());

        let mut rl = Editor::<InspectorHelper>::with_config(config);
        rl.set_helper(Some(helper));

        while let Ok(input) = rl.readline(">> ") {
            rl.add_history_entry(input.as_str());
            block_on(inspector.parse_and_run(input.as_str()));
        }
    } else {
        block_on(inspector.run(matches));
    }

    Ok(())
}
