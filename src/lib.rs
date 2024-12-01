use quinn;
use tokio;

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
}

enum ChannelMsg {}

pub async fn start_communication(channels: Vec<CommChannel>) {
    let chans_instances = std::sync::Arc::new(tokio::sync::RwLock::new(Vec::<
        tokio::sync::Mutex<ChannelInstance>,
    >::new()));
    let channels = std::sync::Arc::new(channels);

    for (i, ch) in channels.iter().enumerate() {
        chans_instances
            .write()
            .await
            .push(tokio::sync::Mutex::new(ChannelInstance {
                connection: None,
                status: ChannelStatus::Unknown(std::time::Instant::now()),
            }));

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
