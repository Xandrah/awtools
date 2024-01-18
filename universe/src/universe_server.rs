use aw_core::*;

use crate::{
    client::{Client, ClientManager},
    configuration,
    database::Database,
    packet_handler,
    universe_license::LicenseGenerator,
};
use std::net::{SocketAddrV4, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct UniverseServer {
    config: configuration::UniverseConfig,
    license_generator: LicenseGenerator,
    client_manager: ClientManager,
    database: Database,
    listener: TcpListener,
}

impl UniverseServer {
    pub fn new(config: configuration::Config) -> Result<Self, String> {
        let database = Database::new(config.mysql, &config.universe)?;

        // The Universe server provides a license to incoming clients, which must contain information
        // about the IP address that the client is connecting to. This could be different from the
        // IP address that the server is actually bound to (e.g. bound to 0.0.0.0 while clients connect
        // to some other IP).
        let bind_socket = SocketAddrV4::new(config.universe.bind_ip, config.universe.port);
        let license_socket_addr =
            SocketAddrV4::new(config.universe.license_ip, config.universe.port);

        let listener = TcpListener::bind(&bind_socket).unwrap();
        listener.set_nonblocking(true).unwrap();

        Ok(Self {
            config: config.universe,
            license_generator: LicenseGenerator::new(&license_socket_addr),
            client_manager: Default::default(),
            database,
            listener,
        })
    }

    pub fn run(&mut self) {
        log::info!(
            "Starting universe on {}:{}. Providing licenses for {}. Protocol version {}.",
            self.config.bind_ip,
            self.config.port,
            self.config.license_ip,
            Self::protocol_version(),
        );

        let running = Arc::new(AtomicBool::new(true));

        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .expect("Error setting Ctrl-C handler");

        while running.load(Ordering::SeqCst) {
            self.accept_new_clients();
            self.service_clients();
            self.client_manager.remove_dead_clients(&self.database);
            self.client_manager.send_heartbeats();
        }

        log::info!("Shutting down universe.");
    }

    fn protocol_version() -> &'static str {
        #[cfg(feature = "protocol_v4")]
        return "4";

        #[cfg(feature = "protocol_v6")]
        return "6";

        return "";
    }

    fn accept_new_clients(&mut self) {
        while let Ok((stream, addr)) = self.listener.accept() {
            let client = Client::new(AWConnection::new(AWProtocol::new(stream)), addr);
            self.client_manager.add_client(client);
        }
    }

    fn service_clients(&mut self) {
        for client in self.client_manager.clients() {
            let messages = client.connection.recv();
            self.handle_messages(messages, client);
        }
    }

    fn handle_messages(&self, messages: Vec<ProtocolMessage>, client: &Client) {
        for message in messages {
            match message {
                ProtocolMessage::Packet(packet) => {
                    self.handle_packet(&packet, client);
                }
                ProtocolMessage::Disconnect => {
                    client.kill();
                }
                ProtocolMessage::StreamKey(_)
                | ProtocolMessage::Encrypt(_)
                | ProtocolMessage::PacketGroup(_) => {
                    panic!("Should not receive these message types on this end.");
                }
            }
        }
    }

    fn handle_packet(&self, packet: &AWPacket, client: &Client) {
        log::debug!("Handling packet {packet:?}");
        match packet.get_opcode() {
            PacketType::PublicKeyRequest => packet_handler::public_key_request(client),
            PacketType::StreamKeyResponse => {
                packet_handler::stream_key_response(client, packet, &self.database)
            }
            PacketType::PublicKeyResponse => packet_handler::public_key_response(client, packet),
            PacketType::Login => packet_handler::login(
                client,
                packet,
                &self.client_manager,
                &self.license_generator,
                &self.database,
            ),
            PacketType::Heartbeat => packet_handler::heartbeat(client),
            PacketType::WorldServerStart => packet_handler::world_server_start(client, packet),
            PacketType::UserList => packet_handler::user_list(client, packet, &self.client_manager),
            PacketType::AttributeChange => packet_handler::attribute_change(
                client,
                packet,
                &self.database,
                &self.client_manager,
            ),
            PacketType::CitizenNext => packet_handler::citizen_next(client, packet, &self.database),
            PacketType::CitizenPrev => packet_handler::citizen_prev(client, packet, &self.database),
            PacketType::CitizenLookupByName => {
                packet_handler::citizen_lookup_by_name(client, packet, &self.database)
            }
            PacketType::CitizenLookupByNumber => {
                packet_handler::citizen_lookup_by_number(client, packet, &self.database)
            }
            PacketType::CitizenChange => {
                packet_handler::citizen_change(client, packet, &self.database)
            }
            PacketType::LicenseAdd => packet_handler::license_add(client, packet, &self.database),
            PacketType::LicenseByName => {
                packet_handler::license_by_name(client, packet, &self.database)
            }
            PacketType::LicenseNext => packet_handler::license_next(client, packet, &self.database),
            PacketType::LicensePrev => packet_handler::license_prev(client, packet, &self.database),
            PacketType::LicenseChange => {
                packet_handler::license_change(client, packet, &self.database)
            }
            PacketType::WorldStart => {
                packet_handler::world_start(client, packet, &self.database, &self.client_manager)
            }
            PacketType::WorldStop => {
                packet_handler::world_stop(client, packet, &self.client_manager)
            }
            PacketType::WorldList => {
                packet_handler::world_list(client, packet, &self.client_manager)
            }
            PacketType::WorldLookup => {
                packet_handler::world_lookup(client, packet, &self.client_manager)
            }
            PacketType::Identify => {
                packet_handler::identify(client, packet, &self.client_manager, &self.database)
            }
            PacketType::WorldStatsUpdate => {
                packet_handler::world_stats_update(client, packet, &self.client_manager)
            }
            PacketType::CitizenAdd => packet_handler::citizen_add(client, packet, &self.database),
            PacketType::ContactAdd => {
                packet_handler::contact_add(client, packet, &self.database, &self.client_manager)
            }
            PacketType::TelegramSend => {
                packet_handler::telegram_send(client, packet, &self.database, &self.client_manager)
            }
            PacketType::TelegramGet => {
                packet_handler::telegram_get(client, packet, &self.database);
            }
            PacketType::SetAFK => packet_handler::set_afk(client, packet),
            PacketType::ContactConfirm => packet_handler::contact_confirm(
                client,
                packet,
                &self.database,
                &self.client_manager,
            ),
            PacketType::ContactList => {
                packet_handler::contact_list(client, packet, &self.database, &self.client_manager)
            }
            _ => {
                log::info!("Unhandled packet {packet:?}");
            }
        }
    }
}
