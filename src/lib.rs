use postcard;
use quinn;
use rustls;
use serde;
use tokio;

use blimp_onboard_software;

pub enum CommChannel {
    Server(core::net::SocketAddr),
    Client(core::net::SocketAddr),
}

#[derive(Debug)]
enum ChannelStatus {
    Unknown(std::time::Instant),
    Checking,
    Connected,
    Listening,
}

struct ChannelInstance {
    connection: Option<quinn::Connection>,
    status: ChannelStatus,
    tx_chan: tokio::sync::broadcast::Sender<ChannelMsg>,
    rx_chan: tokio::sync::broadcast::Receiver<ChannelMsg>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub enum ChannelMsg {
    Handshake,
    Heartbeat,
    MessageG2B(blimp_onboard_software::obsw_algo::MessageG2B),
    MessageB2G(blimp_onboard_software::obsw_algo::MessageB2G),
}

// https://github.com/quinn-rs/quinn/blob/31a0440009afd5a7e29101410aa9d3da2d1f8077/quinn/examples/common/mod.rs#L73
pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];

async fn channel_quic_worker(
    channel_id: usize,
    chans_instances: std::sync::Arc<
        tokio::sync::RwLock<Vec<std::sync::Arc<tokio::sync::Mutex<ChannelInstance>>>>,
    >,
    channels: std::sync::Arc<Vec<CommChannel>>,
    tx_chan_rx: tokio::sync::broadcast::Receiver<ChannelMsg>,
    rx_chan_tx: tokio::sync::broadcast::Sender<ChannelMsg>,
) {
    println!("Starting channel {} worker task...", channel_id);

    let this_channel_inst = chans_instances.read().await[channel_id].clone();
    let this_channel_inst_locked = this_channel_inst.lock().await;
    let conn = this_channel_inst_locked
        .connection
        .as_ref()
        .expect("Started channel_quic_worker for a channel without connection!");
    let channel = &channels[channel_id];

    async fn stream_worker(
        mut stream_tx: quinn::SendStream,
        mut stream_rx: quinn::RecvStream,
        mut tx_chan_rx: tokio::sync::broadcast::Receiver<ChannelMsg>,
        rx_chan_tx: tokio::sync::broadcast::Sender<ChannelMsg>,
    ) {
        println!("Starting stream worker task...");

        let (shutdown_sender_tx, mut shutdown_sender_rx) = tokio::sync::mpsc::channel::<()>(2);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = tx_chan_rx.recv() => {
                        if let Ok(msg) = msg {
                            let msg_ser = postcard::to_stdvec(&msg).unwrap();
                            let len_val_bytes = (msg_ser.len() as u32).to_le_bytes();
                            if matches!(
                                stream_tx.write_all(&len_val_bytes).await,
                                Err(quinn::WriteError::ConnectionLost(_)) | Err(quinn::WriteError::ClosedStream)) {
                                break;
                            }
                            if matches!(
                                stream_tx.write_all(&msg_ser).await,
                                Err(quinn::WriteError::ConnectionLost(_)) | Err(quinn::WriteError::ClosedStream)) {
                                break;
                            }
                        }
                    }
                    _ = shutdown_sender_rx.recv() => {
                        break;
                    }
                };
            }
        });

        let mut frame_buf: [u8; 1024] = [0; 1024];
        let mut frame_recv_len: u32 = 0;
        let mut frame_expected_len: Option<u32> = None;
        loop {
            let msg = stream_rx
                .read(&mut frame_buf[(frame_recv_len as usize)..])
                .await;
            match msg {
                Ok(Some(recv_size)) => {
                    frame_recv_len += recv_size as u32;

                    match frame_expected_len {
                        None if frame_recv_len >= 4 => {
                            let mut frame_len_value_bytes: [u8; 4] = [0; 4];
                            frame_len_value_bytes.copy_from_slice(&frame_buf[0..4]);
                            frame_expected_len = Some(u32::from_le_bytes(frame_len_value_bytes));
                        }
                        _ => {}
                    }
                    if let Some(exp_len) = frame_expected_len {
                        if frame_recv_len >= exp_len + 4 {
                            let frame_slice = &frame_buf[4..(4 + exp_len as usize)];
                            if let Ok(msg_des) = postcard::from_bytes::<ChannelMsg>(frame_slice) {
                                println!("Got message: {:?}", msg_des);
                                rx_chan_tx.send(msg_des).unwrap();
                            } else {
                                eprintln!(
                                    "Couldn't deserialize message received from channel stream!"
                                );
                            }
                        }
                    }
                }
                Ok(None) => {
                    shutdown_sender_tx.send(()).await.unwrap();
                    break;
                }
                Err(err) => {
                    eprintln!("Couldn't receive data from channel stream");
                }
            }
        }
    }

    match channel {
        CommChannel::Server(socket_addr) => {
            println!("Awaiting channel...");
            if let Ok((stream_tx, stream_rx)) = conn.accept_bi().await {
                println!("Opened stream on channel {}", channel_id);
                stream_worker(stream_tx, stream_rx, tx_chan_rx, rx_chan_tx).await;
            } else {
                eprintln!(
                    "Error occurred when opening stream on channel {}",
                    channel_id
                );
            }
        }
        CommChannel::Client(socket_addr) => {
            println!("Opening channel...");
            if let Ok((mut stream_tx, stream_rx)) = conn.open_bi().await {
                stream_tx
                    .write(&postcard::to_stdvec::<ChannelMsg>(&ChannelMsg::Handshake).unwrap())
                    .await
                    .unwrap();
                println!("Opened stream on channel {}", channel_id);
                stream_worker(stream_tx, stream_rx, tx_chan_rx, rx_chan_tx).await;
            } else {
                eprintln!(
                    "Error occurred when opening stream on channel {}",
                    channel_id
                );
            }
        }
    }
}

pub async fn start_communication(channels: Vec<CommChannel>) {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();
    let chans_instances = std::sync::Arc::new(tokio::sync::RwLock::new(Vec::<
        std::sync::Arc<tokio::sync::Mutex<ChannelInstance>>,
    >::new()));
    let channels = std::sync::Arc::new(channels);

    for (i, ch) in channels.iter().enumerate() {
        println!("Beginning initializing channel {}", i);

        let (tx_chan, tx_chan_rx) = tokio::sync::broadcast::channel::<ChannelMsg>(256);
        let (rx_chan_tx, rx_chan) = tokio::sync::broadcast::channel::<ChannelMsg>(256);

        chans_instances
            .write()
            .await
            .push(std::sync::Arc::new(tokio::sync::Mutex::new(
                ChannelInstance {
                    connection: None,
                    status: ChannelStatus::Unknown(std::time::Instant::now()),
                    tx_chan,
                    rx_chan,
                },
            )));

        let chans_instances = chans_instances.clone();
        let channels = channels.clone();
        tokio::spawn(async move {
            println!("Started channel {} monitor task", i);

            let inst_arc = {
                let chans_instances_locked = chans_instances.read().await;
                chans_instances_locked[i].clone()
            };
            let mut inst = inst_arc.lock().await;
            match channels[i] {
                CommChannel::Server(socket_addr) => loop {
                    //println!("Channel {} status: {:?}", i, inst.status);
                    match inst.status {
                        ChannelStatus::Unknown(time) => {
                            if time.elapsed() > std::time::Duration::from_secs(2) {
                                println!("Listening on channel {}", i);
                                inst.status = ChannelStatus::Listening;

                                let cert_path = std::path::Path::new("./cert.der");
                                let key_path = std::path::Path::new("./key.der");

                                let (cert, key) = match std::fs::read(&cert_path)
                                    .and_then(|x| std::fs::read(&key_path).and_then(|y| Ok((x, y))))
                                {
                                    Ok((cert, key)) => (
                                        rustls::pki_types::CertificateDer::from(cert),
                                        rustls::pki_types::PrivateKeyDer::try_from(key)
                                            .expect("Couldn't get key"),
                                    ),
                                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                                        println!("Generating self-signed certificate");
                                        let cert = rcgen::generate_simple_self_signed(
                                            vec!["localhost", "127.0.0.1", "blimp_onboard"]
                                                .iter()
                                                .map(|&x| x.to_string())
                                                .collect::<Vec<String>>(),
                                        )
                                        .unwrap();
                                        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(
                                            cert.key_pair.serialize_der(),
                                        );
                                        let cert = cert.cert.into();

                                        std::fs::write(&cert_path, &cert).expect(
                                            "Couldn't write self-signed certificate to file",
                                        );
                                        std::fs::write(&key_path, key.secret_pkcs8_der())
                                            .expect("couldn't write self-signed key to file");

                                        (cert, key.into())
                                    }
                                    Err(err) => panic!("Couldn't read certificate"),
                                };

                                let mut server_crypto = rustls::ServerConfig::builder()
                                    .with_no_client_auth()
                                    .with_single_cert(vec![cert], key)
                                    .expect("Couldn't create QUIC server config");
                                server_crypto.alpn_protocols =
                                    ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();
                                let server_config =
                                    quinn::ServerConfig::with_crypto(std::sync::Arc::new(
                                        quinn::crypto::rustls::QuicServerConfig::try_from(
                                            server_crypto,
                                        )
                                        .expect("Couldn't create server configuration"),
                                    ));
                                let endpoint = quinn::Endpoint::server(server_config, socket_addr)
                                    .expect("Couldn't open QUIC server endpoint");

                                println!("Awaiting for incoming QUIC client...");
                                let conn = endpoint
                                    .accept()
                                    .await
                                    .expect("Couldn't accept connection")
                                    .await
                                    .expect("Couldn't accept connection");
                                inst.connection = Some(conn);
                                tokio::spawn(channel_quic_worker(
                                    i,
                                    chans_instances.clone(),
                                    channels.clone(),
                                    tx_chan_rx.resubscribe(),
                                    rx_chan_tx.clone(),
                                ));
                            }
                        }
                        ChannelStatus::Checking => panic!("Illegal status for a server channel"),
                        ChannelStatus::Connected => {}
                        ChannelStatus::Listening => {}
                    }
                    tokio::task::yield_now().await;
                },
                CommChannel::Client(socket_addr) => loop {
                    match inst.status {
                        ChannelStatus::Unknown(time) => {
                            if time.elapsed() > std::time::Duration::from_secs(2) {
                                println!("Checking channel {}", i);
                                inst.status = ChannelStatus::Checking;

                                let mut endpoint = quinn::Endpoint::client(
                                    (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
                                )
                                .unwrap();

                                let mut root_certs = rustls::RootCertStore::empty();
                                root_certs
                                    .add(rustls::pki_types::CertificateDer::from(
                                        std::fs::read("cert.der")
                                            .expect("Couldn't read certificate"),
                                    ))
                                    .unwrap();
                                let mut client_crypto = rustls::ClientConfig::builder()
                                    .with_root_certificates(root_certs)
                                    .with_no_client_auth();
                                client_crypto.alpn_protocols =
                                    ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();
                                let client_config = quinn::ClientConfig::new(std::sync::Arc::new(
                                    quinn::crypto::rustls::QuicClientConfig::try_from(
                                        client_crypto,
                                    )
                                    .unwrap(),
                                ));
                                endpoint.set_default_client_config(client_config);
                                println!("Set client's default config");

                                let conn = match endpoint.connect(socket_addr, "blimp_onboard") {
                                    Ok(conn) => conn.await.map_err(|err| {
                                        eprintln!("Connection error: {}", err);
                                        ()
                                    }),
                                    Err(err) => {
                                        eprintln!("Connecting error: {}", err);
                                        Err(())
                                    }
                                };
                                if let Ok(conn) = conn {
                                    println!("Successfully opened connection!");
                                    inst.connection = Some(conn);
                                    inst.status = ChannelStatus::Connected;
                                    tokio::spawn(channel_quic_worker(
                                        i,
                                        chans_instances.clone(),
                                        channels.clone(),
                                        tx_chan_rx.resubscribe(),
                                        rx_chan_tx.clone(),
                                    ));
                                } else {
                                    println!("Couldn't connect");
                                    inst.status = ChannelStatus::Unknown(std::time::Instant::now());
                                }
                            }
                        }
                        ChannelStatus::Checking => {}
                        ChannelStatus::Connected => {}
                        ChannelStatus::Listening => panic!("Illegal status for a client channel"),
                    }
                    tokio::task::yield_now().await;
                },
            }
        });
    }
}
