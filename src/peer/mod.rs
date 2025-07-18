//! Adapters to [RTCPeerConnection][1] and related objects.
//!
//! [1]: https://w3.org/TR/webrtc#rtcpeerconnection-interface

mod component;
pub mod media;
pub mod repo;
mod stream_update_criteria;
mod tracks_request;

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash as _, Hasher as _},
    rc::Rc,
};

use derive_more::with_trait::{Display, From};
use futures::{StreamExt as _, channel::mpsc, future};
use medea_client_api_proto::{
    Command, ConnectionMode, IceConnectionState, MediaSourceKind, MemberId,
    PeerConnectionState, PeerId as Id, PeerId, TrackId, TrackPatchCommand,
    stats::StatId,
};
use medea_macro::dispatchable;
use tracerr::Traced;

#[doc(inline)]
pub use self::{
    component::{Component, DESCRIPTION_APPROVE_TIMEOUT, State},
    media::{
        GetMidsError, InsertLocalTracksError, MediaConnections,
        MediaExchangeState, MediaExchangeStateController, MediaState,
        MediaStateControllable, MuteState, MuteStateController,
        ProhibitedStateError, TrackDirection, TransceiverSide,
        TransitableState, TransitableStateController, media_exchange_state,
        mute_state, receiver, sender,
    },
    platform::RtcPeerConnectionError,
    stream_update_criteria::LocalStreamUpdateCriteria,
    tracks_request::{SimpleTracksRequest, TracksRequest, TracksRequestError},
};
use crate::{
    connection::Connections,
    media::{
        InitLocalTracksError, LocalTracksConstraints, MediaKind, MediaManager,
        MediaStreamSettings, RecvConstraints,
        track::{local, remote},
    },
    platform,
    utils::Caused,
};

/// Errors occurring in [`PeerConnection::update_local_stream()`] method.
#[derive(Caused, Clone, Debug, Display, From)]
#[cause(error = platform::Error)]
pub enum UpdateLocalStreamError {
    /// Errors occurred when [`TracksRequest`] validation fails.
    InvalidLocalTracks(TracksRequestError),

    /// [`MediaManager`] failed to acquire [`local::Track`]s.
    CouldNotGetLocalMedia(#[cause] InitLocalTracksError),

    /// Errors occurred in [`MediaConnections::insert_local_tracks()`] method.
    InsertLocalTracksError(#[cause] InsertLocalTracksError),
}

/// Events emitted from a [`Sender`] or a [`Receiver`].
///
/// [`Receiver`]: receiver::Receiver
/// [`Sender`]: sender::Sender
#[derive(Clone, Copy, Debug)]
pub enum TrackEvent {
    /// Intention of the `MediaTrack` to mute/unmute himself.
    MuteUpdateIntention {
        /// ID of the `MediaTrack` which sends this intention.
        id: TrackId,

        /// The muting intention itself.
        muted: bool,
    },

    /// Intention of the `MediaTrack` to enabled/disable himself.
    MediaExchangeIntention {
        /// ID of the `MediaTrack` which sends this intention.
        id: TrackId,

        /// The enabling/disabling intention itself.
        enabled: bool,
    },
}

/// Local media update errors that [`PeerConnection`] reports in
/// [`PeerEvent::FailedLocalMedia`] messages.
#[derive(Caused, Clone, Debug, Display, From)]
#[cause(error = platform::Error)]
pub enum LocalMediaError {
    /// Error occurred in [`PeerConnection::update_local_stream()`] method.
    UpdateLocalStreamError(#[cause] UpdateLocalStreamError),

    /// Error occurred when creating a new [`Sender`].
    ///
    /// [`Sender`]: sender::Sender
    SenderCreateError(sender::CreateError),
}

/// Events emitted from [`platform::RtcPeerConnection`].
#[dispatchable(self: &Self, async_trait(?Send))]
#[derive(Clone, Debug)]
pub enum PeerEvent {
    /// [`platform::RtcPeerConnection`] discovered new ICE candidate.
    ///
    /// Wrapper around [RTCPeerConnectionIceEvent][1].
    ///
    /// [1]: https://w3.org/TR/webrtc#rtcpeerconnectioniceevent
    IceCandidateDiscovered {
        /// ID of the [`PeerConnection`] that discovered new ICE candidate.
        peer_id: Id,

        /// [`candidate` field][2] of the discovered [RTCIceCandidate][1].
        ///
        /// [1]: https://w3.org/TR/webrtc#dom-rtcicecandidate
        /// [2]: https://w3.org/TR/webrtc#dom-rtcicecandidate-candidate
        candidate: String,

        /// [`sdpMLineIndex` field][2] of the discovered [RTCIceCandidate][1].
        ///
        /// [1]: https://w3.org/TR/webrtc#dom-rtcicecandidate
        /// [2]: https://w3.org/TR/webrtc#dom-rtcicecandidate-sdpmlineindex
        sdp_m_line_index: Option<u16>,

        /// [`sdpMid` field][2] of the discovered [RTCIceCandidate][1].
        ///
        /// [1]: https://w3.org/TR/webrtc#dom-rtcicecandidate
        /// [2]: https://w3.org/TR/webrtc#dom-rtcicecandidate-sdpmid
        sdp_mid: Option<String>,
    },

    /// Error occurred with an [ICE] candidate from a [`PeerConnection`].
    ///
    /// [ICE]: https://webrtcglossary.com/ice
    IceCandidateError {
        /// ID of the [`PeerConnection`] that errored.
        peer_id: Id,

        /// Local IP address used to communicate with a [STUN]/[TURN]
        /// server.
        ///
        /// [STUN]: https://webrtcglossary.com/stun
        /// [TURN]: https://webrtcglossary.com/turn
        address: Option<String>,

        /// Port used to communicate with a [STUN]/[TURN] server.
        ///
        /// [STUN]: https://webrtcglossary.com/stun
        /// [TURN]: https://webrtcglossary.com/turn
        port: Option<u32>,

        /// URL identifying the [STUN]/[TURN] server for which the failure
        /// occurred.
        ///
        /// [STUN]: https://webrtcglossary.com/stun
        /// [TURN]: https://webrtcglossary.com/turn
        url: String,

        /// Numeric [STUN] error code returned by the [STUN]/[TURN] server.
        ///
        /// If no host candidate can reach the server, this error code will be
        /// set to the value `701`, which is outside the [STUN] error code
        /// range. This error is only fired once per server URL while in the
        /// `RTCIceGatheringState` of "gathering".
        ///
        /// [STUN]: https://webrtcglossary.com/stun
        /// [TURN]: https://webrtcglossary.com/turn
        error_code: i32,

        /// [STUN] reason text returned by the [STUN]/[TURN] server.
        ///
        /// If the server could not be reached, this reason test will be set to
        /// an implementation-specific value providing details about
        /// the error.
        ///
        /// [STUN]: https://webrtcglossary.com/stun
        /// [TURN]: https://webrtcglossary.com/turn
        error_text: String,
    },

    /// [`platform::RtcPeerConnection`] received a new [`remote::Track`] from
    /// a remote sender.
    NewRemoteTrack {
        /// Remote `Member` ID.
        sender_id: MemberId,

        /// Received [`remote::Track`].
        track: remote::Track,
    },

    /// [`platform::RtcPeerConnection`] sent new local track to remote members.
    NewLocalTrack {
        /// Local [`local::Track`] that is sent to remote members.
        local_track: Rc<local::Track>,
    },

    /// [`platform::RtcPeerConnection`]'s [ICE connection][1] state changed.
    ///
    /// [1]: https://w3.org/TR/webrtc#dfn-ice-connection-state
    IceConnectionStateChanged {
        /// ID of the [`PeerConnection`] that sends
        /// [`iceconnectionstatechange`][1] event.
        ///
        /// [1]: https://w3.org/TR/webrtc#event-iceconnectionstatechange
        peer_id: Id,

        /// New [`IceConnectionState`].
        ice_connection_state: IceConnectionState,
    },

    /// [`platform::RtcPeerConnection`]'s [connection][1] state changed.
    ///
    /// [1]: https://w3.org/TR/webrtc#dfn-ice-connection-state
    PeerConnectionStateChanged {
        /// ID of the [`PeerConnection`] that sends
        /// [`connectionstatechange`][1] event.
        ///
        /// [1]: https://w3.org/TR/webrtc#event-connectionstatechange
        peer_id: Id,

        /// New [`PeerConnectionState`].
        peer_connection_state: PeerConnectionState,
    },

    /// [`platform::RtcPeerConnection`]'s [`platform::RtcStats`] update.
    StatsUpdate {
        /// ID of the [`PeerConnection`] for which [` platform::RtcStats`] was
        /// sent.
        peer_id: Id,

        /// [` platform::RtcStats`] of this [`PeerConnection`].
        stats: platform::RtcStats,
    },

    /// [`PeerConnection::update_local_stream`] was failed, so
    /// `on_failed_local_stream` callback should be called.
    FailedLocalMedia {
        /// Reasons of local media updating fail.
        error: Traced<LocalMediaError>,
    },

    /// [`Component`] generated a new SDP answer.
    NewSdpAnswer {
        /// ID of the [`PeerConnection`] for which SDP answer was generated.
        peer_id: PeerId,

        /// SDP Answer of the `Peer`.
        sdp_answer: String,

        /// Statuses of `Peer` transceivers.
        transceivers_statuses: HashMap<TrackId, bool>,
    },

    /// [`Component`] generated a new SDP offer.
    NewSdpOffer {
        /// ID of the [`PeerConnection`] for which SDP offer was generated.
        peer_id: PeerId,

        /// SDP Offer of the [`PeerConnection`].
        sdp_offer: String,

        /// Associations between `Track` and transceiver's
        /// [media description][1].
        ///
        /// `mid` is basically an ID of [`m=<media>` section][1] in SDP.
        ///
        /// [1]: https://tools.ietf.org/html/rfc4566#section-5.14
        mids: HashMap<TrackId, String>,

        /// Statuses of [`PeerConnection`] transceivers.
        transceivers_statuses: HashMap<TrackId, bool>,
    },

    /// [`Component`] resends his intentions.
    MediaUpdateCommand {
        /// Actual intentions of the [`Component`].
        command: Command,
    },
}

/// High-level wrapper around a [`platform::RtcPeerConnection`].
#[derive(Debug)]
pub struct PeerConnection {
    /// Unique ID of [`PeerConnection`].
    id: Id,

    /// Underlying [`platform::RtcPeerConnection`].
    peer: Rc<platform::RtcPeerConnection>,

    /// [`sender::Component`]s and [`receiver::Component`]s of this
    /// [`platform::RtcPeerConnection`].
    media_connections: Rc<MediaConnections>,

    /// [`MediaManager`] that will be used to acquire [`local::Track`]s.
    media_manager: Rc<MediaManager>,

    /// [`PeerEvent`]s tx.
    peer_events_sender: Rc<mpsc::UnboundedSender<PeerEvent>>,

    /// Indicator whether the underlying [`platform::RtcPeerConnection`] has a
    /// remote description.
    has_remote_description: Cell<bool>,

    /// Buffer of [`platform::IceCandidate`]s received before a remote
    /// description for the underlying [`platform::RtcPeerConnection`].
    ice_candidates_buffer: RefCell<Vec<platform::IceCandidate>>,

    /// Last hashes of all the [`platform::RtcStats`] which were already sent
    /// to a server, so we won't duplicate stats that were already sent.
    ///
    /// Stores precomputed hashes, since we don't need access to actual stats
    /// values.
    sent_stats_cache: RefCell<HashMap<StatId, u64>>,

    /// Local media stream constraints used in this [`PeerConnection`].
    send_constraints: LocalTracksConstraints,

    /// Collection of [`Connection`]s with a remote `Member`s.
    ///
    /// [`Connection`]: crate::connection::Connection
    connections: Rc<Connections>,

    /// Sender for the [`TrackEvent`]s which should be processed by this
    /// [`PeerConnection`].
    track_events_sender: mpsc::UnboundedSender<TrackEvent>,

    /// Constraints to the [`remote::Track`] from this [`PeerConnection`]. Used
    /// to disable or enable media receiving.
    recv_constraints: Rc<RecvConstraints>,
}

impl PeerConnection {
    /// Creates a new [`PeerConnection`].
    ///
    /// Provided `peer_events_sender` will be used to emit [`PeerEvent`]s from
    /// this peer.
    ///
    /// Provided `ice_servers` will be used by the created
    /// [`platform::RtcPeerConnection`].
    ///
    /// # Errors
    ///
    /// Errors with an [`RtcPeerConnectionError::PeerCreationError`] if
    /// [`platform::RtcPeerConnection`] creating fails.
    pub async fn new(
        state: &State,
        peer_events_sender: mpsc::UnboundedSender<PeerEvent>,
        media_manager: Rc<MediaManager>,
        send_constraints: LocalTracksConstraints,
        connections: Rc<Connections>,
        recv_constraints: Rc<RecvConstraints>,
    ) -> Result<Rc<Self>, Traced<RtcPeerConnectionError>> {
        let peer = Rc::new(
            platform::RtcPeerConnection::new(
                state.ice_servers().clone(),
                state.force_relay(),
            )
            .await
            .map_err(tracerr::map_from_and_wrap!())?,
        );
        let (track_events_sender, mut track_events_rx) = mpsc::unbounded();
        let media_connections = Rc::new(MediaConnections::new(
            Rc::clone(&peer),
            peer_events_sender.clone(),
        ));

        platform::spawn({
            let peer_events_sender = peer_events_sender.clone();
            let peer_id = state.id();

            async move {
                while let Some(e) = track_events_rx.next().await {
                    Self::handle_track_event(peer_id, &peer_events_sender, e);
                }
            }
        });

        let peer = Self {
            id: state.id(),
            peer,
            media_connections,
            media_manager,
            peer_events_sender: Rc::new(peer_events_sender),
            sent_stats_cache: RefCell::new(HashMap::new()),
            has_remote_description: Cell::new(false),
            ice_candidates_buffer: RefCell::new(Vec::new()),
            send_constraints,
            connections,
            track_events_sender,
            recv_constraints,
        };

        peer.bind_event_listeners(state);

        Ok(Rc::new(peer))
    }

    /// Binds all the necessary event listeners to this [`PeerConnection`].
    fn bind_event_listeners(&self, state: &State) {
        // Bind to `icecandidate` event.
        {
            let id = self.id;
            let weak_sender = Rc::downgrade(&self.peer_events_sender);
            self.peer.on_ice_candidate(Some(move |candidate| {
                if let Some(sender) = weak_sender.upgrade() {
                    Self::on_ice_candidate(id, &sender, candidate);
                }
            }));
        }

        // Bind to `icecandidateerror` event.
        {
            let id = self.id;
            let weak_sender = Rc::downgrade(&self.peer_events_sender);
            self.peer.on_ice_candidate_error(Some(move |error| {
                if let Some(sender) = weak_sender.upgrade() {
                    Self::on_ice_candidate_error(id, &sender, error);
                }
            }));
        }

        // Bind to `iceconnectionstatechange` event.
        {
            let id = self.id;
            let weak_sender = Rc::downgrade(&self.peer_events_sender);
            self.peer.on_ice_connection_state_change(Some(
                move |ice_connection_state| {
                    if let Some(sender) = weak_sender.upgrade() {
                        Self::on_ice_connection_state_changed(
                            id,
                            &sender,
                            ice_connection_state,
                        );
                    }
                },
            ));
        }

        // Bind to `connectionstatechange` event.
        {
            let id = self.id;
            let weak_sender = Rc::downgrade(&self.peer_events_sender);
            self.peer.on_connection_state_change(Some(
                move |peer_connection_state| {
                    if let Some(sender) = weak_sender.upgrade() {
                        Self::on_connection_state_changed(
                            id,
                            &sender,
                            peer_connection_state,
                        );
                    }
                },
            ));
        }

        // Bind to `track` event.
        {
            let media_conns = Rc::downgrade(&self.media_connections);
            let connection_mode = state.connection_mode();
            self.peer.on_track(Some(move |track, transceiver| {
                if let Some(c) = media_conns.upgrade() {
                    platform::spawn(async move {
                        if let (Err(mid), ConnectionMode::Mesh) = (
                            c.add_remote_track(track, transceiver).await,
                            connection_mode,
                        ) {
                            log::error!(
                                "Cannot add new remote track with mid={mid}",
                            );
                        }
                    });
                }
            }));
        }
    }

    /// Handles [`TrackEvent`]s emitted from a [`Sender`] or a [`Receiver`].
    ///
    /// Sends a [`PeerEvent::MediaUpdateCommand`] with a
    /// [`Command::UpdateTracks`] on [`TrackEvent::MediaExchangeIntention`] and
    /// [`TrackEvent::MuteUpdateIntention`].
    ///
    /// [`Sender`]: sender::Sender
    /// [`Receiver`]: receiver::Receiver
    fn handle_track_event(
        peer_id: PeerId,
        peer_events_sender: &mpsc::UnboundedSender<PeerEvent>,
        event: TrackEvent,
    ) {
        let patch = match event {
            TrackEvent::MediaExchangeIntention { id, enabled } => {
                TrackPatchCommand { id, muted: None, enabled: Some(enabled) }
            }
            TrackEvent::MuteUpdateIntention { id, muted } => {
                TrackPatchCommand { id, muted: Some(muted), enabled: None }
            }
        };

        _ = peer_events_sender
            .unbounded_send(PeerEvent::MediaUpdateCommand {
                command: Command::UpdateTracks {
                    peer_id,
                    tracks_patches: vec![patch],
                },
            })
            .ok();
    }

    /// Returns all [`TrackId`]s of [`Sender`]s that match the provided
    /// [`LocalStreamUpdateCriteria`] and don't have [`local::Track`].
    ///
    /// [`Sender`]: sender::Sender
    #[must_use]
    pub fn get_senders_without_tracks_ids(
        &self,
        kinds: LocalStreamUpdateCriteria,
    ) -> Vec<TrackId> {
        self.media_connections.get_senders_without_tracks_ids(kinds)
    }

    /// Drops [`local::Track`]s of all [`Sender`]s which are matches provided
    /// [`LocalStreamUpdateCriteria`].
    ///
    /// [`Sender`]: sender::Sender
    pub async fn drop_send_tracks(&self, kinds: LocalStreamUpdateCriteria) {
        self.media_connections.drop_send_tracks(kinds).await;
    }

    /// Filters out already sent stats, and send new stats from the provided
    /// [`platform::RtcStats`].
    pub fn send_peer_stats(&self, stats: platform::RtcStats) {
        let mut stats_cache = self.sent_stats_cache.borrow_mut();
        let stats = platform::RtcStats(
            stats
                .0
                .into_iter()
                .filter(|stat| {
                    let mut hasher = DefaultHasher::new();
                    stat.stats.hash(&mut hasher);
                    let stat_hash = hasher.finish();

                    #[expect( // false positive
                        clippy::option_if_let_else,
                        reason = "false positive: &mut"
                    )]
                    if let Some(last_hash) = stats_cache.get_mut(&stat.id) {
                        if *last_hash == stat_hash {
                            false
                        } else {
                            *last_hash = stat_hash;
                            true
                        }
                    } else {
                        _ = stats_cache.insert(stat.id.clone(), stat_hash);
                        true
                    }
                })
                .collect(),
        );

        if !stats.0.is_empty() {
            drop(self.peer_events_sender.unbounded_send(
                PeerEvent::StatsUpdate { peer_id: self.id, stats },
            ));
        }
    }

    /// Sends [`platform::RtcStats`] update of this [`PeerConnection`] to a
    /// server.
    pub async fn scrape_and_send_peer_stats(&self) {
        match self.peer.get_stats().await {
            Ok(stats) => self.send_peer_stats(stats),
            Err(e) => log::error!("{e}"),
        }
    }

    /// Indicates whether all [`TransceiverSide`]s with the provided
    /// [`MediaKind`], [`TrackDirection`] and [`MediaSourceKind`] are in the
    /// provided [`MediaState`].
    #[must_use]
    pub fn is_all_transceiver_sides_in_media_state(
        &self,
        kind: MediaKind,
        direction: TrackDirection,
        source_kind: Option<MediaSourceKind>,
        state: MediaState,
    ) -> bool {
        self.media_connections.is_all_tracks_in_media_state(
            kind,
            direction,
            source_kind,
            state,
        )
    }

    /// Returns the [`PeerId`] of this [`PeerConnection`].
    pub const fn id(&self) -> PeerId {
        self.id
    }

    /// Handle `icecandidate` event from the underlying peer emitting
    /// [`PeerEvent::IceCandidateDiscovered`] event into this peer's
    /// `peer_events_sender`.
    fn on_ice_candidate(
        id: Id,
        sender: &mpsc::UnboundedSender<PeerEvent>,
        candidate: platform::IceCandidate,
    ) {
        drop(sender.unbounded_send(PeerEvent::IceCandidateDiscovered {
            peer_id: id,
            candidate: candidate.candidate,
            sdp_m_line_index: candidate.sdp_m_line_index,
            sdp_mid: candidate.sdp_mid,
        }));
    }

    /// Handle `icecandidateerror` event from the underlying peer emitting
    /// [`PeerEvent::IceCandidateError`] event into this peer's
    /// `peer_events_sender`.
    fn on_ice_candidate_error(
        id: Id,
        sender: &mpsc::UnboundedSender<PeerEvent>,
        error: platform::IceCandidateError,
    ) {
        drop(sender.unbounded_send(PeerEvent::IceCandidateError {
            peer_id: id,
            address: error.address,
            port: error.port,
            url: error.url,
            error_code: error.error_code,
            error_text: error.error_text,
        }));
    }

    /// Handle `iceconnectionstatechange` event from the underlying peer
    /// emitting [`PeerEvent::IceConnectionStateChanged`] event into this peer's
    /// `peer_events_sender`.
    fn on_ice_connection_state_changed(
        peer_id: Id,
        sender: &mpsc::UnboundedSender<PeerEvent>,
        ice_connection_state: IceConnectionState,
    ) {
        drop(sender.unbounded_send(PeerEvent::IceConnectionStateChanged {
            peer_id,
            ice_connection_state,
        }));
    }

    /// Handles `connectionstatechange` event from the underlying peer emitting
    /// [`PeerEvent::PeerConnectionStateChanged`] event into this peer's
    /// `peer_events_sender`.
    fn on_connection_state_changed(
        peer_id: Id,
        sender: &mpsc::UnboundedSender<PeerEvent>,
        peer_connection_state: PeerConnectionState,
    ) {
        drop(sender.unbounded_send(PeerEvent::PeerConnectionStateChanged {
            peer_id,
            peer_connection_state,
        }));
    }

    /// Sends [`PeerConnection`]'s connection state and ICE connection state to
    /// the server.
    fn send_current_connection_states(&self) {
        Self::on_ice_connection_state_changed(
            self.id,
            &self.peer_events_sender,
            self.peer.ice_connection_state(),
        );

        Self::on_connection_state_changed(
            self.id,
            &self.peer_events_sender,
            self.peer.connection_state(),
        );
    }

    /// Marks [`PeerConnection`] to trigger ICE restart.
    ///
    /// After this function returns, the generated offer is automatically
    /// configured to trigger ICE restart.
    fn restart_ice(&self) {
        self.peer.restart_ice();
    }

    /// Returns all [`TransceiverSide`]s from this [`PeerConnection`] with
    /// provided [`MediaKind`], [`TrackDirection`] and [`MediaSourceKind`].
    pub fn get_transceivers_sides(
        &self,
        kind: MediaKind,
        direction: TrackDirection,
        source_kind: Option<MediaSourceKind>,
    ) -> Vec<Rc<dyn TransceiverSide>> {
        self.media_connections.get_transceivers_sides(
            kind,
            direction,
            source_kind,
        )
    }

    /// Track id to mid relations of all send tracks of this
    /// [`platform::RtcPeerConnection`]. mid is id of [`m= section`][1]. mids
    /// are received directly from registered [`RTCRtpTransceiver`][2]s, and
    /// are being allocated on SDP update.
    ///
    /// # Errors
    ///
    /// Errors if finds transceiver without mid, so must be called after setting
    /// local description if offerer, and remote if answerer.
    ///
    /// [1]: https://tools.ietf.org/html/rfc4566#section-5.14
    /// [2]: https://w3.org/TR/webrtc#rtcrtptransceiver-interface
    fn get_mids(
        &self,
    ) -> Result<HashMap<TrackId, String>, Traced<GetMidsError>> {
        self.media_connections.get_mids().map_err(tracerr::wrap!())
    }

    /// Returns publishing statuses of the all [`Sender`]s from this
    /// [`MediaConnections`].
    ///
    /// [`Sender`]: sender::Sender
    async fn get_transceivers_statuses(&self) -> HashMap<TrackId, bool> {
        self.media_connections.get_transceivers_statuses().await
    }

    /// Updates [`local::Track`]s being used in [`PeerConnection`]s [`Sender`]s.
    /// [`Sender`]s are chosen based on the provided
    /// [`LocalStreamUpdateCriteria`].
    ///
    /// First of all makes sure that [`PeerConnection`] [`Sender`]s are
    /// up-to-date and synchronized with a real object state. If there are no
    /// [`Sender`]s configured in this [`PeerConnection`], then this method is
    /// no-op.
    ///
    /// Secondly, make sure that configured [`LocalTracksConstraints`] are up to
    /// date.
    ///
    /// This function requests local stream from [`MediaManager`]. If stream
    /// returned from [`MediaManager`] is considered new, then this function
    /// will emit [`PeerEvent::NewLocalTrack`] events.
    ///
    /// Constraints being used when requesting stream from [`MediaManager`] are
    /// a result of merging constraints received from this [`PeerConnection`]
    /// [`Sender`]s, which are configured by server during signalling, and
    /// [`LocalTracksConstraints`].
    ///
    /// Returns [`HashMap`] with [`media_exchange_state::Stable`]s updates for
    /// the [`Sender`]s.
    ///
    /// # Errors
    ///
    /// With an [`UpdateLocalStreamError::InvalidLocalTracks`] if the current
    /// state of the [`PeerConnection`]'s [`Sender`]s cannot be represented as
    /// [`SimpleTracksRequest`] (max 1 audio [`Sender`] and max 2 video
    /// [`Sender`]s), or the [`local::Track`]s requested from the
    /// [`MediaManager`] doesn't satisfy [`Sender`]'s constraints.
    ///
    /// With an [`UpdateLocalStreamError::CouldNotGetLocalMedia`] if the
    /// [`local::Track`]s cannot be obtained from the UA.
    ///
    /// With an [`UpdateLocalStreamError::InvalidLocalTracks`] if the
    /// [`local::Track`]s cannot be inserted into [`PeerConnection`]s
    /// [`Sender`]s.
    ///
    /// [`Sender`]: sender::Sender
    /// [1]: https://w3.org/TR/mediacapture-streams#mediastream
    /// [2]: https://w3.org/TR/webrtc#rtcpeerconnection-interface
    pub async fn update_local_stream(
        &self,
        criteria: LocalStreamUpdateCriteria,
    ) -> Result<
        HashMap<TrackId, media_exchange_state::Stable>,
        Traced<UpdateLocalStreamError>,
    > {
        self.inner_update_local_stream(criteria).await.inspect_err(|e| {
            drop(self.peer_events_sender.unbounded_send(
                PeerEvent::FailedLocalMedia {
                    error: tracerr::map_from(e.clone()),
                },
            ));
        })
    }

    /// Returns [`MediaStreamSettings`] for the provided [`MediaKind`] and
    /// [`MediaSourceKind`].
    ///
    /// If [`MediaSourceKind`] is [`None`] then [`MediaStreamSettings`] for all
    /// [`MediaSourceKind`]s will be provided.
    ///
    /// # Errors
    ///
    /// Errors with a [`TracksRequestError`] if failed to create or merge
    /// [`SimpleTracksRequest`].
    pub fn get_media_settings(
        &self,
        kind: MediaKind,
        source_kind: Option<MediaSourceKind>,
    ) -> Result<Option<MediaStreamSettings>, Traced<TracksRequestError>> {
        let mut criteria = LocalStreamUpdateCriteria::empty();
        if let Some(msk) = source_kind {
            criteria.add(kind, msk);
        } else {
            criteria.add(kind, MediaSourceKind::Device);
            criteria.add(kind, MediaSourceKind::Display);
        }

        self.get_simple_tracks_request(criteria)
            .map_err(tracerr::map_from_and_wrap!())
            .map(|opt| opt.map(|s| MediaStreamSettings::from(&s)))
    }

    /// Returns [`SimpleTracksRequest`] for the provided
    /// [`LocalStreamUpdateCriteria`].
    ///
    /// # Errors
    ///
    /// Errors with a [`TracksRequestError`] if failed to create or merge
    /// [`SimpleTracksRequest`].
    fn get_simple_tracks_request(
        &self,
        criteria: LocalStreamUpdateCriteria,
    ) -> Result<Option<SimpleTracksRequest>, Traced<TracksRequestError>> {
        let Some(request) = self.media_connections.get_tracks_request(criteria)
        else {
            return Ok(None);
        };
        let mut required_caps = SimpleTracksRequest::try_from(request)
            .map_err(tracerr::from_and_wrap!())?;
        required_caps
            .merge(self.send_constraints.inner())
            .map_err(tracerr::map_from_and_wrap!())?;

        Ok(Some(required_caps))
    }

    /// Implementation of the [`PeerConnection::update_local_stream`] method.
    async fn inner_update_local_stream(
        &self,
        criteria: LocalStreamUpdateCriteria,
    ) -> Result<
        HashMap<TrackId, media_exchange_state::Stable>,
        Traced<UpdateLocalStreamError>,
    > {
        if let Some(required_caps) = self
            .get_simple_tracks_request(criteria)
            .map_err(tracerr::map_from_and_wrap!())?
        {
            let used_caps = MediaStreamSettings::from(&required_caps);

            let media_tracks = self
                .media_manager
                .get_tracks(used_caps)
                .await
                .map_err(tracerr::map_from_and_wrap!())?;
            let peer_tracks = required_caps
                .parse_tracks(
                    media_tracks.iter().map(|(t, _)| t).cloned().collect(),
                )
                .await
                .map_err(tracerr::map_from_and_wrap!())?;

            let media_exchange_states_updates = self
                .media_connections
                .insert_local_tracks(&peer_tracks)
                .await
                .map_err(tracerr::map_from_and_wrap!())?;

            for (local_track, is_new) in media_tracks {
                if is_new {
                    drop(self.peer_events_sender.unbounded_send(
                        PeerEvent::NewLocalTrack { local_track },
                    ));
                }
            }

            Ok(media_exchange_states_updates)
        } else {
            Ok(HashMap::new())
        }
    }

    /// Returns [`Rc`] to [`TransceiverSide`] with a provided [`TrackId`].
    ///
    /// Returns [`None`] if [`TransceiverSide`] with a provided [`TrackId`]
    /// doesn't exist in this [`PeerConnection`].
    pub fn get_transceiver_side_by_id(
        &self,
        track_id: TrackId,
    ) -> Option<Rc<dyn TransceiverSide>> {
        self.media_connections.get_transceiver_side_by_id(track_id)
    }

    /// Updates underlying [RTCPeerConnection][1]'s remote SDP from answer.
    ///
    /// # Errors
    ///
    /// With [`RtcPeerConnectionError::SetRemoteDescriptionFailed`][3] if
    /// [RTCPeerConnection.setRemoteDescription()][2] fails.
    ///
    /// [1]: https://w3.org/TR/webrtc#rtcpeerconnection-interface
    /// [2]: https://w3.org/TR/webrtc#dom-peerconnection-setremotedescription
    /// [3]: platform::RtcPeerConnectionError::SetRemoteDescriptionFailed
    async fn set_remote_answer(
        &self,
        answer: String,
    ) -> Result<(), Traced<RtcPeerConnectionError>> {
        self.set_remote_description(platform::SdpType::Answer(answer))
            .await
            .map_err(tracerr::wrap!())
    }

    /// Updates underlying [RTCPeerConnection][1]'s remote SDP from offer.
    ///
    /// # Errors
    ///
    /// With [`platform::RtcPeerConnectionError::SetRemoteDescriptionFailed`] if
    /// [RTCPeerConnection.setRemoteDescription()][2] fails.
    ///
    /// [1]: https://w3.org/TR/webrtc#rtcpeerconnection-interface
    /// [2]: https://w3.org/TR/webrtc#dom-peerconnection-setremotedescription
    async fn set_remote_offer(
        &self,
        offer: String,
    ) -> Result<(), Traced<RtcPeerConnectionError>> {
        self.set_remote_description(platform::SdpType::Offer(offer))
            .await
            .map_err(tracerr::wrap!())
    }

    /// Updates underlying [RTCPeerConnection][1]'s remote SDP with given
    /// description.
    ///
    /// # Errors
    ///
    /// With [`platform::RtcPeerConnectionError::SetRemoteDescriptionFailed`] if
    /// [RTCPeerConnection.setRemoteDescription()][2] fails.
    ///
    /// With [`platform::RtcPeerConnectionError::AddIceCandidateFailed`] if
    /// [RtcPeerConnection.addIceCandidate()][3] fails when adding buffered ICE
    /// candidates.
    ///
    /// [1]: https://w3.org/TR/webrtc#rtcpeerconnection-interface
    /// [2]: https://w3.org/TR/webrtc#dom-peerconnection-setremotedescription
    /// [3]: https://w3.org/TR/webrtc#dom-peerconnection-addicecandidate
    async fn set_remote_description(
        &self,
        desc: platform::SdpType,
    ) -> Result<(), Traced<RtcPeerConnectionError>> {
        self.peer
            .set_remote_description(desc)
            .await
            .map_err(tracerr::map_from_and_wrap!())?;
        self.has_remote_description.set(true);
        self.media_connections.sync_receivers().await;

        let ice_candidates_buffer_flush_fut = future::try_join_all(
            self.ice_candidates_buffer.borrow_mut().drain(..).map(
                |candidate| {
                    let peer = Rc::clone(&self.peer);
                    async move {
                        peer.add_ice_candidate(
                            &candidate.candidate,
                            candidate.sdp_m_line_index,
                            &candidate.sdp_mid,
                        )
                        .await
                    }
                },
            ),
        );
        ice_candidates_buffer_flush_fut
            .await
            .map(drop)
            .map_err(tracerr::map_from_and_wrap!())?;

        Ok(())
    }

    /// Adds remote peers [ICE Candidate][1] to this peer.
    ///
    /// # Errors
    ///
    /// With [`RtcPeerConnectionError::AddIceCandidateFailed`] if
    /// [RtcPeerConnection.addIceCandidate()][3] fails to add buffered
    /// [ICE candidates][1].
    ///
    /// [1]: https://tools.ietf.org/html/rfc5245#section-2
    /// [3]: https://w3.org/TR/webrtc#dom-peerconnection-addicecandidate
    pub async fn add_ice_candidate(
        &self,
        candidate: String,
        sdp_m_line_index: Option<u16>,
        sdp_mid: Option<String>,
    ) -> Result<(), Traced<RtcPeerConnectionError>> {
        if self.has_remote_description.get() {
            self.peer
                .add_ice_candidate(&candidate, sdp_m_line_index, &sdp_mid)
                .await
                .map_err(tracerr::map_from_and_wrap!())?;
        } else {
            self.ice_candidates_buffer.borrow_mut().push(
                platform::IceCandidate { candidate, sdp_m_line_index, sdp_mid },
            );
        }
        Ok(())
    }

    /// Removes a [`sender::Component`] and a [`receiver::Component`] with the
    /// provided [`TrackId`] from this [`PeerConnection`].
    pub fn remove_track(&self, track_id: TrackId) {
        self.media_connections.remove_track(track_id);
    }
}

#[cfg(feature = "mockable")]
// TODO: Try remove on next Rust version upgrade.
#[expect(clippy::allow_attributes, reason = "`#[expect]` is not considered")]
#[allow(clippy::multiple_inherent_impl, reason = "feature gated")]
impl PeerConnection {
    /// Returns [`RtcStats`] of this [`PeerConnection`].
    ///
    /// # Errors
    ///
    /// Errors with [`PeerError::RtcPeerConnection`] if failed to get
    /// [`RtcStats`].
    pub async fn get_stats(
        &self,
    ) -> Result<platform::RtcStats, Traced<RtcPeerConnectionError>> {
        self.peer.get_stats().await
    }

    /// Indicates whether all [`Receiver`]s audio tracks are enabled.
    #[must_use]
    pub fn is_recv_audio_enabled(&self) -> bool {
        self.media_connections.is_recv_audio_enabled()
    }

    /// Indicates whether all [`Receiver`]s video tracks are enabled.
    #[must_use]
    pub fn is_recv_video_enabled(&self) -> bool {
        self.media_connections.is_recv_video_enabled()
    }

    /// Returns inner [`IceCandidate`]'s buffer length. Used in tests.
    #[must_use]
    pub fn candidates_buffer_len(&self) -> usize {
        self.ice_candidates_buffer.borrow().len()
    }

    /// Lookups [`Sender`] by provided [`TrackId`].
    #[must_use]
    pub fn get_sender_by_id(&self, id: TrackId) -> Option<Rc<media::Sender>> {
        self.media_connections.get_sender_by_id(id)
    }

    /// Lookups [`sender::State`] by the provided [`TrackId`].
    #[must_use]
    pub fn get_sender_state_by_id(
        &self,
        id: TrackId,
    ) -> Option<Rc<sender::State>> {
        self.media_connections.get_sender_state_by_id(id)
    }

    /// Indicates whether all [`Sender`]s audio tracks are enabled.
    #[must_use]
    pub fn is_send_audio_enabled(&self) -> bool {
        self.media_connections.is_send_audio_enabled()
    }

    /// Indicates whether all [`Sender`]s video tracks are enabled.
    #[must_use]
    pub fn is_send_video_enabled(
        &self,
        source_kind: Option<MediaSourceKind>,
    ) -> bool {
        self.media_connections.is_send_video_enabled(source_kind)
    }

    /// Indicates whether all [`Sender`]s video tracks are unmuted.
    #[must_use]
    pub fn is_send_video_unmuted(
        &self,
        source_kind: Option<MediaSourceKind>,
    ) -> bool {
        self.media_connections.is_send_video_unmuted(source_kind)
    }

    /// Indicates whether all [`Sender`]s audio tracks are unmuted.
    #[must_use]
    pub fn is_send_audio_unmuted(&self) -> bool {
        self.media_connections.is_send_audio_unmuted()
    }

    /// Returns all [`local::Track`]s from [`PeerConnection`]'s
    /// [`Transceiver`]s.
    #[must_use]
    pub fn get_send_tracks(&self) -> Vec<Rc<local::Track>> {
        self.media_connections
            .get_senders()
            .into_iter()
            .filter_map(|sndr| sndr.get_send_track())
            .collect()
    }

    /// Returns [`Rc`] to the [`Receiver`] with the provided [`TrackId`].
    #[must_use]
    pub fn get_receiver_by_id(
        &self,
        id: TrackId,
    ) -> Option<Rc<receiver::Receiver>> {
        self.media_connections.get_receiver_by_id(id)
    }
}

impl Drop for PeerConnection {
    /// Drops `on_track` and `on_ice_candidate` callbacks to prevent possible
    /// leaks.
    fn drop(&mut self) {
        self.peer.on_track::<Box<
            dyn FnMut(platform::MediaStreamTrack, platform::Transceiver),
        >>(None);
        self.peer
            .on_ice_candidate::<Box<dyn FnMut(platform::IceCandidate)>>(None);
        self.peer
            .on_ice_candidate_error::<Box<dyn FnMut(
                platform::IceCandidateError
            )>>(None);
    }
}
