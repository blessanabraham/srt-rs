use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use crate::protocol::{handshake::Handshake, TimeStamp};
use crate::SocketId;
use crate::{packet::*, Connection};

use super::{
    hsv5::{start_hsv5_initiation, StartedInitiator},
    ConnInitSettings, ConnectError, ConnectionReject, ConnectionResult,
};
use ConnectError::*;
use ConnectState::*;
use ConnectionResult::*;

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
enum ConnectState {
    Configured,
    /// keep induction packet around for retransmit
    InductionResponseWait(Packet),
    /// keep conclusion packet around for retransmit
    ConclusionResponseWait(Packet, StartedInitiator),
}

impl Default for ConnectState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectState {
    pub fn new() -> ConnectState {
        Configured
    }
}

pub struct Connect {
    remote: SocketAddr,
    local_addr: IpAddr,
    init_settings: ConnInitSettings,
    state: ConnectState,
    streamid: Option<String>,
}

impl Connect {
    pub fn new(
        remote: SocketAddr,
        local_addr: IpAddr,
        init_settings: ConnInitSettings,
        streamid: Option<String>,
    ) -> Self {
        Connect {
            remote,
            local_addr,
            init_settings,
            state: ConnectState::new(),
            streamid,
        }
    }
    fn on_start(&mut self) -> ConnectionResult {
        let packet = Packet::Control(ControlPacket {
            dest_sockid: SocketId(0),
            timestamp: TimeStamp::from_micros(0), // TODO: this is not zero in the reference implementation
            control_type: ControlTypes::Handshake(HandshakeControlInfo {
                init_seq_num: self.init_settings.starting_send_seqnum,
                max_packet_size: 1500, // TODO: take as a parameter
                max_flow_size: 8192,   // TODO: take as a parameter
                socket_id: self.init_settings.local_sockid,
                shake_type: ShakeType::Induction,
                peer_addr: self.local_addr,
                syn_cookie: 0,
                info: HandshakeVsInfo::V4(SocketType::Datagram),
            }),
        });
        self.state = InductionResponseWait(packet.clone());
        SendPacket((packet, self.remote))
    }

    pub fn wait_for_induction(
        &mut self,
        from: SocketAddr,
        timestamp: TimeStamp,
        info: HandshakeControlInfo,
        now: Instant,
    ) -> ConnectionResult {
        match (info.shake_type, &info.info, from) {
            (ShakeType::Induction, HandshakeVsInfo::V5 { .. }, from) if from == self.remote => {
                let (hsv5, cm) =
                    start_hsv5_initiation(self.init_settings.clone(), self.streamid.clone(), now);

                // send back a packet with the same syn cookie
                let packet = Packet::Control(ControlPacket {
                    timestamp,
                    dest_sockid: SocketId(0),
                    control_type: ControlTypes::Handshake(HandshakeControlInfo {
                        shake_type: ShakeType::Conclusion,
                        socket_id: self.init_settings.local_sockid,
                        info: hsv5,
                        init_seq_num: self.init_settings.starting_send_seqnum,
                        ..info
                    }),
                });
                self.state = ConclusionResponseWait(packet.clone(), cm);
                SendPacket((packet, from))
            }
            (ShakeType::Induction, HandshakeVsInfo::V5 { .. }, from) => {
                NotHandled(UnexpectedHost(self.remote, from))
            }
            (ShakeType::Induction, version, _) => {
                NotHandled(UnsupportedProtocolVersion(version.version()))
            }
            (_, _, _) => NotHandled(InductionExpected(info)),
        }
    }

    fn wait_for_conclusion(
        &mut self,
        from: SocketAddr,
        now: Instant,
        info: HandshakeControlInfo,
        initiator: StartedInitiator,
    ) -> ConnectionResult {
        match (info.shake_type, info.info.version(), from) {
            (ShakeType::Conclusion, 5, from) if from == self.remote => {
                let settings = match initiator.finish_hsv5_initiation(&info, from, now) {
                    Ok(s) => s,
                    Err(rr) => return NotHandled(rr),
                };

                // TODO: no handshake retransmit packet needed? is this right? Needs testing.
                Connected(
                    None,
                    Connection {
                        settings,
                        handshake: Handshake::Connector,
                    },
                )
            }
            (ShakeType::Conclusion, 5, from) => NotHandled(UnexpectedHost(self.remote, from)),
            (ShakeType::Conclusion, version, _) => NotHandled(UnsupportedProtocolVersion(version)),
            (ShakeType::Rejection(rej), _, from) if from == self.remote => {
                Reject(None, ConnectionReject::Rejected(rej))
            }
            (ShakeType::Rejection(_), _, from) => NotHandled(UnexpectedHost(self.remote, from)),
            (ShakeType::Induction, _, _) => NoAction,
            (_, _, _) => NotHandled(ConclusionExpected(info)),
        }
    }

    pub fn handle_packet(&mut self, next: (Packet, SocketAddr), now: Instant) -> ConnectionResult {
        let (packet, from) = next;
        match (self.state.clone(), packet) {
            (InductionResponseWait(_), Packet::Control(control)) => match control.control_type {
                ControlTypes::Handshake(shake) => {
                    self.wait_for_induction(from, control.timestamp, shake, now)
                }
                control_type => NotHandled(HandshakeExpected(control_type)),
            },
            (ConclusionResponseWait(_, cm), Packet::Control(control)) => match control.control_type
            {
                ControlTypes::Handshake(shake) => self.wait_for_conclusion(from, now, shake, cm),
                control_type => NotHandled(HandshakeExpected(control_type)),
            },
            (_, Packet::Data(data)) => NotHandled(ControlExpected(data)),
            (_, _) => NoAction,
        }
    }

    pub fn handle_tick(&mut self, _now: Instant) -> ConnectionResult {
        match &self.state {
            Configured => self.on_start(),
            InductionResponseWait(request_packet) => {
                SendPacket((request_packet.clone(), self.remote))
            }
            ConclusionResponseWait(request_packet, _) => {
                SendPacket((request_packet.clone(), self.remote))
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use rand::random;

    use crate::pending_connection::ConnectionReject;

    use super::*;

    const TEST_SOCKID: SocketId = SocketId(7655);

    #[test]
    fn reject() {
        let mut c = test_connect(Some("#!::u=test".into()));
        c.handle_tick(Instant::now());

        let first = Packet::Control(ControlPacket {
            timestamp: TimeStamp::from_micros(0),
            dest_sockid: TEST_SOCKID,
            control_type: ControlTypes::Handshake(HandshakeControlInfo {
                syn_cookie: 5554,
                socket_id: SocketId(5678),
                info: HandshakeVsInfo::V5(HsV5Info::default()),
                init_seq_num: random(),
                max_packet_size: 8192,
                max_flow_size: 1234,
                shake_type: ShakeType::Induction,
                peer_addr: [127, 0, 0, 1].into(),
            }),
        });

        let resp = c.handle_packet((first, test_remote()), Instant::now());
        assert!(
            matches!(
                resp,
                ConnectionResult::SendPacket((Packet::Control(ControlPacket {
                    control_type: ControlTypes::Handshake(HandshakeControlInfo {
                        shake_type: ShakeType::Conclusion,
                        socket_id,
                        syn_cookie: 5554,
                        ..
                    }), ..
                }), _)) if socket_id == TEST_SOCKID
            ),
            "{:?}",
            resp
        );

        // send rejection
        let rejection = Packet::Control(ControlPacket {
            timestamp: TimeStamp::from_micros(0),
            dest_sockid: TEST_SOCKID,
            control_type: ControlTypes::Handshake(HandshakeControlInfo {
                init_seq_num: random(),
                max_packet_size: 8192,
                max_flow_size: 1234,
                shake_type: ShakeType::Rejection(RejectReason::Server(ServerRejectReason::BadMode)),
                socket_id: SocketId(5678),
                syn_cookie: 2222,
                peer_addr: [127, 0, 0, 1].into(),
                info: HandshakeVsInfo::V5(HsV5Info::default()),
            }),
        });

        let resp = c.handle_packet((rejection, test_remote()), Instant::now());
        assert!(
            matches!(
                resp,
                ConnectionResult::Reject(
                    _,
                    ConnectionReject::Rejected(RejectReason::Server(ServerRejectReason::BadMode)),
                )
            ),
            "{:?}",
            resp
        );
    }

    fn test_remote() -> SocketAddr {
        ([127, 0, 0, 1], 6666).into()
    }

    fn test_connect(sid: Option<String>) -> Connect {
        Connect::new(
            test_remote(),
            [127, 0, 0, 1].into(),
            ConnInitSettings {
                starting_send_seqnum: random(),
                local_sockid: TEST_SOCKID,
                crypto: None,
                send_latency: Duration::from_millis(20),
                recv_latency: Duration::from_millis(20),
            },
            sid,
        )
    }
}
