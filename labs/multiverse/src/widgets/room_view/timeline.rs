use std::sync::Arc;

use imbl::Vector;
use matrix_sdk::ruma::events::room::message::MessageType;
use matrix_sdk_ui::timeline::{
    MembershipChange, MsgLikeContent, MsgLikeKind, TimelineItem, TimelineItemContent,
    TimelineItemKind, VirtualTimelineItem,
};
use ratatui::{prelude::*, widgets::*};

use crate::{ALT_ROW_COLOR, NORMAL_ROW_COLOR, TEXT_COLOR};

pub struct TimelineView<'a> {
    items: &'a Vector<Arc<TimelineItem>>,
}

impl<'a> TimelineView<'a> {
    pub fn new(items: &'a Vector<Arc<TimelineItem>>) -> Self {
        Self { items }
    }
}

impl Widget for &mut TimelineView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let mut content = Vec::new();

        for item in self.items.iter() {
            match item.kind() {
                TimelineItemKind::Event(ev) => {
                    let sender = ev.sender();

                    match ev.content() {
                        TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::Message(message),
                            ..
                        }) => {
                            if let MessageType::Text(text) = message.msgtype() {
                                content.push(format!("{}: {}", sender, text.body))
                            }
                        }

                        TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::Redacted,
                            ..
                        }) => content.push(format!("{}: -- redacted --", sender)),

                        TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::UnableToDecrypt(_),
                            ..
                        }) => content.push(format!("{}: (UTD)", sender)),

                        TimelineItemContent::MembershipChange(m) => {
                            if let Some(change) = m.change() {
                                let display_name =
                                    m.display_name().unwrap_or_else(|| m.user_id().to_string());

                                let change = match change {
                                    MembershipChange::Joined => "has joined the room",
                                    MembershipChange::Left => "has left the room",
                                    MembershipChange::Banned => "has been banned",
                                    MembershipChange::Unbanned => "has been unbanned",
                                    MembershipChange::Kicked => "has been kicked from the room",
                                    MembershipChange::Invited => "has been invited to the room",
                                    MembershipChange::KickedAndBanned => {
                                        "has been kicked and banned from the room"
                                    }
                                    MembershipChange::InvitationAccepted => {
                                        "has accepted the invitation to the room"
                                    }
                                    MembershipChange::InvitationRejected => {
                                        "has rejected the invitation to the room"
                                    }
                                    MembershipChange::Knocked => "knocked on the room",
                                    MembershipChange::KnockAccepted => {
                                        "has accepted a knock on the room"
                                    }
                                    MembershipChange::KnockRetracted => {
                                        "has retracted a knock on the room"
                                    }
                                    MembershipChange::KnockDenied => "has denied a knock",
                                    MembershipChange::None
                                    | MembershipChange::Error
                                    | MembershipChange::InvitationRevoked
                                    | MembershipChange::NotImplemented => {
                                        "has changed it's membership status"
                                    }
                                };

                                content.push(format!("{display_name} {change}"));
                            }
                        }

                        TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::Sticker(_),
                            ..
                        })
                        | TimelineItemContent::ProfileChange(_)
                        | TimelineItemContent::OtherState(_)
                        | TimelineItemContent::FailedToParseMessageLike { .. }
                        | TimelineItemContent::FailedToParseState { .. }
                        | TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::Poll(_),
                            ..
                        })
                        | TimelineItemContent::CallInvite
                        | TimelineItemContent::CallNotify => {
                            continue;
                        }
                    }
                }

                TimelineItemKind::Virtual(virt) => match virt {
                    VirtualTimelineItem::DateDivider(unix_ts) => {
                        content.push(format!("Date: {unix_ts:?}"));
                    }
                    VirtualTimelineItem::ReadMarker => {
                        content.push("Read marker".to_owned());
                    }
                    VirtualTimelineItem::TimelineStart => {
                        content.push("🥳 Timeline start! 🥳".to_owned());
                    }
                },
            }
        }

        let list_items = content
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let bg_color = match i % 2 {
                    0 => NORMAL_ROW_COLOR,
                    _ => ALT_ROW_COLOR,
                };
                let line = Line::styled(line, TEXT_COLOR);
                ListItem::new(line).bg(bg_color)
            })
            .collect::<Vec<_>>();

        let list = List::new(list_items).highlight_spacing(HighlightSpacing::Always);

        let mut dummy_list_state = ListState::default();

        StatefulWidget::render(list, area, buf, &mut dummy_list_state);
    }
}
