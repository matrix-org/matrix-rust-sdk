use std::sync::Arc;

use imbl::Vector;
use indexmap::IndexMap;
use matrix_sdk::ruma::{
    OwnedUserId, UserId,
    events::{receipt::Receipt, room::message::MessageType},
};
use matrix_sdk_ui::timeline::{
    MembershipChange, Message, MsgLikeContent, MsgLikeKind, RoomMembershipChange, ThreadSummary,
    TimelineDetails, TimelineItem, TimelineItemContent, TimelineItemKind, VirtualTimelineItem,
};
use ratatui::{prelude::*, widgets::*};

use crate::{ALT_ROW_COLOR, NORMAL_ROW_COLOR, SELECTED_STYLE_FG, TEXT_COLOR};

pub struct TimelineView<'a> {
    items: &'a Vector<Arc<TimelineItem>>,
    is_thread: bool,
}

impl<'a> TimelineView<'a> {
    pub fn new(items: &'a Vector<Arc<TimelineItem>>, is_thread: bool) -> Self {
        Self { items, is_thread }
    }
}

pub struct TimelineListState {
    state: ListState,
    /// An index from a rendered list item to the original timeline item index
    /// (since some timeline items may not be rendered).
    list_index_to_item_index: Vec<usize>,
}

impl Default for TimelineListState {
    fn default() -> Self {
        let mut state = ListState::default();
        state.select_last();
        Self { state, list_index_to_item_index: Vec::default() }
    }
}

impl TimelineListState {
    pub fn select_next(&mut self) {
        self.state.select_next();
    }
    pub fn select_previous(&mut self) {
        self.state.select_previous();
    }
    pub fn unselect(&mut self) {
        self.state.select(None);
    }
    pub fn selected(&self) -> Option<usize> {
        let rendered_index = self.state.selected()?;
        self.list_index_to_item_index.get(rendered_index).copied()
    }
}

impl StatefulWidget for &mut TimelineView<'_> {
    type State = TimelineListState;

    fn render(self, area: Rect, buf: &mut Buffer, timeline_list_state: &mut Self::State)
    where
        Self: Sized,
    {
        timeline_list_state.list_index_to_item_index.clear();

        let content = self.items.iter().enumerate().filter_map(|(i, item)| {
            let result = format_timeline_item(item, self.is_thread)?;
            timeline_list_state.list_index_to_item_index.push(i);
            Some(result)
        });

        let list_items = content
            .enumerate()
            .map(|(i, line)| {
                let bg_color = match i % 2 {
                    0 => NORMAL_ROW_COLOR,
                    _ => ALT_ROW_COLOR,
                };

                line.fg(TEXT_COLOR).bg(bg_color)
            })
            .collect::<Vec<_>>();

        let list = List::new(list_items)
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol(">")
            .highlight_style(SELECTED_STYLE_FG);

        StatefulWidget::render(list, area, buf, &mut timeline_list_state.state);
    }
}

fn format_timeline_item(item: &Arc<TimelineItem>, is_thread: bool) -> Option<ListItem<'_>> {
    let item = match item.kind() {
        TimelineItemKind::Event(ev) => {
            let sender = ev.sender();

            match ev.content() {
                TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Message(message),
                    ..
                }) => {
                    let thread_summary =
                        if is_thread { None } else { ev.content().thread_summary() };
                    format_text_message(sender, message, thread_summary, ev.read_receipts())?
                }

                TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Redacted,
                    ..
                }) => format!("{sender}: -- redacted --").into(),

                TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::UnableToDecrypt(_),
                    ..
                }) => format!("{sender}: (UTD)").into(),

                TimelineItemContent::MembershipChange(m) => format_membership_change(m)?,

                TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Sticker(_),
                    ..
                })
                | TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Other(_),
                    ..
                })
                | TimelineItemContent::ProfileChange(_)
                | TimelineItemContent::OtherState(_)
                | TimelineItemContent::FailedToParseMessageLike { .. }
                | TimelineItemContent::FailedToParseState { .. }
                | TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Poll(_), ..
                })
                | TimelineItemContent::CallInvite
                | TimelineItemContent::RtcNotification => {
                    return None;
                }
            }
        }

        TimelineItemKind::Virtual(virt) => match virt {
            VirtualTimelineItem::DateDivider(unix_ts) => format!("Date: {unix_ts:?}").into(),
            VirtualTimelineItem::ReadMarker => "Read marker".to_owned().into(),
            VirtualTimelineItem::TimelineStart => "🥳 Timeline start! 🥳".to_owned().into(),
        },
    };

    Some(item)
}

fn format_text_message(
    sender: &UserId,
    message: &Message,
    thread_summary: Option<ThreadSummary>,
    read_receipts: &IndexMap<OwnedUserId, Receipt>,
) -> Option<ListItem<'static>> {
    if let MessageType::Text(text) = message.msgtype() {
        let mut lines = Vec::new();
        let first_line = Line::from(format!("{}: {}", sender, text.body));

        lines.push(first_line);

        if let Some(thread_summary) = thread_summary {
            match thread_summary.latest_event {
                TimelineDetails::Unavailable | TimelineDetails::Pending => {
                    let thread_line = Line::from("  💬 ...");
                    lines.push(thread_line);
                }
                TimelineDetails::Ready(e) => {
                    let sender = e.sender;
                    let content = e.content.as_message().map(|m| m.msgtype());

                    if let Some(MessageType::Text(text)) = content {
                        let replies = if thread_summary.num_replies == 1 {
                            "1 reply".to_owned()
                        } else {
                            format!("{} replies", { thread_summary.num_replies })
                        };
                        let thread_line =
                            Line::from(format!("  💬 {replies} {sender}: {}", text.body));

                        lines.push(thread_line);
                    }
                }
                TimelineDetails::Error(_) => {}
            }
        }

        if !read_receipts.is_empty() {
            // Read by [5 first users who read it], optionally followed by "and X others".
            let mut read_by = read_receipts
                .iter()
                .take(5)
                .map(|(user_id, _)| user_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if read_receipts.len() > 5 {
                let others_count = read_receipts.len() - 5;
                if others_count == 1 {
                    read_by.push_str(" and 1 other");
                } else {
                    read_by = format!("{read_by} and {others_count} others");
                }
            }
            lines.push(Line::from(format!("  👀 read by {read_by}")));
        }

        Some(ListItem::from(lines))
    } else {
        None
    }
}

fn format_membership_change(membership: &RoomMembershipChange) -> Option<ListItem<'static>> {
    if let Some(change) = membership.change() {
        let display_name =
            membership.display_name().unwrap_or_else(|| membership.user_id().to_string());

        let change = match change {
            MembershipChange::Joined => "has joined the room",
            MembershipChange::Left => "has left the room",
            MembershipChange::Banned => "has been banned",
            MembershipChange::Unbanned => "has been unbanned",
            MembershipChange::Kicked => "has been kicked from the room",
            MembershipChange::Invited => "has been invited to the room",
            MembershipChange::KickedAndBanned => "has been kicked and banned from the room",
            MembershipChange::InvitationAccepted => "has accepted the invitation to the room",
            MembershipChange::InvitationRejected => "has rejected the invitation to the room",
            MembershipChange::Knocked => "knocked on the room",
            MembershipChange::KnockAccepted => "has accepted a knock on the room",
            MembershipChange::KnockRetracted => "has retracted a knock on the room",
            MembershipChange::KnockDenied => "has denied a knock",
            MembershipChange::None
            | MembershipChange::Error
            | MembershipChange::InvitationRevoked
            | MembershipChange::NotImplemented => "has changed its membership status",
        };

        Some(format!("{display_name} {change}").into())
    } else {
        None
    }
}
