use std::time::Duration;

use symbols::line;
use tui::backend::Backend;
use tui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use tui::style::{Color, Modifier, Style};
use tui::text::{Span, Spans};
use tui::widgets::{Block, BorderType, Borders, Cell, LineGauge, Paragraph, Row, Tabs, Table};
use tui::{symbols, Frame};
use tui_logger::TuiLoggerWidget;

use super::actions::Actions;
use super::state::AppState;
use crate::app::App;

pub fn draw<B>(rect: &mut Frame<B>, app: &App)
where
    B: Backend,
{

    let size = rect.size();
    
    // Vertical layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(3),   // header
                Constraint::Min(10),     // body
                Constraint::Length(12),  // logs 
                Constraint::Length(3),   //footer
            ]
            .as_ref(),
        )
        .split(size);

    // Title
    let title = draw_title(app.title());
    rect.render_widget(title, chunks[0]);

    // Body & Help
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)].as_ref())
        .split(chunks[1]);

    let body = draw_body(app.is_loading(), app.state());
    rect.render_widget(body, body_chunks[0]);
    let body = draw_body(app.is_loading(), app.state());
    rect.render_widget(body, body_chunks[1]);

    // let help = draw_help(app.actions());
    // rect.render_widget(help, body_chunks[1]);

    // Logs
    let logs = draw_logs();
    rect.render_widget(logs, chunks[2]);

    // Footer
    let footer = draw_footer(app.is_loading(), app.state());
    rect.render_widget(footer, chunks[3]);

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


fn draw_body<'a>(loading: bool, state: &AppState) -> Paragraph<'a> {
    Paragraph::new(vec![
    ])
    .style(Style::default().fg(Color::LightCyan))
    .alignment(Alignment::Left)
    .block(
        Block::default()
            // .title("Body")
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White))
            .border_type(BorderType::Plain),
    )
}

fn draw_footer<'a>(loading: bool, state: &AppState) -> Tabs<'a> {
    if !state.is_initialized() {
        return Tabs::new(vec![Spans::from("initialising")]);
    }
    Tabs::new(vec![Spans::from("ESC / <q> to quit")])
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
                .title("Logs")
                .border_style(Style::default().fg(Color::White).bg(Color::Black))
                .borders(Borders::ALL),
        )
        .style(Style::default().fg(Color::White).bg(Color::Black))
}
