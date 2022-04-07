use std::{
    marker::PhantomData,
    mem::MaybeUninit,
    ops::{Index, IndexMut},
    sync::Arc,
    time::Duration,
};

use enet_sys::{
    enet_host_bandwidth_limit, enet_host_channel_limit, enet_host_check_events, enet_host_connect,
    enet_host_destroy, enet_host_flush, enet_host_service, ENetEvent, ENetHost, ENetPeer,
    ENET_PROTOCOL_MAXIMUM_CHANNEL_COUNT,
};

use crate::{Address, EnetKeepAlive, Error, Event, EventKind, Peer, PeerID};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Represents a bandwidth limit or unlimited.
pub enum BandwidthLimit {
    /// No limit on bandwidth
    Unlimited,
    /// Bandwidth limit in bytes/second
    Limited(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Represents a channel limit or unlimited.
pub enum ChannelLimit {
    /// Maximum limit on the number of channels
    Maximum,
    /// Channel limit
    Limited(enet_sys::size_t),
}

impl ChannelLimit {
    pub(in crate) fn to_enet_val(self) -> enet_sys::size_t {
        match self {
            ChannelLimit::Maximum => 0,
            ChannelLimit::Limited(l) => l,
        }
    }

    fn from_enet_val(enet_val: enet_sys::size_t) -> ChannelLimit {
        const MAX_COUNT: enet_sys::size_t = ENET_PROTOCOL_MAXIMUM_CHANNEL_COUNT as enet_sys::size_t;
        match enet_val {
            MAX_COUNT => ChannelLimit::Maximum,
            0 => panic!("ChannelLimit::from_enet_usize: got 0"),
            lim => ChannelLimit::Limited(lim),
        }
    }
}

impl BandwidthLimit {
    pub(in crate) fn to_enet_u32(self) -> u32 {
        match self {
            BandwidthLimit::Unlimited => 0,
            BandwidthLimit::Limited(l) => l,
        }
    }
}

/// A `Host` represents one endpoint of an ENet connection. Created through
/// `Enet`.
///
/// This type provides functionality such as connection establishment and packet
/// transmission.
pub struct Host<T> {
    inner: *mut ENetHost,
    disconnect_drop: Option<PeerID>,
    _keep_alive: Arc<EnetKeepAlive>,
    _peer_data: PhantomData<*const T>,
}

impl<T> Host<T> {
    pub(in crate) fn new(_keep_alive: Arc<EnetKeepAlive>, inner: *mut ENetHost) -> Host<T> {
        assert!(!inner.is_null());

        Host {
            inner,
            disconnect_drop: None,
            _keep_alive,
            _peer_data: PhantomData,
        }
    }

    /// Sends any queued packets on the host specified to its designated peers.
    ///
    /// This function need only be used in circumstances where one wishes to
    /// send queued packets earlier than in a call to `Host::service()`.
    pub fn flush(&mut self) {
        unsafe {
            enet_host_flush(self.inner);
        }
    }

    /// Sets the bandwith limits for this `Host`.
    pub fn set_bandwith_limits(
        &mut self,
        incoming_bandwith: BandwidthLimit,
        outgoing_bandwidth: BandwidthLimit,
    ) {
        unsafe {
            enet_host_bandwidth_limit(
                self.inner,
                incoming_bandwith.to_enet_u32(),
                outgoing_bandwidth.to_enet_u32(),
            );
        }
    }

    /// Sets the maximum allowed channels of future connections.
    pub fn set_channel_limit(&mut self, max_channel_count: ChannelLimit) {
        unsafe {
            enet_host_channel_limit(self.inner, max_channel_count.to_enet_val());
        }
    }

    /// Returns the limit of channels per connected peer for this `Host`.
    pub fn channel_limit(&self) -> ChannelLimit {
        ChannelLimit::from_enet_val(unsafe { (*self.inner).channelLimit })
    }

    /// Returns the downstream bandwidth of this `Host` in bytes/second.
    pub fn incoming_bandwidth(&self) -> u32 {
        unsafe { (*self.inner).incomingBandwidth }
    }

    /// Returns the upstream bandwidth of this `Host` in bytes/second.
    pub fn outgoing_bandwidth(&self) -> u32 {
        unsafe { (*self.inner).outgoingBandwidth }
    }

    /// Returns the internet address of this `Host`.
    pub fn address(&self) -> Address {
        Address::from_enet_address(&unsafe { (*self.inner).address })
    }

    /// Returns the number of peers allocated for this `Host`.
    pub fn peer_count(&self) -> enet_sys::size_t {
        unsafe { (*self.inner).peerCount }
    }

    /// Returns a mutable reference to a peer at the index, None if the index is invalid.
    pub fn peer_mut(&mut self, idx: PeerID) -> Option<&mut Peer<T>> {
        if idx.0 >= self.peer_count() {
            return None;
        }

        Some(Peer::new_mut(unsafe {
            &mut *((*self.inner).peers.offset(idx.0 as isize))
        }))
    }

    /// Returns a reference to a peer at the index, None if the index is invalid.
    pub fn peer(&self, idx: PeerID) -> Option<&Peer<T>> {
        if idx.0 >= self.peer_count() {
            return None;
        }

        Some(Peer::new(unsafe {
            &*((*self.inner).peers.offset(idx.0 as isize))
        }))
    }

    pub(crate) unsafe fn peer_id(&self, peer: *mut ENetPeer) -> PeerID {
        PeerID(
            (peer as enet_sys::size_t - (*self.inner).peers as enet_sys::size_t)
                / std::mem::size_of::<ENetPeer>() as enet_sys::size_t,
        )
    }

    /// Returns an iterator over all peers connected to this `Host`.
    pub fn peers_mut(&mut self) -> impl Iterator<Item = &'_ mut Peer<T>> {
        let peers = unsafe {
            std::slice::from_raw_parts_mut(
                (*self.inner).peers,
                (*self.inner).peerCount.try_into().unwrap(),
            )
        };

        peers.into_iter().map(|peer| Peer::new_mut(&mut *peer))
    }

    /// Returns an iterator over all peers connected to this `Host`.
    pub fn peers(&self) -> impl Iterator<Item = &'_ Peer<T>> {
        let peers = unsafe {
            std::slice::from_raw_parts(
                (*self.inner).peers,
                (*self.inner).peerCount.try_into().unwrap(),
            )
        };

        peers.into_iter().map(|peer| Peer::new(&*peer))
    }

    fn drop_disconnected(&mut self) {
        // Seemingly, the lifetime of an ENetPeer ends when the Disconnect event is received.
        // However, this is *not really clear* in the ENet docs!
        // It looks like the Peer *might* live longer, but not shorter, so it should be safe
        // to destroy the associated data (if any) here.
        if let Some(idx) = self.disconnect_drop.take() {
            self.peer_mut(idx)
                .expect("Invalid PeerID in disconnect_drop in enet::Host")
                .set_data(None);
        }
    }

    fn process_event(&mut self, sys_event: ENetEvent) -> Option<Event> {
        self.drop_disconnected();

        let event = Event::from_sys_event(sys_event, self);
        if let Some(Event {
            peer_id,
            kind: EventKind::Disconnect { .. },
        }) = event
        {
            self.disconnect_drop = Some(peer_id);
        }

        event
    }

    /// Maintains this host and delivers an event if available.
    ///
    /// This should be called regularly for ENet to work properly with good performance.
    ///
    /// The function won't block if `timeout` is less than 1ms.
    pub fn service(&mut self, timeout: Duration) -> Result<Option<Event>, Error> {
        // ENetEvent is Copy (aka has no Drop impl), so we don't have to make sure we `mem::forget` it later on
        let mut sys_event = MaybeUninit::uninit();

        let res = unsafe {
            enet_host_service(
                self.inner,
                sys_event.as_mut_ptr(),
                timeout.as_millis() as u32,
            )
        };

        match res {
            r if r > 0 => Ok(unsafe { self.process_event(sys_event.assume_init()) }),
            0 => Ok(None),
            r if r < 0 => Err(Error(r)),
            _ => panic!("unreachable"),
        }

        // TODO: check `total*` fields on `inner`, these need to be reset from
        // time to time.
    }

    /// Checks for any queued events on this `Host` and dispatches one if
    /// available
    pub fn check_events(&mut self) -> Result<Option<Event>, Error> {
        // ENetEvent is Copy (aka has no Drop impl), so we don't have to make sure we
        // `mem::forget` it later on
        let mut sys_event = MaybeUninit::uninit();

        let res = unsafe { enet_host_check_events(self.inner, sys_event.as_mut_ptr()) };

        match res {
            r if r > 0 => Ok(unsafe { self.process_event(sys_event.assume_init()) }),
            0 => Ok(None),
            r if r < 0 => Err(Error(r)),
            _ => panic!("unreachable"),
        }
    }

    /// Initiates a connection to a foreign host.
    ///
    /// The connection will not be done until a `Event::Connected` for this peer
    /// was received.
    ///
    /// `channel_count` specifies how many channels to allocate for this peer.
    /// `data` is a user-specified value that can be chosen arbitrarily.
    pub fn connect(
        &mut self,
        address: &Address,
        channel_count: enet_sys::size_t,
        user_data: u32,
    ) -> Result<(&mut Peer<T>, PeerID), Error> {
        let res: *mut ENetPeer = unsafe {
            enet_host_connect(
                self.inner,
                &address.to_enet_address() as *const _,
                channel_count,
                user_data,
            )
        };

        if res.is_null() {
            return Err(Error(0));
        }

        Ok((
            Peer::new_mut(unsafe { &mut *res }),
            // We can do pointer arithmetic here to determine the offset of our new Peer in the
            // list of peers, which is it's PeerID.
            unsafe { self.peer_id(res) },
        ))
    }
}

impl<T> Index<PeerID> for Host<T> {
    type Output = Peer<T>;

    fn index(&self, idx: PeerID) -> &Peer<T> {
        self.peer(idx).expect("invalid peer index")
    }
}

impl<T> IndexMut<PeerID> for Host<T> {
    fn index_mut(&mut self, idx: PeerID) -> &mut Peer<T> {
        self.peer_mut(idx).expect("invalid peer index")
    }
}

impl<T> Drop for Host<T> {
    /// Call the corresponding ENet cleanup-function(s).
    fn drop(&mut self) {
        for peer in self.peers_mut() {
            peer.set_data(None);
        }

        unsafe {
            enet_host_destroy(self.inner);
        }
    }
}
