use crate::protocol::Message;
use fnv::{FnvHashMap, FnvHashSet};
use libp2p::swarm::derive_prelude::FromSwarm;
use libp2p::swarm::{
    ConnectionId, NetworkBehaviour, NotifyHandler, OneShotHandler, THandlerInEvent,
    THandlerOutEvent, ToSwarm,
};
use libp2p::{Multiaddr, PeerId};
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::task::{Context, Poll};

mod protocol;
mod upgrade;

pub use protocol::{BroadcastConfig, Topic};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BroadcastEvent {
    Subscribed(PeerId, Topic),
    Unsubscribed(PeerId, Topic),
    Received(PeerId, Topic, Arc<[u8]>),
}

#[derive(Default)]
pub struct Broadcast {
    config: BroadcastConfig,
    subscriptions: FnvHashSet<Topic>,
    peers: FnvHashMap<PeerId, FnvHashSet<Topic>>,
    topics: FnvHashMap<Topic, FnvHashSet<PeerId>>,
    events: VecDeque<ToSwarm<BroadcastEvent, Message>>,
}

impl fmt::Debug for Broadcast {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Broadcast")
            .field("config", &self.config)
            .field("subscriptions", &self.subscriptions)
            .field("peers", &self.peers)
            .field("topics", &self.topics)
            .finish()
    }
}

impl Broadcast {
    pub fn new(config: BroadcastConfig) -> Self {
        Self {
            config,
            ..Default::default()
        }
    }

    pub fn subscribed(&self) -> impl Iterator<Item = &Topic> + '_ {
        self.subscriptions.iter()
    }

    pub fn peers(&self, topic: &Topic) -> Option<impl Iterator<Item = &PeerId> + '_> {
        self.topics.get(topic).map(|peers| peers.iter())
    }

    pub fn topics(&self, peer: &PeerId) -> Option<impl Iterator<Item = &Topic> + '_> {
        self.peers.get(peer).map(|topics| topics.iter())
    }

    pub fn subscribe(&mut self, topic: Topic) {
        self.subscriptions.insert(topic);
        let msg = Message::Subscribe(topic);
        for peer in self.peers.keys() {
            self.events.push_back(ToSwarm::NotifyHandler {
                peer_id: *peer,
                event: msg.clone(),
                handler: NotifyHandler::Any,
            });
        }
    }

    pub fn unsubscribe(&mut self, topic: &Topic) {
        self.subscriptions.remove(topic);
        let msg = Message::Unsubscribe(*topic);
        if let Some(peers) = self.topics.get(topic) {
            for peer in peers {
                self.events.push_back(ToSwarm::NotifyHandler {
                    peer_id: *peer,
                    event: msg.clone(),
                    handler: NotifyHandler::Any,
                });
            }
        }
    }

    pub fn broadcast(&mut self, topic: &Topic, msg: Arc<[u8]>) {
        let msg = Message::Broadcast(*topic, msg);
        if let Some(peers) = self.topics.get(topic) {
            for peer in peers {
                self.events.push_back(ToSwarm::NotifyHandler {
                    peer_id: *peer,
                    event: msg.clone(),
                    handler: NotifyHandler::Any,
                });
            }
        }
    }

    fn inject_connected(&mut self, peer: &PeerId) {
        self.peers.insert(*peer, FnvHashSet::default());
        for topic in &self.subscriptions {
            self.events.push_back(ToSwarm::NotifyHandler {
                peer_id: *peer,
                event: Message::Subscribe(*topic),
                handler: NotifyHandler::Any,
            });
        }
    }

    fn inject_disconnected(&mut self, peer: &PeerId) {
        if let Some(topics) = self.peers.remove(peer) {
            for topic in topics {
                if let Some(peers) = self.topics.get_mut(&topic) {
                    peers.remove(peer);
                }
            }
        }
    }
}

impl NetworkBehaviour for Broadcast {
    type ConnectionHandler = OneShotHandler<BroadcastConfig, Message, HandlerEvent>;
    type ToSwarm = BroadcastEvent;

    fn handle_established_inbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        _peer: PeerId,
        _local_addr: &Multiaddr,
        _remote_addr: &Multiaddr,
    ) -> Result<libp2p::swarm::THandler<Self>, libp2p::swarm::ConnectionDenied> {
        Ok(OneShotHandler::default())
    }

    fn handle_established_outbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        _peer: PeerId,
        _addr: &Multiaddr,
        _role_override: libp2p::core::Endpoint,
    ) -> Result<libp2p::swarm::THandler<Self>, libp2p::swarm::ConnectionDenied> {
        Ok(OneShotHandler::default())
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        match event {
            FromSwarm::ConnectionEstablished(c) => {
                if c.other_established == 0 {
                    self.inject_connected(&c.peer_id);
                }
            }
            FromSwarm::ConnectionClosed(c) => {
                if c.remaining_established == 0 {
                    self.inject_disconnected(&c.peer_id);
                }
            }
            _ => {}
        }
    }

    fn on_connection_handler_event(
        &mut self,
        peer_id: PeerId,
        _connection_id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        use HandlerEvent::*;
        use Message::*;
        if let Ok(event) = event {
            let ev = match event {
                Rx(Subscribe(topic)) => {
                    let peers = self.topics.entry(topic).or_default();
                    self.peers.get_mut(&peer_id).unwrap().insert(topic);
                    peers.insert(peer_id);
                    BroadcastEvent::Subscribed(peer_id, topic)
                }
                Rx(Broadcast(topic, msg)) => BroadcastEvent::Received(peer_id, topic, msg),
                Rx(Unsubscribe(topic)) => {
                    self.peers.get_mut(&peer_id).unwrap().remove(&topic);
                    if let Some(peers) = self.topics.get_mut(&topic) {
                        peers.remove(&peer_id);
                    }
                    BroadcastEvent::Unsubscribed(peer_id, topic)
                }
                Tx => {
                    return;
                }
            };
            self.events.push_back(ToSwarm::GenerateEvent(ev));
        }
    }

    fn poll(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        if let Some(event) = self.events.pop_front() {
            Poll::Ready(event)
        } else {
            Poll::Pending
        }
    }
}

/// Transmission between the `OneShotHandler` and the `BroadcastHandler`.
#[derive(Debug)]
pub enum HandlerEvent {
    /// We received a `Message` from a remote.
    Rx(Message),
    /// We successfully sent a `Message`.
    Tx,
}

impl From<Message> for HandlerEvent {
    fn from(message: Message) -> Self {
        Self::Rx(message)
    }
}

impl From<()> for HandlerEvent {
    fn from(_: ()) -> Self {
        Self::Tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct DummySwarm {
        peer_id: PeerId,
        behaviour: Arc<Mutex<Broadcast>>,
        connections: FnvHashMap<PeerId, Arc<Mutex<Broadcast>>>,
    }

    impl DummySwarm {
        fn new() -> Self {
            Self {
                peer_id: PeerId::random(),
                behaviour: Default::default(),
                connections: Default::default(),
            }
        }

        fn peer_id(&self) -> &PeerId {
            &self.peer_id
        }

        fn dial(&mut self, other: &mut DummySwarm) {
            self.behaviour
                .lock()
                .unwrap()
                .inject_connected(other.peer_id());
            self.connections
                .insert(*other.peer_id(), other.behaviour.clone());
            other
                .behaviour
                .lock()
                .unwrap()
                .inject_connected(self.peer_id());
            other
                .connections
                .insert(*self.peer_id(), self.behaviour.clone());
        }

        fn next(&self) -> Option<BroadcastEvent> {
            let waker = futures::task::noop_waker();
            let mut ctx = Context::from_waker(&waker);
            let mut me = self.behaviour.lock().unwrap();
            loop {
                match me.poll(&mut ctx) {
                    Poll::Ready(ToSwarm::NotifyHandler { peer_id, event, .. }) => {
                        if let Some(other) = self.connections.get(&peer_id) {
                            let mut other = other.lock().unwrap();
                            other.on_connection_handler_event(
                                *self.peer_id(),
                                ConnectionId::new_unchecked(0),
                                Ok(HandlerEvent::Rx(event)),
                            );
                        }
                    }
                    Poll::Ready(ToSwarm::GenerateEvent(event)) => {
                        return Some(event);
                    }
                    Poll::Ready(_) => panic!(),
                    Poll::Pending => {
                        return None;
                    }
                }
            }
        }

        fn subscribe(&self, topic: Topic) {
            let mut me = self.behaviour.lock().unwrap();
            me.subscribe(topic);
        }

        fn unsubscribe(&self, topic: &Topic) {
            let mut me = self.behaviour.lock().unwrap();
            me.unsubscribe(topic);
        }

        fn broadcast(&self, topic: &Topic, msg: Arc<[u8]>) {
            let mut me = self.behaviour.lock().unwrap();
            me.broadcast(topic, msg);
        }
    }

    #[test]
    fn test_broadcast() {
        let topic = Topic::new(b"topic");
        let msg = Arc::new(*b"msg");
        let mut a = DummySwarm::new();
        let mut b = DummySwarm::new();

        a.subscribe(topic);
        a.dial(&mut b);
        assert!(a.next().is_none());
        assert_eq!(
            b.next().unwrap(),
            BroadcastEvent::Subscribed(*a.peer_id(), topic)
        );
        b.subscribe(topic);
        assert!(b.next().is_none());
        assert_eq!(
            a.next().unwrap(),
            BroadcastEvent::Subscribed(*b.peer_id(), topic)
        );
        b.broadcast(&topic, msg.clone());
        assert!(b.next().is_none());
        assert_eq!(
            a.next().unwrap(),
            BroadcastEvent::Received(*b.peer_id(), topic, msg)
        );
        a.unsubscribe(&topic);
        assert!(a.next().is_none());
        assert_eq!(
            b.next().unwrap(),
            BroadcastEvent::Unsubscribed(*a.peer_id(), topic)
        );
    }
}
