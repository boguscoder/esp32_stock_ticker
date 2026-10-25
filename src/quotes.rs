//! qt.gtimg.cn parser: `~`-separated GBK rows, scanned as raw bytes since the
//! Chinese name field breaks UTF-8. Fields: [3]=price [4]=prev close [30]=ts
//! [31]=change [32]=change % — all stored scaled ×100.

use core::fmt;

pub type Symbol = &'static str;

pub const SYMBOLS: [Symbol; 9] = [
    "AMD", "MSFT", "XCEM", "VXUS", "VOO", "NVDA", "FDLO", "META", "NATO",
];

pub const SYMBOL_COUNT: usize = SYMBOLS.len();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    pub price: i32,
    pub prev_close: i32,
    pub change: i32,
    pub change_pct: i32,
    pub name: heapless::String<96>,
}

pub fn parse_quotes(body: &[u8]) -> [Option<Quote>; SYMBOL_COUNT] {
    let mut out = [const { None }; SYMBOL_COUNT];
    for (i, sym) in SYMBOLS.iter().enumerate() {
        out[i] = parse_symbol_quote(body, sym);
    }
    out
}

pub fn parse_timestamp(body: &[u8]) -> Option<heapless::String<20>> {
    // First row that yields a timestamp; a malformed row may not abort the scan.
    for sym in SYMBOLS.iter() {
        let Some(row) = symbol_row(body, sym) else {
            continue;
        };
        let Some(ts) = field(row, 30) else {
            continue;
        };
        let Some(short) = ts.get(5..16) else {
            continue;
        };
        // "10-23 16:00"
        let Ok(short) = core::str::from_utf8(short) else {
            continue;
        };
        let mut out = heapless::String::<20>::new();
        if out.push_str(short).is_err() {
            continue;
        }
        return Some(out);
    }
    None
}

fn symbol_row<'a>(body: &'a [u8], label: &str) -> Option<&'a [u8]> {
    let mut needle = heapless::String::<48>::new();
    needle.push_str("v_us").ok()?;
    needle.push_str(label).ok()?;
    needle.push_str("=\"").ok()?;

    let start = find_bytes(body, needle.as_bytes())? + needle.len();
    let end = body[start..].iter().position(|&c| c == b'"')? + start;
    Some(&body[start..end])
}

fn parse_symbol_quote(body: &[u8], label: &str) -> Option<Quote> {
    let row = symbol_row(body, label)?;

    let price = field(row, 3).and_then(parse_fixed)?;
    let prev_close = field(row, 4).and_then(parse_fixed).unwrap_or(0);
    let name = field(row, 46).and_then(decode_name).unwrap_or_default();

    let change = match field(row, 31).and_then(parse_fixed) {
        Some(c) => c,
        None => price - prev_close,
    };
    let change_pct = match field(row, 32).and_then(parse_fixed) {
        Some(p) => p,
        None if prev_close != 0 => (change as i64 * 10_000 / prev_close as i64) as i32,
        None => 0,
    };

    Some(Quote {
        price,
        prev_close,
        change,
        change_pct,
        name,
    })
}

fn field(row: &[u8], idx: usize) -> Option<&[u8]> {
    row.split(|&c| c == b'~').nth(idx).filter(|f| !f.is_empty())
}

fn parse_fixed(bytes: &[u8]) -> Option<i32> {
    let mut sign = 1i32;
    let mut rest = bytes;

    if rest.first() == Some(&b'-') {
        sign = -1;
        rest = &rest[1..];
    } else if rest.first() == Some(&b'+') {
        rest = &rest[1..];
    }

    let dot = rest.iter().position(|&c| c == b'.');
    let (int_part, frac_part) = match dot {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };

    if int_part.is_empty() || int_part.len() > 10 {
        return None;
    }
    let mut whole: i64 = 0;
    for &c in int_part {
        if !c.is_ascii_digit() {
            return None;
        }
        whole = whole * 10 + (c - b'0') as i64;
    }

    let mut frac: i64 = 0;
    let mut frac_len = 0usize;
    if let Some(fp) = frac_part {
        for &c in fp {
            if !c.is_ascii_digit() {
                return None;
            }
            if frac_len < 2 {
                frac = frac * 10 + (c - b'0') as i64;
                frac_len += 1;
            }
        }
    }
    while frac_len < 2 {
        frac *= 10;
        frac_len += 1;
    }

    let value = i32::try_from(whole * 100 + frac).ok()?;
    Some(sign * value)
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn decode_name(bytes: &[u8]) -> Option<heapless::String<96>> {
    let ascii_end = bytes
        .iter()
        .position(|c| !c.is_ascii())
        .unwrap_or(bytes.len());
    let capped = &bytes[..ascii_end.min(96)]; // longer names marquee on the tile
    if capped.is_empty() {
        return None;
    }
    let mut out = heapless::String::<96>::new();
    out.push_str(core::str::from_utf8(capped).unwrap_or(""))
        .ok();
    Some(out)
}

pub fn fmt_money(value: i32, with_sign: bool) -> heapless::String<24> {
    let mut s = heapless::String::<24>::new();
    if value != 0 && (with_sign || value < 0) {
        s.push(if value > 0 { '+' } else { '-' }).ok();
    }

    let abs = value.abs();
    let whole = (abs / 100) as u32;
    let frac = abs % 100;

    // Build least-significant-first digit buffer
    let mut digits = [0u8; 11];
    let mut nd = 0usize;
    if whole == 0 {
        digits[0] = b'0';
        nd = 1;
    } else {
        let mut w = whole;
        while w > 0 {
            digits[nd] = b'0' + (w % 10) as u8;
            nd += 1;
            w /= 10;
        }
    }

    // Emit most-significant-first, comma every 3 digits
    for i in 0..nd {
        let remaining = nd - 1 - i;
        s.push(digits[remaining] as char).ok();
        if remaining > 0 && remaining.is_multiple_of(3) {
            s.push(',').ok();
        }
    }

    core::fmt::Write::write_fmt(&mut s, format_args!(".{:02}", frac)).ok();
    s
}

pub fn fmt_pct(hundredths: i32) -> heapless::String<16> {
    let mut s = heapless::String::<16>::new();
    if hundredths > 0 {
        s.push('+').ok();
    } else if hundredths < 0 {
        s.push('-').ok();
    }
    let abs = hundredths.abs();
    core::fmt::Write::write_fmt(&mut s, format_args!("{}.{:02}%", abs / 100, abs % 100)).ok();
    s
}

impl fmt::Display for Quote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let money = fmt_money(self.price, false);
        let pct = fmt_pct(self.change_pct);
        write!(f, "{} ({})", money.as_str(), pct.as_str())
    }
}
