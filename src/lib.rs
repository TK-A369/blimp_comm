use postcard;
use quinn;
use serde;
use tokio;

use blimp_onboard_software;

pub enum CommChannel {
    Server(core::net::SocketAddr),
    Client(core::net::SocketAddr),
}

enum ChannelStatus {
    Unknown(std::time::Instant),
    Checking,
    Connected,
}

struct ChannelInstance {
    connection: Option<quinn::Connection>,
    status: ChannelStatus,
    tx_chan: tokio::sync::mpsc::Sender<ChannelMsg>,
    rx_chan: tokio::sync::broadcast::Receiver<ChannelMsg>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub enum ChannelMsg {
    Handshake,
    Heartbeat,
    MessageG2B(blimp_onboard_software::obsw_algo::MessageG2B),
    MessageB2G(blimp_onboard_software::obsw_algo::MessageB2G),
}

async fn channel_quic_worker(
    channel_id: usize,
    chans_instances: std::sync::Arc<
        tokio::sync::RwLock<Vec<std::sync::Arc<tokio::sync::Mutex<ChannelInstance>>>>,
    >,
    channels: std::sync::Arc<Vec<CommChannel>>,
) {
    let this_channel_inst = chans_instances.read().await[channel_id].clone();
    let this_channel_inst_locked = this_channel_inst.lock().await;
    let conn = this_channel_inst_locked
        .connection
        .as_ref()
        .expect("Started channel_quic_worker for a channel without connection!");
    let channel = &channels[channel_id];

    async fn stream_worker(mut stream_tx: quinn::SendStream, mut stream_rx: quinn::RecvStream) {
        let mut rx_buf: [u8; 1024] = [0; 1024];
        let mut frame_buf: [u8; 1024] = [0; 1024];
        let mut frame_recv_len: u32 = 0;
        let mut frame_expected_len: Option<u32> = None;
        loop {
            let msg = stream_rx
                .read(&mut rx_buf[(frame_recv_len as usize)..])
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
                            } else {
                                eprintln!(
                                    "Couldn't deserialize message received from channel stream!"
                                );
                            }
                        }
                    }
                }
                Ok(None) => {
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
            if let Ok((mut stream_tx, mut stream_rx)) = conn.accept_bi().await {
                println!("Opened stream on channel {}", channel_id);
                stream_worker(stream_tx, stream_rx).await;
            } else {
                eprintln!(
                    "Error occurred when opening stream on channel {}",
                    channel_id
                );
            }
        }
        CommChannel::Client(socket_addr) => {
            if let Ok((mut stream_tx, mut stream_rx)) = conn.open_bi().await {
                stream_tx
                    .write(&postcard::to_stdvec::<ChannelMsg>(&ChannelMsg::Handshake).unwrap());
                println!("Opened stream on channel {}", channel_id);
                stream_worker(stream_tx, stream_rx);
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
    let chans_instances = std::sync::Arc::new(tokio::sync::RwLock::new(Vec::<
        std::sync::Arc<tokio::sync::Mutex<ChannelInstance>>,
    >::new()));
    let channels = std::sync::Arc::new(channels);

    for (i, ch) in channels.iter().enumerate() {
        chans_instances
            .write()
            .await
            .push(std::sync::Arc::new(tokio::sync::Mutex::new(
                ChannelInstance {
                    connection: None,
                    status: ChannelStatus::Unknown(std::time::Instant::now()),
                },
            )));

        let chans_instances = chans_instances.clone();
        let channels = channels.clone();
        tokio::spawn(async move {
            match channels[i] {
                CommChannel::Server(socket_addr) => {
                    //let endpoint
                }
                CommChannel::Client(socket_addr) => loop {
                    let chans_instances_locked = chans_instances.read().await;
                    let mut inst = chans_instances_locked[i].lock().await;
                    match inst.status {
                        ChannelStatus::Unknown(time) => {
                            if time.elapsed() > std::time::Duration::from_secs(2) {
                                println!("Checking channel {}", i);
                                inst.status = ChannelStatus::Checking;

                                let endpoint = quinn::Endpoint::client(
                                    (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
                                )
                                .unwrap();
                                let conn = endpoint
                                    .connect(socket_addr, "blimp_onboard")
                                    .unwrap()
                                    .await
                                    .unwrap();
                                inst.connection = Some(conn);
                                tokio::spawn(channel_quic_worker(i, chans_instances.clone()));
                            }
                        }
                        ChannelStatus::Checking => {}
                        ChannelStatus::Connected => {}
                    }
                },
            }
        });
    }
}
