use crate::quotes::{parse_quotes, parse_timestamp, Quote, SYMBOLS, SYMBOL_COUNT};
use core::net::Ipv4Addr;
use embassy_executor::Spawner;
use embassy_net::{tcp::client::TcpClientState, Stack};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::WIFI;
use esp_radio::wifi::WifiController;
use static_cell::StaticCell;

pub static QUOTES: Mutex<CriticalSectionRawMutex, [Option<Quote>; SYMBOL_COUNT]> =
    Mutex::new([const { None }; SYMBOL_COUNT]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub ssid: &'static str,
    pub phase: Phase,
    pub ip: Option<Ipv4Addr>,
    pub quote_time: Option<heapless::String<20>>,
}

impl Status {
    const fn new() -> Self {
        Self {
            ssid: env!("WIFI_SSID"),
            phase: Phase::Offline,
            ip: None,
            quote_time: None,
        }
    }

    pub fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
    }

    pub fn set_ip(&mut self, ip: Ipv4Addr) {
        self.ip = Some(ip);
    }

    pub fn set_quote_time(&mut self, ts: heapless::String<20>) {
        self.quote_time = Some(ts);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Offline,
    Connecting,
    Online,
}

pub static STATUS: Mutex<CriticalSectionRawMutex, Status> = Mutex::new(Status::new());

pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

pub fn start_networking(wifi_peripheral: WIFI<'static>, spawner: Spawner) {
    let (controller, stack) = waveshare_net::start_station(
        wifi_peripheral,
        env!("WIFI_SSID"),
        env!("WIFI_PASS"),
        spawner,
    );

    spawner.spawn(refresh_task(controller, stack).unwrap());
}

#[embassy_executor::task]
async fn refresh_task(mut controller: WifiController<'static>, stack: Stack<'static>) {
    static TCP: StaticCell<TcpClientState<1, 2048, 2048>> = StaticCell::new();
    static RX: StaticCell<[u8; 8192]> = StaticCell::new();
    let tcp_state = TCP.init(TcpClientState::new());
    let rx = RX.init([0u8; 8192]);

    let mut associated = false;
    let mut consecutive_failures = 0u32;

    loop {
        if !associated {
            STATUS.lock().await.set_phase(Phase::Connecting);
            let joined =
                waveshare_net::connect_with_retries(&mut controller, 5, Duration::from_secs(10))
                    .await;
            if !joined {
                STATUS.lock().await.set_phase(Phase::Offline);
                Timer::after(POLL_INTERVAL).await;
                continue;
            }
            associated = true;
        }

        // Online = the stack holds an address, not just an association.
        if let Some(ip) = waveshare_net::wait_for_ip(&stack, Duration::from_millis(250), 40).await {
            let mut status = STATUS.lock().await;
            status.set_ip(ip);
            status.set_phase(Phase::Online);
        }

        if let Some((quotes, ts)) = fetch_quotes(stack, tcp_state, &mut rx[..]).await {
            *QUOTES.lock().await = quotes;
            if let Some(ts) = ts {
                STATUS.lock().await.set_quote_time(ts);
            }
            consecutive_failures = 0;
        } else {
            consecutive_failures += 1;
        }

        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            STATUS.lock().await.set_phase(Phase::Offline);
            let _ = controller.disconnect_async().await;
            associated = false;
            consecutive_failures = 0;
        } else {
            Timer::after(POLL_INTERVAL).await;
        }
    }
}

async fn fetch_quotes(
    stack: Stack<'_>,
    tcp_state: &'static TcpClientState<1, 2048, 2048>,
    rx: &mut [u8],
) -> Option<([Option<Quote>; SYMBOL_COUNT], Option<heapless::String<20>>)> {
    let mut url = heapless::String::<128>::new();
    url.push_str("http://qt.gtimg.cn/q=us").ok();
    for (i, sym) in SYMBOLS.iter().enumerate() {
        if i > 0 {
            url.push_str(",us").ok();
        }
        url.push_str(sym).ok();
    }

    let body = waveshare_net::http_get(stack, tcp_state, rx, url.as_str()).await?;

    Some((parse_quotes(body), parse_timestamp(body)))
}
