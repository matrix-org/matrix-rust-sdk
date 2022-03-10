use std::time::Duration;

use symbols::line;
use tui::backend::Backend;
use tui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use tui::style::{Color, Modifier, Style};
use tui::text::{Span, Spans};
use tui::widgets::{Block, BorderType, Borders, Cell, LineGauge, Paragraph, Table, Row, Tabs, List, ListItem};
use tui::{symbols, Frame};
use tui_logger::TuiLoggerWidget;

use super::actions::{Actions, Action};
use super::state::{AppState, Syncv2State, SlidingSyncState};
use crate::app::App;

pub fn draw<B>(rect: &mut Frame<B>, app: &App)
where
    B: Backend,
{

    let size = rect.size();
    let state = app.state();
    let show_logs = state.show_logs();

    let mut areas = vec![
        Constraint::Length(3),   // header
        Constraint::Length(2),   // help
        Constraint::Min(10),     // body
        Constraint::Length(3),   // body-footer
    ];
    if show_logs {
        areas.push(
            Constraint::Length(12),  // logs 
        );
    }

    // Vertical layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(areas)
        .split(size);

    // Title
    let title = draw_title(app.title());
    rect.render_widget(title, chunks[0]);

    // help
    let footer = draw_help(app.actions());
    rect.render_widget(footer, chunks[1]);

    // Body
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(35), Constraint::Min(23)].as_ref())
        .split(chunks[2]);

    let sliding = app.state().get_sliding();
    let rooms = draw_rooms(sliding);
    if let Some(sliding) = app.state().get_sliding() {
        rect.render_stateful_widget(rooms, body_chunks[0], &mut sliding.rooms_state.clone());
    } else {
        rect.render_widget(rooms, body_chunks[0]);
    }

    let body_details = draw_details(app.state().get_sliding());
    rect.render_widget(body_details, body_chunks[1]);

    let v3_footer = draw_status(app.state().get_sliding());
    rect.render_widget(v3_footer, chunks[3]);

    if show_logs {
        // Logs
        let logs = draw_logs();
        rect.render_widget(logs, chunks[4]);
    };
}

fn draw_title<'a>(title: Option<String>) -> Paragraph<'a> {
    Paragraph::new(title.map(|n| format!("Sliding Sync for: {}", n)).unwrap_or_else(||"loading...".to_owned()))
        .style(Style::default().fg(Color::Green))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .style(Style::default().fg(Color::White))
                .border_type(BorderType::Plain),
        )
}


fn calc_v2<'a>(state: Option<&Syncv2State>) -> Vec<ListItem<'a>> {
    if state.is_none() {
        return vec![ListItem::new("Sync v2 hasn't started yet")]
    }

    let state = state.expect("We've tested before");
    let mut paras = vec![];

    if let Some(dur) = state.time_to_first_render() {
        paras.push(ListItem::new(format!("took {}s", dur.as_secs())));
    } else {
        paras.push(ListItem::new(format!("loading for {}s", state.started().elapsed().as_secs())));

    }

    if let Some(count) = state.rooms_count() {
        paras.push(ListItem::new(format!("to load {} rooms", count)));
    }

    return paras;

}


fn draw_rooms<'a>(state: Option<&SlidingSyncState>) -> Table<'a> {
    Table::new(calc_sliding(state))
    .style(Style::default().fg(Color::White))
    .highlight_style(Style::default().fg(Color::LightCyan).add_modifier(Modifier::ITALIC))
    .highlight_symbol(">>")
    .widths(&[Constraint::Min(30), Constraint::Max(4)])
    .block(
        Block::default()
            .title(" Rooms ")
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White))
            .border_type(BorderType::Plain)
            
    )
}

fn draw_status<'a>(state: Option<&SlidingSyncState>) -> Tabs<'a> {
    let tabs = if let Some(state) = state {
        let mut tabs = vec![];
        if let Some(dur) = state.time_to_first_render() {
            tabs.push(Spans::from(format!("First view: {}s", dur.as_secs())));

            if let Some(dur) = state.time_to_full_sync() {
                tabs.push(Spans::from(format!("Full sync: {}s", dur.as_secs())));
                if let Some(count) = state.total_rooms_count() {
                    tabs.push(Spans::from(format!("{} rooms", count)));
                }
            } else {
                tabs.push(Spans::from(format!("Loaded {:} rooms in {}s", state.loaded_rooms_count(), state.started().elapsed().as_secs())));
            }

        } else {
            tabs.push(Spans::from(format!("loading for {}s", state.started().elapsed().as_secs())));
        }
        tabs
    } else {
        vec![Spans::from("Sliding sync hasn't started yet")]
    };

    Tabs::new(tabs)
    .style(Style::default().fg(Color::LightCyan))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White))
            .border_type(BorderType::Plain),
    )
}


fn calc_sliding<'a>(state: Option<&SlidingSyncState>) -> Vec<Row<'a>> {
    if state.is_none() {
        return vec![
            Row::new(vec![Cell::from("Sliding sync hasn't started yet")])
            ]
    }

    let state = state.expect("We've tested before");
    let mut paras = vec![];

    for r in state.view().get_rooms(None, None) {
        let mut cells = vec![
            Cell::from(r.name.unwrap_or("unknown".to_string()))
        ];
        if let Some(c) = r.notification_count {
            let count: u32 = c.try_into().unwrap_or_default();
            if count > 0 {
                cells.push(Cell::from(c.to_string()))
            }
        } 
        paras.push(Row::new(cells));
    }

    return paras;

}


fn draw_details<'a>(state: Option<&SlidingSyncState>) -> Table<'a> {
    let selected_room = if let Some(r) = state.and_then(|i| i.current_room_summary())
    {
        r
    } else {
        return Table::new(vec![
            Row::new(vec![Cell::from("Choose a room with up/down and press <enter> to select")])
        ])
    };

    let mut details = vec![
        Row::new(vec![Cell::from("-- Status Events")])
    ];

    for (title, count) in selected_room.state_events_counts {
        details.push(
            Row::new(vec![Cell::from(title), Cell::from(format!("{}", count))])
        )
    }

    Table::new(details)
    .style(Style::default().fg(Color::LightCyan))
    .widths(&[Constraint::Min(30), Constraint::Min(6), Constraint::Min(30)])
    .block(
        Block::default()
            .title(format!(" {} ", selected_room.name))
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White))
            .border_type(BorderType::Plain),
    )
}

fn draw_v2<'a>(state: Option<&Syncv2State>) -> List<'a> {
    List::new(calc_v2(state))
    .style(Style::default().fg(Color::LightCyan))
    .block(
        Block::default()
            .title(" Sync v2 ")
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White))
            .border_type(BorderType::Plain),
    )
}

fn draw_help<'a>(actions: &Actions) -> Tabs<'a> {
    Tabs::new(actions.actions().iter().map(|a| {
        Spans::from(
            format!("{}: {}",
                a.keys().iter().map(|k| k.to_string()).collect::<Vec<String>>().join(" / "),
                a.to_string()
        ))
    }).collect())
    .style(Style::default().fg(Color::LightGreen))
    .block(
        Block::default()
            // .title("Body")
            .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
            .style(Style::default().fg(Color::White))
            .border_type(BorderType::Plain),
    )
}

fn draw_logs<'a>() -> TuiLoggerWidget<'a> {
    TuiLoggerWidget::default()
        .style_error(Style::default().fg(Color::Red))
        .style_debug(Style::default().fg(Color::Green))
        .style_warn(Style::default().fg(Color::Yellow))
        .style_trace(Style::default().fg(Color::Gray))
        .style_info(Style::default().fg(Color::Blue))
        .block(
            Block::default()
                .title(" Logs ")
                .border_style(Style::default().fg(Color::White).bg(Color::Black))
                .borders(Borders::ALL),
        )
        .style(Style::default().fg(Color::White).bg(Color::Black))
}
