use std::sync::Arc;

use imbl::Vector;
use matrix_sdk::ruma::{events::room::message::MessageType, UserId};
use matrix_sdk_ui::timeline::{
    MembershipChange, Message, MsgLikeContent, MsgLikeKind, RoomMembershipChange, ThreadSummary,
    TimelineDetails, TimelineItem, TimelineItemContent, TimelineItemKind, VirtualTimelineItem,
};
use ratatui::{prelude::*, widgets::*};

use crate::{ALT_ROW_COLOR, NORMAL_ROW_COLOR, SELECTED_STYLE_FG, TEXT_COLOR};

pub struct TimelineView<'a> {
    items: &'a Vector<Arc<TimelineItem>>,
}

impl<'a> TimelineView<'a> {
    pub fn new(items: &'a Vector<Arc<TimelineItem>>) -> Self {
        Self { items }
    }
}

impl StatefulWidget for &mut TimelineView<'_> {
    type State = ListState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State)
    where
        Self: Sized,
    {
        let content = self.items.iter().map(format_timeline_item);

        let list_items = content
            .flatten()
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

        StatefulWidget::render(list, area, buf, state);
    }
}

fn format_timeline_item(item: &Arc<TimelineItem>) -> Option<ListItem<'_>> {
    let item = match item.kind() {
        TimelineItemKind::Event(ev) => {
            // TODO: Once the SDK allows you to filter out messages that are part of a
            // thread, switch to that mechanism instead of manually returning `None` here.
            if ev.content().thread_root().is_some() {
                return None;
            }

            let sender = ev.sender();

            match ev.content() {
                TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Message(message),
                    ..
                }) => format_text_message(sender, message, ev.content().thread_summary())?,

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
                | TimelineItemContent::ProfileChange(_)
                | TimelineItemContent::OtherState(_)
                | TimelineItemContent::FailedToParseMessageLike { .. }
                | TimelineItemContent::FailedToParseState { .. }
                | TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Poll(_), ..
                })
                | TimelineItemContent::CallInvite
                | TimelineItemContent::CallNotify => {
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
            | MembershipChange::NotImplemented => "has changed it's membership status",
        };

        Some(format!("{display_name} {change}").into())
    } else {
        None
    }
}
