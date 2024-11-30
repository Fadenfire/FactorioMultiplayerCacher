use crate::dedup::{ChunkKey, FactorioWorldDescription};
use crate::factorio_protocol::{FactorioPacket, FactorioPacketHeader, MapReadyForDownloadData, PacketType, ServerToClientHeartbeatPacket, TransferBlockPacket, TransferBlockRequestPacket, TRANSFER_BLOCK_SIZE};
use crate::protocol::{Datagram, RequestChunksMessage, SendChunksMessage, WorldReadyMessage, UDP_PEER_IDLE_TIMEOUT};
use crate::proxy::{PacketDirection, UDP_QUEUE_SIZE};
use crate::{dedup, protocol, utils};
use bytes::{Bytes, BytesMut};
use log::{error, info};
use quinn_proto::VarInt;
use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::UdpSocket;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub async fn run_server_proxy(
	connection: Arc<quinn::Connection>,
	factorio_addr: SocketAddr,
) -> anyhow::Result<()> {
	let mut outgoing_queues: HashMap<VarInt, mpsc::Sender<Bytes>> = HashMap::new();
	
	loop {
		select! {
            result = connection.read_datagram() => {
                let datagram = Datagram::decode(result?)?;

                if let Some(outgoing_queue) = outgoing_queues.get(&datagram.peer_id) {
                    let _ = outgoing_queue.try_send(datagram.data);
                }
            }
            result = connection.accept_bi() => {
                let (send_stream, mut recv_stream) = result?;
                let peer_id: VarInt = recv_stream.read_u32_le().await?.into();

                let localhost: IpAddr = if factorio_addr.is_ipv6() {
                    Ipv6Addr::LOCALHOST.into()
                } else {
                    Ipv4Addr::LOCALHOST.into()
                };

                let socket = UdpSocket::bind((localhost, 0)).await?;
				
                let (receive_queue_tx, receive_queue_rx) = mpsc::channel(UDP_QUEUE_SIZE);

                tokio::spawn(proxy_server(ProxyServerArgs {
                    connection: connection.clone(),
                    peer_id,

                    socket,
                    factorio_addr,

                    receive_queue_rx,

                    comp_stream: (send_stream, recv_stream),
                }));

                outgoing_queues.insert(peer_id, receive_queue_tx);
            }
        }
	}
}

struct ProxyServerArgs {
	connection: Arc<quinn::Connection>,
	peer_id: VarInt,
	
	socket: UdpSocket,
	factorio_addr: SocketAddr,
	
	receive_queue_rx: mpsc::Receiver<Bytes>,
	
	comp_stream: (quinn::SendStream, quinn::RecvStream),
}

async fn proxy_server(mut args: ProxyServerArgs) {
	let mut buf = BytesMut::new();
	let mut out_packets = Vec::new();
	
	let mut proxy_state = ServerProxyState::new(args.comp_stream);
	
	loop {
		buf.clear();
		buf.reserve(8192);
		
		select! {
            result = args.socket.recv_buf_from(&mut buf) => {
                let Ok((_, remote_addr)) = result else { return };

                // Drop any packets that don't originate from the server
                if remote_addr != args.factorio_addr { continue; }

                proxy_state.on_packet_from_server(buf.split().freeze(), &mut out_packets).await;
            }
            result = args.receive_queue_rx.recv() => {
                let Some(packet_data) = result else { return; };

                out_packets.push((packet_data, PacketDirection::ToServer));
            }
            _ = tokio::time::sleep(UDP_PEER_IDLE_TIMEOUT) => return
        }
		
		for (packet_data, dir) in out_packets.drain(..) {
			match dir {
				PacketDirection::ToClient => {
					Datagram::new(args.peer_id, packet_data).encode(&mut buf);
					
					if args.connection.send_datagram(buf.split().freeze()).is_err() {
						return;
					}
				}
				PacketDirection::ToServer => {
					if args.socket.send_to(&packet_data, args.factorio_addr).await.is_err() {
						return;
					}
				}
			}
		}
	}
}

struct ServerProxyState {
	phase: ServerProxyPhase,
	comp_stream: Option<(quinn::SendStream, quinn::RecvStream)>,
}

enum ServerProxyPhase {
	WaitingForWorld,
	DownloadingWorld(DownloadingWorldState),
	Done,
}

struct DownloadingWorldState {
	world_info: MapReadyForDownloadData,
	world_block_count: u32,
	download_start_time: Instant,
	
	held_packets: Vec<Bytes>,
	received_blocks: Vec<TransferBlockPacket>,
	block_request_queue: BTreeSet<u32>,
	inflight_block_requests: BTreeSet<u32>,
	last_block_time: Instant,
}

impl ServerProxyState {
	const INFLIGHT_BLOCK_REQUEST_LIMIT: usize = 16;
	
	pub fn new(comp_stream: (quinn::SendStream, quinn::RecvStream)) -> Self {
		Self {
			phase: ServerProxyPhase::WaitingForWorld,
			comp_stream: Some(comp_stream),
		}
	}
	
	pub async fn on_packet_from_server(
		&mut self,
		in_packet_data: Bytes,
		out_packets: &mut Vec<(Bytes, PacketDirection)>,
	) {
		match &mut self.phase {
			ServerProxyPhase::WaitingForWorld => {
				if let Ok((header, msg_data)) =
					FactorioPacketHeader::decode(in_packet_data.clone())
				{
					if header.packet_type == PacketType::ServerToClientHeartbeat {
						let result = ServerToClientHeartbeatPacket::decode(msg_data)
							.and_then(ServerToClientHeartbeatPacket::try_decode_map_ready);
						
						if let Ok(Some(world_info)) = result {
							self.transition_to_downloading_world(in_packet_data, world_info, out_packets);
							return;
						}
					}
				}
			}
			ServerProxyPhase::DownloadingWorld(state) => {
				if let Ok((header, msg_data)) =
					FactorioPacketHeader::decode(in_packet_data.clone())
				{
					if header.packet_type == PacketType::TransferBlock {
						let Ok(transfer_block) = TransferBlockPacket::decode(msg_data) else { return; };
						
						if state.inflight_block_requests.remove(&transfer_block.block_id) ||
							state.block_request_queue.remove(&transfer_block.block_id)
						{
							state.received_blocks.push(transfer_block);
							
							state.last_block_time = Instant::now();
						}
						
						if state.block_request_queue.is_empty() && state.inflight_block_requests.is_empty() {
							self.finalize_world(out_packets).await;
							return;
						} else {
							Self::request_next_blocks(state, out_packets);
						}
					} else {
						state.held_packets.push(in_packet_data);
					}
				}
				
				if state.last_block_time.elapsed() > Duration::from_millis(100) {
					for &block_id in &state.inflight_block_requests {
						let request = TransferBlockRequestPacket { block_id };
						out_packets.push((request.encode_full_packet(), PacketDirection::ToServer));
					}
					
					Self::request_next_blocks(state, out_packets);
				}
				
				return;
			}
			ServerProxyPhase::Done => {}
		}
		
		out_packets.push((in_packet_data, PacketDirection::ToClient));
	}
	
	fn transition_to_downloading_world(
		&mut self,
		in_packet_data: Bytes,
		world_info: MapReadyForDownloadData,
		out_packets: &mut Vec<(Bytes, PacketDirection)>,
	) {
		info!("Got world info: {:?}", world_info);
		info!("Downloading world from server");
		
		let world_block_count = (world_info.world_size + TRANSFER_BLOCK_SIZE - 1) / TRANSFER_BLOCK_SIZE;
		let aux_block_count = (world_info.aux_size + TRANSFER_BLOCK_SIZE - 1) / TRANSFER_BLOCK_SIZE;
		
		let total_block_count = world_block_count + aux_block_count;
		
		let mut state = DownloadingWorldState {
			world_info,
			world_block_count,
			download_start_time: Instant::now(),
			
			held_packets: vec![in_packet_data],
			received_blocks: Vec::new(),
			block_request_queue: BTreeSet::from_iter(0..total_block_count),
			inflight_block_requests: BTreeSet::new(),
			last_block_time: Instant::now(),
		};
		
		Self::request_next_blocks(&mut state, out_packets);
		
		self.phase = ServerProxyPhase::DownloadingWorld(state);
	}
	
	fn request_next_blocks(state: &mut DownloadingWorldState, out_packets: &mut Vec<(Bytes, PacketDirection)>) {
		while state.inflight_block_requests.len() < Self::INFLIGHT_BLOCK_REQUEST_LIMIT {
			let Some(block_id) = state.block_request_queue.pop_first() else { return; };
			state.inflight_block_requests.insert(block_id);
			
			let request = TransferBlockRequestPacket { block_id };
			out_packets.push((request.encode_full_packet(), PacketDirection::ToServer));
		}
	}
	
	async fn finalize_world(&mut self, out_packets: &mut Vec<(Bytes, PacketDirection)>) {
		let state = match &mut self.phase {
			ServerProxyPhase::DownloadingWorld(state) => state,
			_ => unreachable!(),
		};
		
		info!("Downloading world took {}ms", state.download_start_time.elapsed().as_millis());
		
		state.received_blocks.sort_by_key(|block| block.block_id);
		
		let mut received_data = BytesMut::new();
		
		for block in state.received_blocks.drain(..) {
			received_data.extend_from_slice(&block.data);
		}
		
		let received_data = received_data.freeze();
		
		let aux_data_offset = state.world_block_count * TRANSFER_BLOCK_SIZE;
		
		if received_data.len() < (aux_data_offset as usize + state.world_info.aux_size as usize) {
			error!("Received data length is smaller than expected length, received length: {}", received_data.len());
			
			self.phase = ServerProxyPhase::Done;
			return;
		}
		
		let world_data = received_data.slice(..state.world_info.world_size as usize);
		let aux_data = received_data.slice(aux_data_offset as usize..(aux_data_offset + state.world_info.aux_size) as usize);
		
		let start_time = Instant::now();
		
		let result: anyhow::Result<_> = async {
			tokio::task::spawn_blocking(move || dedup::deconstruct_world(&world_data, &aux_data)).await?
		}.await;
		
		info!("Deconstructing world took {}ms", start_time.elapsed().as_millis());
		
		let (world_description, chunks) = match result {
			Ok(result) => result,
			Err(err) => {
				error!("Error trying to deconstruct world: {:?}", err);
				
				self.phase = ServerProxyPhase::Done;
				return;
			}
		};
		
		let new_world_info = MapReadyForDownloadData {
			world_size: world_description.world_size,
			world_crc: world_description.reconstructed_crc,
			..state.world_info
		};
		
		info!("Reconstructed world info: {:?}", new_world_info);
		
		let comp_stream = self.comp_stream.take().unwrap();
		
		tokio::spawn(async move {
			if let Err(err) = transfer_world_data(comp_stream.0, comp_stream.1, world_description, chunks).await {
				error!("Error trying to transfer world data: {:?}", err);
			}
		});
		
		let mut old_world_info_encoded = Vec::new();
		let mut new_world_info_encoded = Vec::new();
		
		state.world_info.encode(&mut old_world_info_encoded);
		new_world_info.encode(&mut new_world_info_encoded);
		
		// TODO: Apply this replacement to all packets after this point
		for mut held_packet_data in state.held_packets.drain(..) {
			if let Some(pos) = held_packet_data.windows(old_world_info_encoded.len()).position(|w| w == old_world_info_encoded) {
				let mut new_packet_data = BytesMut::from(held_packet_data);
				new_packet_data[pos..pos + old_world_info_encoded.len()].copy_from_slice(&new_world_info_encoded);
				
				held_packet_data = new_packet_data.freeze();
			}
			
			out_packets.push((held_packet_data, PacketDirection::ToClient));
		}
		
		self.phase = ServerProxyPhase::Done;
	}
}

async fn transfer_world_data(
	mut send_stream: quinn::SendStream,
	mut recv_stream: quinn::RecvStream,
	world_description: FactorioWorldDescription,
	chunks: HashMap<ChunkKey, Bytes>,
) -> anyhow::Result<()> {
	info!("Transferring world data");
	
	let original_world_size = world_description.original_world_size as u64;
	let mut total_transferred = 0;
	let start_time = Instant::now();
	
	let world_ready_message = protocol::encode_message_async(WorldReadyMessage {
		world: world_description,
	}).await?;
	
	total_transferred += world_ready_message.len() as u64;
	info!("Sending world description, size: {}B", utils::abbreviate_number(world_ready_message.len() as u64));
	
	protocol::write_message(&mut send_stream, world_ready_message).await?;
	
	let mut buf = BytesMut::new();
	
	while let Ok(request_data) = protocol::read_message(&mut recv_stream, &mut buf).await {
		let request: RequestChunksMessage = protocol::decode_message_async(request_data).await?;
		
		let response = SendChunksMessage {
			chunks: request.requested_chunks.iter()
				.map(|&key| chunks.get(&key).expect("Client requested chunk that we don't have").clone())
				.collect()
		};
		
		let response_data = protocol::encode_message_async(response).await?;
		total_transferred += response_data.len() as u64;
		
		info!("Sending batch of {} chunks, size: {}B",
			request.requested_chunks.len(),
			utils::abbreviate_number(response_data.len() as u64)
		);
		
		protocol::write_message(&mut send_stream, response_data).await?;
	}
	
	let elapsed = start_time.elapsed();
	
	info!("Finished sending world in {}s, total transferred: {}B, original size: {}B, dedup ratio: {:.2}%, avg rate: {}B/s",
		elapsed.as_secs(),
		utils::abbreviate_number(total_transferred),
		utils::abbreviate_number(original_world_size),
		(total_transferred as f64 / original_world_size as f64) * 100.0,
		utils::abbreviate_number((total_transferred as f64 / elapsed.as_millis() as f64 * 1000.0) as u64),
	);
	
	Ok(())
}