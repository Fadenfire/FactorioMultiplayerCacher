use crate::chunk_cache::ChunkCache;
use crate::proxy::{client_proxy, server_proxy};
use anyhow::Context;
use argh::FromArgs;
use log::{error, info};
use quinn::Endpoint;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{lookup_host, UdpSocket};
use tokio::select;

mod chunker;
mod factorio_protocol;
mod utils;
mod proxy;
mod quic;
mod protocol;
mod zip_writer;
mod dedup;
mod chunk_cache;
mod rev_crc;

#[derive(FromArgs)]
/// Factorio cacher
struct Args {
	#[argh(subcommand)]
    subcommand: Subcommand,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Subcommand {
	Client(ClientArgs),
	Server(ServerArgs),
}

#[derive(FromArgs)]
/// Run the client
#[argh(subcommand, name = "client")]
struct ClientArgs {
	#[argh(option, short = 'p', default = "60120")]
	/// port that factorio clients use to connect, defaults to 60120
	port: u16,
	
	#[argh(option, short = 'h', default = "IpAddr::V4(Ipv4Addr::UNSPECIFIED)")]
	/// host that factorio clients use to connect, defaults to 0.0.0.0
	host: IpAddr,
	
	#[argh(positional)]
	/// factorio-cacher server address in host:port form
	server_address: String,
	
	#[argh(option, short = 'c')]
	/// location of cache file, defaults to 'persistent-cache' in the CWD
	cache_path: Option<PathBuf>,
	
	#[argh(option, default = "500_000_000")]
	/// max size of the chunk cache, defaults to 500MB
	cache_limit: u64,
	
	#[argh(option, default = "60")]
	/// how often to try to save the cache in seconds, defaults to 60s
	cache_save_interval: u64,
}

#[derive(FromArgs)]
/// Run the server
#[argh(subcommand, name = "server")]
struct ServerArgs {
	#[argh(option, short = 'p', default = "60130")]
	/// port that factorio-cacher clients use to connect, defaults to 60130
	port: u16,
	
	#[argh(option, short = 'h', default = "IpAddr::V4(Ipv4Addr::UNSPECIFIED)")]
	/// host that factorio-cacher clients use to connect, defaults to 0.0.0.0
	host: IpAddr,
	
	#[argh(positional)]
	/// factorio server address in host:port form
	factorio_address: String,
}

#[tokio::main()]
async fn main() {
	let args: Args = argh::from_env();
	
	setup_logging();
	
	match args.subcommand {
		Subcommand::Client(client_args) => subcommand_client(client_args).await,
		Subcommand::Server(server_args) => subcommand_server(server_args).await,
	}
}

async fn subcommand_client(args: ClientArgs) {
	let server_address = lookup_host(args.server_address.as_str()).await
		.expect("Error looking up host")
		.next()
		.expect("No server address found");
	
	let local_address = SocketAddr::new(if server_address.is_ipv6() {
		Ipv6Addr::UNSPECIFIED.into()
	} else {
		Ipv4Addr::UNSPECIFIED.into()
	}, 0);
	
	let mut endpoint = Endpoint::client(local_address).unwrap();
	endpoint.set_default_client_config(quic::make_client_config());
	
	select! {
		result = run_client(&endpoint, server_address, &args) => result.unwrap(),
		_ = tokio::signal::ctrl_c() => {}
	}
	
	endpoint.close(0u32.into(), b"quit");
	
	select! {
		_ = endpoint.wait_idle() => {},
		_ = tokio::signal::ctrl_c() => {}
	}
	
	info!("Shutdown");
}

async fn run_client(endpoint: &Endpoint, server_address: SocketAddr, args: &ClientArgs) -> anyhow::Result<()> {
	let cache_path = args.cache_path.clone()
		.unwrap_or_else(|| std::path::absolute("persistent-cache").unwrap());
	
	info!("Connecting...");
	
	let quic_connection = Arc::new(endpoint.connect(server_address, "localhost")?.await.context("QUIC connecting")?);
	
	let listen_address = SocketAddr::new(args.host, args.port);
	let socket = Arc::new(UdpSocket::bind(listen_address).await?);
	
	info!("Connected");
	
	let chunk_cache;
	
	if cache_path.exists() {
		info!("Loading cache from {}", cache_path.display());
		
		let compressed_size = tokio::fs::metadata(&cache_path).await?.len();
		chunk_cache = Arc::new(ChunkCache::load_from_file(args.cache_limit, cache_path.clone()).await?);
		
		info!(
			"Loaded {} chunks ({}B, {}B compressed) from the cache",
			chunk_cache.len(),
			utils::abbreviate_number(chunk_cache.total_size()),
			utils::abbreviate_number(compressed_size)
		);
	} else {
		chunk_cache = Arc::new(ChunkCache::new(args.cache_limit));
	}
	
	info!("The cache has a limit of {}B", utils::abbreviate_number(args.cache_limit));
	
	chunk_cache.start_writer(cache_path, Duration::from_secs(args.cache_save_interval));
	
	info!("Listening on {}", listen_address);
	
	client_proxy::run_client_proxy(socket.clone(), quic_connection.clone(), chunk_cache.clone()).await?;
	
	Ok(())
}

async fn subcommand_server(args: ServerArgs) {
	let factorio_address = lookup_host(args.factorio_address.as_str()).await
		.expect("Error looking up host")
		.next()
		.expect("No server address found");
	
	let listen_address = SocketAddr::new(args.host, args.port);
	let endpoint = Endpoint::server(quic::make_server_config(), listen_address).unwrap();
	
	select! {
		result = run_server(&endpoint, factorio_address) => result.unwrap(),
		_ = tokio::signal::ctrl_c() => {}
	}
	
	endpoint.close(0u32.into(), b"quit");
	
	select! {
		_ = endpoint.wait_idle() => {},
		_ = tokio::signal::ctrl_c() => {}
	}
	
	info!("Shutdown");
}

async fn run_server(endpoint: &Endpoint, factorio_address: SocketAddr) -> anyhow::Result<()> {
	info!("Started");
	
	loop {
		let connection = endpoint.accept().await.unwrap().await?;
		
		tokio::spawn(async move {
			let client_address = connection.remote_address();
			
			info!("Client from {:?} connected", client_address);
			
			if let Err(err) = server_proxy::run_server_proxy(Arc::new(connection), factorio_address).await {
				error!("Error running server: {:?}", err);
			}
			
			info!("Client from {:?} disconnected", client_address);
		});
	}
}

fn setup_logging() {
	use simplelog::*;
	
	let config = ConfigBuilder::new()
		.set_time_format_custom(format_description!("[[[hour repr:12]:[minute]:[second] [period]]"))
		.set_time_offset_to_local().unwrap()
		.build();
	
	TermLogger::init(LevelFilter::Info, config, TerminalMode::Stdout, ColorChoice::Auto).expect("Unable to init logger");
}
