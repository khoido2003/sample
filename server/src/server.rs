use std::{
    error::Error,
    f32::consts::PI,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{net::UdpSocket, sync::RwLock, time::interval};

/////////////////////////////////////////////////////////

const MAX_CLIENT: usize = 32;
const SECONDS_PER_TICK: f32 = 1.0 / 60.0;
const ACCELERATION: f32 = 0.5;
const MAX_SPEED: f32 = 5.0;
const TURN_SPEED: f32 = PI / 2.0;
const CLIENT_TIMEOUT: f32 = 5.0;
const FAKE_LAG_S: f32 = 0.2;
const SOCKET_BUFFER_SIZE: usize = 1024;
const INPUT_BUFFER_SIZE: usize = 60;

#[repr(u8)]
enum ClientMessage {
    Join = 0,
    Leave = 1,
    Input = 2,
}

#[repr(u8)]
enum ServerMessage {
    JoinResult = 0,
    State = 1,
}

#[derive(Debug, PartialEq, Clone)]
struct IpEndpoint {
    address: SocketAddr,
}

#[derive(Debug, Default, Clone)]
struct PlayerState {
    x: f32,
    y: f32,
    facing: f32,
    speed: f32,
}

#[derive(Debug, Default, Clone)]
struct PlayerInput {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
}

#[derive(Clone, Debug)]
struct Packet {
    data: Vec<u8>,
    endpoint: SocketAddr,
    release_time: Instant,
}

impl Default for Packet {
    fn default() -> Self {
        Packet {
            data: Vec::new(),
            endpoint: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
            release_time: Instant::now(),
        }
    }
}

struct RingBuffer<T> {
    items: Vec<T>,
    head: usize,
    tail: usize,
    capacity: usize,
}

impl<T: Clone + Default> RingBuffer<T> {
    fn new(capacity: usize) -> Self {
        RingBuffer {
            items: vec![T::default(); capacity],
            head: 0,
            tail: 0,
            capacity,
        }
    }

    fn push(&mut self, item: T) -> bool {
        let next_tail = (self.tail + 1) % self.capacity;

        if next_tail == self.head {
            false
        } else {
            self.items[self.tail] = item;
            self.tail = next_tail;
            true
        }
    }

    fn pop(&mut self) -> Option<T> {
        if self.head == self.tail {
            None
        } else {
            let item = std::mem::take(&mut self.items[self.head]);
            self.head = (self.head + 1) % self.capacity;
            Some(item)
        }
    }
}

struct PacketBuffer(RingBuffer<Packet>);

impl PacketBuffer {
    fn new() -> Self {
        PacketBuffer(RingBuffer::new(
            (1.0 / SECONDS_PER_TICK * FAKE_LAG_S) as usize + 2,
        ))
    }

    fn push(&mut self, data: Vec<u8>, endpoint: SocketAddr) {
        self.0.push(Packet {
            data,
            endpoint,
            release_time: Instant::now(),
        });
    }
    fn pop(&mut self) -> Option<(Vec<u8>, SocketAddr)> {
        self.0.pop().map(|p| (p.data, p.endpoint))
    }
}

fn tick_player(state: &mut PlayerState, input: &PlayerInput) {
    if input.up {
        state.speed += ACCELERATION * SECONDS_PER_TICK;
        state.speed = state.speed.min(MAX_SPEED);
    }

    if input.down {
        state.speed -= ACCELERATION * SECONDS_PER_TICK;
        state.speed = state.speed.max(0.0);
    }

    if input.left {
        state.facing -= TURN_SPEED * SECONDS_PER_TICK;
    }

    if input.right {
        state.facing += TURN_SPEED * SECONDS_PER_TICK;
    }

    state.x += state.speed * SECONDS_PER_TICK * state.facing.sin();
    state.y += state.speed * SECONDS_PER_TICK * state.facing.cos();
}

pub struct ServerContext {
    socket: UdpSocket,
    client_enpoints: Vec<Option<IpEndpoint>>,
    time_since_heard: Vec<f32>,
    client_objects: Vec<PlayerState>,
    input_buffer: Vec<Vec<(PlayerInput, bool)>>,

    send_buffer: PacketBuffer,
    recv_buffer: PacketBuffer,
}

struct Globals {
    tick_number: usize,
}

impl ServerContext {
    pub fn new(socket: UdpSocket) -> Self {
        ServerContext {
            socket,
            client_enpoints: vec![None; MAX_CLIENT],
            time_since_heard: vec![0.0; MAX_CLIENT],
            client_objects: vec![PlayerState::default(); MAX_CLIENT],
            input_buffer: vec![vec![(PlayerInput::default(), false); MAX_CLIENT]; 60],

            send_buffer: PacketBuffer::new(),
            recv_buffer: PacketBuffer::new(),
        }
    }
}

pub async fn start_server() -> Result<(), Box<dyn Error + Send + Sync>> {
    let socket = UdpSocket::bind("0.0.0.0:9999").await?;
    let server_context = Arc::new(RwLock::new(ServerContext::new(socket)));
    tokio::spawn(listen_handler(server_context.clone()));

    Ok(())
}

async fn listen_handler(
    server_context: Arc<RwLock<ServerContext>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut raw_buffer = vec![0u8; SOCKET_BUFFER_SIZE];
    let mut tick_interval = interval(tokio::time::Duration::from_secs_f32(SECONDS_PER_TICK));

    let mut globals = Globals { tick_number: 0 };

    loop {
        tokio::select! {
            result = async {
                let server_context = server_context.read().await;
                server_context.socket.recv_from(&mut raw_buffer).await
            } => {

                if let Ok((bytes_received, from_addr)) = result {
                    let mut server_context = server_context.write().await;
                    server_context.recv_buffer.push(raw_buffer[..bytes_received].to_vec(), from_addr);
                }
            }

            _ = tick_interval.tick() => {

                let tick_start = Instant::now();
                process(server_context.clone()).await;

                let mut context = server_context.write().await;
                let tick_index = globals.tick_number % INPUT_BUFFER_SIZE;

                globals.tick_number += 1;

                let current_inputs = &context.input_buffer[tick_index].clone();
                let current_client_endpoints = &context.client_enpoints.clone();

                // Update simulation
                (0..MAX_CLIENT).for_each(|i| {
                    if context.client_enpoints[i].is_some() {
                        let (input, has_input) = &current_inputs[i];
                        if *has_input {
                            tick_player(&mut context.client_objects[i], input);
                        }
                    }
                });

                (0..MAX_CLIENT).for_each(|i| {
                    if let Some(endpoint) = &current_client_endpoints[i] {
                        let mut packet = vec![ServerMessage::State as u8];

                        packet.extend_from_slice(&globals.tick_number.to_le_bytes());
                        packet.extend_from_slice(&context.client_objects[i].x.to_le_bytes());
                        packet.extend_from_slice(&context.client_objects[i].y.to_le_bytes());
                        packet.extend_from_slice(&context.client_objects[i].facing.to_le_bytes());
                        packet.extend_from_slice(&context.client_objects[i].speed.to_le_bytes());

                        for j in 0..MAX_CLIENT {
                            if i != j && context.client_enpoints[j].is_some() {
                                packet.extend_from_slice(&(j as u16).to_le_bytes());
                                packet.extend_from_slice(&context.client_objects[j].x.to_le_bytes());
                                packet.extend_from_slice(&context.client_objects[j].y.to_le_bytes());
                                packet.extend_from_slice(&context.client_objects[j].facing.to_le_bytes());
                            }
                        }
                        context.send_buffer.push(packet, endpoint.address);
                    }
                });

                while let Some((data, addr)) = context.send_buffer.pop() {
                    context.socket.send_to(&data, addr).await?;
                }

                globals.tick_number += 1;

                let elapsed = tick_start.elapsed();
                if elapsed > Duration::from_secs_f32(SECONDS_PER_TICK) {
                    println!("Tick {} took too long: {:?}", globals.tick_number, elapsed);
                }
            }
        }
    }
}

async fn process(server_context: Arc<RwLock<ServerContext>>) {
    let mut server_context = server_context.write().await;

    while let Some((data, from_addr)) = server_context.recv_buffer.pop() {
        if data.is_empty() {
            continue;
        }

        match data[0] {
            x if x == ClientMessage::Join as u8 => {
                let slot = server_context
                    .client_enpoints
                    .iter()
                    .position(|e| e.is_none());

                let mut response = vec![ServerMessage::JoinResult as u8];
                if let Some(slot) = slot {
                    response.push(1);
                    response.extend_from_slice(&(slot as u16).to_le_bytes());
                    server_context.send_buffer.push(response, from_addr);

                    server_context.client_enpoints[slot] = Some(IpEndpoint { address: from_addr });

                    server_context.time_since_heard[slot] = 0.0;
                    server_context.client_objects[slot] = PlayerState::default();

                    println!("Client {} joined from {}", slot, from_addr);
                }
            }

            x if x == ClientMessage::Leave as u8 && data.len() >= 3 => {
                let slot = u16::from_le_bytes(data[1..3].try_into().unwrap()) as usize;

                if slot < MAX_CLIENT
                    && server_context.client_enpoints[slot]
                        .as_ref()
                        .is_some_and(|e| e.address == from_addr)
                {
                    let input_tick = u32::from_le_bytes(data[3..7].try_into().unwrap());

                    let input = data[7];
                    let buffer_idx = (input_tick % INPUT_BUFFER_SIZE as u32) as usize;

                    server_context.input_buffer[buffer_idx][slot] = (
                        PlayerInput {
                            up: input & 0x1 != 0,
                            down: input & 0x2 != 0,
                            left: input & 0x4 != 0,
                            right: input & 0x8 != 0,
                        },
                        true,
                    );

                    server_context.time_since_heard[slot] = 0.0;
                }
            }

            x if x == ClientMessage::Input as u8 && data.len() >= 7 => {
                let slot = u16::from_le_bytes(data[1..3].try_into().unwrap()) as usize;
                if slot < MAX_CLIENT
                    && server_context.client_enpoints[slot]
                        .as_ref()
                        .is_some_and(|e| e.address == from_addr)
                {
                    let input_tick = u32::from_le_bytes(data[3..7].try_into().unwrap());

                    let input = data[7];
                    let buffer_idx = (input_tick % INPUT_BUFFER_SIZE as u32) as usize;
                    server_context.input_buffer[buffer_idx][slot] = (
                        PlayerInput {
                            up: input & 0x1 != 0,
                            down: input & 0x2 != 0,
                            left: input & 0x4 != 0,
                            right: input & 0x8 != 0,
                        },
                        true,
                    );
                    server_context.time_since_heard[slot] = 0.0;
                }
            }

            _ => {}
        }
    }
}
