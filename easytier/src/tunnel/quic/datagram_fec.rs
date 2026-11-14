//! Forward error correction below QUIC.
//!
//! The HTTP3 tunnel carries everything on reliable QUIC streams, so one lost
//! UDP datagram stalls every later packet until QUIC retransmits it. This layer
//! protects the QUIC datagrams themselves: sources leave at once and unchanged
//! apart from an eight-byte trailer, and repair datagrams over a sliding window
//! let the receiver rebuild a lost one without waiting a round trip. QUIC then
//! sees a cleaner path, which is what its retransmission, BBR and the stripe
//! scheduler across parallel connections all read.
//!
//! Everything here is a synchronous state machine; `fec_session` drives it.
//!
//! Wire format, once both ends hold the key the HTTP3 request exchanged inside
//! TLS: every datagram this layer emits is `body ‖ value ‖ tag`, four bytes
//! each. The tag is a keyed hash of the kind, the value and the start of the
//! body, and the value is masked with a second keyed hash, so the trailer is
//! indistinguishable from QUIC ciphertext. A datagram whose trailer does not
//! verify is an ordinary QUIC datagram and passes through untouched; that is
//! also how the two directions switch on without any ordering requirement.
//!
//! - source: the QUIC datagram; value is its sequence number.
//! - repair: nine bytes copied from a recent short-header packet, a masked
//!   `first sequence, count` header, then the coded symbol; value is the
//!   repair number, which also seeds the coefficients.
//! - report: the receiver's counters, from which the sender measures the loss
//!   of its own direction on real traffic.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use reed_solomon_erasure::galois_8::{div, mul, mul_slice_xor};

pub(crate) const TRAILER_SIZE: usize = 8;
/// Larger datagrams are sent unprotected. QUIC on this tunnel stays at 1200.
pub(crate) const MAX_SOURCE_SIZE: usize = 1350;
const PREFIX_SIZE: usize = 9;
const REPAIR_HEADER_SIZE: usize = 5;
const REPORT_BODY_SIZE: usize = 12;
const TAG_PREFIX: usize = 16;

const MIN_WINDOW: usize = 8;
const MAX_WINDOW: usize = 32;
/// A repair that arrives later than QUIC's own retransmission is useless.
const SOURCE_LIFETIME: Duration = Duration::from_millis(150);
/// How long a partly filled allowance waits for more sources.
const TAIL_DELAY: Duration = Duration::from_millis(10);

const RECEIVE_WINDOW: usize = 256;
const MAX_ROWS: usize = 32;
const ROW_LIFETIME: Duration = Duration::from_millis(400);
const REPORT_INTERVAL: Duration = Duration::from_millis(50);

/// Adaptive mode leaves a clean path alone and never exceeds one repair per
/// source.
const ADAPTIVE_ON: f32 = 0.003;
const ADAPTIVE_OFF: f32 = 0.001;
const ADAPTIVE_FLOOR: f32 = 0.05;
const ADAPTIVE_CEILING: f32 = 1.0;
const MIN_SAMPLE_PACKETS: u32 = 32;
const SLOW_SAMPLE_PACKETS: u32 = 8;
const SLOW_SAMPLE_AGE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Source = 1,
    Repair = 2,
    Report = 3,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FecKey([u64; 4]);

impl FecKey {
    pub(crate) fn new(bytes: [u8; 16]) -> Self {
        let a = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let b = u64::from_le_bytes(bytes[8..].try_into().unwrap());
        Self([
            a,
            b,
            a.rotate_left(29) ^ 0x6a09_e667_f3bc_c908,
            b.rotate_left(41) ^ 0xbb67_ae85_84ca_a73b,
        ])
    }

    fn hash(&self, domain: u64, value: u64, data: &[u8]) -> u64 {
        seahash::hash_seeded(
            data,
            self.0[0] ^ domain,
            self.0[1] ^ value,
            self.0[2],
            self.0[3],
        )
    }

    fn tag(&self, kind: Kind, value: u32, body: &[u8]) -> u32 {
        let prefix = &body[..body.len().min(TAG_PREFIX)];
        self.hash(
            kind as u64 | (body.len() as u64) << 8,
            u64::from(value),
            prefix,
        ) as u32
    }

    fn mask(&self, tag: u32, body: &[u8]) -> u32 {
        let prefix = &body[..body.len().min(TAG_PREFIX)];
        (self.hash(0x4d41_534b, u64::from(tag), prefix) >> 32) as u32
    }

    /// Appends the trailer that marks `body` as a datagram of this layer.
    pub(crate) fn seal(&self, kind: Kind, value: u32, body: &mut Vec<u8>) {
        let tag = self.tag(kind, value, body);
        let masked = value ^ self.mask(tag, body);
        body.extend_from_slice(&masked.to_le_bytes());
        body.extend_from_slice(&tag.to_le_bytes());
    }

    /// The kind, value and body length of a datagram of this layer, or `None`
    /// for anything else, which is then QUIC's.
    pub(crate) fn open(&self, datagram: &[u8]) -> Option<(Kind, u32, usize)> {
        let body_len = datagram.len().checked_sub(TRAILER_SIZE)?;
        let (body, trailer) = datagram.split_at(body_len);
        let masked = u32::from_le_bytes(trailer[..4].try_into().unwrap());
        let tag = u32::from_le_bytes(trailer[4..].try_into().unwrap());
        let value = masked ^ self.mask(tag, body);
        [Kind::Source, Kind::Repair, Kind::Report]
            .into_iter()
            .find(|kind| self.tag(*kind, value, body) == tag)
            .map(|kind| (kind, value, body_len))
    }

    fn header_mask(&self, repair: u32) -> [u8; 8] {
        self.hash(0x4844_5253, u64::from(repair), &[]).to_le_bytes()
    }
}

/// The coefficient of the `index`-th source of a repair. Part of the wire
/// format: both ends derive it from the repair number alone.
fn coefficient(repair: u32, index: usize) -> u8 {
    let mut value = (u64::from(repair) << 32) | index as u64;
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^= value >> 31;
    (value % 255 + 1) as u8
}

/// A coded symbol is the little-endian length followed by the datagram; a
/// shorter symbol counts as padded with zeros.
fn symbol_of(datagram: &[u8]) -> Vec<u8> {
    let mut symbol = Vec::with_capacity(datagram.len() + 2);
    symbol.extend_from_slice(&(datagram.len() as u16).to_le_bytes());
    symbol.extend_from_slice(datagram);
    symbol
}

fn datagram_of(symbol: &[u8]) -> Option<&[u8]> {
    let len = usize::from(u16::from_le_bytes(symbol.get(..2)?.try_into().ok()?));
    let datagram = symbol.get(2..2 + len)?;
    (len > 0 && symbol[2 + len..].iter().all(|&byte| byte == 0)).then_some(datagram)
}

fn add_scaled(target: &mut Vec<u8>, factor: u8, source: &[u8]) {
    if target.len() < source.len() {
        target.resize(source.len(), 0);
    }
    mul_slice_xor(factor, source, &mut target[..source.len()]);
}

/// How many recent sources one repair covers. A low ratio needs a long window
/// for every source to be covered at all; a high ratio keeps it short so a
/// repair arrives soon after the loss it mends.
fn window_for(ratio: f32) -> usize {
    ((3.0 / ratio.max(0.01)).round() as usize).clamp(MIN_WINDOW, MAX_WINDOW)
}

struct TxSource {
    seq: u32,
    symbol: Vec<u8>,
    at: Instant,
}

pub(crate) struct Encoder {
    key: FecKey,
    sources: VecDeque<TxSource>,
    next_seq: u32,
    next_repair: u32,
    /// Repairs owed for the sources sent so far, in thousandths of a repair.
    credit: u32,
    /// Since when a source has been waiting for a repair that covers it.
    uncovered_since: Option<Instant>,
    prefix: Option<[u8; PREFIX_SIZE]>,
}

impl Encoder {
    pub(crate) fn new(key: FecKey) -> Self {
        Self {
            key,
            sources: VecDeque::with_capacity(MAX_WINDOW),
            next_seq: rand::random(),
            next_repair: rand::random(),
            credit: 0,
            uncovered_since: None,
            prefix: None,
        }
    }

    /// Turns a QUIC datagram into a source and appends the repairs that are
    /// now due to `repairs`. The source is to be sent first.
    pub(crate) fn source(
        &mut self,
        datagram: &[u8],
        ratio: f32,
        now: Instant,
        repairs: &mut Vec<Vec<u8>>,
    ) -> Vec<u8> {
        self.expire(now);
        let seq = self.next_seq;
        self.next_seq = seq.wrapping_add(1);
        let mut sealed = Vec::with_capacity(datagram.len() + TRAILER_SIZE);
        sealed.extend_from_slice(datagram);
        self.key.seal(Kind::Source, seq, &mut sealed);

        // A repair dressed like the connection's own short-header packets.
        if datagram.len() >= PREFIX_SIZE && datagram[0] & 0x80 == 0 {
            self.prefix = Some(datagram[..PREFIX_SIZE].try_into().unwrap());
        }
        if ratio <= 0.0 {
            // Nothing will cover it; do not let history build up either.
            self.sources.clear();
            self.credit = 0;
            self.uncovered_since = None;
            return sealed;
        }
        if self.sources.len() == MAX_WINDOW {
            self.sources.pop_front();
        }
        self.sources.push_back(TxSource {
            seq,
            symbol: symbol_of(datagram),
            at: now,
        });
        self.uncovered_since.get_or_insert(now);
        self.credit += (ratio * 1000.0).round() as u32;
        while self.credit >= 1000 {
            self.credit -= 1000;
            self.repair(ratio, repairs);
        }
        sealed
    }

    /// Covers a tail that no further source completed an allowance for.
    pub(crate) fn tick(&mut self, ratio: f32, now: Instant, repairs: &mut Vec<Vec<u8>>) {
        self.expire(now);
        if self
            .uncovered_since
            .is_some_and(|since| since + TAIL_DELAY <= now)
        {
            if ratio > 0.0 {
                self.credit = 0;
                self.repair(ratio, repairs);
            }
            self.uncovered_since = None;
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.uncovered_since.map(|since| since + TAIL_DELAY)
    }

    fn expire(&mut self, now: Instant) {
        while self
            .sources
            .front()
            .is_some_and(|source| source.at + SOURCE_LIFETIME <= now)
        {
            self.sources.pop_front();
        }
        if self.sources.is_empty() {
            self.uncovered_since = None;
        }
    }

    fn repair(&mut self, ratio: f32, repairs: &mut Vec<Vec<u8>>) {
        let count = self.sources.len().min(window_for(ratio));
        if count == 0 {
            return;
        }
        let covered = self.sources.range(self.sources.len() - count..);
        let repair = self.next_repair;
        self.next_repair = repair.wrapping_add(1);
        let first = covered.clone().next().unwrap().seq;
        let len = covered
            .clone()
            .map(|source| source.symbol.len())
            .max()
            .unwrap();

        let mut body = Vec::with_capacity(PREFIX_SIZE + REPAIR_HEADER_SIZE + len + TRAILER_SIZE);
        match self.prefix {
            Some(prefix) => body.extend_from_slice(&prefix),
            None => {
                let mut prefix: [u8; PREFIX_SIZE] = rand::random();
                prefix[0] = 0x40 | (prefix[0] & 0x3f);
                body.extend_from_slice(&prefix);
            }
        }
        let mask = self.key.header_mask(repair);
        let mut header = [0; REPAIR_HEADER_SIZE];
        header[..4].copy_from_slice(&first.to_le_bytes());
        header[4] = count as u8;
        body.extend(header.iter().zip(mask).map(|(byte, mask)| byte ^ mask));
        let start = body.len();
        body.resize(start + len, 0);
        for (index, source) in covered.enumerate() {
            mul_slice_xor(
                coefficient(repair, index),
                &source.symbol,
                &mut body[start..start + source.symbol.len()],
            );
        }
        self.key.seal(Kind::Repair, repair, &mut body);
        repairs.push(body);
        self.uncovered_since = None;
    }
}

/// One linear equation over sources the receiver does not have.
struct Row {
    /// Unknown sources and their coefficients, ascending by sequence.
    terms: Vec<(u64, u8)>,
    data: Vec<u8>,
    at: Instant,
}

impl Row {
    fn coefficient(&self, seq: u64) -> Option<u8> {
        self.terms
            .binary_search_by_key(&seq, |term| term.0)
            .ok()
            .map(|index| self.terms[index].1)
    }

    /// Adds `factor` times `other`; equal coefficients cancel.
    fn add(&mut self, factor: u8, other: &Row) {
        add_scaled(&mut self.data, factor, &other.data);
        let mut merged = Vec::with_capacity(self.terms.len() + other.terms.len());
        let (mut left, mut right) = (self.terms.iter().peekable(), other.terms.iter().peekable());
        loop {
            match (left.peek(), right.peek()) {
                (Some(&&(a, value)), Some(&&(b, _))) if a < b => {
                    merged.push((a, value));
                    left.next();
                }
                (Some(&&(a, _)), Some(&&(b, value))) if a > b => {
                    merged.push((b, mul(factor, value)));
                    right.next();
                }
                (Some(&&(a, mine)), Some(&&(_, theirs))) => {
                    let value = mine ^ mul(factor, theirs);
                    if value != 0 {
                        merged.push((a, value));
                    }
                    left.next();
                    right.next();
                }
                (Some(&&term), None) => {
                    merged.push(term);
                    left.next();
                }
                (None, Some(&&(b, value))) => {
                    merged.push((b, mul(factor, value)));
                    right.next();
                }
                (None, None) => break,
            }
        }
        self.terms = merged;
        self.at = self.at.min(other.at);
    }

    fn scale(&mut self, factor: u8) {
        for term in &mut self.terms {
            term.1 = mul(term.1, factor);
        }
        for byte in &mut self.data {
            *byte = mul(*byte, factor);
        }
    }
}

struct Slot {
    seq: u64,
    symbol: Vec<u8>,
}

/// What the receiver has counted, for the report to the sender.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// The sender's highest sequence number seen, directly or in a repair.
    pub highest: u32,
    /// Sources that arrived themselves.
    pub received: u32,
    /// Sources rebuilt from repairs.
    pub recovered: u32,
}

impl Report {
    fn encode(self) -> [u8; REPORT_BODY_SIZE] {
        let mut body = [0; REPORT_BODY_SIZE];
        body[..4].copy_from_slice(&self.highest.to_le_bytes());
        body[4..8].copy_from_slice(&self.received.to_le_bytes());
        body[8..].copy_from_slice(&self.recovered.to_le_bytes());
        body
    }

    fn decode(body: &[u8]) -> Option<Self> {
        let body = body.get(..REPORT_BODY_SIZE)?;
        let field = |at: usize| u32::from_le_bytes(body[at..at + 4].try_into().unwrap());
        Some(Self {
            highest: field(0),
            received: field(4),
            recovered: field(8),
        })
    }
}

pub(crate) struct Decoder {
    key: FecKey,
    highest: Option<u64>,
    slots: Vec<Option<Slot>>,
    rows: Vec<Row>,
    recovered: VecDeque<Vec<u8>>,
    received_count: u32,
    recovered_count: u32,
    reported: Option<(u32, Instant)>,
    next_report: u32,
}

impl Decoder {
    pub(crate) fn new(key: FecKey) -> Self {
        Self {
            key,
            highest: None,
            slots: (0..RECEIVE_WINDOW).map(|_| None).collect(),
            rows: Vec::new(),
            recovered: VecDeque::new(),
            received_count: 0,
            recovered_count: 0,
            reported: None,
            next_report: rand::random(),
        }
    }

    /// The 64-bit sequence a 32-bit wire value stands for near the high-water
    /// mark.
    fn expand(&self, seq: u32) -> u64 {
        let Some(highest) = self.highest else {
            return u64::from(seq) + (1 << 32);
        };
        let delta = seq.wrapping_sub(highest as u32) as i32;
        highest.wrapping_add_signed(i64::from(delta))
    }

    fn known(&self, seq: u64) -> Option<&[u8]> {
        self.slots[seq as usize % RECEIVE_WINDOW]
            .as_ref()
            .filter(|slot| slot.seq == seq)
            .map(|slot| slot.symbol.as_slice())
    }

    fn in_window(&self, seq: u64) -> bool {
        self.highest
            .is_none_or(|highest| seq + RECEIVE_WINDOW as u64 > highest)
    }

    fn advance(&mut self, seq: u64) {
        if self.highest.is_none_or(|highest| seq > highest) {
            self.highest = Some(seq);
            // An equation over a source that left the window can never close.
            self.rows
                .retain(|row| row.terms[0].0 + RECEIVE_WINDOW as u64 > seq);
        }
    }

    fn store(&mut self, seq: u64, symbol: Vec<u8>) {
        self.slots[seq as usize % RECEIVE_WINDOW] = Some(Slot { seq, symbol });
        let mut touched = false;
        for row in &mut self.rows {
            if let Ok(index) = row.terms.binary_search_by_key(&seq, |term| term.0) {
                let (_, factor) = row.terms.remove(index);
                let symbol = &self.slots[seq as usize % RECEIVE_WINDOW]
                    .as_ref()
                    .unwrap()
                    .symbol;
                add_scaled(&mut row.data, factor, symbol);
                touched = true;
            }
        }
        if touched {
            self.solve();
        }
    }

    /// A source that arrived itself. `datagram` is the QUIC datagram without
    /// the trailer.
    pub(crate) fn source(&mut self, seq: u32, datagram: &[u8], now: Instant) {
        self.expire(now);
        self.received_count = self.received_count.wrapping_add(1);
        let seq = self.expand(seq);
        if !self.in_window(seq) || datagram.len() > MAX_SOURCE_SIZE {
            return;
        }
        self.advance(seq);
        if self.known(seq).is_none() {
            self.store(seq, symbol_of(datagram));
        }
    }

    /// A repair datagram without its trailer.
    pub(crate) fn repair(&mut self, repair: u32, body: &[u8], now: Instant) {
        self.expire(now);
        let Some(coded) = body.get(PREFIX_SIZE + REPAIR_HEADER_SIZE..) else {
            return;
        };
        let mask = self.key.header_mask(repair);
        let mut header = [0; REPAIR_HEADER_SIZE];
        for (index, byte) in header.iter_mut().enumerate() {
            *byte = body[PREFIX_SIZE + index] ^ mask[index];
        }
        let first = self.expand(u32::from_le_bytes(header[..4].try_into().unwrap()));
        let count = usize::from(header[4]);
        if !(1..=MAX_WINDOW).contains(&count)
            || !(3..=MAX_SOURCE_SIZE + 2).contains(&coded.len())
            || !self.in_window(first)
        {
            return;
        }
        // The sender got this far, whether or not those sources arrived.
        self.advance(first + count as u64 - 1);
        if !self.in_window(first) {
            return;
        }
        let mut row = Row {
            terms: Vec::new(),
            data: coded.to_vec(),
            at: now,
        };
        for index in 0..count {
            let seq = first + index as u64;
            let factor = coefficient(repair, index);
            match self.known(seq) {
                Some(symbol) => add_scaled(&mut row.data, factor, symbol),
                None => row.terms.push((seq, factor)),
            }
        }
        if row.terms.is_empty() {
            return;
        }
        if self.rows.len() == MAX_ROWS {
            let oldest = (0..self.rows.len())
                .min_by_key(|&index| self.rows[index].at)
                .unwrap();
            self.rows.remove(oldest);
        }
        self.rows.push(row);
        self.solve();
    }

    /// Reduces the equations against each other and takes every source that
    /// is left alone in one.
    fn solve(&mut self) {
        loop {
            let mut index = 0;
            while index < self.rows.len() {
                let Some(&(pivot, factor)) = self.rows[index].terms.first() else {
                    self.rows.remove(index);
                    continue;
                };
                self.rows[index].scale(div(1, factor));
                let reduced = std::mem::replace(
                    &mut self.rows[index],
                    Row {
                        terms: Vec::new(),
                        data: Vec::new(),
                        at: Instant::now(),
                    },
                );
                for (other, row) in self.rows.iter_mut().enumerate() {
                    if other != index
                        && let Some(factor) = row.coefficient(pivot)
                    {
                        row.add(factor, &reduced);
                    }
                }
                self.rows[index] = reduced;
                index += 1;
            }
            let Some(index) = self.rows.iter().position(|row| row.terms.len() == 1) else {
                return;
            };
            let row = self.rows.remove(index);
            let seq = row.terms[0].0;
            let Some(datagram) = datagram_of(&row.data) else {
                // A combination that is not a datagram poisons whatever it
                // was reduced into.
                self.rows.clear();
                return;
            };
            self.recovered.push_back(datagram.to_vec());
            self.recovered_count = self.recovered_count.wrapping_add(1);
            let len = datagram.len() + 2;
            let mut symbol = row.data;
            symbol.truncate(len);
            // The reduction already removed this source from every other
            // equation; it only has to be known to later repairs.
            self.slots[seq as usize % RECEIVE_WINDOW] = Some(Slot { seq, symbol });
        }
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        self.rows.retain(|row| row.at + ROW_LIFETIME > now);
    }

    /// A datagram rebuilt from repairs, for QUIC to receive.
    pub(crate) fn take_recovered(&mut self) -> Option<Vec<u8>> {
        self.recovered.pop_front()
    }

    pub(crate) fn report(&self) -> Option<Report> {
        Some(Report {
            highest: self.highest? as u32,
            received: self.received_count,
            recovered: self.recovered_count,
        })
    }

    /// When the counters should next go to the sender, if they changed.
    pub(crate) fn report_deadline(&self, now: Instant) -> Option<Instant> {
        let received = self.report()?.received;
        match self.reported {
            Some((reported, _)) if reported == received => None,
            Some((_, at)) => Some(at + REPORT_INTERVAL),
            None => Some(now),
        }
    }

    /// The report datagram to send now, if one is due.
    pub(crate) fn take_report(&mut self, now: Instant) -> Option<Vec<u8>> {
        if self.report_deadline(now)? > now {
            return None;
        }
        let report = self.report()?;
        self.reported = Some((report.received, now));
        let mut body: Vec<u8> = Vec::with_capacity(PREFIX_SIZE + REPORT_BODY_SIZE + 16);
        let mut prefix: [u8; PREFIX_SIZE] = rand::random();
        prefix[0] = 0x40 | (prefix[0] & 0x3f);
        body.extend_from_slice(&prefix);
        let counter = self.next_report;
        self.next_report = counter.wrapping_add(1);
        let mask = self.key.hash(0x5250_5254, u64::from(counter), &[]);
        let mask = [mask.to_le_bytes(), (!mask).rotate_left(23).to_le_bytes()].concat();
        body.extend(
            report
                .encode()
                .iter()
                .zip(&mask)
                .map(|(byte, mask)| byte ^ mask),
        );
        // Vary the length the way acknowledgement-only packets vary.
        let padding: [u8; 16] = rand::random();
        body.extend_from_slice(&padding[..usize::from(padding[0] & 0x0f)]);
        self.key.seal(Kind::Report, counter, &mut body);
        Some(body)
    }

    /// The counters in a report datagram without its trailer.
    pub(crate) fn parse_report(key: &FecKey, counter: u32, body: &[u8]) -> Option<Report> {
        let masked = body.get(PREFIX_SIZE..PREFIX_SIZE + REPORT_BODY_SIZE)?;
        let mask = key.hash(0x5250_5254, u64::from(counter), &[]);
        let mask = [mask.to_le_bytes(), (!mask).rotate_left(23).to_le_bytes()].concat();
        let plain: Vec<u8> = masked
            .iter()
            .zip(&mask)
            .map(|(byte, mask)| byte ^ mask)
            .collect();
        Report::decode(&plain)
    }
}

/// How much redundancy the local end sends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RatioMode {
    /// Follow the loss the peer measures on this direction.
    Adaptive,
    /// Repairs per source, fixed.
    Fixed(f32),
}

impl RatioMode {
    /// `percent` is the configured redundancy; zero selects adaptive mode.
    pub(crate) fn from_percent(percent: u32) -> Self {
        match percent {
            0 => Self::Adaptive,
            percent => Self::Fixed(percent.min(100) as f32 / 100.0),
        }
    }
}

/// Measures the loss of the local sending direction from the peer's reports
/// and derives the redundancy from it.
pub(crate) struct RatioController {
    mode: RatioMode,
    base: Option<(Report, Instant)>,
    /// Smoothed loss before recovery.
    loss: f32,
    /// Smoothed loss the repairs did not mend.
    residual: f32,
    /// Raised while losses come in bursts the plain rule does not cover.
    boost: f32,
    active: bool,
    measured: bool,
}

impl RatioController {
    pub(crate) fn new(mode: RatioMode) -> Self {
        Self {
            mode,
            base: None,
            loss: 0.0,
            residual: 0.0,
            boost: 1.0,
            active: false,
            measured: false,
        }
    }

    pub(crate) fn on_report(&mut self, report: Report, now: Instant) {
        let Some((base, since)) = self.base else {
            self.base = Some((report, now));
            return;
        };
        let expected = report.highest.wrapping_sub(base.highest);
        // A reordered report, or a peer that restarted its counters.
        if expected > u32::MAX / 2 {
            if now.duration_since(since) > SLOW_SAMPLE_AGE {
                self.base = Some((report, now));
            }
            return;
        }
        if expected < MIN_SAMPLE_PACKETS
            && !(expected >= SLOW_SAMPLE_PACKETS && now.duration_since(since) >= SLOW_SAMPLE_AGE)
        {
            return;
        }
        let received = report.received.wrapping_sub(base.received);
        let recovered = report.recovered.wrapping_sub(base.recovered);
        self.base = Some((report, now));
        let expected = expected as f32;
        let loss = (1.0 - received as f32 / expected).clamp(0.0, 1.0);
        let residual = (1.0 - (received as f32 + recovered as f32) / expected).clamp(0.0, 1.0);

        // Rise quickly, so a path that turns bad is covered within a few
        // reports, and fall slowly, so one clean interval does not undo it.
        let smooth = |old: f32, new: f32| old + (new - old) * if new > old { 0.5 } else { 0.15 };
        self.loss = if self.measured {
            smooth(self.loss, loss)
        } else {
            loss
        };
        self.residual = if self.measured {
            smooth(self.residual, residual)
        } else {
            residual
        };
        self.measured = true;

        if self.active && residual > 0.005 {
            self.boost = (self.boost * 1.25).min(2.5);
        } else if residual < 0.001 {
            self.boost = (self.boost * 0.97).max(1.0);
        }
        self.active = if self.active {
            self.loss >= ADAPTIVE_OFF
        } else {
            self.loss >= ADAPTIVE_ON
        };
        if !self.active {
            self.boost = 1.0;
        }
    }

    /// Repairs to send per source right now.
    pub(crate) fn ratio(&self) -> f32 {
        match self.mode {
            RatioMode::Fixed(ratio) => ratio,
            // Until the first sample, a new connection on a path that may be
            // bad gets the floor rather than nothing.
            RatioMode::Adaptive if !self.measured => ADAPTIVE_FLOOR,
            RatioMode::Adaptive if !self.active => 0.0,
            RatioMode::Adaptive => {
                // Random loss `p` needs slightly more than p / (1 - p) repairs
                // per source; the margin covers the variance within a window.
                let needed = self.loss / (1.0 - self.loss).max(0.05);
                ((needed * 1.6 + 0.04) * self.boost).clamp(ADAPTIVE_FLOOR, ADAPTIVE_CEILING)
            }
        }
    }

    /// The measured loss of the sending direction, once a sample exists.
    pub(crate) fn loss(&self) -> Option<f32> {
        self.measured.then_some(self.loss)
    }

    pub(crate) fn residual(&self) -> Option<f32> {
        self.measured.then_some(self.residual)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> FecKey {
        FecKey::new(*b"0123456789abcdef")
    }

    fn datagram(index: usize) -> Vec<u8> {
        let len = 40 + (index * 37) % 1100;
        (0..len)
            .map(|byte| (byte as u8).wrapping_mul(31) ^ index as u8)
            .enumerate()
            .map(|(at, byte)| if at == 0 { byte & 0x7f } else { byte })
            .collect()
    }

    /// Feeds what a lossy path lets through to a decoder and returns the
    /// datagrams QUIC would get.
    fn deliver(decoder: &mut Decoder, datagram: &[u8], now: Instant) -> Vec<Vec<u8>> {
        let key = key();
        let mut out = Vec::new();
        match key.open(datagram) {
            Some((Kind::Source, seq, len)) => {
                decoder.source(seq, &datagram[..len], now);
                out.push(datagram[..len].to_vec());
            }
            Some((Kind::Repair, repair, len)) => decoder.repair(repair, &datagram[..len], now),
            Some((Kind::Report, ..)) => unreachable!(),
            None => out.push(datagram.to_vec()),
        }
        while let Some(recovered) = decoder.take_recovered() {
            out.push(recovered);
        }
        out
    }

    #[test]
    fn trailers_identify_each_kind_and_leave_quic_alone() {
        let key = key();
        for kind in [Kind::Source, Kind::Repair, Kind::Report] {
            for body_len in [0, 1, 15, 16, 17, 1200] {
                let body: Vec<u8> = (0..body_len).map(|byte| byte as u8).collect();
                let mut sealed = body.clone();
                key.seal(kind, 0xdead_beef, &mut sealed);
                assert_eq!(sealed.len(), body_len + TRAILER_SIZE);
                assert_eq!(key.open(&sealed), Some((kind, 0xdead_beef, body_len)));
                // Another key, or damage to the start, is not this layer's.
                assert_eq!(FecKey::new([7; 16]).open(&sealed), None);
                if body_len > 0 {
                    let mut damaged = sealed.clone();
                    damaged[0] ^= 1;
                    assert_eq!(key.open(&damaged), None);
                }
            }
        }
        for index in 0..2000 {
            assert_eq!(key.open(&datagram(index)), None);
        }
        assert_eq!(key.open(&[0; 7]), None);
    }

    #[test]
    fn sources_keep_the_datagram_and_only_grow_by_the_trailer() {
        let mut encoder = Encoder::new(key());
        let mut repairs = Vec::new();
        let original = datagram(3);
        let sealed = encoder.source(&original, 0.2, Instant::now(), &mut repairs);
        assert_eq!(&sealed[..original.len()], original.as_slice());
        assert_eq!(sealed.len(), original.len() + TRAILER_SIZE);
        assert!(repairs.is_empty());
    }

    #[test]
    fn the_ratio_sets_how_many_repairs_follow_the_sources() {
        for (ratio, expected) in [(0.0, 0), (0.05, 50), (0.2, 200), (0.5, 500), (1.0, 1000)] {
            let mut encoder = Encoder::new(key());
            let mut repairs = Vec::new();
            let now = Instant::now();
            for index in 0..1000 {
                encoder.source(&datagram(index), ratio, now, &mut repairs);
            }
            assert!(
                (repairs.len() as i32 - expected).abs() <= 1,
                "ratio {ratio}: {} repairs",
                repairs.len()
            );
            for repair in &repairs {
                assert!(matches!(key().open(repair), Some((Kind::Repair, ..))));
                assert!(repair.len() <= 1200 + 2 + PREFIX_SIZE + REPAIR_HEADER_SIZE + TRAILER_SIZE);
            }
        }
    }

    #[test]
    fn a_tail_is_covered_after_the_delay_and_an_idle_path_sends_nothing() {
        let mut encoder = Encoder::new(key());
        let mut repairs = Vec::new();
        let now = Instant::now();
        encoder.source(&datagram(1), 0.1, now, &mut repairs);
        assert!(repairs.is_empty());
        assert_eq!(encoder.deadline(), Some(now + TAIL_DELAY));
        encoder.tick(0.1, now + TAIL_DELAY / 2, &mut repairs);
        assert!(repairs.is_empty());
        encoder.tick(0.1, now + TAIL_DELAY, &mut repairs);
        assert_eq!(repairs.len(), 1);
        assert_eq!(encoder.deadline(), None);
        encoder.tick(0.1, now + TAIL_DELAY * 10, &mut repairs);
        assert_eq!(repairs.len(), 1);

        // Adaptive mode on a clean path: no history and no repairs at all.
        let mut encoder = Encoder::new(key());
        let mut repairs = Vec::new();
        encoder.source(&datagram(2), 0.0, now, &mut repairs);
        encoder.tick(0.0, now + TAIL_DELAY, &mut repairs);
        assert!(repairs.is_empty());
        assert_eq!(encoder.deadline(), None);
    }

    #[test]
    fn a_single_loss_is_rebuilt_from_the_next_repair() {
        let mut encoder = Encoder::new(key());
        let mut decoder = Decoder::new(key());
        let now = Instant::now();
        let mut delivered = Vec::new();
        for index in 0..5 {
            let mut repairs = Vec::new();
            let source = encoder.source(&datagram(index), 0.2, now, &mut repairs);
            if index != 2 {
                delivered.extend(deliver(&mut decoder, &source, now));
            }
            for repair in repairs {
                delivered.extend(deliver(&mut decoder, &repair, now));
            }
        }
        let mut expected: Vec<_> = (0..5).map(datagram).collect();
        delivered.sort();
        expected.sort();
        assert_eq!(delivered, expected);
        let report = decoder.report().unwrap();
        assert_eq!((report.received, report.recovered), (4, 1));
    }

    #[test]
    fn a_burst_is_rebuilt_once_enough_repairs_cover_it() {
        let mut encoder = Encoder::new(key());
        let mut decoder = Decoder::new(key());
        let now = Instant::now();
        let mut delivered = Vec::new();
        for index in 0..12 {
            let mut repairs = Vec::new();
            let source = encoder.source(&datagram(index), 0.5, now, &mut repairs);
            // Three in a row are lost, and so is the repair among them.
            if !(3..6).contains(&index) {
                delivered.extend(deliver(&mut decoder, &source, now));
                for repair in repairs {
                    delivered.extend(deliver(&mut decoder, &repair, now));
                }
            }
        }
        let mut expected: Vec<_> = (0..12).map(datagram).collect();
        delivered.sort();
        expected.sort();
        assert_eq!(delivered, expected);
    }

    #[test]
    fn a_late_source_closes_the_equations_it_was_missing_from() {
        let mut encoder = Encoder::new(key());
        let mut decoder = Decoder::new(key());
        let now = Instant::now();
        let mut sources = Vec::new();
        let mut repairs = Vec::new();
        for index in 0..5 {
            sources.push(encoder.source(&datagram(index), 0.2, now, &mut repairs));
        }
        assert_eq!(repairs.len(), 1);
        // Two are missing when the repair arrives: one equation, two unknowns.
        for index in [0, 1, 4] {
            deliver(&mut decoder, &sources[index], now);
        }
        assert!(deliver(&mut decoder, &repairs[0], now).is_empty());
        // One of them was only reordered; the other is then recoverable.
        let delivered = deliver(&mut decoder, &sources[2], now);
        assert_eq!(delivered, vec![datagram(2), datagram(3)]);
    }

    #[test]
    fn random_loss_is_mostly_recovered_at_a_matching_ratio() {
        // A fixed generator keeps the loss pattern, and so the assertion, the
        // same on every run.
        let mut state = 0x1234_5678_9abc_def0_u64;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for (loss, ratio, limit) in [(0.02, 0.1, 0.004), (0.1, 0.25, 0.02), (0.2, 0.5, 0.03)] {
            let mut encoder = Encoder::new(key());
            let mut decoder = Decoder::new(key());
            let mut now = Instant::now();
            let total = 20_000;
            let mut delivered = 0usize;
            for index in 0..total {
                now += Duration::from_micros(500);
                let mut repairs = Vec::new();
                let source = encoder.source(&datagram(index), ratio, now, &mut repairs);
                for wire in std::iter::once(source).chain(repairs) {
                    if random() >= loss {
                        for datagram in deliver(&mut decoder, &wire, now) {
                            assert!(!datagram.is_empty());
                            delivered += 1;
                        }
                    }
                }
            }
            let residual = 1.0 - delivered as f64 / total as f64;
            assert!(
                residual < limit,
                "loss {loss} ratio {ratio}: residual {residual}"
            );
            let report = decoder.report().unwrap();
            let measured = 1.0 - f64::from(report.received) / total as f64;
            assert!((measured - loss).abs() < 0.01, "measured {measured}");
        }
    }

    #[test]
    fn sequence_numbers_wrap_without_losing_the_window() {
        let mut encoder = Encoder::new(key());
        encoder.next_seq = u32::MAX - 3;
        let mut decoder = Decoder::new(key());
        let now = Instant::now();
        let mut delivered = Vec::new();
        for index in 0..10 {
            let mut repairs = Vec::new();
            let source = encoder.source(&datagram(index), 0.2, now, &mut repairs);
            if index != 4 && index != 8 {
                delivered.extend(deliver(&mut decoder, &source, now));
            }
            for repair in repairs {
                delivered.extend(deliver(&mut decoder, &repair, now));
            }
        }
        assert_eq!(delivered.len(), 10);
    }

    #[test]
    fn stale_and_foreign_repairs_do_not_produce_datagrams() {
        let mut encoder = Encoder::new(key());
        let mut decoder = Decoder::new(key());
        let now = Instant::now();
        let mut repairs = Vec::new();
        let mut sources = Vec::new();
        for index in 0..5 {
            sources.push(encoder.source(&datagram(index), 0.2, now, &mut repairs));
        }
        let (_, repair, len) = key().open(&repairs[0]).unwrap();
        // The equation expires before anything completes it.
        deliver(&mut decoder, &sources[0], now);
        decoder.repair(repair, &repairs[0][..len], now);
        decoder.expire(now + ROW_LIFETIME);
        for index in [1, 3, 4] {
            assert_eq!(
                deliver(&mut decoder, &sources[index], now + ROW_LIFETIME).len(),
                1
            );
        }
        // Truncated repairs are ignored.
        decoder.repair(repair, &repairs[0][..PREFIX_SIZE + 2], now);
        assert!(decoder.take_recovered().is_none());
    }

    #[test]
    fn reports_round_trip_and_are_paced() {
        let mut encoder = Encoder::new(key());
        let mut decoder = Decoder::new(key());
        let now = Instant::now();
        assert!(decoder.take_report(now).is_none());
        let mut repairs = Vec::new();
        let source = encoder.source(&datagram(0), 0.2, now, &mut repairs);
        deliver(&mut decoder, &source, now);
        let wire = decoder.take_report(now).unwrap();
        let (kind, counter, len) = key().open(&wire).unwrap();
        assert_eq!(kind, Kind::Report);
        assert_eq!(
            Decoder::parse_report(&key(), counter, &wire[..len]),
            decoder.report()
        );
        // Nothing new: no report. Something new: only after the interval.
        assert!(decoder.take_report(now + REPORT_INTERVAL).is_none());
        let source = encoder.source(&datagram(1), 0.2, now, &mut repairs);
        deliver(&mut decoder, &source, now);
        assert!(decoder.take_report(now + REPORT_INTERVAL / 2).is_none());
        assert!(decoder.take_report(now + REPORT_INTERVAL).is_some());
    }

    fn report(highest: u32, received: u32, recovered: u32) -> Report {
        Report {
            highest,
            received,
            recovered,
        }
    }

    #[test]
    fn adaptive_ratio_follows_measured_loss_and_rests_on_a_clean_path() {
        let mut controller = RatioController::new(RatioMode::Adaptive);
        let mut now = Instant::now();
        assert_eq!(controller.ratio(), ADAPTIVE_FLOOR);
        assert_eq!(controller.loss(), None);
        controller.on_report(report(1000, 1000, 0), now);
        assert_eq!(controller.loss(), None);

        // Too few packets say nothing yet.
        controller.on_report(report(1010, 1005, 0), now);
        assert_eq!(controller.loss(), None);

        // 10% loss, all of it recovered.
        let (mut highest, mut received, mut recovered) = (1000, 1000, 0);
        for _ in 0..10 {
            now += Duration::from_millis(100);
            highest += 1000;
            received += 900;
            recovered += 100;
            controller.on_report(report(highest, received, recovered), now);
        }
        let loss = controller.loss().unwrap();
        assert!((loss - 0.1).abs() < 0.005, "loss {loss}");
        let ratio = controller.ratio();
        assert!((0.2..0.25).contains(&ratio), "ratio {ratio}");
        assert!(controller.residual().unwrap() < 0.001);

        // The same loss arriving in bursts the repairs do not mend raises it.
        for _ in 0..5 {
            now += Duration::from_millis(100);
            highest += 1000;
            received += 900;
            recovered += 50;
            controller.on_report(report(highest, received, recovered), now);
        }
        assert!(controller.ratio() > ratio * 1.5);

        // A path that turns clean winds the redundancy down to nothing.
        for _ in 0..60 {
            now += Duration::from_millis(100);
            highest += 1000;
            received += 1000;
            controller.on_report(report(highest, received, recovered), now);
        }
        assert_eq!(controller.ratio(), 0.0);
        assert!(controller.loss().unwrap() < ADAPTIVE_OFF);
    }

    #[test]
    fn adaptive_ratio_is_bounded_and_fixed_mode_ignores_reports() {
        let mut controller = RatioController::new(RatioMode::Adaptive);
        let mut now = Instant::now();
        controller.on_report(report(0, 0, 0), now);
        let (mut highest, mut received) = (0, 0);
        for _ in 0..20 {
            now += Duration::from_millis(100);
            highest += 1000;
            received += 400;
            controller.on_report(report(highest, received, 0), now);
        }
        assert_eq!(controller.ratio(), ADAPTIVE_CEILING);

        // A low-rate flow is sampled once it has been slow for long enough.
        let mut controller = RatioController::new(RatioMode::Adaptive);
        controller.on_report(report(0, 0, 0), now);
        controller.on_report(report(10, 8, 0), now + Duration::from_secs(1));
        assert_eq!(controller.loss(), None);
        controller.on_report(report(10, 8, 0), now + SLOW_SAMPLE_AGE);
        assert!((controller.loss().unwrap() - 0.2).abs() < 1e-6);

        assert_eq!(RatioMode::from_percent(0), RatioMode::Adaptive);
        assert_eq!(RatioMode::from_percent(250), RatioMode::Fixed(1.0));
        let mut controller = RatioController::new(RatioMode::from_percent(30));
        controller.on_report(report(0, 0, 0), now);
        controller.on_report(report(1000, 500, 0), now);
        assert_eq!(controller.ratio(), 0.3);
        assert!((controller.loss().unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn window_grows_as_the_ratio_falls() {
        assert_eq!(window_for(1.0), MIN_WINDOW);
        assert_eq!(window_for(0.5), MIN_WINDOW);
        assert_eq!(window_for(0.2), 15);
        assert_eq!(window_for(0.05), MAX_WINDOW);
        assert_eq!(window_for(0.0), MAX_WINDOW);
    }
}
