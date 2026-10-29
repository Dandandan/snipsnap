pub fn literal_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9u32;
    (0..len)
        .map(|i| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8 ^ i as u8
        })
        .collect()
}

pub fn period_bytes(period: usize, len: usize) -> Vec<u8> {
    (0..len).map(|i| b'a' + (i % period) as u8).collect()
}

pub fn mixed_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| match i & 63 {
            0..=47 => b'0' + (i % 10) as u8,
            _ => ((i * 31) >> 3) as u8,
        })
        .collect()
}

pub fn arrowish_strings(len: usize) -> Vec<u8> {
    const WORDS: &[&[u8]] = &[
        b"customer_id",
        b"order_total",
        b"2026-06-06",
        b"warehouse",
        b"region",
        b"status=closed",
        b"sku",
        b"quantity",
    ];
    let mut out = Vec::with_capacity(len);
    let mut i = 0usize;
    while out.len() < len {
        let word = WORDS[i & (WORDS.len() - 1)];
        let remaining = len - out.len();
        out.extend_from_slice(&word[..word.len().min(remaining)]);
        if out.len() < len {
            out.push(b'\n');
        }
        i = i.wrapping_add(1);
    }
    out
}

/// Pseudo-random, log-like text: low-entropy tokens drawn from a vocabulary,
/// which yields a realistic mix of short literals and short copies.
pub fn log_lines(len: usize) -> Vec<u8> {
    const LEVELS: &[&str] = &["INFO", "WARN", "DEBUG", "ERROR"];
    const SERVICES: &[&str] = &["api", "auth", "billing", "search", "ingest", "web"];
    const PATHS: &[&str] = &[
        "/v1/users",
        "/v1/orders",
        "/health",
        "/v2/search?q=",
        "/static/app.js",
    ];
    let mut state = 0x2545_f491u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut out = Vec::with_capacity(len + 256);
    while out.len() < len {
        let r = next();
        let line = format!(
            "2026-10-{:02}T{:02}:{:02}:{:02}.{:03}Z {} {} request_id={:016x} path={}{} status={} latency_ms={}\n",
            1 + r % 28,
            (r >> 5) % 24,
            (r >> 10) % 60,
            (r >> 16) % 60,
            (r >> 22) % 1000,
            LEVELS[(r >> 32) as usize % LEVELS.len()],
            SERVICES[(r >> 36) as usize % SERVICES.len()],
            next() & 0xffff_ffff,
            PATHS[(r >> 40) as usize % PATHS.len()],
            (r >> 44) % 100,
            [200, 200, 200, 404, 500][(r >> 52) as usize % 5],
            (r >> 56) % 250,
        );
        out.extend_from_slice(line.as_bytes());
    }
    out.truncate(len);
    out
}

/// Little-endian integers with small deltas, as found in columnar data.
pub fn int_column(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut value = 1_000_000u32;
    let mut state = 0x1234_5678u32;
    while out.len() + 4 <= len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        value = value.wrapping_add((state >> 28) & 3);
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.resize(len, 0);
    out
}
