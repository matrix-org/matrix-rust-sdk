use std::time::Duration;

use symbols::line;
use tui::backend::Backend;
use tui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use tui::style::{Color, Modifier, Style};
use tui::text::{Span, Spans};
use tui::widgets::{Block, BorderType, Borders, Cell, LineGauge, Paragraph, Row, Tabs, List, ListItem, Table};
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
        Constraint::Min(10),     // body
        Constraint::Length(3),   // body-footer
    ];
    if show_logs {
        areas.push(
            Constraint::Length(12),  // logs 
        );
    }

    areas.push(
        Constraint::Length(3),   // footer
    );

    // Vertical layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(areas)
        .split(size);

    // Title
    let title = draw_title(app.title());
    rect.render_widget(title, chunks[0]);

    // Body
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 3), Constraint::Ratio(2, 3)].as_ref())
        .split(chunks[1]);

    let sliding = app.state().get_sliding();
    let rooms = draw_sliding(sliding);
    if let Some(sliding) = app.state().get_sliding() {
        rect.render_stateful_widget(rooms, body_chunks[0], &mut sliding.rooms_state.clone());
    } else {
        rect.render_widget(rooms, body_chunks[0]);
    }

    let bodyv2 = draw_v2(app.state().get_v2());
    rect.render_widget(bodyv2, body_chunks[1]);

    // Body
    let body_footer_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 3), Constraint::Ratio(2, 3)].as_ref())
        .split(chunks[2]);


    let v3_footer = draw_sliding_footer(app.state().get_sliding());
    rect.render_widget(v3_footer, body_footer_chunks[0]);


    let footer_num = if show_logs {
        // Logs
        let logs = draw_logs();
        rect.render_widget(logs, chunks[3]);
        4
    } else { 3 };

    // Footer
    let footer = draw_footer(app.actions());
    rect.render_widget(footer, chunks[footer_num]);

}

fn draw_title<'a>(title: Option<String>) -> Paragraph<'a> {
    Paragraph::new(title.map(|n| format!("Sliding Sync for: {}", n)).unwrap_or_else(||"loading...".to_owned()))
        .style(Style::default().fg(Color::LightCyan))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
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


fn draw_sliding<'a>(state: Option<&SlidingSyncState>) -> List<'a> {
    List::new(calc_sliding(state))
    .style(Style::default().fg(Color::White))
    .highlight_style(Style::default().fg(Color::LightCyan).add_modifier(Modifier::ITALIC))
    .highlight_symbol(">>")
    .block(
        Block::default()
            .title(" Rooms ")
            .borders(Borders::LEFT | Borders::TOP | Borders::RIGHT)
            .style(Style::default().fg(Color::White))
            .border_type(BorderType::Plain)
            
    )
}

fn draw_sliding_footer<'a>(state: Option<&SlidingSyncState>) -> Tabs<'a> {
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
            .title(" Status ")
            .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
            .style(Style::default().fg(Color::White))
            .border_type(BorderType::Plain),
    )
}


fn calc_sliding<'a>(state: Option<&SlidingSyncState>) -> Vec<ListItem<'a>> {
    if state.is_none() {
        return vec![ListItem::new("Sliding sync hasn't started yet")]
    }

    let state = state.expect("We've tested before");
    let mut paras = vec![];

    for r in state.view().get_rooms(None, None) {
        paras.push(ListItem::new(format!("{} ({:?})", r.name.unwrap_or("unknown".to_string()), r.notification_count)));
    }

    return paras;

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

fn draw_footer<'a>(actions: &Actions) -> Tabs<'a> {
    Tabs::new(actions.actions().iter().map(|a| {
        Spans::from(
            format!("{}: {}",
                a.keys().iter().map(|k| k.to_string()).collect::<Vec<String>>().join(" / "),
                a.to_string()
        ))
    }).collect())
    .style(Style::default().fg(Color::LightCyan))
    .block(
        Block::default()
            // .title("Body")
            .borders(Borders::ALL)
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
