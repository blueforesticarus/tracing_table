//! CODEX GENERATED
//!
//! dump and replay for benchmark
//!
//! Version 2 input capture: consecutive MessagePack arrays in a checksummed LZ4 frame.
//! This deliberately replaces v1's storage snapshot. No table layouts or extracted
//! arguments enter this format; sequence numbers, scalar bits, and timestamps survive replay.
use super::{
    FieldValues, InputValue, RecordingLayer, SpanId, Time, log_callsite_adapter,
    template::CompiledTemplate,
    value::{InputVisitor, visit_values},
};
use lz4_flex::frame::{BlockMode, BlockSize, FrameDecoder, FrameEncoder, FrameInfo};
use parking_lot::Mutex;
use rmpv::Value as Mp;
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    sync::{Arc, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{Metadata, Subscriber, field::FieldSet};
use tracing_core::{Callsite, Kind, callsite::Identifier};
use tracing_log::NormalizeEvent;
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn array(values: impl IntoIterator<Item = Mp>) -> Mp {
    Mp::Array(values.into_iter().collect())
}
fn optional(value: Option<u64>) -> Mp {
    value.map(Mp::from).unwrap_or(Mp::Nil)
}
fn text(value: Option<&str>) -> Mp {
    value.map(Mp::from).unwrap_or(Mp::Nil)
}

// Numeric payloads use explicit little-endian bytes: MessagePack integers alone
// cannot distinguish signedness, represent 128 bits, or preserve every NaN bit.
fn encode_value(value: &Option<InputValue>) -> Mp {
    let Some(value) = value else {
        return Mp::Nil;
    };
    let (tag, payload) = match value {
        InputValue::F64(v) => ("f64", Mp::Binary(v.to_le_bytes().to_vec())),
        InputValue::I64(v) => ("i64", Mp::Binary(v.to_le_bytes().to_vec())),
        InputValue::U64(v) => ("u64", Mp::Binary(v.to_le_bytes().to_vec())),
        InputValue::I128(v) => ("i128", Mp::Binary(v.to_le_bytes().to_vec())),
        InputValue::U128(v) => ("u128", Mp::Binary(v.to_le_bytes().to_vec())),
        InputValue::Bool(v) => ("bool", (*v).into()),
        InputValue::Text(v) => ("text", v.as_str().into()),
        InputValue::Bytes(v) => ("bytes", Mp::Binary(v.clone())),
    };
    array([tag.into(), payload])
}
fn encode_fields(values: &[Option<InputValue>]) -> Mp {
    array(values.iter().map(encode_value))
}
fn items(value: &Mp) -> io::Result<&[Mp]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid("expected array"))
}
fn string(value: &Mp) -> io::Result<&str> {
    value.as_str().ok_or_else(|| invalid("expected UTF-8 text"))
}
fn number(value: &Mp) -> io::Result<u64> {
    value
        .as_u64()
        .ok_or_else(|| invalid("expected unsigned integer"))
}
fn opt_number(value: &Mp) -> io::Result<Option<u64>> {
    if value.is_nil() {
        Ok(None)
    } else {
        number(value).map(Some)
    }
}
fn binary<const N: usize>(value: &Mp) -> io::Result<[u8; N]> {
    value
        .as_slice()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| invalid("invalid scalar width"))
}
fn decode_value(value: &Mp) -> io::Result<Option<InputValue>> {
    if value.is_nil() {
        return Ok(None);
    }
    let v = items(value)?;
    if v.len() != 2 {
        return Err(invalid("invalid value"));
    }
    Ok(Some(match string(&v[0])? {
        "f64" => InputValue::F64(u64::from_le_bytes(binary(&v[1])?)),
        "i64" => InputValue::I64(i64::from_le_bytes(binary(&v[1])?)),
        "u64" => InputValue::U64(u64::from_le_bytes(binary(&v[1])?)),
        "i128" => InputValue::I128(i128::from_le_bytes(binary(&v[1])?)),
        "u128" => InputValue::U128(u128::from_le_bytes(binary(&v[1])?)),
        "bool" => InputValue::Bool(v[1].as_bool().ok_or_else(|| invalid("expected bool"))?),
        "text" => InputValue::Text(string(&v[1])?.to_owned()),
        "bytes" => InputValue::Bytes(
            v[1].as_slice()
                .ok_or_else(|| invalid("expected bytes"))?
                .to_vec(),
        ),
        _ => return Err(invalid("unknown value type")),
    }))
}
fn decode_fields(value: &Mp) -> io::Result<Vec<Option<InputValue>>> {
    items(value)?.iter().map(decode_value).collect()
}

struct CountWriter<W> {
    inner: W,
    bytes: u64,
}
impl<W: Write> Write for CountWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.inner.write(bytes)?;
        self.bytes += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
// FrameDecoder treats physical EOF before an end marker as a clean EOF. Translate
// it to InvalidData so a missing LZ4 trailer cannot masquerade as a complete fixture.
struct FrameInput<R>(R);
impl<R: Read> Read for FrameInput<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.0.read(bytes)?;
        if count == 0 && !bytes.is_empty() {
            return Err(invalid("truncated LZ4 frame"));
        }
        Ok(count)
    }
}
struct CountReader<R> {
    inner: R,
    bytes: u64,
}
impl<R: Read> Read for CountReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(bytes)?;
        self.bytes += count as u64;
        Ok(count)
    }
}

/// Cloneable capture handle and tracing layer. Call `finish` explicitly to write
/// the completeness footer and LZ4 trailer; callbacks after finish are ignored.
pub struct Capture<W: Write + Send> {
    state: Arc<Mutex<CaptureState<W>>>,
}
impl<W: Write + Send> Clone for Capture<W> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}
struct CaptureState<W: Write> {
    output: Option<CountWriter<FrameEncoder<W>>>,
    sites: HashMap<usize, u64>,
    records: u64,
    next_span: u64,
    started: Instant,
    shift: u8,
    error: Option<String>,
    finished_bytes: u64,
    sampled_timestamp: u64,
}
impl<W: Write> CaptureState<W> {
    fn write(&mut self, value: Mp) -> io::Result<()> {
        rmpv::encode::write_value(self.output.as_mut().expect("capture open"), &value)
            .map_err(io::Error::other)
    }
    fn site(&mut self, metadata: &'static Metadata<'static>, is_log: bool) -> io::Result<u64> {
        let key = metadata as *const Metadata<'static> as usize;
        if let Some(id) = self.sites.get(&key) {
            return Ok(*id);
        }
        let id = self.sites.len() as u64;
        let template = if metadata.is_event() && metadata.fields().field("message").is_some() {
            super::template::lookup(
                metadata.file().unwrap_or("<unknown>"),
                metadata.line().unwrap_or(0),
                is_log,
                metadata.module_path(),
            )
        } else {
            None
        };
        self.write(array([
            "callsite".into(),
            id.into(),
            metadata.name().into(),
            metadata.target().into(),
            metadata.level().to_string().into(),
            metadata.is_event().into(),
            text(metadata.file()),
            optional(metadata.line().map(u64::from)),
            text(metadata.module_path()),
            array(metadata.fields().iter().map(|f| Mp::from(f.name()))),
            text(template.as_ref().map(|t| t.format.as_str())),
            is_log.into(),
        ]))?;
        self.sites.insert(key, id);
        Ok(id)
    }
    fn operation(&mut self, kind: &str, tail: impl IntoIterator<Item = Mp>) -> io::Result<()> {
        // Sequence and time are acquired under the same lock, defining the captured order.
        let timestamp = self.sampled_timestamp;
        let prefix = [kind.into(), self.records.into(), timestamp.into()];
        self.write(array(prefix.into_iter().chain(tail)))?;
        self.records += 1;
        Ok(())
    }
}
impl<W: Write + Send> Capture<W> {
    pub fn new(output: W, timestamp_bit_shift: u8) -> io::Result<Self> {
        if timestamp_bit_shift > 63 {
            return Err(invalid("timestamp shift exceeds 63"));
        }
        let info = FrameInfo::new()
            .block_size(BlockSize::Max64KB)
            .block_mode(BlockMode::Linked)
            .content_checksum(true);
        let mut state = CaptureState {
            output: Some(CountWriter {
                inner: FrameEncoder::with_frame_info(info, output),
                bytes: 0,
            }),
            sites: HashMap::new(),
            records: 0,
            next_span: 1,
            started: Instant::now(),
            shift: timestamp_bit_shift,
            error: None,
            finished_bytes: 0,
            sampled_timestamp: 0,
        };
        let origin = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos()
            .to_string();
        state.write(array([
            "tracing_table.input".into(),
            2.into(),
            timestamp_bit_shift.into(),
            origin.into(),
        ]))?;
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }
    fn capture(&self, write: impl FnOnce(&mut CaptureState<W>) -> io::Result<()>) {
        let mut state = self.state.lock();
        if state.output.is_none() || state.error.is_some() {
            return;
        }
        // Match live timestamp semantics: sample before field formatting and lookup.
        state.sampled_timestamp = (state.started.elapsed().as_nanos() as u64) >> state.shift;
        if let Err(error) = write(&mut state) {
            state.error = Some(error.to_string());
        }
    }
    /// Uncompressed MessagePack bytes, used for the reference fixture target.
    pub fn uncompressed_bytes(&self) -> u64 {
        let s = self.state.lock();
        s.output.as_ref().map_or(s.finished_bytes, |o| o.bytes)
    }
    pub fn records(&self) -> u64 {
        self.state.lock().records
    }
    pub fn finish(&self) -> io::Result<()> {
        let mut s = self.state.lock();
        if let Some(error) = &s.error {
            return Err(io::Error::other(error.clone()));
        }
        let Some(output) = s.output.as_ref() else {
            return Ok(());
        };
        let before = output.bytes;
        let count = s.records;
        let sites = s.sites.len() as u64;
        s.write(array([
            "end".into(),
            count.into(),
            sites.into(),
            before.into(),
        ]))?;
        let output = s.output.take().unwrap();
        s.finished_bytes = output.bytes;
        let mut writer = output.inner.finish().map_err(io::Error::other)?;
        writer.flush()
    }
}

#[derive(Clone, Copy)]
struct CapturedSpan(u64);
fn captured_parent<S: Subscriber + for<'a> LookupSpan<'a>>(
    contextual: bool,
    explicit: Option<&tracing_core::span::Id>,
    ctx: &Context<'_, S>,
) -> Option<u64> {
    let span = if contextual {
        ctx.lookup_current()
    } else {
        explicit.and_then(|id| ctx.span(id))
    };
    span.and_then(|s| s.extensions().get::<CapturedSpan>().map(|v| v.0))
}
impl<S, W> Layer<S> for Capture<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: Write + Send + 'static,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let metadata = log_callsite_adapter(event);
        let parent = captured_parent(event.is_contextual(), event.parent(), &ctx);
        self.capture(|state| {
            let site = state.site(metadata, event.is_log())?;
            let mut visitor = InputVisitor::new(metadata.fields().len(), event.is_log());
            event.record(&mut visitor);
            state.operation(
                "event",
                [
                    site.into(),
                    optional(parent),
                    encode_fields(&visitor.values),
                ],
            )
        });
    }
    fn on_new_span(
        &self,
        attrs: &tracing_core::span::Attributes<'_>,
        id: &tracing_core::span::Id,
        ctx: Context<'_, S>,
    ) {
        let parent = captured_parent(attrs.is_contextual(), attrs.parent(), &ctx);
        self.capture(|state| {
            let site = state.site(attrs.metadata(), false)?;
            let mut visitor = InputVisitor::new(attrs.metadata().fields().len(), false);
            attrs.record(&mut visitor);
            let captured = state.next_span;
            state.next_span += 1;
            state.operation(
                "span",
                [
                    site.into(),
                    captured.into(),
                    optional(parent),
                    encode_fields(&visitor.values),
                ],
            )?;
            ctx.span(id)
                .unwrap()
                .extensions_mut()
                .insert(CapturedSpan(captured));
            Ok(())
        });
    }
    fn on_record(
        &self,
        id: &tracing_core::span::Id,
        values: &tracing_core::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).unwrap();
        let captured = span.extensions().get::<CapturedSpan>().copied();
        if let Some(captured) = captured {
            self.capture(|state| {
                let mut visitor = InputVisitor::new(span.fields().len(), false);
                values.record(&mut visitor);
                state.operation(
                    "record",
                    [captured.0.into(), encode_fields(&visitor.values)],
                )
            });
        }
    }
    fn on_follows_from(
        &self,
        id: &tracing_core::span::Id,
        follows: &tracing_core::span::Id,
        ctx: Context<'_, S>,
    ) {
        let a = ctx
            .span(id)
            .and_then(|s| s.extensions().get::<CapturedSpan>().copied());
        let b = ctx
            .span(follows)
            .and_then(|s| s.extensions().get::<CapturedSpan>().copied());
        if let (Some(a), Some(b)) = (a, b) {
            self.capture(|s| s.operation("follows", [a.0.into(), b.0.into()]));
        }
    }
    fn on_close(&self, id: tracing_core::span::Id, ctx: Context<'_, S>) {
        if let Some(captured) = ctx
            .span(&id)
            .and_then(|s| s.extensions().get::<CapturedSpan>().copied())
        {
            self.capture(|s| s.operation("close", [captured.0.into()]));
        }
    }
}

/// Callsite identities and format strings are fixture inputs, never extracted captures.
#[derive(Debug)]
pub struct DumpCallsite {
    pub metadata: &'static Metadata<'static>,
    pub format: Option<String>,
    pub is_log: bool,
    /// Encoded metadata bytes in the input stream (diagnostics, not storage).
    pub msgpack_bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Event {
        site: usize,
        parent: Option<u64>,
        fields: Vec<Option<InputValue>>,
    },
    Span {
        site: usize,
        id: u64,
        parent: Option<u64>,
        fields: Vec<Option<InputValue>>,
    },
    Record {
        id: u64,
        fields: Vec<Option<InputValue>>,
    },
    /// Capture keeps every callback; replay stores the latest predecessor on close.
    Follows {
        id: u64,
        follows: u64,
    },
    Close {
        id: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Encoded operation bytes, including MessagePack framing.
    pub msgpack_bytes: u64,
    pub timestamp: u64,
    pub operation: Operation,
}
#[derive(Debug)]
pub struct Fixture {
    pub timestamp_bit_shift: u8,
    pub started_unix_ns: u128,
    pub callsites: Vec<DumpCallsite>,
    pub records: Vec<Record>,
    pub msgpack_bytes: u64,
}

// tracing metadata has a static lifetime by design. Allocate it once when loading,
// outside measured replay; repeated replay reuses these bounded fixture allocations.
struct ImportedCallsite(OnceLock<&'static Metadata<'static>>);
impl Callsite for ImportedCallsite {
    fn set_interest(&self, _: tracing_core::subscriber::Interest) {}
    fn metadata(&self) -> &Metadata<'_> {
        self.0.get().expect("initialized imported metadata")
    }
}
fn leak(value: &str) -> &'static str {
    Box::leak(value.to_owned().into_boxed_str())
}
fn decode_site(v: &[Mp]) -> io::Result<DumpCallsite> {
    if v.len() != 12 {
        return Err(invalid("invalid callsite record"));
    }
    let level: tracing::Level = string(&v[4])?.parse().map_err(io::Error::other)?;
    let event = v[5]
        .as_bool()
        .ok_or_else(|| invalid("invalid callsite kind"))?;
    let file = if v[6].is_nil() {
        None
    } else {
        Some(leak(string(&v[6])?))
    };
    let line = opt_number(&v[7])?
        .map(u32::try_from)
        .transpose()
        .map_err(io::Error::other)?;
    let module = if v[8].is_nil() {
        None
    } else {
        Some(leak(string(&v[8])?))
    };
    let names = items(&v[9])?
        .iter()
        .map(|v| string(v).map(leak))
        .collect::<io::Result<Vec<_>>>()?;
    let names = Box::leak(names.into_boxed_slice());
    let format = if v[10].is_nil() {
        None
    } else {
        Some(string(&v[10])?.to_owned())
    };
    if let Some(format) = &format {
        CompiledTemplate::compile(format).map_err(invalid)?;
    }
    let is_log = v[11]
        .as_bool()
        .ok_or_else(|| invalid("invalid log origin"))?;
    let callsite = Box::leak(Box::new(ImportedCallsite(OnceLock::new())));
    let metadata = Box::leak(Box::new(Metadata::new(
        leak(string(&v[2])?),
        leak(string(&v[3])?),
        level,
        file,
        line,
        module,
        FieldSet::new(names, Identifier(callsite)),
        if event { Kind::EVENT } else { Kind::SPAN },
    )));
    callsite.0.set(metadata).unwrap();
    Ok(DumpCallsite {
        metadata,
        format,
        is_log,
        msgpack_bytes: 0,
    })
}
impl Fixture {
    /// Read streaming LZ4/MessagePack and reject incomplete, reordered, or invalid
    /// lifecycles before any benchmark is timed. No load timing is collected.
    pub fn load(input: impl Read) -> io::Result<Self> {
        let mut input = CountReader {
            inner: FrameDecoder::new(FrameInput(input)),
            bytes: 0,
        };
        let next = |input: &mut CountReader<FrameDecoder<_>>| {
            rmpv::decode::read_value(input).map_err(io::Error::other)
        };
        let header = next(&mut input)?;
        let h = items(&header)?;
        if h.len() != 4 || string(&h[0])? != "tracing_table.input" || number(&h[1])? != 2 {
            return Err(invalid(
                "expected v2 replay fixture; v1 storage snapshots are retired",
            ));
        }
        let shift = u8::try_from(number(&h[2])?).map_err(io::Error::other)?;
        if shift > 63 {
            return Err(invalid("timestamp shift exceeds 63"));
        }
        let origin = string(&h[3])?.parse().map_err(io::Error::other)?;
        let mut fixture = Self {
            timestamp_bit_shift: shift,
            started_unix_ns: origin,
            callsites: Vec::new(),
            records: Vec::new(),
            msgpack_bytes: 0,
        };
        let mut spans = HashMap::<u64, (usize, bool)>::new();
        loop {
            let before = input.bytes;
            let record = next(&mut input)?;
            let v = items(&record)?;
            if v.is_empty() {
                return Err(invalid("empty record"));
            }
            let kind = string(&v[0])?;
            if kind == "end" {
                if v.len() != 4
                    || number(&v[1])? != fixture.records.len() as u64
                    || number(&v[2])? != fixture.callsites.len() as u64
                    || number(&v[3])? != before
                {
                    return Err(invalid("fixture footer counts differ"));
                }
                fixture.msgpack_bytes = input.bytes;
                // Consume and verify the LZ4 trailer too, not just the MessagePack footer.
                let mut trailing = [0];
                if input.read(&mut trailing)? != 0 {
                    return Err(invalid("trailing fixture data"));
                }
                if input.inner.get_mut().0.read(&mut trailing)? != 0 {
                    return Err(invalid("trailing compressed data"));
                }
                return Ok(fixture);
            }
            if kind == "callsite" {
                if v.len() < 2 || number(&v[1])? != fixture.callsites.len() as u64 {
                    return Err(invalid("noncontiguous callsite IDs"));
                }
                let mut callsite = decode_site(v)?;
                callsite.msgpack_bytes = input.bytes - before;
                fixture.callsites.push(callsite);
                continue;
            }
            if v.len() < 4 || number(&v[1])? != fixture.records.len() as u64 {
                return Err(invalid("noncontiguous operation sequence"));
            }
            let timestamp = number(&v[2])?;
            let known_span = |id: u64| {
                if spans.get(&id).is_some_and(|v| v.1) {
                    Ok(id)
                } else {
                    Err(invalid("reference to unknown/closed span"))
                }
            };
            let operation = match (kind, v.len()) {
                ("event", 6) | ("span", 7) => {
                    let site = usize::try_from(number(&v[3])?).map_err(io::Error::other)?;
                    let metadata = fixture
                        .callsites
                        .get(site)
                        .ok_or_else(|| invalid("unknown callsite"))?
                        .metadata;
                    if metadata.is_event() != (kind == "event") {
                        return Err(invalid("callsite kind differs"));
                    }
                    let parent_at = if kind == "event" { 4 } else { 5 };
                    let parent = opt_number(&v[parent_at])?.map(known_span).transpose()?;
                    let fields = decode_fields(&v[parent_at + 1])?;
                    if fields.len() != metadata.fields().len() {
                        return Err(invalid("field count differs"));
                    }
                    if kind == "event" {
                        Operation::Event {
                            site,
                            parent,
                            fields,
                        }
                    } else {
                        let id = number(&v[4])?;
                        if id == 0 || spans.contains_key(&id) {
                            return Err(invalid("duplicate span ID"));
                        }
                        spans.insert(id, (site, true));
                        Operation::Span {
                            site,
                            id,
                            parent,
                            fields,
                        }
                    }
                }
                ("record", 5) => {
                    let id = known_span(number(&v[3])?)?;
                    let fields = decode_fields(&v[4])?;
                    if fields.len() != fixture.callsites[spans[&id].0].metadata.fields().len() {
                        return Err(invalid("span update field count differs"));
                    }
                    Operation::Record { id, fields }
                }
                ("follows", 5) => Operation::Follows {
                    id: known_span(number(&v[3])?)?,
                    follows: known_span(number(&v[4])?)?,
                },
                ("close", 4) => {
                    let id = known_span(number(&v[3])?)?;
                    spans.get_mut(&id).unwrap().1 = false;
                    Operation::Close { id }
                }
                _ => return Err(invalid("unknown operation or wrong record width")),
            };
            fixture.records.push(Record {
                msgpack_bytes: input.bytes - before,
                timestamp,
                operation,
            });
        }
    }

    /// Replay normal storage work using original timestamps and fresh matchers.
    /// Metadata decoding is outside this method; registration and compilation are inside.
    pub fn replay(&self) -> RecordingLayer {
        self.replay_with_templates(true)
    }

    /// Select whether captured source formats participate in extraction. Turning
    /// it off keeps raw messages and bypasses matcher compilation and matching.
    ///
    /// Actual source locations and/or embedded section are not used in replay mode
    pub fn replay_with_templates(&self, extract_templates: bool) -> RecordingLayer {
        // Keep replay's implicit addressing aligned with live recording.
        self.replay_with_options(extract_templates, super::StringStorage::default())
    }

    /// Replay selects physical string addressing without changing captured inputs.
    pub fn replay_with_options(
        &self,
        extract_templates: bool,
        string_storage: super::StringStorage,
    ) -> RecordingLayer {
        let mut recorder = RecordingLayer::new();
        recorder.string_storage = string_storage;
        recorder.timestamp_bit_shift = self.timestamp_bit_shift;
        recorder.system_time = UNIX_EPOCH
            + std::time::Duration::from_nanos((self.started_unix_ns % 1_000_000_000) as u64)
            + std::time::Duration::from_secs((self.started_unix_ns / 1_000_000_000) as u64);
        recorder.global_memory_cap = usize::MAX;
        recorder.callsite_memory_cap = usize::MAX;
        struct Span {
            site: usize,
            stored: SpanId,
            fields: Option<FieldValues>,
            follows: Option<SpanId>,
        }
        let mut spans = HashMap::<u64, Span>::new();
        let mut registered = vec![false; self.callsites.len()];
        for record in &self.records {
            let time = Time {
                value: record.timestamp,
                shift: self.timestamp_bit_shift,
            };
            match &record.operation {
                Operation::Event {
                    site,
                    parent,
                    fields,
                }
                | Operation::Span {
                    site,
                    parent,
                    fields,
                    ..
                } => {
                    let callsite = &self.callsites[*site];
                    if !registered[*site] {
                        drop(recorder.register_with_template(callsite.metadata, || {
                            if extract_templates {
                                callsite.format.as_ref().map(|f| {
                                    CompiledTemplate::compile(f).expect("validated template")
                                })
                            } else {
                                None
                            }
                        }));
                        registered[*site] = true;
                    }
                    let parent = parent.map(|id| spans[&id].stored);
                    let stored =
                        recorder.record_start(callsite.metadata, time, parent, false, |visitor| {
                            visit_values(callsite.metadata, fields, visitor)
                        });
                    if let Operation::Span { id, .. } = record.operation {
                        spans.insert(
                            id,
                            Span {
                                site: *site,
                                stored,
                                fields: None,
                                follows: None,
                            },
                        );
                    }
                }
                Operation::Record { id, fields } => {
                    let span = spans.get_mut(id).unwrap();
                    let metadata = self.callsites[span.site].metadata;
                    let updates = span.fields.get_or_insert_with(|| {
                        FieldValues::new(metadata.fields().len(), 0, string_storage)
                    });
                    updates.record(|visitor| visit_values(metadata, fields, visitor));
                }
                Operation::Follows { id, follows } => {
                    // Resolve while the predecessor is alive; it may close first.
                    let follows = spans[follows].stored;
                    spans.get_mut(id).unwrap().follows = Some(follows);
                }
                Operation::Close { id } => {
                    let mut span = spans.remove(id).unwrap();
                    recorder.close_span(
                        self.callsites[span.site].metadata,
                        span.stored,
                        time,
                        span.follows,
                        span.fields.as_mut(),
                    );
                }
            }
        }
        recorder
    }
}
