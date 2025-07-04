use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};

use renet::{ClientId, RenetServer};
use steamworks::{
    networking_sockets::{InvalidHandle, ListenSocket, NetConnection},
    networking_types::{ListenSocketEvent, NetConnectionEnd, NetworkingConfigEntry, SendFlags},
    Client, ClientManager, FriendFlags, Friends, LobbyId, Manager, Matchmaking, SteamId,
};

use super::MAX_MESSAGE_BATCH_SIZE;

pub enum AccessPermission {
    /// Everyone can connect
    Public,
    /// No one can connect
    Private,
    /// Only friends from the host can connect
    FriendsOnly,
    /// Only user from this list can connect
    InList(HashSet<SteamId>),
    /// Users that are in the lobby can connect
    InLobby(LobbyId),
}

pub struct SteamServerConfig {
    pub max_clients: usize,
    pub access_permission: AccessPermission,
}

#[cfg_attr(feature = "bevy", derive(bevy_ecs::resource::Resource))]
pub struct SteamServerTransport<Manager = ClientManager> {
    listen_socket: Vec<ListenSocket<Manager>>,
    matchmaking: Matchmaking<Manager>,
    friends: Friends<Manager>,
    max_clients: usize,
    access_permission: AccessPermission,
    connections: HashMap<ClientId, NetConnection<Manager>>,
}

pub struct SteamServerSocketOptions {
    p2p: bool,
    socket_addr: Option<SocketAddr>,
    configs: Vec<NetworkingConfigEntry>,
}

impl Default for SteamServerSocketOptions {
    fn default() -> Self {
        Self::new_p2p()
    }
}

impl SteamServerSocketOptions {
    pub fn new_p2p() -> Self {
        Self {
            p2p: true,
            socket_addr: None,
            configs: vec![],
        }
    }

    pub fn new_address(socket_addr: SocketAddr) -> Self {
        Self {
            p2p: false,
            socket_addr: Some(socket_addr),
            configs: vec![],
        }
    }

    pub fn with_p2p(mut self) -> Self {
        self.p2p = true;
        self
    }

    pub fn with_address(mut self, socket_addr: SocketAddr) -> Self {
        self.socket_addr = Some(socket_addr);
        self
    }

    pub fn with_config(mut self, config_option: NetworkingConfigEntry) -> Self {
        self.configs.push(config_option);
        self
    }
}

impl<T: Manager + 'static> SteamServerTransport<T> {
    pub fn new(client: &Client<T>, config: SteamServerConfig, socket_options: SteamServerSocketOptions) -> Result<Self, InvalidHandle> {
        let options = socket_options.configs;
        let networking = client.networking_sockets();

        let mut listen_socket = vec![];
        if socket_options.p2p {
            listen_socket.push(networking.create_listen_socket_p2p(0, options.clone())?);
        }
        if let Some(addr) = socket_options.socket_addr {
            listen_socket.push(networking.create_listen_socket_ip(addr, options.clone())?);
        }

        let matchmaking = client.matchmaking();
        let friends = client.friends();

        Ok(Self {
            listen_socket,
            matchmaking,
            friends,
            max_clients: config.max_clients,
            access_permission: config.access_permission,
            connections: HashMap::new(),
        })
    }

    pub fn max_clients(&self) -> usize {
        self.max_clients
    }

    /// Update the access permission to the server,
    /// this change only applies to new connections.
    pub fn set_access_permissions(&mut self, access_permission: AccessPermission) {
        self.access_permission = access_permission;
    }

    /// Disconnects a client from the server.
    pub fn disconnect_client(&mut self, client_id: ClientId, server: &mut RenetServer, flush_last_packets: bool) {
        if let Some((_key, value)) = self.connections.remove_entry(&client_id) {
            let _ = value.close(NetConnectionEnd::AppGeneric, Some("Client was kicked"), flush_last_packets);
        }
        server.remove_connection(client_id);
    }

    /// Disconnects all active clients including the host client from the server.
    pub fn disconnect_all(&mut self, server: &mut RenetServer, flush_last_packets: bool) {
        let keys = self.connections.keys().cloned().collect::<Vec<ClientId>>();
        for client_id in keys {
            let _ = self.connections.remove_entry(&client_id).unwrap().1.close(
                NetConnectionEnd::AppGeneric,
                Some("Client was kicked"),
                flush_last_packets,
            );
            server.remove_connection(client_id);
        }
    }

    /// Update server connections, and receive packets from the network.
    pub fn update(&mut self, server: &mut RenetServer) {
        for listen_socket in self.listen_socket.iter() {
            while let Some(event) = listen_socket.try_receive_event() {
                match event {
                    ListenSocketEvent::Connected(event) => {
                        if let Some(steam_id) = event.remote().steam_id() {
                            server.add_connection(steam_id.raw());
                            self.connections.insert(steam_id.raw(), event.take_connection());
                        }
                    }
                    ListenSocketEvent::Disconnected(event) => {
                        if let Some(steam_id) = event.remote().steam_id() {
                            server.remove_connection(steam_id.raw());
                            self.connections.remove(&steam_id.raw());
                        }
                    }
                    ListenSocketEvent::Connecting(event) => {
                        if server.connected_clients() >= self.max_clients {
                            event.reject(NetConnectionEnd::AppGeneric, Some("Too many clients"));
                            continue;
                        }

                        let Some(steam_id) = event.remote().steam_id() else {
                            event.reject(NetConnectionEnd::AppGeneric, Some("Invalid steam id"));
                            continue;
                        };

                        let permitted = match &self.access_permission {
                            AccessPermission::Public => true,
                            AccessPermission::Private => false,
                            AccessPermission::FriendsOnly => {
                                let friend = self.friends.get_friend(steam_id);
                                friend.has_friend(FriendFlags::IMMEDIATE)
                            }
                            AccessPermission::InList(list) => list.contains(&steam_id),
                            AccessPermission::InLobby(lobby) => {
                                let users_in_lobby = self.matchmaking.lobby_members(*lobby);
                                users_in_lobby.contains(&steam_id)
                            }
                        };

                        if permitted {
                            if let Err(e) = event.accept() {
                                log::error!("Failed to accept connection from {steam_id:?}: {e}");
                            }
                        } else {
                            event.reject(NetConnectionEnd::AppGeneric, Some("Not allowed"));
                        }
                    }
                }
            }
        }

        for (client_id, connection) in self.connections.iter_mut() {
            // TODO this allocates on the side of steamworks.rs and should be avoided, PR needed
            if let Ok(messages) = connection.receive_messages(MAX_MESSAGE_BATCH_SIZE) {
                messages.iter().for_each(|message| {
                    if let Err(e) = server.process_packet_from(message.data(), *client_id) {
                        log::error!("Error while processing payload for {}: {}", client_id, e);
                    };
                });
            }
        }
    }

    /// Send packets to connected clients.
    pub fn send_packets(&mut self, server: &mut RenetServer) {
        'clients: for client_id in server.clients_id() {
            let Some(connection) = self.connections.get(&client_id) else {
                log::error!("Error while sending packet: connection not found");
                continue;
            };
            let packets = server.get_packets_to_send(client_id).unwrap();
            // TODO: while this works fine we should probaly use the send_messages function from the listen_socket
            for packet in packets {
                if let Err(e) = connection.send_message(&packet, SendFlags::UNRELIABLE) {
                    log::error!("Failed to send packet to client {client_id}: {e}");
                    continue 'clients;
                }
            }

            if let Err(e) = connection.flush_messages() {
                log::error!("Failed flush messages for {client_id}: {e}");
            }
        }
    }
}
