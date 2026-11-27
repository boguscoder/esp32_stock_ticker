//! hq.sinajs.cn `gb_*` rows: [1]=price [2]=change % [4]=change [26]=prev close;
//! extended [21]=price [22]=change % [23]=change, which mirror the close until
//! that session prints a trade. Stamps: [24]=extended [25]=regular. Money
//! scales ×100.

pub struct Symbol {
    pub label: &'static str,
    pub name: &'static str,
}

#[rustfmt::skip]
pub const SYMBOLS: &[Symbol] = &[
    Symbol { label: "VOO",  name: "Vanguard Index Funds S&P 500 ETF USD" },
    Symbol { label: "META", name: "Meta Platforms, Inc." },
    Symbol { label: "MSFT", name: "Microsoft Corporation" },
    Symbol { label: "VXUS", name: "Vanguard Total International Stock Index Fund" },
    Symbol { label: "NATO", name: "Themes Transatlantic Defense ETF" },
    Symbol { label: "NVDA", name: "Nvidia Corporation" },
    Symbol { label: "AMD",  name: "Advanced Micro Devices, Inc." },
    Symbol { label: "FDLO", name: "Fidelity Low Volatility Factor ETF" },
];

pub const SYMBOL_COUNT: usize = SYMBOLS.len();

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct QuoteValue {
    pub price: i32,
    pub change: i32,
    pub change_pct: i32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    pub day: QuoteValue,
    pub ah: Option<QuoteValue>,
}

pub fn parse_quotes(body: &[u8]) -> [Option<Quote>; SYMBOL_COUNT] {
    let mut out = [const { None }; SYMBOL_COUNT];
    for (i, sym) in SYMBOLS.iter().enumerate() {
        out[i] = parse_symbol_quote(body, sym.label);
    }
    out
}

fn symbol_row<'a>(body: &'a [u8], label: &str) -> Option<&'a [u8]> {
    let mut needle = heapless::String::<32>::new();
    needle.push_str("hq_str_gb_").ok()?;
    for c in label.chars() {
        needle.push(c.to_ascii_lowercase()).ok()?;
    }
    needle.push_str("=\"").ok()?;

    let start = find_bytes(body, needle.as_bytes())? + needle.len();
    let end = body[start..].iter().position(|&c| c == b'"')? + start;
    Some(&body[start..end])
}

fn parse_symbol_quote(body: &[u8], label: &str) -> Option<Quote> {
    let row = symbol_row(body, label)?;

    let price = field(row, 1).and_then(parse_fixed)?;

    let prev_close = field(row, 26).and_then(parse_fixed).unwrap_or(0);
    let change = match field(row, 4).and_then(parse_fixed) {
        Some(c) => c,
        None => price - prev_close,
    };
    let change_pct = match field(row, 2).and_then(parse_fixed) {
        Some(p) => p,
        None if prev_close != 0 => (change as i64 * 10_000 / prev_close as i64) as i32,
        None => 0,
    };

    let extended_price = field(row, 21).and_then(parse_fixed).unwrap_or(0);
    let extended_change = field(row, 23).and_then(parse_fixed).unwrap_or(0);
    let ah = if extended_price > 0 && extended_price != price && extended_change != 0 {
        Some(QuoteValue {
            price: extended_price,
            change: extended_change,
            change_pct: field(row, 22).and_then(parse_fixed).unwrap_or(0),
        })
    } else {
        None
    };

    Some(Quote {
        day: QuoteValue {
            price,
            change,
            change_pct,
        },
        ah,
    })
}

fn field(row: &[u8], idx: usize) -> Option<&[u8]> {
    row.split(|&c| c == b',').nth(idx).filter(|f| !f.is_empty())
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
