use std::sync::Arc;
use std::collections::HashMap;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::{API,APIBuilder};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::error::Result;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use ruma::events::call::answer::CallAnswerEventContent;
use ruma::events::call::invite::CallInviteEventContent;
//use ruma::events::call::{CallId, PartyId, SessionId, VoipVersion};
use tokio::sync::Mutex;


pub struct Client {
    // ...existing fields...
    rtc: RtcService, 
}

pub struct RtcService {
    // Reference back to the main client for sending events
    client: Arc<Client>,
    // Map of active calls: Call ID -> CallSession
    active_calls: Mutex<HashMap<String, Arc<CallSession>>>,
    // Key provider (as discussed previously)
    // key_provider: Arc<dyn RtcKeyProvider>,
}

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
    pub pending_ice_candidates: Mutex<Vec<webrtc::ice_transport::ice_candidate::RTCIceCandidate>>,
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
            ice_servers: vec![
                RTCIceServer {
                    urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                    ..Default::default()
                },
            ],
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
        });

        // 5. Setup event handlers (Crucial for signaling)
        let session_clone = Arc::clone(&session);
        session_clone.peer_connection.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    // NOTE: In a real application, you would send this candidate
                    // struct's information (via JSON/string) to the remote peer
                    // using your signaling server (e.g., WebSocket).
                    
                    println!("Local ICE Candidate found: {}", candidate.address);
                    
                    // Since this is just an example, we skip actual signaling
                    // but this is where the outbound message happens.
                }
            })
        }));

        Ok(session)
    }

    pub async fn create_offer(
        &self, 
        call_id: CallId, 
        session_id: SessionId, 
        party_id: PartyId
    ) -> Result<InviteContent> {
        if !self.is_initiator {
            return Err(webrtc::error::Error::new("Only the initiator can create an offer.".to_owned()));
        }
        
        // 1. Create the offer
        let offer = self.peer_connection.create_offer(None).await?;

        // 2. Set the offer as the Local Description
        self.peer_connection.set_local_description(offer.clone()).await?;

        // 3. Wrap the SDP into the Matrix event content structure
        let content = InviteContent::new(
            call_id,
            party_id,
            session_id,
            offer.sdp,
            VoipVersion::V0, // Use the appropriate VOIP version
        );

        Ok(content)
    }

    /// Receiver side: Handles the incoming Offer, sets it as the Remote Description, 
    /// creates an Answer, sets it as the Local Description, and returns the Answer.
    pub async fn handle_offer(&self, offer: RTCSessionDescription) -> Result<RTCSessionDescription> {
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
            self.peer_connection.add_ice_candidate(
                RTCIceCandidateInit {
                    candidate: candidate.to_string(),
                    ..Default::default()}).await?;
        }
        
        // The answer SDP should now be sent back to the initiator.
        Ok(answer)
    }

    /// Initiator side: Handles the incoming Answer from the remote peer.
    pub async fn handle_answer(&self, answer: RTCSessionDescription) -> Result<()> {
        if !self.is_initiator {
            return Err(webrtc::error::Error::new("Only the initiator handles the answer.".to_owned()));
        }

        // 1. Set the remote description (the received answer)
        self.peer_connection.set_remote_description(answer).await?;

        // 2. Handle any pending ICE candidates
        let mut candidates = self.pending_ice_candidates.lock().await;
        for candidate in candidates.drain(..) {
            self.peer_connection.add_ice_candidate(
                RTCIceCandidateInit {
                    candidate: candidate.to_string(),
                    ..Default::default()}).await?;
        }

        Ok(())
    }

    /// Handles an incoming ICE candidate from the remote peer.
    /// 
    /// If the remote description has already been set, the candidate is added immediately.
    /// Otherwise, it is buffered in `pending_ice_candidates`.
    pub async fn add_remote_candidate(&self, candidate: RTCIceCandidate) -> Result<()> {
        let pc_state = self.peer_connection.remote_description().await;

        if pc_state.is_some() {
            // Remote description is set, add the candidate immediately.
            self.peer_connection.add_ice_candidate(
                RTCIceCandidateInit {
                    candidate: candidate.to_string(),
                    ..Default::default()}).await?;
        } else {
            // Remote description is not set yet (signaling race condition), buffer the candidate.
            let mut candidates = self.pending_ice_candidates.lock().await;
            candidates.push(candidate);
        }
        
        Ok(())
    }

}
