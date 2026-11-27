use crate::quotes::{parse_quotes, Quote, SYMBOLS, SYMBOL_COUNT};
use core::cell::Cell;
use embassy_executor::Spawner;
use embassy_net::{tcp::client::TcpClientState, Stack};
use embassy_sync::blocking_mutex::{raw::CriticalSectionRawMutex, Mutex};
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::WIFI;
use esp_radio::wifi::WifiController;

pub static QUOTES: Mutex<CriticalSectionRawMutex, Cell<[Option<Quote>; SYMBOL_COUNT]>> =
    Mutex::new(Cell::new([const { None }; SYMBOL_COUNT]));

pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
const QUOTE_SOURCE: &str = "hq.sinajs.cn";
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
    let (tcp_state, rx) = waveshare_net::http_buffers();

    let mut associated = false;
    let mut consecutive_failures = 0u32;

    loop {
        if !associated {
            if !waveshare_net::connect_with_retries(&mut controller, 5, Duration::from_secs(10))
                .await
            {
                Timer::after(POLL_INTERVAL).await;
                continue;
            }
            associated = true;
        }

        // Online = the stack holds an address, not just an association.
        waveshare_net::wait_for_ip(&stack, Duration::from_millis(250), 40).await;

        if let Some(quotes) = fetch_quotes(stack, tcp_state, rx).await {
            QUOTES.lock(|q| q.set(quotes));
            consecutive_failures = 0;
        } else {
            consecutive_failures += 1;
        }

        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
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
) -> Option<[Option<Quote>; SYMBOL_COUNT]> {
    let mut url = heapless::String::<128>::new();
    url.push_str("http://").ok();
    url.push_str(QUOTE_SOURCE).ok();
    url.push_str("/list=").ok();
    for (i, sym) in SYMBOLS.iter().enumerate() {
        if i > 0 {
            url.push_str(",").ok();
        }
        url.push_str("gb_").ok();
        for c in sym.label.chars() {
            url.push(c.to_ascii_lowercase()).ok();
        }
    }

    let body = waveshare_net::http_get(
        stack,
        tcp_state,
        rx,
        url.as_str(),
        &[("Referer", "https://finance.sina.com.cn")],
    )
    .await?;

    Some(parse_quotes(body))
}
