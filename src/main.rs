#![no_std]
#![no_main]

mod net;
mod quotes;
mod screen;

use crate::{
    net::{POLL_INTERVAL, QUOTES, STATUS},
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
const STALE_AFTER: Duration = Duration::from_secs(POLL_INTERVAL.as_secs() * 10);

#[derive(Copy, Clone)]
enum Render {
    Band,
    Full,
}

#[derive(Copy, Clone)]
enum Mode {
    Ticker,
    Status { refresh_at: Instant },
    Asleep { wake_at: Instant },
}

struct Ui {
    quotes: [Option<Quote>; SYMBOL_COUNT],
    status: net::Status,
    mode: Mode,
    tile: usize,
    tile_start: Instant,
    tile_until: Instant,
    last_quote_at: Instant,
    render: Option<Render>,
}

impl Ui {
    fn new() -> Self {
        let quotes = QUOTES.lock(|q| q.get());
        let now = Instant::now();
        let tile_until = now + Self::tile_dwell(0);
        Self {
            quotes,
            status: STATUS.lock(|s| s.borrow().clone()),
            mode: Mode::Ticker,
            tile: 0,
            tile_start: now,
            tile_until,
            last_quote_at: now,
            render: Some(Render::Full),
        }
    }

    fn tile_dwell(tile: usize) -> Duration {
        Duration::from_millis(tile_dwell_ms(SYMBOLS[tile].name))
    }

    fn quotes_stale(&self) -> bool {
        self.last_quote_at.elapsed() >= STALE_AFTER
    }

    fn page(&self) -> Page {
        match self.mode {
            Mode::Status { .. } => Page::Status,
            _ => Page::Ticker,
        }
    }

    fn restart_tile(&mut self, now: Instant) {
        self.tile_start = now;
        self.tile_until = now + Self::tile_dwell(self.tile);
    }

    fn wake(&mut self, now: Instant) {
        self.mode = Mode::Ticker;
        self.tile = 0;
        self.render = Some(Render::Full);
        self.restart_tile(now);
    }

    fn sleep(&mut self, now: Instant) {
        let next = if self.quotes_stale() {
            POLL_INTERVAL
        } else {
            SCREEN_PHASE
        };
        self.mode = Mode::Asleep {
            wake_at: now + next,
        };
    }

    fn scroll(&self) -> i32 {
        if !matches!(self.mode, Mode::Ticker) {
            return 0;
        }
        marquee_scroll(
            SYMBOLS[self.tile].name,
            self.tile_start.elapsed().as_millis(),
        )
    }

    fn refresh_from_net(&mut self) {
        let now = Instant::now();
        let was_waiting = self.quotes.iter().all(Option::is_none);
        let quotes_now = QUOTES.lock(|q| q.get());
        let status_now = STATUS.lock(|s| s.borrow().clone());
        let quotes_changed = quotes_now != self.quotes;
        if quotes_changed || status_now != self.status {
            let was_stale = self.quotes_stale();
            self.quotes = quotes_now;
            self.status = status_now;
            if quotes_changed {
                self.last_quote_at = now;
                if was_stale && matches!(self.mode, Mode::Asleep { .. }) {
                    self.wake(now);
                } else if was_waiting && self.quotes.iter().any(Option::is_some) {
                    self.tile = 0;
                    self.restart_tile(now);
                }
            }
            self.render = Some(Render::Full);
        }
    }

    fn take_render(&mut self) -> Option<Render> {
        self.render.take()
    }

    fn observe_scroll(&mut self) {
        if self.render.is_none() && matches!(self.mode, Mode::Ticker) && self.scroll() != 0 {
            self.render = Some(Render::Band);
        }
    }

    fn advance_tile(&mut self) {
        self.tile = (self.tile + 1) % SYMBOL_COUNT;
        self.restart_tile(Instant::now());
        self.render = Some(Render::Full);
    }

    fn on_button(&mut self, event: ButtonEvent) {
        let now = Instant::now();
        if matches!(self.mode, Mode::Asleep { .. }) {
            self.wake(now);
        }

        match event {
            ButtonEvent::LongPress => {
                if matches!(self.mode, Mode::Status { .. }) {
                    self.mode = Mode::Ticker;
                    self.restart_tile(now);
                } else {
                    self.mode = Mode::Status {
                        refresh_at: now + SCREEN_PHASE,
                    };
                }
                self.render = Some(Render::Full);
            }
            ButtonEvent::ShortPress => match self.mode {
                Mode::Status { .. } => {
                    self.mode = Mode::Ticker;
                    self.render = Some(Render::Full);
                    self.restart_tile(now);
                }
                _ => self.advance_tile(),
            },
        }
    }

    fn on_deadline(&mut self) {
        let now = Instant::now();

        match self.mode {
            Mode::Asleep { wake_at } => {
                if now < wake_at {
                    return;
                }
                if self.quotes_stale() {
                    self.mode = Mode::Asleep {
                        wake_at: now + POLL_INTERVAL,
                    };
                } else {
                    self.wake(now);
                }
            }
            Mode::Status { .. } => {
                self.mode = Mode::Status {
                    refresh_at: now + SCREEN_PHASE,
                };
            }
            Mode::Ticker => {
                if now >= self.tile_until {
                    if self.tile + 1 == SYMBOL_COUNT {
                        self.sleep(now);
                    } else {
                        self.advance_tile();
                    }
                } else {
                    self.observe_scroll();
                }
            }
        }
    }

    fn backlight_duty(&self) -> u8 {
        if matches!(self.mode, Mode::Asleep { .. }) {
            0
        } else {
            BACKLIGHT_BRIGHTNESS
        }
    }

    fn next_deadline(&self) -> Instant {
        match self.mode {
            Mode::Asleep { wake_at } => wake_at,
            Mode::Status { refresh_at } => refresh_at,
            Mode::Ticker => {
                let mut next = self.tile_until;
                if let Some(step) = self.next_band_step() {
                    next = next.min(step);
                }
                next
            }
        }
    }

    fn next_band_step(&self) -> Option<Instant> {
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
    let mut ui = Ui::new();

    loop {
        let scroll = ui.scroll();
        match ui.take_render() {
            Some(Render::Band) => {
                redraw_marquee(&mut display, SYMBOLS[ui.tile].name, scroll).await;
            }
            Some(Render::Full) => {
                draw(&mut display, ui.page(), ui.tile, &ui.quotes, scroll).await;
            }
            None => {}
        }
        backlight_channel.set_duty(ui.backlight_duty()).unwrap();

        match select(BUTTON_EVENTS.wait(), Timer::at(ui.next_deadline())).await {
            Either::First(event) => ui.on_button(event),
            Either::Second(()) => ui.on_deadline(),
        }
        ui.refresh_from_net();
    }
}
