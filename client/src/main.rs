use std::{
    error::Error,
    f32::consts::PI,
    sync::Arc,
    time::{Duration, Instant},
};

use minifb::{Key, Window, WindowOptions};
use rand::Rng;
use tokio::{net::UdpSocket, sync::RwLock, time::interval};

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

    fn size(&self) -> usize {
        if self.tail >= self.head {
            self.tail - self.head
        } else {
            self.capacity - self.head + self.tail
        }
    }
}

/////////////////////////////////////////////////////////////
const ACCELERATION: f32 = 0.5;
const MAX_SPEED: f32 = 5.0;
const TURN_SPEED: f32 = PI / 2.0;
const MAX_CLIENTS: usize = 32;
const SECONDS_PER_TICK: f32 = 1.0 / 60.0;
const FAKE_LAG_S: f32 = 0.2;
const SOCKET_BUFFER_SIZE: usize = 1024;
const WINDOW_WIDTH: usize = 800;
const WINDOW_HEIGHT: usize = 600;
const MAX_PREDICTED_TICKS: usize = (1.0 / SECONDS_PER_TICK * 2.0) as usize;

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

#[derive(Default, Debug, Clone)]
struct PlayerState {
    x: f32,
    y: f32,
    facing: f32,
    speed: f32,
}

#[derive(Default, Debug, Clone)]
struct PlayerInput {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
}

struct Vertex {
    x: f32,
    y: f32,
    color: u32,
}

#[derive(Clone)]
struct Packet {
    data: Vec<u8>,
    endpoint: String,
    release_time: Instant,
}

impl Default for Packet {
    fn default() -> Self {
        Packet {
            data: vec![],
            endpoint: String::new(),
            release_time: Instant::now(),
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

    fn push(&mut self, data: &[u8], endpoint: &str) {
        self.0.push(Packet {
            data: data.to_vec(),
            endpoint: endpoint.to_string(),
            release_time: Instant::now() + Duration::from_secs_f32(FAKE_LAG_S),
        });
    }

    fn pop(&mut self) -> Option<(Vec<u8>, String)> {
        self.0.pop().map(|p| (p.data, p.endpoint))
    }
}

struct Globals {
    tick_number: u32,
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

struct ClientContext {
    client_id: Option<u16>,
    local_player: PlayerState,
    prediction_history: RingBuffer<(PlayerState, PlayerInput)>,
    objects: Vec<PlayerState>,
    num_objects: usize,
    tick_number: u32,
    target_tick_number: u32,
    send_buffer: PacketBuffer,
    recv_buffer: PacketBuffer,
    vertices: Vec<Vertex>,
    frame_buffer: Vec<u32>,
}

impl ClientContext {
    fn new() -> Self {
        let mut vertices = Vec::with_capacity(MAX_CLIENTS * 4);
        let mut rng = rand::thread_rng();

        for _ in 0..MAX_CLIENTS {
            let r = rng.gen_range(0..2) * 255;
            let g = rng.gen_range(0..2) * 255;
            let b = rng.gen_range(0..2) * 255;

            let color = (r << 16) | (g << 8) | b;
            for _ in 0..4 {
                vertices.push(Vertex {
                    x: 0.0,
                    y: 0.0,
                    color,
                });
            }
        }
        ClientContext {
            client_id: None,
            local_player: PlayerState::default(),
            prediction_history: RingBuffer::new(MAX_PREDICTED_TICKS),
            objects: vec![PlayerState::default(); MAX_CLIENTS],
            num_objects: 0,
            tick_number: 0,
            target_tick_number: 0,
            send_buffer: PacketBuffer::new(),
            recv_buffer: PacketBuffer::new(),
            vertices,
            frame_buffer: vec![0; WINDOW_WIDTH * WINDOW_HEIGHT],
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let server_address = "127.0.0.1:9999";

    let context = Arc::new(RwLock::new(ClientContext::new()));

    let mut buffer = [0u8; SOCKET_BUFFER_SIZE];
    buffer[0] = ClientMessage::Join as u8;

    {
        let mut ctx = context.write().await;
        ctx.send_buffer.push(&buffer[..1], server_address);
    }

    tokio::spawn(network_handler(socket.clone(), context.clone()));
    game_loop(socket, context).await?;
    Ok(())
}

async fn network_handler(socket: Arc<UdpSocket>, context: Arc<RwLock<ClientContext>>) {
    let mut buffer = vec![0u8; SOCKET_BUFFER_SIZE];
    loop {
        if let Ok((bytes_received, _)) = socket.recv_from(&mut buffer).await {
            let mut ctx = context.write().await;
            ctx.recv_buffer
                .push(&buffer[..bytes_received], "127.0.0.1:9999");
        }
    }
}

async fn game_loop(
    socket: Arc<UdpSocket>,
    context: Arc<RwLock<ClientContext>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut window = Window::new(
        "Fps client",
        WINDOW_WIDTH,
        WINDOW_HEIGHT,
        WindowOptions {
            resize: false,
            ..WindowOptions::default()
        },
    )?;

    window.limit_update_rate(Some(std::time::Duration::from_secs_f32(SECONDS_PER_TICK)));

    let mut tick_interval = interval(Duration::from_secs_f32(SECONDS_PER_TICK));
    let server_addr = "127.0.0.1:9999";

    while window.is_open() && !window.is_key_down(Key::Escape) {
        tokio::select! {
            _ = tick_interval.tick() => {
                let mut ctx = context.write().await; // Hold write lock throughout
                let tick_start = Instant::now();

                // Handle input
                let input = PlayerInput {
                    up: window.is_key_down(Key::W),
                    down: window.is_key_down(Key::S),
                    left: window.is_key_down(Key::A),
                    right: window.is_key_down(Key::D),
                };
                let mut buffer = [0u8; 8];
                if let Some(client_id) = ctx.client_id {
                    buffer[0] = ClientMessage::Input as u8;
                    buffer[1..3].copy_from_slice(&client_id.to_le_bytes());
                    buffer[3..7].copy_from_slice(&ctx.target_tick_number.to_le_bytes());
                    buffer[7] = (input.up as u8) | ((input.down as u8) << 1) | ((input.left as u8) << 2) | ((input.right as u8) << 3);
                    ctx.send_buffer.push(&buffer[..8], server_addr);
                }

                // Process received packets
                while let Some((data, _)) = ctx.recv_buffer.pop() {
                    if data.len() > 1 {
                        match data[0] {
                            x if x == ServerMessage::JoinResult as u8 && data.len() >= 4 => {
                                if data[1] == 1 {
                                    ctx.client_id = Some(u16::from_le_bytes(data[2..4].try_into().unwrap()));
                                    println!("Joined with ID {}", ctx.client_id.unwrap());
                                }
                            }
                            x if x == ServerMessage::State as u8 => {
                                let received_tick = u32::from_le_bytes(data[1..5].try_into().unwrap());
                                let mut offset = 5;
                                ctx.local_player = PlayerState {
                                    x: f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()),
                                    y: f32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()),
                                    facing: f32::from_le_bytes(data[offset + 8..offset + 12].try_into().unwrap()),
                                    speed: f32::from_le_bytes(data[offset + 12..offset + 16].try_into().unwrap()),
                                };
                                offset += 16;
                                ctx.num_objects = 0;
                                while offset + 14 <= data.len() {
                                    let id = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
                                    offset += 2;
                                    if id < MAX_CLIENTS { // Validate id
                                        ctx.objects[id].x = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                                        offset += 4;
                                        ctx.objects[id].y = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                                        offset += 4;
                                        ctx.objects[id].facing = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                                        offset += 4;
                                        if id != ctx.client_id.unwrap_or(0) as usize { ctx.num_objects += 1; }
                                    } else {
                                        // Skip invalid id and advance offset
                                        offset += 12; // 4 (x) + 4 (y) + 4 (facing)
                                        println!("Warning: Received invalid client ID {} (max: {})", id, MAX_CLIENTS - 1);
                                    }
                                }

                                let mut history_size = ctx.prediction_history.size();
                                let mut head_tick = ctx.tick_number - history_size as u32;
                                while history_size > 0 && head_tick < received_tick {
                                    ctx.prediction_history.pop();
                                    head_tick += 1;
                                    history_size -= 1;
                                }
                                if history_size > 0 && head_tick == received_tick {
                                    let predicted = ctx.prediction_history.items[ctx.prediction_history.head].0.clone();
                                    let dx = predicted.x - ctx.local_player.x;
                                    let dy = predicted.y - ctx.local_player.y;
                                    let error_sq = dx * dx + dy * dy;
                                    if error_sq > 0.01 * 0.01 {
                                        println!("Misprediction at tick {}: error {}", received_tick, error_sq.sqrt());
                                        let mut temp_player = ctx.local_player.clone();
                                        let mut i = ctx.prediction_history.head;
                                        while i != ctx.prediction_history.tail {
                                            let input = ctx.prediction_history.items[i].1.clone();
                                            tick_player(&mut temp_player, &input);
                                            ctx.prediction_history.items[i] = (temp_player.clone(), input);
                                            i = (i + 1) % MAX_PREDICTED_TICKS;
                                        }
                                        ctx.local_player = temp_player;
                                    }
                                }

                                let ticks_to_predict = (0.4 / SECONDS_PER_TICK) as u32 + 2;
                                ctx.target_tick_number = received_tick + ticks_to_predict;
                            }
                            _ => {}
                        }
                    }
                }
                // Predict
                for _ in ctx.tick_number..ctx.target_tick_number {
                    let local_player = ctx.local_player.clone();
                    ctx.prediction_history.push((local_player, input.clone()));
                    tick_player(&mut ctx.local_player, &input);
                    ctx.tick_number += 1;
                }

                // Update vertices
                const SIZE: f32 = 0.05;
                const SCALE: f32 = 0.01;
                let mut vert_idx = 0;
                for i in 0..MAX_CLIENTS {
                    let state = if i == ctx.client_id.unwrap_or(0) as usize { &ctx.local_player } else { &ctx.objects[i] };
                    if i == ctx.client_id.unwrap_or(0) as usize || (i < ctx.num_objects || ctx.objects[i].x != 0.0) {
                        let x = state.x * SCALE;
                        let y = state.y * -SCALE;
                        // Immutable borrow of `state` ends here
                        ctx.vertices[vert_idx].x = x - SIZE; ctx.vertices[vert_idx].y = y - SIZE;
                        ctx.vertices[vert_idx + 1].x = x - SIZE; ctx.vertices[vert_idx + 1].y = y + SIZE;
                        ctx.vertices[vert_idx + 2].x = x + SIZE; ctx.vertices[vert_idx + 2].y = y + SIZE;
                        ctx.vertices[vert_idx + 3].x = x + SIZE; ctx.vertices[vert_idx + 3].y = y - SIZE;
                        vert_idx += 4;
                    }
                }                while vert_idx < ctx.vertices.len() {
                    ctx.vertices[vert_idx].x = 0.0;
                    ctx.vertices[vert_idx].y = 0.0;
                    vert_idx += 1;
                }

                // Render (still under write lock)
                ctx.frame_buffer.fill(0);
                for i in 0..ctx.vertices.len() / 4 {
                    let verts = &ctx.vertices[i * 4..(i + 1) * 4];
                    // Compute bounds first, ending the immutable borrow
                    let min_x = (verts.iter().map(|v| v.x).fold(f32::INFINITY, f32::min) * WINDOW_WIDTH as f32 + WINDOW_WIDTH as f32 / 2.0) as isize;
                    let max_x = (verts.iter().map(|v| v.x).fold(f32::NEG_INFINITY, f32::max) * WINDOW_WIDTH as f32 + WINDOW_WIDTH as f32 / 2.0) as isize;
                    let min_y = (verts.iter().map(|v| v.y).fold(f32::INFINITY, f32::min) * WINDOW_HEIGHT as f32 + WINDOW_HEIGHT as f32 / 2.0) as isize;
                    let max_y = (verts.iter().map(|v| v.y).fold(f32::NEG_INFINITY, f32::max) * WINDOW_HEIGHT as f32 + WINDOW_HEIGHT as f32 / 2.0) as isize;
                    let color = verts[0].color; // Store color before borrow ends
                                                // Immutable borrow of `verts` ends here
                    for y in min_y.max(0)..max_y.min(WINDOW_HEIGHT as isize) {
                        for x in min_x.max(0)..max_x.min(WINDOW_WIDTH as isize) {
                            ctx.frame_buffer[y as usize * WINDOW_WIDTH + x as usize] = color;
                        }
                    }
                }
                window.update_with_buffer(&ctx.frame_buffer, WINDOW_WIDTH, WINDOW_HEIGHT)?;
                // Send packets
                while let Some((data, addr)) = ctx.send_buffer.pop() {
                    socket.send_to(&data, &addr).await?;
                }

                let elapsed = tick_start.elapsed();
                if elapsed < Duration::from_secs_f32(SECONDS_PER_TICK) {
                    tokio::time::sleep(Duration::from_secs_f32(SECONDS_PER_TICK) - elapsed).await;
                }
            }
        }
    }

    // Leave
    let mut ctx = context.write().await;
    if let Some(client_id) = ctx.client_id {
        let mut buffer = [0u8; 3];
        buffer[0] = ClientMessage::Leave as u8;
        buffer[1..3].copy_from_slice(&client_id.to_le_bytes());
        ctx.send_buffer.push(&buffer[..3], server_addr);
        while let Some((data, addr)) = ctx.send_buffer.pop() {
            socket.send_to(&data, &addr).await?;
        }
    }

    Ok(())
}
