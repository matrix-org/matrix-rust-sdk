use std::{
    collections::{BTreeMap, HashMap},
    fmt, fs,
    io::{self, BufRead, Write},
    ops::Not,
    path,
    str::FromStr,
};

use chrono::{DateTime, FixedOffset};
use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};

use crate::Result;

const OUTPUT_TEMPLATE: &str = include_str!("overview.template.html");

lazy_static! {
    static ref LINE_PARSER: Regex = {
        RegexBuilder::new(
            r#"
            # Let's start.
            ^

            # Datetime of the log line.
            (?<datetime>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)

            # Log level.
            \s+(?<level>\S+)

            # Target.
            \s(?<target>matrix_[\w_]+(::[\w_]+)*):

            # The log message. We don't care about it.
            (?<message>.*)

            # The source file and line.
            \|\scrates/
            (?<source_file>[^:]+)
            :(?<source_line>\d+)

            # The spans.
            \s\|\sspans:\s
            (?<spans>.+)
        "#,
        )
        .ignore_whitespace(true)
        .build()
        .expect("Failed to build the `line_parser` regex")
    };
    static ref MESSAGE_PARSER: Regex = {
        RegexBuilder::new(
            r#"
            # Let's start.
            ^

            # Anything (trimmed)…
            \s*(?<message>.*?)

            # … until optional fields!
            (\s(?<fields>[\w\d_]+=.*))?$
        "#,
        )
        .ignore_whitespace(true)
        .build()
        .expect("Failed to build the `message_parser` regex")
    };
    static ref FIELDS_PARSER: Regex = {
        RegexBuilder::new(
            r#"
            # Let's start.
            ^

            # A name.
            (?<name>[\w\d_]+)
            # Equal
            =
            # A value, which can be anything: it stops when a new field is found.
            (?<value>.*?)

            # The new field (and everything else).
            (?<next_fields>\s[\w\d_]+=.+)?$
        "#,
        )
        .ignore_whitespace(true)
        .build()
        .expect("Failed to build the `fields_parser` regex")
    };
}

pub(super) fn run(log_path: path::PathBuf, output_path: path::PathBuf) -> Result<()> {
    let line_parser = &*LINE_PARSER;

    let log_file = fs::File::open(&log_path)?;
    let mut output_file = io::BufWriter::new(fs::File::create(&output_path)?);

    let reader = io::BufReader::new(log_file);
    let mut number_of_analysed_lines = 0;
    let mut number_of_matched_lines = 0;

    let mut tree = TargetTree::default();

    for (line_nth, line) in reader.lines().enumerate().map(|(nth, line)| {
        (
            nth + 1,
            line.unwrap_or_else(|error| {
                panic!("Failed to read line #{nth}\n{error}");
            }),
        )
    }) {
        number_of_analysed_lines += 1;

        if let Some(captures) = line_parser.captures(&line) {
            number_of_matched_lines += 1;

            let date_time = DateTime::parse_from_rfc3339(
                captures.name("datetime").expect("Failed to capture `datetime`").as_str(),
            )
            .expect("Failed to parse `datetime`");
            let full_target = captures.name("target").expect("Failed to capture `target`").as_str();
            let level = captures
                .name("level")
                .expect("Failed to capture `level`")
                .as_str()
                .parse()
                .expect("Failed to parse the `level`");
            let message = captures.name("message").expect("Failed to capture `message`").as_str();
            let source_file =
                captures.name("source_file").expect("Failed to capture `source_file`").as_str();
            let source_line = captures
                .name("source_line")
                .expect("Failed to capture `source_line`")
                .as_str()
                .parse()
                .expect("Failed to parse the `source_line`");
            let spans = captures.name("spans").expect("Failed to capture `spans`").as_str();

            let mut tree_node = &mut tree;

            for target in full_target.split("::") {
                tree_node =
                    match tree_node.entry(target.to_owned()).or_insert_with(|| Target::Node {
                        number_of_errors: 0,
                        number_of_warnings: 0,
                        node: TargetTree::default(),
                    }) {
                        Target::Leaf(_) => panic!("Expect a `Node`, not a `Leaf`"),
                        Target::Node { number_of_errors, number_of_warnings, node } => {
                            // Adjust number of specific log levels all the targets.
                            match level {
                                Level::Error => *number_of_errors += 1,
                                Level::Warn => *number_of_warnings += 1,
                                _ => (),
                            }

                            node
                        }
                    };
            }

            let span =
                Span { message: message.to_owned(), log_line: line_nth, raw: spans.to_owned() };

            tree_node
                // `LogLevels`
                .entry("".to_owned())
                .or_insert_with(|| Target::Leaf(LogLevels::default()))
                .as_leaf_mut()
                .expect("Expect a `Leaf`, not a `Node`")
                // `LogLocations`
                .entry(level)
                .or_insert_with(LogLocations::default)
                // `SourceFile`
                .entry(source_file.to_owned())
                .or_insert_with(Spans::default)
                // `Spans`
                .entry(source_line)
                .or_insert_with(BTreeMap::default)
                // `Span`!
                .insert(date_time, span);
        }
    }

    fn render_target_tree(tree: TargetTree, buffer: &mut io::BufWriter<fs::File>) {
        if tree.is_empty() {
            return;
        }

        writeln!(buffer, "<ul>").unwrap();

        for (target_name, target) in tree {
            match target {
                Target::Node { node, number_of_errors, number_of_warnings } => {
                    render_node(target_name, node, number_of_errors, number_of_warnings, buffer);
                }

                Target::Leaf(log_levels) => {
                    render_leaf(log_levels, buffer);
                }
            }
        }

        writeln!(buffer, "</ul>").unwrap();
    }

    fn render_node(
        target_name: String,
        node: TargetTree,
        number_of_errors: usize,
        number_of_warnings: usize,
        buffer: &mut io::BufWriter<fs::File>,
    ) {
        writeln!(
            buffer,
            "<li>\n\
            <details><summary>{target_name}\
            <span class=\"aside\">\
            <span class=\"highlight\"><input type=\"checkbox\" title=\"Highlight\" /></span>\
            <span class=\"error\" title=\"Number of errors\">{number_of_errors}</span>\
            <span class=\"warn\" title=\"Number of warnings\">{number_of_warnings}</span>\
            </span>\
            </summary>"
        )
        .unwrap();
        render_target_tree(node, buffer);
        writeln!(buffer, "</details></li>").unwrap();
    }

    fn render_leaf(log_levels: LogLevels, buffer: &mut io::BufWriter<fs::File>) {
        writeln!(
            buffer,
            "<li>\n\
            <details open><summary>(root)\
            <span class=\"aside\">\
            <span class=\"highlight\"><input type=\"checkbox\" title=\"Highlight\" /></span>\
            </span>\
            </summary>"
        )
        .unwrap();

        if log_levels.is_empty().not() {
            writeln!(buffer, "<ul>").unwrap();

            for (level, locations) in log_levels {
                render_locations(level, locations, buffer);
            }

            writeln!(buffer, "</ul>").unwrap();
        }

        writeln!(buffer, "</details></li>").unwrap();
    }

    fn render_locations(
        level: Level,
        locations: LogLocations,
        buffer: &mut io::BufWriter<fs::File>,
    ) {
        for (source_file, spans) in locations {
            writeln!(
                buffer,
                "<li>\n\
                <details><summary><span class=\"{level_class}\">{level}</span> <code>{source_file}</code>\
                <span class=\"aside\">\
                <span class=\"highlight\"><input type=\"checkbox\" title=\"Highlight\" /></span>\
                </span>\
                </summary>\n\
                <ul>",
                level_class = level.css(),
            )
            .unwrap();
            render_spans(spans, buffer);
            writeln!(
                buffer,
                "</ul>\n\
                </details></li>"
            )
            .unwrap();
        }
    }

    fn render_spans(spans: Spans, buffer: &mut io::BufWriter<fs::File>) {
        for (source_line, all_spans) in spans {
            writeln!(
                buffer,
                "<li>\n\
                <details><summary>{n} ocurrrence{s} at line <code>{source_line}</code>\
                <span class=\"aside\">\
                <span class=\"highlight\"><input type=\"checkbox\" title=\"Highlight\" /></span>\
                </span>\
                </summary>\n\
                <ol class=\"spans\">",
                n = all_spans.len(),
                s = if all_spans.len() > 1 { "s" } else { "" }
            )
            .unwrap();

            for (date_time, span) in all_spans {
                render_span(date_time, span, buffer);
            }

            writeln!(
                buffer,
                "</ol>\n\
                </details></li>"
            )
            .unwrap();
        }
    }

    fn render_span(
        date_time: DateTime<FixedOffset>,
        span: Span,
        buffer: &mut io::BufWriter<fs::File>,
    ) {
        let Span { message, log_line, .. } = span;

        let message_parser = &*MESSAGE_PARSER;
        let fields_parser = &*FIELDS_PARSER;

        if let Some(captures) = message_parser.captures(&message) {
            writeln!(
                buffer,
                "<li><time datetime=\"{date_time}\">{time}</time> {message} <span title=\"Log line\">[#{log_line}]</span>\
                <span class=\"aside\">\
                <span class=\"highlight\"><input type=\"checkbox\" title=\"Highlight\" /></span>\
                </span>\
                ",
                message = captures.name("message").expect("Failed to parse the `message`").as_str(),
                date_time = date_time.format("%+"),
                time = date_time.format("%H:%M:%S%.3f"),
            )
            .unwrap();

            if let Some(fields) = captures.name("fields") {
                writeln!(buffer, "<ul class=\"fields\"></li>").unwrap();

                let mut fields = &message[fields.start()..];

                while let Some(captures) = fields_parser.captures(fields) {
                    let name = captures
                        .name("name")
                        .expect("Failed to parse the `name` of the field")
                        .as_str();
                    let value = captures
                        .name("value")
                        .expect("Failed to parse the `value` of the field")
                        .as_str();

                    writeln!(buffer, "<li><span>{name}</span><span>{value}</span></li>").unwrap();

                    if let Some(next_fields) = captures.name("next_fields") {
                        fields = &message[next_fields.start()..];
                    } else {
                        break;
                    }
                }

                writeln!(buffer, "</ul>").unwrap();
            }

            writeln!(buffer, "</li>").unwrap();
        } else {
            writeln!(buffer, "<li>{message}</li>").unwrap();
        }
    }
    let output = OUTPUT_TEMPLATE.replace("{log_file}", &log_path.to_string_lossy());

    output_file.write_all(output.as_bytes()).expect(
        "Failed to write the
    output",
    );

    render_target_tree(tree, &mut output_file);

    output_file.write_all("</main></body></html>".as_bytes()).expect("Failed to write the output");

    println!(
        "\nNumber of analysed log lines: {number_of_analysed_lines}\n\
        Number of matched lines: {number_of_matched_lines}\n\
        Output file: {output_path:?}\n\
        Done!"
    );

    Ok(())
}

type TargetName = String;
type TargetTree = BTreeMap<TargetName, Target>;

#[derive(Debug)]
enum Target {
    Node {
        /// Number of errors for this node entirely (including sub-trees).
        number_of_errors: usize,

        /// Number of warnings for this node entirely (including sub-trees).
        number_of_warnings: usize,

        /// The Node.
        node: TargetTree,
    },
    Leaf(LogLevels),
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

type LogLevels = BTreeMap<Level, LogLocations>;

type SourceFile = String;
type SourceLine = usize;
type LogLocations = HashMap<SourceFile, Spans>;

type Spans = BTreeMap<SourceLine, BTreeMap<DateTime<FixedOffset>, Span>>;

#[derive(Debug)]
struct Span {
    message: String,
    log_line: usize,
    #[allow(dead_code)]
    raw: String,
}

impl Target {
    fn as_leaf_mut(&mut self) -> Option<&mut LogLevels> {
        match self {
            Self::Leaf(log_levels) => Some(log_levels),
            Self::Node { .. } => None,
        }
    }
}

impl Level {
    fn css(&self) -> &str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl FromStr for Level {
    type Err = ();

    fn from_str(str: &str) -> Result<Self, Self::Err> {
        Ok(match str {
            "ERROR" => Self::Error,
            "WARN" => Self::Warn,
            "INFO" => Self::Info,
            "DEBUG" => Self::Debug,
            "TRACE" => Self::Trace,
            _ => return Err(()),
        })
    }
}

impl fmt::Display for Level {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}",
            match self {
                Self::Error => "ERROR",
                Self::Warn => "WARNING",
                Self::Info => "INFO",
                Self::Debug => "DEBUG",
                Self::Trace => "TRACE",
            }
        )
    }
}
