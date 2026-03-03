use std::{
    fmt, fs,
    io::{self, BufRead, Write},
    path,
    str::FromStr,
    time::Duration,
};

use chrono::{DateTime, FixedOffset};
use regex::RegexBuilder;

use crate::Result;

const OUTPUT_TEMPLATE: &str = include_str!("timer.template.html");

pub(super) fn run(log_path: path::PathBuf, output_path: path::PathBuf) -> Result<()> {
    let find_timer = RegexBuilder::new(
        r#"
            # Let's start.
            ^

            # Datetime of the log line.
            (?<datetime>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)

            # Log level.
            \s+(?<level>\S+)

            # Target.
            \s(?<target>matrix_[\w_]+(::[\w_]+)*):

            # The timer message.
            \s(?<message>(Timer\s)?_.*?_)
            \sfinished\sin
            # The timer duration.
            \s(?<duration>\d(\.\d+)?.*s)

            # The source file and line.
            \s\|\scrates/
            (?<source_file>[^:]+)
            :(?<source_line>\d+)
        "#,
    )
    .ignore_whitespace(true)
    .build()
    .expect("Failed to build the `find_timer` regex");

    let log_file = fs::File::open(&log_path)?;
    let mut output_file = io::BufWriter::new(fs::File::create(&output_path)?);

    let reader = io::BufReader::new(log_file);
    let mut number_of_analysed_lines = 0;
    let mut number_of_matched_lines = 0;

    let mut timers = Vec::<Timer>::new();

    for line in reader.lines().enumerate().map(|(nth, line)| {
        line.unwrap_or_else(|error| {
            panic!("Failed to read line #{nth}\n{error}");
        })
    }) {
        number_of_analysed_lines += 1;

        if let Some(captures) = find_timer.captures(&line) {
            number_of_matched_lines += 1;

            let date_time = DateTime::parse_from_rfc3339(
                captures.name("datetime").expect("Failed to capture `datetime`").as_str(),
            )
            .expect("Failed to parse `datetime`");
            let level = captures
                .name("level")
                .expect("Failed to capture `level`")
                .as_str()
                .parse()
                .expect("Failed to parse the `level`");
            let target = captures.name("target").expect("Failed to capture `target`").as_str();
            let message = captures.name("message").expect("Failed to capture `message`").as_str();
            let duration_as_str =
                captures.name("duration").expect("Failed to capture `duration`").as_str();
            let duration = {
                let duration = duration_as_str;

                // According to
                // https://github.com/rust-lang/rust/blob/0028f344ce9f64766259577c998a1959ca1f6a0b/library/core/src/time.rs#L1488-L15
                // 10, there is 4 possible suffixes. Let's get through them.

                let pivot = duration.floor_char_boundary(duration.len().saturating_sub(2));

                Duration::from_nanos(match &duration[pivot..] {
                    // nanoseconds
                    "ns" => {
                        let duration: f64 = duration[..pivot].parse().unwrap();

                        duration.abs().round() as u64
                    }

                    "µs" => {
                        let duration: f64 = duration[..pivot].parse().unwrap();
                        (duration * 1_000.0).abs().round() as u64
                    }

                    "ms" => {
                        let duration: f64 = duration[..pivot].parse().unwrap();

                        (duration * 1_000_000.0).abs().round() as u64
                    }

                    // seconds!
                    _ => {
                        let duration: f64 = duration[..pivot.saturating_add(1)].parse().unwrap();

                        (duration * 1_000_000_000.0).abs().round() as u64
                    }
                })
            };
            let source_file =
                captures.name("source_file").expect("Failed to capture `source_file`").as_str();
            let source_line = captures
                .name("source_line")
                .expect("Failed to capture `source_line`")
                .as_str()
                .parse()
                .expect("Failed to parse the `source_line`");

            // Ensure we have parsed the duration correctly (in case Rust stdlib changes
            // something).
            assert_eq!(
                duration_as_str,
                format!("{duration:?}"),
                "Failed to parse the `duration` as expected"
            );

            timers.push(Timer {
                date_time,
                level,
                target: target.to_owned(),
                message: message[1..message.len() - 1].to_owned(),
                duration,
                source_file: source_file.to_owned(),
                source_line,
            });
        }
    }

    timers.sort_by(|a, b| a.duration.cmp(&b.duration).reverse());

    fn render(timers: Vec<Timer>, buffer: &mut io::BufWriter<fs::File>) {
        for Timer { duration, level, message, target, source_file, source_line, date_time } in
            timers
        {
            writeln!(
                buffer,
                "<tr>\n\
                <td>\n\
                  <div class=\"span\" style=\"--duration: {duration}\"><span>{duration_label:?}</span></div>\n\
                </td>\n\
                <td>{message}</td>\n\
                <td><span class=\"{level_class}\">{level}</span></td>\n\
                <td><code>{target}</code></td>\n\
                <td><code>{source_file}:{source_line}</code></td>\n\
                <td><time datetime=\"{date_time}\">{time}</time></td>\n\
                </tr>\n",
                level_class = level.css(),
                duration = duration.as_nanos(),
                duration_label = duration,
                date_time = date_time.format("%+"),
                time = date_time.format("%H:%M:%S%.3f"),
            )
            .unwrap();
        }
    }

    let output = OUTPUT_TEMPLATE.replace("{log_file}", &log_path.to_string_lossy()).replace(
        "{longest-duration}",
        &timers.get(0).map(|timer| timer.duration.as_nanos()).unwrap_or(0).to_string(),
    );

    output_file.write_all(output.as_bytes()).expect("Failed to write the output");

    render(timers, &mut output_file);

    output_file
        .write_all(
            "  </tbody>
</table>

</main>

</body>
</html>"
                .as_bytes(),
        )
        .expect("Failed to write the output");

    println!(
        "\nNumber of analysed log lines: {number_of_analysed_lines}\n\
        Number of matched lines: {number_of_matched_lines}\n\
        Output file: {output_path:?}\n\
        Done!"
    );

    Ok(())
}

#[derive(Debug)]
struct Timer {
    date_time: DateTime<FixedOffset>,
    level: Level,
    target: String,
    message: String,
    duration: Duration,
    source_file: String,
    source_line: usize,
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
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
