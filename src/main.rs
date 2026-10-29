#![no_std]
#![no_main]

mod net;
mod quotes;
mod screen;

use crate::{
    net::{QUOTES, STATUS},
    quotes::{Quote, SYMBOLS, SYMBOL_COUNT},
    screen::{draw, marquee_scroll, next_marquee_step, redraw_marquee, tile_dwell_ms, Page},
};
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::{
    interrupt::software::SoftwareInterruptControl, ledc::channel::ChannelIFace,
    timer::timg::TimerGroup,
};
use waveshare_button::{button_task, ButtonEvent, BUTTON_EVENTS};
use waveshare_display::{init_display, setup_backlight};

esp_bootloader_esp_idf::esp_app_desc!();

const BACKLIGHT_BRIGHTNESS: u8 = 5;
const SCREEN_PHASE: Duration = Duration::from_millis(60 * 1000);

#[derive(Copy, Clone, PartialEq)]
enum Render {
    None,
    Band,
    Full,
}

struct Ui {
    quotes: [Option<Quote>; SYMBOL_COUNT],
    status: net::Status,
    page: Page,
    tile: usize,
    awake: bool,
    tile_start: Instant,
    tile_until: Instant,
    screen_until: Instant,
    last_scroll: i32,
    render: Render,
}

impl Ui {
    async fn fresh() -> Self {
        let quotes = QUOTES.lock().await.clone();
        let tile_start = Instant::now();
        let tile_until = tile_start + Self::tile_dwell(&quotes, 0);
        Self {
            quotes,
            status: STATUS.lock().await.clone(),
            page: Page::Ticker,
            tile: 0,
            awake: true,
            tile_start,
            tile_until,
            screen_until: Instant::now() + SCREEN_PHASE,
            last_scroll: 0,
            render: Render::Full,
        }
    }

    fn tile_dwell(quotes: &[Option<Quote>; SYMBOL_COUNT], tile: usize) -> Duration {
        Duration::from_millis(tile_dwell_ms(
            quotes[tile].as_ref().map(|_| SYMBOLS[tile].name),
        ))
    }

    fn scroll(&self) -> i32 {
        if self.page != Page::Ticker || self.quotes[self.tile].is_none() {
            return 0;
        }
        marquee_scroll(
            SYMBOLS[self.tile].name,
            self.tile_start.elapsed().as_millis(),
        )
    }

    async fn sync_shared(&mut self) {
        let quotes_now = QUOTES.lock().await.clone();
        let status_now = STATUS.lock().await.clone();
        if quotes_now != self.quotes || status_now != self.status {
            self.quotes = quotes_now;
            self.status = status_now;
            // Fresh data outranks a pending band step.
            self.render = Render::Full;
        }
    }

    fn take_render(&mut self) -> Render {
        let r = self.render;
        self.render = Render::None;
        r
    }

    fn observe_scroll(&mut self) {
        if self.render == Render::None && self.awake && self.page == Page::Ticker {
            let s = self.scroll();
            if s != self.last_scroll {
                self.render = Render::Band;
            }
        }
    }

    fn advance_tile(&mut self) {
        self.tile = (self.tile + 1) % SYMBOL_COUNT;
        self.tile_start = Instant::now();
        self.tile_until = self.tile_start + Self::tile_dwell(&self.quotes, self.tile);
        self.render = Render::Full;
    }

    fn on_button(&mut self, event: ButtonEvent) {
        self.screen_until = Instant::now() + SCREEN_PHASE;
        if !self.awake {
            self.awake = true;
            self.render = Render::Full;
            self.tile_start = Instant::now();
            self.tile_until = self.tile_start + Self::tile_dwell(&self.quotes, self.tile);
        }
        match event {
            ButtonEvent::LongPress => {
                self.page = self.page.next();
                self.render = Render::Full;
            }
            ButtonEvent::ShortPress => match self.page {
                Page::Ticker => self.advance_tile(),
                Page::Status => {
                    self.page = Page::Ticker;
                    self.render = Render::Full;
                }
            },
        }
        self.observe_scroll();
    }

    fn on_timeout(&mut self) {
        let now = Instant::now();
        if now >= self.screen_until {
            self.screen_until = now + SCREEN_PHASE;
            self.awake = !self.awake;
            if self.awake {
                self.render = Render::Full;
                self.tile_start = now;
                self.tile_until = self.tile_start + Self::tile_dwell(&self.quotes, self.tile);
            }
        }
        if self.awake && self.page == Page::Ticker && now >= self.tile_until {
            self.advance_tile();
        }
        self.observe_scroll();
    }

    fn backlight_duty(&self) -> u8 {
        if self.awake {
            BACKLIGHT_BRIGHTNESS
        } else {
            0
        }
    }

    fn next_wakeup(&self) -> Instant {
        let mut next = self.screen_until;
        if self.awake && self.page == Page::Ticker {
            next = next.min(self.tile_until);
            if let Some(step) = self.next_band_step() {
                next = next.min(step);
            }
        }
        next
    }

    fn next_band_step(&self) -> Option<Instant> {
        self.quotes[self.tile].as_ref()?;
        let at = next_marquee_step(
            SYMBOLS[self.tile].name,
            self.tile_start.elapsed().as_millis(),
        )?;
        Some(self.tile_start + Duration::from_millis(at))
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_alloc::heap_allocator!(size: 96 * 1024);

    let config = esp_hal::Config::default();
    let peripherals = esp_hal::init(config);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_intr = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_intr.software_interrupt0);

    net::start_networking(peripherals.WIFI, spawner);
    spawner.spawn(button_task(peripherals.GPIO9.into()).unwrap());

    setup_backlight!(
        peripherals.LEDC,
        peripherals.GPIO22,
        backlight_channel,
        BACKLIGHT_BRIGHTNESS
    );

    let mut display = init_display!(peripherals: peripherals);
    let mut ui = Ui::fresh().await;

    loop {
        // One scroll read per paint: paint and the stored rebase must agree.
        let scroll = ui.scroll();
        match ui.take_render() {
            Render::Band => {
                redraw_marquee(&mut display, SYMBOLS[ui.tile].name, scroll).await;
                ui.last_scroll = scroll;
            }
            Render::Full => {
                draw(&mut display, ui.page, ui.tile, &ui.quotes, scroll).await;
                ui.last_scroll = scroll;
            }
            Render::None => {}
        }
        backlight_channel.set_duty(ui.backlight_duty()).unwrap();

        let which = select(BUTTON_EVENTS.wait(), Timer::at(ui.next_wakeup())).await;
        match which {
            Either::First(event) => ui.on_button(event),
            Either::Second(()) => ui.on_timeout(),
        }
        ui.sync_shared().await;
    }
}
