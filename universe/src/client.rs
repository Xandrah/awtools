use std::{
    cell::{Ref, RefCell, RefMut},
    io::Write,
    net::{IpAddr, SocketAddr},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    database::{
        citizen::{CitizenDB, CitizenQuery},
        Database,
    },
    packet_handler::{self, update_contacts_of_user},
    player::{PlayerInfo, PlayerState},
    world::{World, WorldServerInfo},
    AWConnection, AWCryptRSA,
};
use aw_core::{AWPacket, PacketType, ReasonCode};
use byteorder::{LittleEndian, WriteBytesExt};
use num_derive::FromPrimitive;

/// Game-related client state
#[derive(Default)]
pub struct UserInfo {
    pub client_type: Option<ClientType>,
    pub entity: Option<Entity>,
}

#[derive(Debug)]
pub enum Entity {
    Player(PlayerInfo),
    WorldServer(WorldServerInfo),
}

impl Entity {
    pub fn new_citizen(
        citizen_id: u32,
        privilege_id: Option<u32>,
        session_id: u16,
        build: i32,
        username: &str,
        ip: IpAddr,
    ) -> Self {
        Self::Player(PlayerInfo {
            build,
            session_id,
            citizen_id: Some(citizen_id),
            privilege_id: privilege_id,
            username: username.to_string(),
            nonce: None,
            world: None,
            ip,
            state: PlayerState::Online,
            afk: false,
        })
    }

    pub fn new_tourist(session_id: u16, build: i32, username: &str, ip: IpAddr) -> Self {
        Self::Player(PlayerInfo {
            build,
            session_id,
            citizen_id: None,
            privilege_id: None,
            username: username.to_string(),
            nonce: None,
            world: None,
            ip,
            state: PlayerState::Online,
            afk: false,
        })
    }

    pub fn is_player(&self) -> bool {
        matches!(self, Entity::Player(_))
    }

    pub fn is_world(&self) -> bool {
        matches!(self, Entity::WorldServer(_))
    }
}

pub struct Client {
    pub connection: AWConnection,
    pub dead: RefCell<bool>,
    pub rsa: AWCryptRSA,
    user_info: RefCell<UserInfo>,
    pub addr: SocketAddr,
    pub last_heartbeat: u64,
}

impl Client {
    pub fn new(connection: AWConnection, addr: SocketAddr) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Current time is before the unix epoch.")
            .as_secs();

        Self {
            connection,
            dead: RefCell::new(false),
            rsa: AWCryptRSA::new(),
            user_info: RefCell::new(Default::default()),
            addr,
            last_heartbeat: now,
        }
    }

    pub fn kill(&self) {
        *self.dead.borrow_mut() = true;
    }

    pub fn is_dead(&self) -> bool {
        *self.dead.borrow()
    }

    pub fn info_mut(&self) -> RefMut<UserInfo> {
        self.user_info.borrow_mut()
    }

    pub fn info(&self) -> Ref<UserInfo> {
        self.user_info.borrow()
    }

    pub fn has_admin_permissions(&self) -> bool {
        if let Some(Entity::Player(info)) = &self.info().entity {
            info.citizen_id == Some(1) || info.privilege_id == Some(1)
        } else {
            false
        }
    }
}

#[derive(FromPrimitive, Clone, Copy, Debug, PartialEq)]
pub enum ClientType {
    World = 1,
    UnspecifiedHuman = 2, // Temporary state between Citizen or Tourist
    Bot = 3,
    Citizen = 4,
    Tourist = 5,
}

#[derive(Default)]
pub struct ClientManager {
    clients: Vec<Client>,
}

impl ClientManager {
    pub fn create_session_id(&self) -> u16 {
        let mut new_session_id: u16 = 0;
        loop {
            new_session_id += 1;
            if self.get_client_by_session_id(new_session_id).is_none() {
                break;
            }
        }
        new_session_id
    }

    pub fn get_client_by_session_id(&self, session_id: u16) -> Option<&Client> {
        for client in self.clients() {
            if let Some(Entity::Player(info)) = &client.info().entity {
                if info.session_id == session_id {
                    return Some(client);
                }
            }
        }
        None
    }

    pub fn get_client_by_citizen_id(&self, citizen_id: u32) -> Option<&Client> {
        for client in self.clients() {
            if let Some(Entity::Player(info)) = &client.info().entity {
                if info.citizen_id == Some(citizen_id) {
                    return Some(client);
                }
            }
        }
        None
    }

    pub fn add_client(&mut self, client: Client) {
        self.clients.push(client);
    }

    pub fn clients(&self) -> &Vec<Client> {
        &self.clients
    }

    pub fn remove_dead_clients(&mut self, database: &Database) {
        for client in self.clients().iter().filter(|x| x.is_dead()) {
            log::info!("Disconnected {}", client.addr.ip());
            if let Some(Entity::WorldServer(server_info)) = &mut client.info_mut().entity {
                packet_handler::world_server_hide_all(server_info);
            }
            if let Some(Entity::WorldServer(server_info)) = &client.info().entity {
                World::send_updates_to_all(&server_info.worlds, self);
            }

            if let Some(Entity::Player(player)) = &mut client.info_mut().entity {
                player.state = PlayerState::Offline;
            }
            if let Some(Entity::Player(player)) = &client.info().entity {
                PlayerInfo::send_update_to_all(player, self);

                if let Some(citizen_id) = player.citizen_id {
                    // Update the user's friends to tell them this user is now offline
                    update_contacts_of_user(citizen_id, database, self);
                }
            }
        }
        self.clients = self.clients.drain(..).filter(|x| !x.is_dead()).collect();
    }

    pub fn check_tourist(&self, username: &str) -> Result<(), ReasonCode> {
        check_valid_name(username, true)?;

        for other_client in self.clients() {
            if let Some(Entity::Player(info)) = &other_client.info().entity {
                if info.username == username {
                    return Err(ReasonCode::NameAlreadyUsed);
                }
            }
        }

        Ok(())
    }

    pub fn check_citizen(
        &self,
        db: &Database,
        client: &Client,
        username: &Option<String>,
        password: Option<&String>,
        password_hash: Option<&Vec<u8>>,
        priv_id: Option<u32>,
        priv_pass: &Option<String>,
    ) -> Result<CitizenQuery, ReasonCode> {
        // Name and password must be present
        let username = username.as_ref().ok_or(ReasonCode::NoSuchCitizen)?;
        if username.is_empty() {
            return Err(ReasonCode::NoSuchCitizen);
        }

        // Name cannot be bot or tourist
        if username.starts_with('[') || username.starts_with('"') {
            return Err(ReasonCode::NoSuchCitizen);
        }

        // Checks if acquiring a privilege
        if let Some(priv_id) = priv_id.filter(|x| *x != 0) {
            // Get acting citizen
            let priv_citizen = db
                .citizen_by_number(priv_id)
                .map_err(|_| ReasonCode::NoSuchActingCitizen)?;

            // Is it enabled?
            if priv_citizen.enabled == 0 && priv_citizen.id != 1 {
                return Err(ReasonCode::NoSuchActingCitizen);
            }

            // Is the priv pass present and correct?
            let priv_pass = priv_pass
                .as_ref()
                .ok_or(ReasonCode::ActingPasswordInvalid)?;
            if *priv_pass != priv_citizen.priv_pass {
                return Err(ReasonCode::ActingPasswordInvalid);
            }
        }

        // Get login citizen
        let login_citizen = db
            .citizen_by_name(username)
            .or(Err(ReasonCode::NoSuchCitizen))?;

        // Is login password correct?
        #[cfg(feature = "protocol_v4")]
        {
            let password = password.ok_or(ReasonCode::InvalidPassword)?;
            if password.is_empty() {
                return Err(ReasonCode::InvalidPassword);
            }
            if login_citizen.password != *password {
                return Err(ReasonCode::InvalidPassword);
            }
        }
        #[cfg(feature = "protocol_v6")]
        {
            let mut correct_password_buf = Vec::<u8>::new();
            correct_password_buf.write_u32::<LittleEndian>(login_citizen.password.len() as u32);
            correct_password_buf.write_all(
                &login_citizen
                    .password
                    .as_bytes()
                    .iter()
                    .rev()
                    .map(|x| *x)
                    .collect::<Vec<u8>>(),
            );

            let hashed_correct_password = md5::compute(correct_password_buf.to_vec());

            let Some(password_hash) = password_hash else {
                return Err(ReasonCode::InvalidPassword);
            };

            if *password_hash != hashed_correct_password.to_vec() {
                return Err(ReasonCode::InvalidPassword);
            }
        }

        // Is it enabled?
        if login_citizen.enabled == 0 {
            return Err(ReasonCode::CitizenDisabled);
        }

        // Is this citizen already logged in?
        for other_client in self.clients() {
            if let Some(Entity::Player(info)) = &other_client.info().entity {
                if info.citizen_id == Some(login_citizen.id) {
                    // Don't give an error if the client is already logged in as this user.
                    if client as *const Client != other_client as *const Client {
                        return Err(ReasonCode::IdentityAlreadyInUse);
                    }
                }
            }
        }

        Ok(login_citizen)
    }

    pub fn send_heartbeats(&mut self) {
        for client in &mut self.clients {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Current time is before the unix epoch.")
                .as_secs();

            // 30 seconds between each heartbeat
            let next_heartbeat = client.last_heartbeat + 30;

            if next_heartbeat <= now {
                log::info!("Sending heartbeat to {}", client.addr.ip());
                let packet = AWPacket::new(PacketType::Heartbeat);
                client.connection.send(packet);
                client.last_heartbeat = now;
            }
        }
    }

    pub fn get_world_by_name(&self, name: &str) -> Option<World> {
        for client in self.clients() {
            if let Some(Entity::WorldServer(server)) = &client.info().entity {
                for world in &server.worlds {
                    if world.name.eq_ignore_ascii_case(name) {
                        return Some(world.clone());
                    }
                }
            }
        }
        None
    }

    pub fn get_world_infos(&self) -> Vec<World> {
        // Get a list of all the worlds
        let mut world_list = Vec::<World>::new();
        for client in self.clients() {
            if let Some(Entity::WorldServer(world_server)) = &client.info().entity {
                world_list.extend(world_server.worlds.clone());
            }
        }
        world_list
    }

    pub fn get_player_infos(&self) -> Vec<PlayerInfo> {
        // Get a list of all the player infos
        let mut player_list = Vec::<PlayerInfo>::new();
        for client in self.clients() {
            if let Some(Entity::Player(player_info)) = &client.info().entity {
                player_list.push(player_info.clone());
            }
        }
        player_list
    }
}

fn check_valid_name(name: &str, is_tourist: bool) -> Result<(), ReasonCode> {
    let mut name = name.to_string();

    if is_tourist {
        // Tourist names must start and end with quotes
        if !name.starts_with('"') || !name.ends_with('"') {
            return Err(ReasonCode::NoSuchCitizen);
        }

        // Strip quotes to continue check
        name.remove(0);
        name.remove(name.len() - 1);
    }

    if name.len() < 2 {
        return Err(ReasonCode::NameTooShort);
    }

    if name.ends_with(' ') {
        return Err(ReasonCode::NameEndsWithBlank);
    }

    if name.starts_with(' ') {
        return Err(ReasonCode::NameContainsInvalidBlank);
    }

    if !name.chars().all(char::is_alphanumeric) {
        return Err(ReasonCode::NameContainsNonalphanumericChar);
    }

    Ok(())
}
