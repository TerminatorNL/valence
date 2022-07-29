use std::mem;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use log::LevelFilter;
use num::Integer;
use rayon::iter::{IndexedParallelIterator, IntoParallelRefMutIterator, ParallelIterator};
use valence::biome::Biome;
use valence::block::BlockState;
use valence::client::{Event, Hand};
use valence::config::{Config, ServerListPing};
use valence::dimension::{Dimension, DimensionId};
use valence::entity::data::Pose;
use valence::entity::{EntityEnum, EntityId, EntityKind, Event as EntityEvent};
use valence::server::{Server, SharedServer, ShutdownResult};
use valence::text::{Color, TextFormat};
use valence::{async_trait, ident};

pub fn main() -> ShutdownResult {
    env_logger::Builder::new()
        .filter_module("valence", LevelFilter::Trace)
        .parse_default_env()
        .init();

    valence::start_server(
        Game {
            player_count: AtomicUsize::new(0),
        },
        ServerState {
            board: vec![false; SIZE_X * SIZE_Z].into_boxed_slice(),
            board_buf: vec![false; SIZE_X * SIZE_Z].into_boxed_slice(),
        },
    )
}

struct Game {
    player_count: AtomicUsize,
}

struct ServerState {
    board: Box<[bool]>,
    board_buf: Box<[bool]>,
}

#[derive(Default)]
struct ClientState {
    /// The client's player entity.
    player: EntityId,
}

const MAX_PLAYERS: usize = 10;

const SIZE_X: usize = 100;
const SIZE_Z: usize = 100;
const BOARD_Y: i32 = 50;

#[async_trait]
impl Config for Game {
    type ChunkState = ();
    type ClientState = ClientState;
    type EntityState = ();
    type ServerState = ServerState;
    type WorldState = ();

    fn max_connections(&self) -> usize {
        // We want status pings to be successful even if the server is full.
        MAX_PLAYERS + 64
    }

    fn online_mode(&self) -> bool {
        // You'll want this to be true on real servers.
        false
    }

    fn dimensions(&self) -> Vec<Dimension> {
        vec![Dimension {
            fixed_time: Some(6000),
            ..Dimension::default()
        }]
    }

    fn biomes(&self) -> Vec<Biome> {
        vec![Biome {
            name: ident!("valence:default_biome"),
            grass_color: Some(0x00ff00),
            ..Biome::default()
        }]
    }

    async fn server_list_ping(
        &self,
        _server: &SharedServer<Self>,
        _remote_addr: SocketAddr,
    ) -> ServerListPing {
        ServerListPing::Respond {
            online_players: self.player_count.load(Ordering::SeqCst) as i32,
            max_players: MAX_PLAYERS as i32,
            description: "Hello Valence!".color(Color::AQUA),
            favicon_png: Some(include_bytes!("../assets/favicon.png")),
        }
    }

    fn init(&self, server: &mut Server<Self>) {
        let world = server.worlds.create(DimensionId::default(), ()).1;
        world.meta.set_flat(true);

        for chunk_z in -2..Integer::div_ceil(&(SIZE_X as i32), &16) + 2 {
            for chunk_x in -2..Integer::div_ceil(&(SIZE_Z as i32), &16) + 2 {
                world.chunks.create((chunk_x as i32, chunk_z as i32), ());
            }
        }
    }

    fn update(&self, server: &mut Server<Self>) {
        let (world_id, world) = server.worlds.iter_mut().next().unwrap();

        let spawn_pos = [
            SIZE_X as f64 / 2.0,
            BOARD_Y as f64 + 1.0,
            SIZE_Z as f64 / 2.0,
        ];

        server.clients.retain(|_, client| {
            if client.created_tick() == server.shared.current_tick() {
                if self
                    .player_count
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                        (count < MAX_PLAYERS).then_some(count + 1)
                    })
                    .is_err()
                {
                    client.disconnect("The server is full!".color(Color::RED));
                    return false;
                }

                client.spawn(world_id);
                client.teleport(spawn_pos, 0.0, 0.0);

                world.meta.player_list_mut().insert(
                    client.uuid(),
                    client.username().to_owned(),
                    client.textures().cloned(),
                    client.game_mode(),
                    0,
                    None,
                );

                client.state.player = server
                    .entities
                    .create_with_uuid(EntityKind::Player, client.uuid(), ())
                    .unwrap()
                    .0;

                client.send_message("Welcome to Conway's game of life in Minecraft!".italic());
                client.send_message("Hold the left mouse button to bring blocks to life.".italic());
            }

            if client.is_disconnected() {
                self.player_count.fetch_sub(1, Ordering::SeqCst);
                server.entities.delete(client.state.player);
                world.meta.player_list_mut().remove(client.uuid());
                return false;
            }

            let player = server.entities.get_mut(client.state.player).unwrap();

            if client.position().y <= 0.0 {
                client.teleport(spawn_pos, client.yaw(), client.pitch());
            }

            while let Some(event) = client.pop_event() {
                match event {
                    Event::Digging { position, .. } => {
                        if (0..SIZE_X as i32).contains(&position.x)
                            && (0..SIZE_Z as i32).contains(&position.z)
                            && position.y == BOARD_Y
                        {
                            server.state.board
                                [position.x as usize + position.z as usize * SIZE_X] = true;
                        }
                    }
                    Event::ArmSwing(hand) => match hand {
                        Hand::Main => player.trigger_event(EntityEvent::SwingMainHand),
                        Hand::Off => player.trigger_event(EntityEvent::SwingOffHand),
                    },
                    _ => {}
                }
            }

            player.set_world(client.world());
            player.set_position(client.position());
            player.set_yaw(client.yaw());
            player.set_head_yaw(client.yaw());
            player.set_pitch(client.pitch());
            player.set_on_ground(client.on_ground());

            if let EntityEnum::Player(player) = player.view_mut() {
                if client.is_sneaking() {
                    player.set_pose(Pose::Sneaking);
                } else {
                    player.set_pose(Pose::Standing);
                }

                player.set_sprinting(client.is_sprinting());
            }

            true
        });

        if server.shared.current_tick() % 4 != 0 {
            return;
        }

        server
            .state
            .board_buf
            .par_iter_mut()
            .enumerate()
            .for_each(|(i, cell)| {
                let cx = (i % SIZE_X) as i32;
                let cz = (i / SIZE_Z) as i32;

                let mut live_count = 0;
                for z in cz - 1..=cz + 1 {
                    for x in cx - 1..=cx + 1 {
                        if !(x == cx && z == cz) {
                            let i = x.rem_euclid(SIZE_X as i32) as usize
                                + z.rem_euclid(SIZE_Z as i32) as usize * SIZE_X;
                            if server.state.board[i] {
                                live_count += 1;
                            }
                        }
                    }
                }

                if server.state.board[cx as usize + cz as usize * SIZE_X] {
                    *cell = (2..=3).contains(&live_count);
                } else {
                    *cell = live_count == 3;
                }
            });

        mem::swap(&mut server.state.board, &mut server.state.board_buf);

        let min_y = server.shared.dimensions().next().unwrap().1.min_y;

        for chunk_x in 0..Integer::div_ceil(&SIZE_X, &16) {
            for chunk_z in 0..Integer::div_ceil(&SIZE_Z, &16) {
                let chunk = world
                    .chunks
                    .get_mut((chunk_x as i32, chunk_z as i32))
                    .unwrap();
                for x in 0..16 {
                    for z in 0..16 {
                        let cell_x = chunk_x * 16 + x;
                        let cell_z = chunk_z * 16 + z;

                        if cell_x < SIZE_X && cell_z < SIZE_Z {
                            let b = if server.state.board[cell_x + cell_z * SIZE_X] {
                                BlockState::GRASS_BLOCK
                            } else {
                                BlockState::DIRT
                            };
                            chunk.set_block_state(x, (BOARD_Y - min_y) as usize, z, b);
                        }
                    }
                }
            }
        }
    }
}
