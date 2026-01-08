use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use js_int::UInt;
use ruma::events::call::{
    SessionDescription,
    answer::CallAnswerEventContent,
    candidates::{CallCandidatesEventContent, Candidate},
    invite::CallInviteEventContent,
    member::{Application, CallMemberStateKey, MembershipData},
};
use ruma::{
    DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, OwnedVoipId, UserId,
    VoipVersionId,
};
use tokio::sync::Mutex;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::{API, APIBuilder};
use webrtc::data_channel::RTCDataChannel;
use webrtc::error::Result;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::Room;

/// Extracted details about a MatrixRTC membership for wiring WebRTC signaling.
#[derive(Clone, Debug)]
pub struct MatrixRtcCallMember {
    /// The user that owns this membership.
    pub user_id: OwnedUserId,
    /// The device that owns this membership.
    pub device_id: OwnedDeviceId,
    /// The call identifier advertised in the membership application.
    pub call_id: String,
    /// When this membership was first seen (used for oldest-member selection).
    pub created_ts: Option<MilliSecondsSinceUnixEpoch>,
    /// The raw state key for tie-breaking.
    pub state_key: CallMemberStateKey,
}

impl MatrixRtcCallMember {
    fn from_membership(
        state_key: &CallMemberStateKey,
        membership: MembershipData<'_>,
    ) -> Option<Self> {
        let Application::Call(call) = membership.application() else {
            return None;
        };
        Some(Self {
            user_id: state_key.user_id().to_owned(),
            device_id: membership.device_id().to_owned(),
            call_id: call.call_id.clone(),
            created_ts: membership.created_ts(),
            state_key: state_key.clone(),
        })
    }
}

/// Collect the current room-call memberships for a room to drive WebRTC signaling.
pub fn active_room_call_memberships(room: &Room) -> Vec<MatrixRtcCallMember> {
    room.info
        .read()
        .active_room_call_memberships()
        .into_iter()
        .filter_map(|(state_key, membership)| {
            MatrixRtcCallMember::from_membership(&state_key, membership)
        })
        .collect()
}

/// Determine whether the local device should initiate the call based on membership ordering.
pub fn is_call_initiator(
    memberships: &[MatrixRtcCallMember],
    local_user_id: &UserId,
    local_device_id: &DeviceId,
) -> bool {
    let mut ordered = memberships.to_vec();
    ordered.sort_by(|left, right| {
        left.created_ts.cmp(&right.created_ts).then_with(|| left.state_key.cmp(&right.state_key))
    });

    ordered.first().is_some_and(|member| {
        member.user_id == local_user_id && member.device_id == local_device_id
    })
}

/// Identifiers used when sending MatrixRTC signaling events.
#[derive(Clone, Debug)]
pub struct MatrixRtcSignalingIds {
    /// The call identifier shared by all participants.
    pub call_id: OwnedVoipId,
    /// A per-device identifier for VoIP v1 calls.
    pub party_id: OwnedVoipId,
    /// The VoIP version to advertise in signaling events.
    pub version: VoipVersionId,
}

impl MatrixRtcSignalingIds {
    /// Build identifiers for a VoIP v0 call.
    pub fn version_0(call_id: OwnedVoipId) -> Self {
        Self { call_id: call_id.clone(), party_id: call_id, version: VoipVersionId::V0 }
    }

    /// Build identifiers for a VoIP v1 call.
    pub fn version_1(call_id: OwnedVoipId, party_id: OwnedVoipId) -> Self {
        Self { call_id, party_id, version: VoipVersionId::V1 }
    }
}

/// A callback that delivers outgoing MatrixRTC candidates to your signaling layer.
pub type CandidateSender = Arc<
    dyn Fn(CallCandidatesEventContent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

/// Represents a single WebRTC call session.
pub struct CallSession {
    /// The API object used for creating peer connections and other WebRTC entities.
    pub api: Arc<API>,

    /// The central object managing the connection, tracks, and candidates.
    pub peer_connection: Arc<RTCPeerConnection>,

    /// Optional DataChannel for sending arbitrary data.
    pub data_channel: Option<Arc<RTCDataChannel>>,

    /// State flag: true if this session initiated the call (created the Offer).
    pub is_initiator: bool,

    /// Storage for pending ICE candidates that arrived before the remote description was set.
    pub pending_ice_candidates: Mutex<Vec<RTCIceCandidateInit>>,

    /// Optional hook used to send MatrixRTC candidate events.
    candidate_sender: Mutex<Option<(MatrixRtcSignalingIds, CandidateSender)>>,
}

impl CallSession {
    /// Creates a new CallSession, setting up the MediaEngine, API, and PeerConnection.
    pub async fn new(is_initiator: bool) -> Result<Arc<Self>> {
        // 1. Setup Media Engine (registers codecs and transports)
        let mut m = MediaEngine::default();
        m.register_default_codecs()?; // Simple setup for common codecs (e.g., VP8/Opus)

        // 2. Create the API object
        let api = Arc::new(APIBuilder::new().with_media_engine(m).build());

        // 3. Configure the Peer Connection (STUN servers are mandatory for NAT traversal)
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        // 4. Create the Peer Connection
        let peer_connection = Arc::new(api.new_peer_connection(config).await?);

        let session = Arc::new(Self {
            api,
            peer_connection,
            data_channel: None, // Will be set up later if needed
            is_initiator,
            pending_ice_candidates: Mutex::new(Vec::new()),
            candidate_sender: Mutex::new(None),
        });

        // 5. Setup event handlers (Crucial for signaling)
        let session_clone = Arc::clone(&session);
        let peer_connection = Arc::clone(&session.peer_connection);
        peer_connection.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let session_clone = Arc::clone(&session_clone);
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Some((ids, sender)) =
                        session_clone.candidate_sender.lock().await.as_ref().cloned()
                    {
                        if let Ok(content) = CallSession::candidates_event(&ids, vec![candidate]) {
                            (sender)(content).await;
                        }
                    }
                }
            })
        }));

        Ok(session)
    }

    /// Register a callback that publishes outgoing ICE candidates as MatrixRTC events.
    pub async fn set_candidate_sender(&self, ids: MatrixRtcSignalingIds, sender: CandidateSender) {
        *self.candidate_sender.lock().await = Some((ids, sender));
    }

    /// Create an m.call.invite event from a WebRTC offer.
    pub async fn create_invite_event(
        &self,
        ids: &MatrixRtcSignalingIds,
        lifetime_ms: u64,
    ) -> Result<CallInviteEventContent> {
        if !self.is_initiator {
            return Err(webrtc::error::Error::new(
                "Only the initiator can create an offer.".to_owned(),
            ));
        }

        // 1. Create the offer
        let offer = self.peer_connection.create_offer(None).await?;

        // 2. Set the offer as the Local Description
        self.peer_connection.set_local_description(offer.clone()).await?;

        // 3. Wrap the SDP into the Matrix event content structure
        let offer = to_matrix_session_description(offer);
        let lifetime = UInt::new_saturating(lifetime_ms);
        let content = match ids.version {
            VoipVersionId::V0 => {
                CallInviteEventContent::version_0(ids.call_id.clone(), lifetime, offer)
            }
            VoipVersionId::V1 => CallInviteEventContent::version_1(
                ids.call_id.clone(),
                ids.party_id.clone(),
                lifetime,
                offer,
            ),
            _ => CallInviteEventContent::new(
                ids.call_id.clone(),
                lifetime,
                offer,
                ids.version.clone(),
            ),
        };

        Ok(content)
    }

    /// Receiver side: Handles the incoming Offer, sets it as the Remote Description,
    /// creates an Answer, sets it as the Local Description, and returns the Answer.
    pub async fn handle_offer(
        &self,
        offer: RTCSessionDescription,
    ) -> Result<RTCSessionDescription> {
        if self.is_initiator {
            return Err(webrtc::error::Error::new("Initiator cannot handle an offer.".to_owned()));
        }

        // 1. Set the remote description (the received offer)
        self.peer_connection.set_remote_description(offer).await?;

        // 2. Create the answer
        let answer = self.peer_connection.create_answer(None).await?;

        // 3. Set the answer as the Local Description
        self.peer_connection.set_local_description(answer.clone()).await?;

        // 4. Handle any pending ICE candidates that arrived before the Remote Description was set
        let mut candidates = self.pending_ice_candidates.lock().await;
        for candidate in candidates.drain(..) {
            self.peer_connection.add_ice_candidate(candidate).await?;
        }

        // The answer SDP should now be sent back to the initiator.
        Ok(answer)
    }

    /// Receiver side: Handles the incoming MatrixRTC invite and returns an answer event.
    pub async fn handle_invite_event(
        &self,
        invite: &CallInviteEventContent,
        ids: &MatrixRtcSignalingIds,
    ) -> Result<CallAnswerEventContent> {
        let answer = self.handle_offer(from_matrix_session_description(&invite.offer)?).await?;
        let answer = to_matrix_session_description(answer);
        let content = match invite.version {
            VoipVersionId::V0 => CallAnswerEventContent::version_0(answer, invite.call_id.clone()),
            VoipVersionId::V1 => CallAnswerEventContent::version_1(
                answer,
                invite.call_id.clone(),
                ids.party_id.clone(),
            ),
            _ => {
                CallAnswerEventContent::new(answer, invite.call_id.clone(), invite.version.clone())
            }
        };

        Ok(content)
    }

    /// Initiator side: Handles the incoming Answer from the remote peer.
    pub async fn handle_answer(&self, answer: RTCSessionDescription) -> Result<()> {
        if !self.is_initiator {
            return Err(webrtc::error::Error::new(
                "Only the initiator handles the answer.".to_owned(),
            ));
        }

        // 1. Set the remote description (the received answer)
        self.peer_connection.set_remote_description(answer).await?;

        // 2. Handle any pending ICE candidates
        let mut candidates = self.pending_ice_candidates.lock().await;
        for candidate in candidates.drain(..) {
            self.peer_connection.add_ice_candidate(candidate).await?;
        }

        Ok(())
    }

    /// Initiator side: Handles an incoming MatrixRTC answer event.
    pub async fn handle_answer_event(&self, answer: &CallAnswerEventContent) -> Result<()> {
        self.handle_answer(from_matrix_session_description(&answer.answer)?).await
    }

    /// Handles an incoming ICE candidate from the remote peer.
    ///
    /// If the remote description has already been set, the candidate is added immediately.
    /// Otherwise, it is buffered in `pending_ice_candidates`.
    pub async fn add_remote_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        let pc_state = self.peer_connection.remote_description().await;

        if pc_state.is_some() {
            // Remote description is set, add the candidate immediately.
            self.peer_connection.add_ice_candidate(candidate).await?;
        } else {
            // Remote description is not set yet (signaling race condition), buffer the candidate.
            let mut candidates = self.pending_ice_candidates.lock().await;
            candidates.push(candidate);
        }

        Ok(())
    }

    /// Handles an incoming MatrixRTC candidates event.
    pub async fn handle_candidates_event(
        &self,
        candidates: &CallCandidatesEventContent,
    ) -> Result<()> {
        for candidate in &candidates.candidates {
            let init = RTCIceCandidateInit {
                candidate: candidate.candidate.clone(),
                sdp_mid: candidate.sdp_mid.clone(),
                sdp_mline_index: candidate
                    .sdp_m_line_index
                    .map(|index| u16::try_from(i64::from(index)).unwrap_or_default()),
                username_fragment: None,
            };
            self.add_remote_candidate(init).await?;
        }

        Ok(())
    }

    /// Convert a batch of local ICE candidates into an m.call.candidates event.
    pub fn candidates_event(
        ids: &MatrixRtcSignalingIds,
        candidates: Vec<RTCIceCandidate>,
    ) -> Result<CallCandidatesEventContent> {
        let candidates = candidates
            .into_iter()
            .filter_map(|candidate| match candidate.to_json() {
                Ok(json) => {
                    let mut candidate = Candidate::new(json.candidate);
                    candidate.sdp_mid = json.sdp_mid;
                    candidate.sdp_m_line_index =
                        json.sdp_mline_index.map(|index| UInt::new_saturating(u64::from(index)));
                    Some(candidate)
                }
                Err(_) => None,
            })
            .collect::<Vec<_>>();

        let content = match ids.version {
            VoipVersionId::V0 => {
                CallCandidatesEventContent::version_0(ids.call_id.clone(), candidates)
            }
            VoipVersionId::V1 => CallCandidatesEventContent::version_1(
                ids.call_id.clone(),
                ids.party_id.clone(),
                candidates,
            ),
            _ => CallCandidatesEventContent::new(
                ids.call_id.clone(),
                candidates,
                ids.version.clone(),
            ),
        };

        Ok(content)
    }
}

fn to_matrix_session_description(desc: RTCSessionDescription) -> SessionDescription {
    SessionDescription::new(desc.sdp_type.to_string(), desc.sdp)
}

fn from_matrix_session_description(desc: &SessionDescription) -> Result<RTCSessionDescription> {
    match desc.session_type.as_str() {
        "answer" => RTCSessionDescription::answer(desc.sdp.clone()),
        "offer" => RTCSessionDescription::offer(desc.sdp.clone()),
        "pranswer" => RTCSessionDescription::pranswer(desc.sdp.clone()),
        other => {
            Err(webrtc::error::Error::new(format!("Unsupported session description type: {other}")))
        }
    }
}
