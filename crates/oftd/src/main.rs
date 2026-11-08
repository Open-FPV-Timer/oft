use clap::Parser;
use oft_sim::fake_node::FakeNode;
use oft_sim::{NodeClock, Pass, Repeat, Scene};
use oftd::link::FakeLink;
use oftd::protocol::Config;
use oftd::{serve, Timer, TimerConfig};
use std::net::SocketAddr;

/// oftd — OFT Timer for Linux.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address to serve the OFT Protocol on.
    #[arg(long, default_value = "0.0.0.0:7270")]
    listen: SocketAddr,
    /// Run against a simulated Node instead of hardware.
    #[arg(long)]
    fake: bool,
    /// Fake Node: seconds between synthetic passes (0 = none).
    #[arg(long, default_value_t = 8.0)]
    fake_lap_s: f64,
    /// Fake Node: pass height above the floor, dB.
    #[arg(long, default_value_t = 36.0)]
    fake_height_db: f64,
    /// Serial port of a real Node, e.g. /dev/ttyACM0 (XIAO C5 over USB-C) or a USB-UART adapter.
    #[arg(long)]
    serial: Option<String>,
    /// Baud rate for --serial.
    #[arg(long, default_value_t = 921_600)]
    baud: u32,
    /// Timer id advertised to clients.
    #[arg(long)]
    id: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let link: Box<dyn oftd::link::NodeLink> = if args.fake {
        let mut scene = Scene::default();
        if args.fake_lap_s > 0.0 {
            scene.repeat = Some(Repeat {
                first_us: 2_000_000,
                period_us: (args.fake_lap_s * 1e6) as u64,
                pass: Pass { center_us: 0, height_db: args.fake_height_db, width_us: 130_000.0, dip: 0.4 },
            });
        }
        // Fake Node booted 1.5 s before us and runs 30 ppm fast.
        Box::new(FakeLink(FakeNode::new(scene, NodeClock { offset_us: -1_500_000, ppm: 30.0 }, 42)))
    } else if let Some(port) = args.serial {
        match oftd::serial::SerialLink::open(&port, args.baud) {
            Ok(l) => Box::new(l),
            Err(e) => {
                eprintln!("oftd: cannot open {port}: {e}");
                std::process::exit(2);
            }
        }
    } else {
        eprintln!("oftd: choose --fake or --serial <port>");
        std::process::exit(2);
    };

    let mut cfg = TimerConfig::default();
    if let Some(id) = args.id {
        cfg.id = id;
    }
    let timer = Timer::new(cfg, link, Config::default());
    let driver = tokio::spawn(timer.clone().run());
    match serve(timer.clone(), args.listen).await {
        Ok((addr, server)) => {
            eprintln!("oftd {} serving http://{addr}/oft/v1  (ws: /oft/v1/stream)", timer.cfg.id);
            let _ = tokio::join!(server, driver);
        }
        Err(e) => {
            eprintln!("oftd: cannot bind {}: {e}", args.listen);
            std::process::exit(1);
        }
    }
}
