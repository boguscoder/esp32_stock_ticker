use crate::net::{QUOTE_SOURCE, STATUS};
use crate::quotes::{fmt_money, fmt_pct, Quote, SYMBOLS, SYMBOL_COUNT};
use core::{
    fmt::{Display, Write},
    sync::atomic::{AtomicU32, Ordering},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_time::Instant;
use embedded_graphics::{
    geometry::Size,
    mono_font::{ascii::FONT_10X20, MonoTextStyleBuilder},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, Triangle},
    text::{Alignment, Text},
};
use embedded_graphics_framebuf::FrameBuf;
use waveshare_display::{Display as Panel, LandscapeDisplay, DISPLAY_HEIGHT, DISPLAY_WIDTH};

pub const BACKGROUND: Rgb565 = Rgb565::BLACK;
const GAIN: Rgb565 = Rgb565::new(0, 63, 10);
const LOSS: Rgb565 = Rgb565::new(31, 10, 10);
const DIM: Rgb565 = Rgb565::new(16, 32, 16);
const GHOST: Rgb565 = Rgb565::new(4, 7, 10);
const FOREGROUND: Rgb565 = Rgb565::WHITE;

pub const WIDTH: i32 = DISPLAY_WIDTH;
pub const HEIGHT: i32 = DISPLAY_HEIGHT;

static FRAMEBUFFER: Mutex<CriticalSectionRawMutex, [Rgb565; (WIDTH * HEIGHT) as usize]> =
    Mutex::new([Rgb565::BLACK; (WIDTH * HEIGHT) as usize]);

// Per-render accent for the Status page: xorshift seeded from the clock, then
// mod over a legible-on-black palette so it can never fade into the background.
static RNG_STATE: AtomicU32 = AtomicU32::new(0);

const ACCENTS: [Rgb565; 8] = [
    Rgb565::new(31, 42, 0),  // amber
    Rgb565::new(0, 55, 31),  // cyan
    Rgb565::new(31, 8, 31),  // magenta
    Rgb565::new(10, 63, 0),  // lime
    Rgb565::new(8, 31, 31),  // sky
    Rgb565::new(31, 16, 26), // pink
    Rgb565::new(0, 46, 24),  // teal
    Rgb565::new(21, 8, 31),  // purple
];

fn random_accent() -> Rgb565 {
    let mut x = match RNG_STATE.load(Ordering::Relaxed) {
        0 => Instant::now().as_millis() as u32 | 1,
        seeded => seeded,
    };
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    RNG_STATE.store(x, Ordering::Relaxed);
    ACCENTS[((x >> 16) as usize) % ACCENTS.len()]
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Page {
    Ticker,
    Status,
}

impl Page {
    pub fn next(self) -> Self {
        match self {
            Page::Ticker => Page::Status,
            Page::Status => Page::Ticker,
        }
    }
}

pub async fn draw(
    raw_display: &mut Panel<'_>,
    page: Page,
    tile: usize,
    quotes: &[Option<Quote>; SYMBOL_COUNT],
    scroll: i32,
) {
    let mut fb_guard = FRAMEBUFFER.lock().await;
    let mut fbuf = FrameBuf::new(&mut *fb_guard, WIDTH as usize, HEIGHT as usize);

    fbuf.clear(BACKGROUND).unwrap();

    // In-memory drawing can't fail; discard the Results.
    match page {
        Page::Ticker => match &quotes[tile] {
            Some(quote) => draw_tile(
                &mut fbuf,
                SYMBOLS[tile].label,
                quote,
                tile,
                SYMBOL_COUNT,
                scroll,
            ),
            None => draw_placeholder(&mut fbuf, SYMBOLS[tile].label, tile, SYMBOL_COUNT),
        },
        Page::Status => draw_status(&mut fbuf).await,
    };

    let mut panel = LandscapeDisplay { base: raw_display };
    panel.draw_iter(&fbuf).unwrap();
}

// Marquee-only step: repaint the name band into the framebuffer and push just
// those rows with a single window write, keeping ticks cheap for the button task.
pub async fn redraw_marquee(raw_display: &mut Panel<'_>, name: &str, scroll: i32) {
    let mut fb_guard = FRAMEBUFFER.lock().await;
    {
        let mut fbuf = FrameBuf::new(&mut *fb_guard, WIDTH as usize, HEIGHT as usize);
        draw_name_band(&mut fbuf, name, scroll);
    }

    let mut panel = LandscapeDisplay { base: raw_display };
    panel
        .flush_rows(NAME_BAND_TOP, NAME_BAND_H, &*fb_guard)
        .unwrap();
}

const PRICE_SCALE: u32 = 5; // title band scale — ticker & price share one height
const NAME_SCALE: u32 = 3; // 5x7 glyphs scaled ×3 → 21px tall

// Marquee: names wider than the panel sweep left once, stopping tail-side;
// scroll is quantized so repaint cadence doesn't starve the button task.
// Derived from the glyph cell so the marquee math can never drift from it.
const NAME_FONT_PX: i32 = 6 * NAME_SCALE as i32;
const NAME_MARGIN: i32 = 8;
// Name band rows cleared/redrawn on every marquee frame.
const NAME_BAND_TOP: i32 = 60;
const NAME_BAND_H: i32 = 46;
const NAME_PAUSE_MS: u64 = 1200;
const NAME_SPEED_PX_S: i32 = 100;
const MARQUEE_STEP_PX: i32 = 6;
pub const BASE_TILE_DWELL_MS: u64 = 4000;

struct Marquee {
    width_px: i32,
    stop_px: i32,
}

// None while the name fits: it centers instead of scrolling.
fn marquee(name: &str) -> Option<Marquee> {
    let width_px = (name.len() as i32) * NAME_FONT_PX;
    if width_px + NAME_MARGIN <= WIDTH {
        return None;
    }
    Some(Marquee {
        width_px,
        stop_px: width_px - (WIDTH - 2 * NAME_MARGIN),
    })
}

pub fn tile_dwell_ms(name: Option<&str>) -> u64 {
    let Some(name) = name else {
        return BASE_TILE_DWELL_MS;
    };
    match marquee(name) {
        None => BASE_TILE_DWELL_MS,
        Some(m) => (NAME_PAUSE_MS + m.stop_px as u64 * 1000 / NAME_SPEED_PX_S as u64 + 400)
            .clamp(BASE_TILE_DWELL_MS, 12_000),
    }
}

pub fn marquee_scroll(name: &str, elapsed_ms: u64) -> i32 {
    if elapsed_ms <= NAME_PAUSE_MS {
        return 0;
    }
    match marquee(name) {
        None => 0,
        Some(m) => {
            let swept = (elapsed_ms - NAME_PAUSE_MS) as i32 * NAME_SPEED_PX_S / 1000;
            (swept / MARQUEE_STEP_PX * MARQUEE_STEP_PX).min(m.stop_px)
        }
    }
}

// Millis from `elapsed_ms` at which the scroll value next changes, unless the
// sweep already sat at its stop.
pub fn next_marquee_step(name: &str, elapsed_ms: u64) -> Option<u64> {
    let m = marquee(name)?;
    let step_ms = MARQUEE_STEP_PX as u64 * 1000 / NAME_SPEED_PX_S as u64;
    let settled = elapsed_ms.saturating_sub(NAME_PAUSE_MS);
    if settled as i32 * NAME_SPEED_PX_S / 1000 >= m.stop_px {
        return None;
    }
    Some(NAME_PAUSE_MS + settled - settled % step_ms + step_ms)
}
// 5x7 glyphs (5-bit rows, MSB left), slots: 0-9, A-Z, '.', '-', ',', space.
const GLYPHS: [[u8; 7]; 40] = [
    // 0-9
    [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
    [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
    [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
    [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
    [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
    [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
    [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
    [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
    [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
    // A-Z
    [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11], // A
    [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E], // B
    [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E], // C
    [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E], // D
    [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F], // E
    [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10], // F
    [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F], // G
    [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11], // H
    [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E], // I
    [0x03, 0x01, 0x01, 0x01, 0x01, 0x11, 0x0E], // J
    [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11], // K
    [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F], // L
    [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11], // M
    [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11], // N
    [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E], // O
    [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10], // P
    [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D], // Q
    [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11], // R
    [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E], // S
    [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04], // T
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E], // U
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04], // V
    [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A], // W
    [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11], // X
    [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04], // Y
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F], // Z
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C], // .
    [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00], // -
    [0x00; 7],                                  // space
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x18], // ,
];

fn glyph_slot(c: char) -> Option<usize> {
    match c {
        '0'..='9' => Some(c as usize - '0' as usize),
        'A'..='Z' => Some(10 + c as usize - 'A' as usize),
        '.' => Some(36),
        '-' => Some(37),
        ',' => Some(39),
        ' ' => Some(38),
        _ => None,
    }
}

fn glyph(c: char) -> Option<&'static [u8; 7]> {
    Some(&GLYPHS[glyph_slot(c)?])
}

fn draw_big_text<T>(target: &mut T, text: &str, center_x: i32, top: i32, scale: u32, color: Rgb565)
where
    T: DrawTarget<Color = Rgb565>,
{
    let char_count = text.chars().count() as i32;
    if char_count == 0 {
        return;
    }
    // 5 glyph columns + 1 column of inter-character spacing
    let cell = (5 + 1) * scale as i32;
    let width = char_count * cell - scale as i32;
    let mut x = center_x - width / 2;
    let fill = PrimitiveStyleBuilder::new().fill_color(color).build();

    for ch in text.chars() {
        if let Some(g) = glyph(ch) {
            for (row, &bits) in g.iter().enumerate() {
                for col in 0..5 {
                    if bits & (0x10 >> col) != 0 {
                        Rectangle::new(
                            Point::new(
                                x + (col * scale as usize) as i32,
                                top + (row * scale as usize) as i32,
                            ),
                            Size::new(scale, scale),
                        )
                        .into_styled(fill)
                        .draw(target)
                        .ok();
                    }
                }
            }
        }
        x += cell;
    }
}

fn draw_title<T>(target: &mut T, label: &str, price: &str, color: Rgb565)
where
    T: DrawTarget<Color = Rgb565>,
{
    let margin = 8;
    let measure = |s: &str, scale: u32| s.len() as i32 * 6 * scale as i32 - scale as i32;
    let mut scale = PRICE_SCALE;
    while measure(label, scale) + measure(price, scale) + 2 * margin > WIDTH && scale > 3 {
        scale -= 1;
    }
    draw_big_text(
        target,
        label,
        margin + measure(label, scale) / 2,
        14,
        scale,
        FOREGROUND,
    );
    draw_big_text(
        target,
        price,
        WIDTH - margin - measure(price, scale) / 2,
        14,
        scale,
        color,
    );
}

// Name band: clear, uppercase, then center or sweep per the marquee math.
fn draw_name_band<T>(target: &mut T, name: &str, scroll: i32)
where
    T: DrawTarget<Color = Rgb565>,
{
    // Band cleared each tile frame; marquee-only repaints rely on it.
    Rectangle::new(
        Point::new(0, NAME_BAND_TOP),
        Size::new(WIDTH as u32, NAME_BAND_H as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(BACKGROUND))
    .draw(target)
    .ok();

    if name.is_empty() {
        return;
    }
    let mut upper = heapless::String::<96>::new();
    for c in name.chars() {
        upper.push(c.to_ascii_uppercase()).ok();
    }
    // FrameBuf drops out-of-bounds pixels — that clip is the marquee.
    let cx = match marquee(upper.as_str()) {
        None => WIDTH / 2,
        Some(m) => NAME_MARGIN - scroll + (m.width_px - NAME_SCALE as i32) / 2,
    };
    draw_big_text(target, upper.as_str(), cx, 73, NAME_SCALE, DIM);
}

fn draw_tile<T>(target: &mut T, label: &str, quote: &Quote, index: usize, count: usize, scroll: i32)
where
    T: DrawTarget<Color = Rgb565>,
{
    let color = if quote.change > 0 {
        GAIN
    } else if quote.change < 0 {
        LOSS
    } else {
        DIM
    };
    let price = fmt_money(quote.price, false);
    draw_title(target, label, price.as_str(), color);

    draw_name_band(target, SYMBOLS[index].name, scroll);

    let row_style = MonoTextStyleBuilder::new()
        .font(&FONT_10X20)
        .text_color(color)
        .background_color(BACKGROUND)
        .build();

    if color == DIM {
        Text::with_alignment(
            "NO DATA",
            Point::new(WIDTH / 2, 131),
            row_style,
            Alignment::Center,
        )
        .draw(target)
        .ok();
    } else {
        let mut row = heapless::String::<40>::new();
        if quote.extended {
            row.push_str("AH ").ok();
        }
        row.push_str(fmt_money(quote.change, true).as_str()).ok();
        row.push(' ').ok();
        row.push_str(fmt_pct(quote.change_pct).as_str()).ok();

        // Unit width: 16px arrow + 8px gap + text; center it on the panel.
        let unit_left = WIDTH / 2 - (row.len() as i32 * 10 + 24) / 2;
        let row_cx = unit_left + 24 + (row.len() as i32 * 10) / 2;
        Text::with_alignment(
            row.as_str(),
            Point::new(row_cx, 131),
            row_style,
            Alignment::Center,
        )
        .draw(target)
        .ok();

        let tx = unit_left + 8;
        let (arrow, fill_color) = if quote.change > 0 {
            (
                Triangle::new(
                    Point::new(tx, 119),
                    Point::new(tx - 8, 131),
                    Point::new(tx + 8, 131),
                ),
                GAIN,
            )
        } else {
            (
                Triangle::new(
                    Point::new(tx, 131),
                    Point::new(tx - 8, 119),
                    Point::new(tx + 8, 119),
                ),
                LOSS,
            )
        };
        arrow
            .into_styled(PrimitiveStyle::with_fill(fill_color))
            .draw(target)
            .ok();
    }

    draw_dots(target, index, count);
}

fn draw_placeholder<T>(target: &mut T, label: &str, index: usize, count: usize)
where
    T: DrawTarget<Color = Rgb565>,
{
    draw_title(target, label, "--.--", GHOST);

    let style = MonoTextStyleBuilder::new()
        .font(&FONT_10X20)
        .text_color(DIM)
        .background_color(BACKGROUND)
        .build();
    Text::with_alignment(
        "waiting for data...",
        Point::new(WIDTH / 2, 131),
        style,
        Alignment::Center,
    )
    .draw(target)
    .ok();

    draw_dots(target, index, count);
}

fn draw_dots<T>(target: &mut T, index: usize, count: usize)
where
    T: DrawTarget<Color = Rgb565>,
{
    const DOT_SPACING: i32 = 18;
    const DOT_DIAMETER: u32 = 5;
    let cy = HEIGHT - 12;
    let start_x = WIDTH / 2 - ((count as i32 - 1) * DOT_SPACING) / 2;

    for (i, dot_x) in (0..count).map(|i| (i, start_x + i as i32 * DOT_SPACING)) {
        let style = if i == index {
            PrimitiveStyle::with_fill(FOREGROUND)
        } else {
            PrimitiveStyle::with_fill(GHOST)
        };
        Circle::new(Point::new(dot_x - 2, cy - 2), DOT_DIAMETER)
            .into_styled(style)
            .draw(target)
            .ok();
    }
}

async fn draw_status<T>(target: &mut T)
where
    T: DrawTarget<Color = Rgb565>,
{
    let mut text = heapless::String::<160>::new();
    {
        let status = STATUS.lock().await;
        write!(
            &mut text,
            "SSID: {}\nPhase: {:?}\nIP: {}\nQuote time: {}\nSource: {}",
            status.ssid,
            status.phase,
            opt_display(&status.ip),
            opt_display(&status.quote_time),
            QUOTE_SOURCE,
        )
        .ok();
    }

    let style = MonoTextStyleBuilder::new()
        .font(&FONT_10X20)
        .text_color(random_accent())
        .background_color(BACKGROUND)
        .build();
    Text::with_alignment(text.as_str(), Point::new(10, 30), style, Alignment::Left)
        .draw(target)
        .ok();
}

fn opt_display<T: Display>(value: &Option<T>) -> heapless::String<48> {
    let mut out = heapless::String::<48>::new();
    match value {
        Some(v) => write!(out, "{}", v).ok(),
        None => write!(out, "N/A").ok(),
    };
    out
}
