use crate::either::Either::{Left, Right};
use crate::{
    config::Config,
    error::{ErrorKind, Result},
    net::{connection::ActiveConnections, events::SocketEvent, link_conditioner::LinkConditioner},
    packet::{DeliveryGuarantee, Outgoing, Packet},
};
use crossbeam_channel::{self, unbounded, Receiver, SendError, Sender, TryRecvError};
use log::error;
use std::{
    self, io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket},
    thread::{sleep, yield_now},
    time::{Duration, Instant},
};

/// A reliable UDP socket implementation with configurable reliability and ordering guarantees.
#[derive(Debug)]
pub struct Socket {
    socket: UdpSocket,
    config: Config,
    connections: ActiveConnections,
    recv_buffer: Vec<u8>,
    link_conditioner: Option<LinkConditioner>,
    event_sender: Sender<SocketEvent>,
    packet_receiver: Receiver<Packet>,

    receiver: Receiver<SocketEvent>,
    sender: Sender<Packet>,
}

enum UdpSocketState {
    Empty,
    MaybeMore,
}

impl Socket {
    /// Binds to the socket and then sets up `ActiveConnections` to manage the "connections".
    /// Because UDP connections are not persistent, we can only infer the status of the remote
    /// endpoint by looking to see if they are still sending packets or not
    pub fn bind<A: ToSocketAddrs>(addresses: A) -> Result<Self> {
        Socket::bind_with_config(addresses, Config::default())
    }

    /// Bind to any local port on the system, if available
    pub fn bind_any() -> Result<Self> {
        Self::bind_any_with_config(Config::default())
    }

    /// Bind to any local port on the system, if available, with a given config
    pub fn bind_any_with_config(config: Config) -> Result<Self> {
        let loopback = Ipv4Addr::new(127, 0, 0, 1);
        let address = SocketAddrV4::new(loopback, 0);
        let socket = UdpSocket::bind(address)?;
        Self::bind_internal(socket, config)
    }

    /// Binds to the socket and then sets up `ActiveConnections` to manage the "connections".
    /// Because UDP connections are not persistent, we can only infer the status of the remote
    /// endpoint by looking to see if they are still sending packets or not
    ///
    /// This function allows you to configure laminar with the passed configuration.
    pub fn bind_with_config<A: ToSocketAddrs>(addresses: A, config: Config) -> Result<Self> {
        let socket = UdpSocket::bind(addresses)?;
        Self::bind_internal(socket, config)
    }

    fn bind_internal(socket: UdpSocket, config: Config) -> Result<Self> {
        socket.set_nonblocking(true)?;
        let (event_sender, event_receiver) = unbounded();
        let (packet_sender, packet_receiver) = unbounded();
        Ok(Socket {
            recv_buffer: vec![0; config.receive_buffer_max_size],
            socket,
            config,
            connections: ActiveConnections::new(),
            link_conditioner: None,
            event_sender,
            packet_receiver,

            sender: packet_sender,
            receiver: event_receiver,
        })
    }

    /// Returns a handle to the packet sender which provides a thread-safe way to enqueue packets
    /// to be processed. This should be used when the socket is busy running its polling loop in a
    /// separate thread.
    pub fn get_packet_sender(&mut self) -> Sender<Packet> {
        self.sender.clone()
    }

    /// Returns a handle to the event receiver which provides a thread-safe way to retrieve events
    /// from the socket. This should be used when the socket is busy running its polling loop in
    /// a separate thread.
    pub fn get_event_receiver(&mut self) -> Receiver<SocketEvent> {
        self.receiver.clone()
    }

    /// Send a packet
    pub fn send(&mut self, packet: Packet) -> Result<()> {
        match self.sender.send(packet) {
            Ok(_) => Ok(()),
            Err(error) => Err(ErrorKind::SendError(SendError(SocketEvent::Packet(
                error.0,
            )))),
        }
    }

    /// Receive a packet
    pub fn recv(&mut self) -> Option<SocketEvent> {
        match self.receiver.try_recv() {
            Ok(pkt) => Some(pkt),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => panic!["This can never happen"],
        }
    }

    /// Entry point to the run loop. This should run in a spawned thread since calls to `poll.poll`
    /// are blocking. We will default this to sleeping for 1ms.
    pub fn start_polling(&mut self) {
        self.start_polling_with_duration(Some(Duration::from_millis(1)))
    }

    /// Run the polling loop with a specified sleep duration. This should run in a spawned thread
    /// since calls to `poll.poll` are blocking.
    pub fn start_polling_with_duration(&mut self, sleep_duration: Option<Duration>) {
        // Nothing should break out of this loop!
        loop {
            self.manual_poll(Instant::now());
            match sleep_duration {
                None => yield_now(),
                Some(duration) => sleep(duration),
            };
        }
    }

    /// Process any inbound/outbound packets and handle idle clients
    pub fn manual_poll(&mut self, time: Instant) {
        // First we pull all newly arrived packets and handle them
        loop {
            match self.recv_from(time) {
                Ok(UdpSocketState::MaybeMore) => continue,
                Ok(UdpSocketState::Empty) => break,
                Err(e) => error!("Encountered an error receiving data: {:?}", e),
            }
        }

        // Now grab all the packets waiting to be sent and send them
        while let Ok(p) = self.packet_receiver.try_recv() {
            if let Err(e) = self.send_to(p, time) {
                match e {
                    ErrorKind::IOError(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    _ => error!("There was an error sending packet: {:?}", e),
                }
            }
        }

        // Finally check for idle clients
        if let Err(e) = self.handle_idle_clients(time) {
            error!("Encountered an error when sending TimeoutEvent: {:?}", e);
        }
    }

    /// Set the link conditioner for this socket. See [LinkConditioner] for further details.
    pub fn set_link_conditioner(&mut self, link_conditioner: Option<LinkConditioner>) {
        self.link_conditioner = link_conditioner;
    }

    /// Iterate through all of the idle connections based on `idle_connection_timeout` config and
    /// remove them from the active connections. For each connection removed, we will send a
    /// `SocketEvent::TimeOut` event to the `event_sender` channel.
    fn handle_idle_clients(&mut self, time: Instant) -> Result<()> {
        let idle_addresses = self
            .connections
            .idle_connections(self.config.idle_connection_timeout, time);
        for address in idle_addresses {
            self.connections.remove_connection(&address);
            self.event_sender.send(SocketEvent::Timeout(address))?;
        }

        Ok(())
    }

    // Serializes and sends a `Packet` on the socket. On success, returns the number of bytes written.
    fn send_to(&mut self, packet: Packet, time: Instant) -> Result<usize> {
        let connection =
            self.connections
                .get_or_insert_connection(packet.addr(), &self.config, time);

        let dropped = connection.gather_dropped_packets();
        let mut processed_packets: Vec<Outgoing> = dropped
            .iter()
            .flat_map(|waiting_packet| {
                connection.process_outgoing(
                    &waiting_packet.payload,
                    // Because a delivery guarantee is only sent with reliable packets
                    DeliveryGuarantee::Reliable,
                    // This is stored with the dropped packet because they could be mixed
                    waiting_packet.ordering_guarantee,
                    waiting_packet.item_identifier,
                    time,
                )
            })
            .collect();

        let processed_packet = connection.process_outgoing(
            packet.payload(),
            packet.delivery_guarantee(),
            packet.order_guarantee(),
            None,
            time,
        )?;

        processed_packets.push(processed_packet);

        let mut bytes_sent = 0;

        for processed_packet in processed_packets {
            if self.should_send_packet() {
                match processed_packet {
                    Outgoing::Packet(outgoing) => {
                        bytes_sent += self.send_packet(&packet.addr(), &outgoing.contents())?;
                    }
                    Outgoing::Fragments(packets) => {
                        for outgoing in packets {
                            bytes_sent += self.send_packet(&packet.addr(), &outgoing.contents())?;
                        }
                    }
                }
            }
        }
        Ok(bytes_sent)
    }

    // On success the packet will be sent on the `event_sender`
    fn recv_from(&mut self, time: Instant) -> Result<UdpSocketState> {
        match self.socket.recv_from(&mut self.recv_buffer) {
            Ok((recv_len, address)) => {
                if recv_len == 0 {
                    return Err(ErrorKind::ReceivedDataToShort)?;
                }
                let received_payload = &self.recv_buffer[..recv_len];

                if !self.connections.exists(&address) {
                    self.event_sender.send(SocketEvent::Connect(address))?;
                }

                let connection =
                    self.connections
                        .get_or_create_connection(address, &self.config, time);

                match connection {
                    Left(existing) => {
                        existing.process_incoming(received_payload, &self.event_sender, time)?;
                    }
                    Right(mut anonymous) => {
                        anonymous.process_incoming(received_payload, &self.event_sender, time)?;
                    }
                }
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::WouldBlock {
                    error!("Encountered an error receiving data: {:?}", e);
                    return Err(e.into());
                } else {
                    return Ok(UdpSocketState::Empty);
                }
            }
        }
        Ok(UdpSocketState::MaybeMore)
    }

    // Send a single packet over the UDP socket.
    fn send_packet(&self, addr: &SocketAddr, payload: &[u8]) -> Result<usize> {
        let bytes_sent = self.socket.send_to(payload, addr)?;
        Ok(bytes_sent)
    }

    // In the presence of a link conditioner, we would like it to determine whether or not we should
    // send a packet.
    fn should_send_packet(&mut self) -> bool {
        if let Some(link_conditioner) = &mut self.link_conditioner {
            link_conditioner.should_send()
        } else {
            true
        }
    }

    #[cfg(test)]
    fn connection_count(&self) -> usize {
        self.connections.count()
    }

    #[cfg(test)]
    fn forget_all_incoming_packets(&mut self) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        loop {
            match self.socket.recv_from(&mut self.recv_buffer) {
                Ok((recv_len, _address)) => {
                    if recv_len == 0 {
                        panic!("Received data too short");
                    }
                    let received_payload = &self.recv_buffer[..recv_len];
                }
                Err(e) => {
                    if e.kind() != io::ErrorKind::WouldBlock {
                        panic!("Encountered an error receiving data: {:?}", e);
                    } else {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        net::constants::{ACKED_PACKET_HEADER, FRAGMENT_HEADER_SIZE, STANDARD_HEADER_SIZE},
        Config, LinkConditioner, Packet, Socket, SocketEvent,
    };
    use std::collections::HashSet;
    use std::net::{SocketAddr, UdpSocket};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn binding_to_any() {
        assert![Socket::bind_any().is_ok()];
        assert![Socket::bind_any_with_config(Config::default()).is_ok()];
    }

    #[test]
    fn using_sender_and_receiver() {
        let server_addr = "127.0.0.1:12310".parse::<SocketAddr>().unwrap();
        let client_addr = "127.0.0.1:12311".parse::<SocketAddr>().unwrap();

        let mut server = Socket::bind(server_addr).unwrap();
        let mut client = Socket::bind(client_addr).unwrap();

        let time = Instant::now();

        let mut sender = client.get_packet_sender();
        let mut receiver = server.get_event_receiver();

        sender.send(Packet::reliable_unordered(
            server_addr,
            b"Hello world!".iter().cloned().collect::<Vec<_>>(),
        ));

        client.manual_poll(time);
        server.manual_poll(time);

        assert_eq![Ok(SocketEvent::Connect(client_addr)), receiver.recv()];
        if let SocketEvent::Packet(packet) = receiver.recv().unwrap() {
            assert_eq![b"Hello world!", packet.payload()];
        } else {
            panic!["Did not receive a packet when it should"];
        }
    }

    #[test]
    fn initial_packet_is_resent() {
        let mut server = Socket::bind("127.0.0.1:12335".parse::<SocketAddr>().unwrap()).unwrap();
        let mut client = Socket::bind("127.0.0.1:12336".parse::<SocketAddr>().unwrap()).unwrap();

        let time = Instant::now();

        // Send a packet that the server ignores/drops
        client.send(Packet::reliable_unordered(
            "127.0.0.1:12335".parse::<SocketAddr>().unwrap(),
            b"Do not arrive".iter().cloned().collect::<Vec<_>>(),
        ));
        client.manual_poll(time);

        // Drop the inbound packet, this simulates a network error
        server.forget_all_incoming_packets();

        // Send a packet that the server receives
        for id in 0..u8::max_value() {
            client.send(create_test_packet(id, "127.0.0.1:12335"));

            server.send(create_test_packet(id, "127.0.0.1:12336"));

            client.manual_poll(time);
            server.manual_poll(time);

            while let Some(SocketEvent::Packet(pkt)) = server.recv() {
                if pkt.payload() == b"Do not arrive" {
                    return;
                }
            }
            while let Some(_) = client.recv() {}
        }

        panic!["Did not receive the ignored packet"];
    }

    #[test]
    fn receiving_does_not_allow_denial_of_service() {
        let mut server = Socket::bind("127.0.0.1:12337".parse::<SocketAddr>().unwrap()).unwrap();
        let mut client = Socket::bind("127.0.0.1:12338".parse::<SocketAddr>().unwrap()).unwrap();

        // Send a bunch of packets to a server
        for _ in 0..3 {
            client.send(Packet::unreliable(
                "127.0.0.1:12337".parse::<SocketAddr>().unwrap(),
                vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
            ));
        }

        let time = Instant::now();

        client.manual_poll(time);
        server.manual_poll(time);

        for _ in 0..6 {
            assert![server.recv().is_some()];
        }
        assert![server.recv().is_none()];

        // The server shall not have any connection in its connection table even though it received
        // packets
        assert_eq![0, server.connection_count()];

        server.send(Packet::unreliable(
            "127.0.0.1:12338".parse::<SocketAddr>().unwrap(),
            vec![1],
        ));

        server.manual_poll(time);

        // The server only adds to its table after having sent explicitly
        assert_eq![1, server.connection_count()];
    }

    #[test]
    fn initial_sequenced_is_resent() {
        let mut server = Socket::bind("127.0.0.1:12329".parse::<SocketAddr>().unwrap()).unwrap();
        let mut client = Socket::bind("127.0.0.1:12330".parse::<SocketAddr>().unwrap()).unwrap();

        let time = Instant::now();

        // Send a packet that the server ignores/drops
        client.send(Packet::reliable_sequenced(
            "127.0.0.1:12329".parse::<SocketAddr>().unwrap(),
            b"Do not arrive".iter().cloned().collect::<Vec<_>>(),
            None,
        ));
        client.manual_poll(time);

        // Drop the inbound packet, this simulates a network error
        server.forget_all_incoming_packets();

        // Send a packet that the server receives
        for id in 0..36 {
            client.send(create_sequenced_packet(id, "127.0.0.1:12329"));

            server.send(create_sequenced_packet(id, "127.0.0.1:12330"));

            client.manual_poll(time);
            server.manual_poll(time);

            while let Some(SocketEvent::Packet(pkt)) = server.recv() {
                if pkt.payload() == b"Do not arrive" {
                    panic!["Sequenced packet arrived while it should not"];
                }
            }
            while let Some(_) = client.recv() {}
        }
    }

    #[test]
    fn initial_ordered_is_resent() {
        let mut server = Socket::bind("127.0.0.1:12333".parse::<SocketAddr>().unwrap()).unwrap();
        let mut client = Socket::bind("127.0.0.1:12334".parse::<SocketAddr>().unwrap()).unwrap();

        let time = Instant::now();

        // Send a packet that the server ignores/drops
        client.send(Packet::reliable_ordered(
            "127.0.0.1:12333".parse::<SocketAddr>().unwrap(),
            b"Do not arrive".iter().cloned().collect::<Vec<_>>(),
            None,
        ));
        client.manual_poll(time);

        // Drop the inbound packet, this simulates a network error
        server.forget_all_incoming_packets();

        // Send a packet that the server receives
        for id in 0..36 {
            client.send(create_ordered_packet(id, "127.0.0.1:12333"));

            server.send(create_ordered_packet(id, "127.0.0.1:12334"));

            client.manual_poll(time);
            server.manual_poll(time);

            while let Some(SocketEvent::Packet(pkt)) = server.recv() {
                if pkt.payload() == b"Do not arrive" {
                    return;
                }
            }
            while let Some(_) = client.recv() {}
        }

        panic!["Did not receive the ignored packet"];
    }

    #[test]
    fn do_not_duplicate_sequenced_packets_when_received() {
        let server_addr = "127.0.0.1:12325".parse::<SocketAddr>().unwrap();
        let client_addr = "127.0.0.1:12326".parse::<SocketAddr>().unwrap();

        let mut server = Socket::bind(server_addr).unwrap();
        let mut client = Socket::bind(client_addr).unwrap();

        let time = Instant::now();

        for id in 0..100 {
            client.send(Packet::reliable_sequenced(server_addr, vec![id], None));
            client.manual_poll(time);
        }

        server.manual_poll(time);

        let mut seen = HashSet::new();

        while let Some(message) = server.recv() {
            match message {
                SocketEvent::Connect(connect_event) => {}
                SocketEvent::Packet(packet) => {
                    let byte = packet.payload()[0];
                    assert![!seen.contains(&byte)];
                    seen.insert(byte);
                }
                SocketEvent::Timeout(timeout_event) => {
                    panic!["This should not happen, as we've not advanced time"];
                }
            }
        }

        assert_eq![100, seen.len()];
    }

    #[test]
    fn manual_polling_socket() {
        let mut server = Socket::bind("127.0.0.1:12339".parse::<SocketAddr>().unwrap()).unwrap();
        let mut client = Socket::bind("127.0.0.1:12340".parse::<SocketAddr>().unwrap()).unwrap();

        for _ in 0..3 {
            client.send(Packet::unreliable(
                "127.0.0.1:12339".parse::<SocketAddr>().unwrap(),
                vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
            ));
        }

        let time = Instant::now();

        client.manual_poll(time);
        server.manual_poll(time);

        assert!(server.recv().is_some());
        assert!(server.recv().is_some());
        assert!(server.recv().is_some());
    }

    #[test]
    fn can_send_and_receive() {
        let mut server = Socket::bind("127.0.0.1:12342".parse::<SocketAddr>().unwrap()).unwrap();
        let mut client = Socket::bind("127.0.0.1:12341".parse::<SocketAddr>().unwrap()).unwrap();

        for _ in 0..3 {
            client.send(Packet::unreliable(
                "127.0.0.1:12342".parse::<SocketAddr>().unwrap(),
                vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
            ));
        }

        let now = Instant::now();
        client.manual_poll(now);
        server.manual_poll(now);

        assert!(server.recv().is_some());
        assert!(server.recv().is_some());
        assert!(server.recv().is_some());
    }

    #[test]
    fn sending_large_unreliable_packet_should_fail() {
        let mut server = Socket::bind("127.0.0.1:12370".parse::<SocketAddr>().unwrap()).unwrap();

        assert_eq!(
            server
                .send_to(
                    Packet::unreliable("127.0.0.1:12360".parse().unwrap(), vec![1; 5000]),
                    Instant::now(),
                )
                .is_err(),
            true
        );
    }

    #[test]
    fn send_returns_right_size() {
        let mut server = Socket::bind("127.0.0.1:12371".parse::<SocketAddr>().unwrap()).unwrap();

        assert_eq!(
            server
                .send_to(
                    Packet::unreliable("127.0.0.1:12361".parse().unwrap(), vec![1; 1024]),
                    Instant::now(),
                )
                .unwrap(),
            1024 + STANDARD_HEADER_SIZE as usize
        );
    }

    #[test]
    fn fragmentation_send_returns_right_size() {
        let mut server = Socket::bind("127.0.0.1:12372".parse::<SocketAddr>().unwrap()).unwrap();

        let fragment_packet_size = STANDARD_HEADER_SIZE + FRAGMENT_HEADER_SIZE;

        // the first fragment of an sequence of fragments contains also the acknowledgment header.
        assert_eq!(
            server
                .send_to(
                    Packet::reliable_unordered("127.0.0.1:12362".parse().unwrap(), vec![1; 4000]),
                    Instant::now(),
                )
                .unwrap(),
            4000 + (fragment_packet_size * 4 + ACKED_PACKET_HEADER) as usize
        );
    }

    #[test]
    fn connect_event_occurs() {
        let mut server = Socket::bind("127.0.0.1:12345".parse::<SocketAddr>().unwrap()).unwrap();
        let mut client = Socket::bind("127.0.0.1:12344".parse::<SocketAddr>().unwrap()).unwrap();

        client.send(Packet::unreliable(
            "127.0.0.1:12345".parse().unwrap(),
            vec![0, 1, 2],
        ));

        let now = Instant::now();
        client.manual_poll(now);
        server.manual_poll(now);

        assert_eq!(
            server.recv().unwrap(),
            SocketEvent::Connect("127.0.0.1:12344".parse().unwrap())
        );
    }

    #[test]
    fn disconnect_event_occurs() {
        let mut config = Config::default();
        config.idle_connection_timeout = Duration::from_millis(1);

        let mut server = Socket::bind("127.0.0.1:12347".parse::<SocketAddr>().unwrap()).unwrap();
        let mut client = Socket::bind("127.0.0.1:12346".parse::<SocketAddr>().unwrap()).unwrap();

        client.send(Packet::unreliable(
            "127.0.0.1:12347".parse().unwrap(),
            vec![0, 1, 2],
        ));

        let now = Instant::now();
        client.manual_poll(now);
        server.manual_poll(now);

        assert_eq!(
            server.recv().unwrap(),
            SocketEvent::Connect("127.0.0.1:12346".parse().unwrap())
        );
        assert_eq!(
            server.recv().unwrap(),
            SocketEvent::Packet(Packet::unreliable(
                "127.0.0.1:12346".parse().unwrap(),
                vec![0, 1, 2]
            ))
        );

        // Acknowledge the client
        server.send(Packet::unreliable(
            "127.0.0.1:12346".parse().unwrap(),
            vec![],
        ));

        server.manual_poll(now);
        client.manual_poll(now);
        server.manual_poll(now + Duration::new(5, 0));

        assert_eq!(
            server.recv().unwrap(),
            SocketEvent::Timeout("127.0.0.1:12346".parse().unwrap())
        );
    }

    const LOCAL_ADDR: &str = "127.0.0.1:13000";
    const REMOTE_ADDR: &str = "127.0.0.1:14000";

    fn create_test_packet(id: u8, addr: &str) -> Packet {
        let payload = vec![id];
        Packet::reliable_unordered(addr.parse().unwrap(), payload)
    }

    fn create_ordered_packet(id: u8, addr: &str) -> Packet {
        let payload = vec![id];
        Packet::reliable_ordered(addr.parse().unwrap(), payload, None)
    }

    fn create_sequenced_packet(id: u8, addr: &str) -> Packet {
        let payload = vec![id];
        Packet::reliable_sequenced(addr.parse().unwrap(), payload, None)
    }

    #[test]
    fn multiple_sends_should_start_sending_dropped() {
        // Start up a server and a client.
        let mut server = Socket::bind(REMOTE_ADDR.parse::<SocketAddr>().unwrap()).unwrap();

        let mut client = Socket::bind(LOCAL_ADDR.parse::<SocketAddr>().unwrap()).unwrap();

        let now = Instant::now();

        // Send enough packets to ensure that we must have dropped packets.
        for i in 0..35 {
            client.send(create_test_packet(i, REMOTE_ADDR));
        }

        let mut events = Vec::new();

        loop {
            client.manual_poll(now);
            server.manual_poll(now);
            if let Some(event) = server.recv() {
                events.push(event);
            } else {
                break;
            }
        }

        // Ensure that we get the correct number of events to the server.
        // 35 connect events plus the 35 messages
        assert_eq!(events.len(), 70);

        // Finally the server decides to send us a message back. This necessarily will include
        // the ack information for 33 of the sent 35 packets.
        server.send(create_test_packet(0, LOCAL_ADDR));

        server.manual_poll(now);
        client.manual_poll(now);

        // Block to ensure that the client gets the server message before moving on.
        client.recv();

        // This next sent message should end up sending the 2 unacked messages plus the new messages
        // with payload 35
        events.clear();
        client.send(create_test_packet(35, REMOTE_ADDR));

        client.manual_poll(now);
        server.manual_poll(now);

        loop {
            client.manual_poll(now);
            server.manual_poll(now);
            if let Some(event) = server.recv() {
                events.push(event);
            } else {
                break;
            }
        }

        let sent_events: Vec<u8> = events
            .iter()
            .flat_map(|e| match e {
                SocketEvent::Packet(p) => Some(p.payload()[0]),
                _ => None,
            })
            .collect();
        assert_eq!(sent_events, vec![35]);
    }

    #[quickcheck_macros::quickcheck]
    fn do_not_panic_on_arbitrary_packets(bytes: Vec<u8>) {
        let receiver = "127.0.0.1:12332".parse::<SocketAddr>().unwrap();
        let sender = "127.0.0.1:12331".parse::<SocketAddr>().unwrap();

        let mut server = Socket::bind(receiver).unwrap();

        let client = UdpSocket::bind(sender).unwrap();

        client.send_to(&bytes, receiver);

        let time = Instant::now();
        server.manual_poll(time);
    }

    #[test]
    fn really_bad_network_keeps_chugging_along() {
        let server_addr = "127.0.0.1:12320".parse::<SocketAddr>().unwrap();
        let client_addr = "127.0.0.1:12321".parse::<SocketAddr>().unwrap();

        let mut server = Socket::bind(server_addr).unwrap();
        let mut client = Socket::bind(client_addr).unwrap();

        let time = Instant::now();

        // We give both the server and the client a really bad bidirectional link
        let link_conditioner = {
            let mut lc = LinkConditioner::new();
            lc.set_packet_loss(0.9);
            Some(lc)
        };

        client.set_link_conditioner(link_conditioner.clone());
        server.set_link_conditioner(link_conditioner);

        let mut set = HashSet::new();

        // We chat 100 packets between the client and server, which will re-send any non-acked
        // packets
        let mut send_many_packets = |dummy: Option<u8>| {
            for id in 0..100 {
                client.send(Packet::reliable_unordered(
                    server_addr,
                    vec![dummy.unwrap_or(id)],
                ));

                server.send(Packet::reliable_unordered(client_addr, vec![255]));

                client.manual_poll(time);
                server.manual_poll(time);

                while let Some(_) = client.recv() {}
                while let Some(event) = server.recv() {
                    match event {
                        SocketEvent::Packet(pkt) => {
                            set.insert(pkt.payload()[0]);
                        }
                        SocketEvent::Timeout(_) => {
                            panic!["Unable to time out, time has not advanced"]
                        }
                        SocketEvent::Connect(_) => {}
                    }
                }
            }

            return set.len();
        };

        // The first chatting sequence sends packets 0..100 from the client to the server. After
        // this we just chat with a value of 255 so we don't accidentally overlap those chatting
        // packets with the packets we want to ack.
        send_many_packets(None);
        send_many_packets(Some(255));
        send_many_packets(Some(255));
        send_many_packets(Some(255));

        // 101 because we have 0..100 and 255 from the dummies
        assert_eq![101, send_many_packets(Some(255))];
    }
}
