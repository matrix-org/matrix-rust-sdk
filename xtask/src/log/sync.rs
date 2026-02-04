use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, Write},
    ops::Sub,
    path,
    time::Duration,
};

use chrono::{DateTime, FixedOffset, TimeDelta};
use indexmap::{IndexMap, map::Entry};
use regex::RegexBuilder;
use url::{Position as UrlPosition, Url};

use crate::Result;

const OUTPUT_TEMPLATE: &str = include_str!("sync.template.html");

pub(super) fn run(log_path: path::PathBuf, output_path: path::PathBuf) -> Result<()> {
    let find_sync = RegexBuilder::new(
        r#"
            # Datetime of the log line.
            (?<datetime>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)

            # Ensure it's about the `http_client` scope.
            .*matrix_sdk::http_client

            # Ensure it's about a sync.
            .*>\ssync_once\{
                conn_id="(?<connection_id>[^"]+)"

                # Since https://github.com/matrix-org/matrix-rust-sdk/pull/6118,
                # we have `pos` and `timeout`. We must consider they are
                # optional.
                (
                    .*
                    \spos="(?<pos>[^"]+)"
                    \stimeout=(?<timeout>\d+)
                )?
            \}

            # Let's capture some data about `send()`!
            \s>\ssend\{
                request_id="REQ-(?<request_id>\d+)"
                \smethod=(?<method>\S+)
                \suri="(?<uri>[^"]+)"
                # If there is a `request_size`.
                (.*\srequest_size="(?<request_size>[^"]+)")?
                # If this is a response, there is a `status`.
                (.*\sstatus=(?<status>\d+))?
                # If there is a `response_size`.
                (.*\sresponse_size="(?<response_size>[^"]+)")?
        "#,
    )
    .ignore_whitespace(true)
    .build()
    .expect("Failed to build the `find_sync_start regex`");

    let log_file = fs::File::open(&log_path)?;
    let mut output_file = fs::File::create(&output_path)?;

    let reader = io::BufReader::new(log_file);
    let mut number_of_analysed_lines = 0;
    let mut number_of_matched_lines = 0;
    let mut smallest_start_at = None;
    let mut largest_end_at = None;

    let mut spans: BTreeMap<ConnectionId, IndexMap<RequestId, Span>> = BTreeMap::new();

    for (line_nth, line) in reader.lines().enumerate().map(|(nth, line)| {
        (
            nth + 1,
            line.unwrap_or_else(|error| {
                panic!("Failed to read line #{nth}\n{error}");
            }),
        )
    }) {
        number_of_analysed_lines += 1;

        if let Some(captures) = find_sync.captures(&line) {
            number_of_matched_lines += 1;

            let date_time = DateTime::parse_from_rfc3339(
                captures.name("datetime").expect("Failed to capture `datetime`").as_str(),
            )
            .expect("Failed to parse `datetime`");
            let connection_id =
                captures.name("connection_id").expect("Failed to capture `connection_id`").as_str();
            let pos = captures.name("pos").map(|pos| pos.as_str());
            let timeout = captures
                .name("timeout")
                .map(|timeout| timeout.as_str().parse::<u64>().ok().map(Duration::from_millis))
                .flatten();
            let request_id = captures
                .name("request_id")
                .expect("Failed to capture `request_id`")
                .as_str()
                .parse()
                .expect("Failed to parse `request_id`");
            let method = captures.name("method").expect("Failed to capture `method`").as_str();
            let uri = captures.name("uri").expect("Failed to capture `uri`").as_str();
            let request_size =
                captures.name("request_size").map(|request_size| request_size.as_str());
            let response_size =
                captures.name("response_size").map(|response_size| response_size.as_str());
            let status =
                captures.name("status").map(|status| status.as_str().parse().ok()).flatten();

            if let Some(smallest_start_at_inner) = smallest_start_at {
                if smallest_start_at_inner > date_time {
                    smallest_start_at = Some(date_time.clone());
                }
            } else {
                smallest_start_at = Some(date_time.clone());
            }

            if let Some(largest_end_at_inner) = largest_end_at {
                if largest_end_at_inner < date_time {
                    largest_end_at = Some(date_time.clone());
                }
            } else {
                largest_end_at = Some(date_time.clone());
            }

            let spans_for_connection_id = spans.entry(connection_id.to_owned()).or_default();

            match spans_for_connection_id.entry(request_id) {
                Entry::Vacant(entry) => {
                    entry.insert(Span {
                        status: None,
                        method: method.to_owned(),
                        uri: uri.to_owned(),
                        pos: pos.map(ToOwned::to_owned),
                        timeout,
                        request_size: request_size.map(ToOwned::to_owned),
                        response_size: response_size.map(ToOwned::to_owned),
                        start_at: date_time,
                        duration: TimeDelta::zero(),
                        request_log_line: line_nth,
                        response_log_line: None,
                    });
                }
                Entry::Occupied(mut entry) => {
                    let span = entry.get_mut();

                    if let Some(status) = status {
                        span.status = Some(status);
                    }

                    span.duration = date_time.sub(&span.start_at);

                    if let Some(request_size) = request_size {
                        span.request_size = Some(request_size.to_owned());
                    }

                    if let Some(response_size) = response_size {
                        span.response_size = Some(response_size.to_owned());
                    }

                    span.response_log_line = Some(line_nth);
                }
            }
        }
    }

    let smallest_start_at =
        smallest_start_at.map(|date_time| date_time.timestamp_millis()).unwrap_or_default();
    let largest_end_at =
        largest_end_at.map(|date_time| date_time.timestamp_millis()).unwrap_or_default();
    let end_at = largest_end_at.saturating_sub(smallest_start_at).to_string();
    let rows = spans
        .iter()
        .map(|(connection_id, spans)| {
            spans.iter().map(
                |(
                    request_id,
                    Span {
                        status,
                        method,
                        uri,
                        pos,
                        timeout,
                        request_size,
                        response_size,
                        start_at,
                        duration,
                        request_log_line,
                        response_log_line
                    },
                )| {
                    let uri = Url::parse(uri);
                    let duration = duration.num_milliseconds();
                    let timeout = timeout.map(|timeout| timeout.as_millis());

                    format!(
                        "    <tr id=\"{connection_id}-{request_id}\">
      <td><code>{connection_id}</code></td>
      <td><a href=\"#{connection_id}-{request_id}\" title=\"Permalink to this line\"><code>{request_id}</code></a></td>
      <td data-status-family=\"{status_family}\"><span>{status}</span></td>
      <td><code>{method}</code></td>
      <td title=\"{domain}\">{domain}</td>
      <td title=\"{path}\">{path}</td>
      <td>{timeout}</td>
      <td>{request_size}</td>
      <td>{response_size}</td>
      <td>{time}</td>
      <td>
        <div class=\"span\" style=\"--start-at: {start_at}; --duration: {duration}\"><span>{duration_label}</span></div>
        <details>
          <summary><span class=\"hidden\">information</span></summary>
          <ul>
            <li>Request log line number: {request_log_line}</li>
            <li>Response log line number: {response_log_line}</li>
            <li>Sliding Sync <code>pos</code>: <span class=\"text-overflow\">{pos}</span></li>
          </ul>
        </details>
      </td>
    </tr>
",
                        connection_id = connection_id.clone(),
                        status = status
                            .map(|status| status.to_string())
                            .unwrap_or_else(|| "×".to_owned()),
                        status_family = status
                            .map(|status| (if status > 0 { status / 100 } else { 0 } ).to_string())
                            .unwrap_or_else(|| "cancelled".to_owned()),
                        domain = uri
                            .as_ref()
                            .map(|uri| uri[UrlPosition::BeforeHost..UrlPosition::BeforePath].to_owned())
                            .unwrap_or_default(),
                        path = uri
                            .as_ref()
                            .map(|uri| uri[UrlPosition::BeforePath..].to_owned())
                            .unwrap_or_default(),
                        timeout = timeout
                            .map(|timeout| format!("{timeout}ms"))
                            .unwrap_or_else(|| "(unknown)".to_owned()),
                        request_size = request_size
                            .clone()
                            .map(|request_size| request_size.to_string())
                            .unwrap_or_else(|| "".to_owned()),
                        response_size = response_size
                            .clone()
                            .map(|response_size| response_size.to_string())
                            .unwrap_or_else(|| "".to_owned()),
                        time = start_at.format("%H:%M:%S%.3f"),
                        start_at = start_at
                            .timestamp_millis()
                            .saturating_sub(smallest_start_at),
                        duration_label = if duration > 0 { format!("{duration}ms") } else { "<em>cancelled</em>".to_owned() },
                        response_log_line = response_log_line.map(|line| line.to_string()).unwrap_or_else(|| "(none)".to_owned()),
                        pos = pos.clone().unwrap_or_else(|| "(unknown)".to_owned())
                    )
                },
            )
        })
        .flatten()
        .collect::<String>();

    let output = OUTPUT_TEMPLATE
        .replace("{log_file}", &log_path.to_string_lossy())
        .replace("{end_at}", &end_at)
        .replace("{rows}", &rows);

    output_file.write_all(output.as_bytes()).expect("Failed to write the output");

    println!(
        "\nNumber of analysed log lines: {number_of_analysed_lines}\n\
        Number of matched lines: {number_of_matched_lines}\n\
        Output file: {output_path:?}\n\
        Done!"
    );

    Ok(())
}

type ConnectionId = String;
type RequestId = u32;

#[derive(Debug)]
struct Span {
    status: Option<u8>,
    method: String,
    uri: String,
    pos: Option<String>,
    timeout: Option<Duration>,
    request_size: Option<String>,
    response_size: Option<String>,
    start_at: DateTime<FixedOffset>,
    duration: TimeDelta,
    request_log_line: usize,
    response_log_line: Option<usize>,
}
