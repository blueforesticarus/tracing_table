This crate implements an memory-efficient cache for tracing logs, with support for per-callsite rollover. 

# Rollover
Every callsite (instance of trace!, info!, span! etc.) get's it's own cache of logs, with it's own rollover. This means that quick spamming logs (such as is common for level=trace) will not push uncommon logs (an important error, for example) off the queue. 

# Components
1. smartstring: inlines strings shorter than the pointer length and interns others on the heap, avoiding duplicates.

TODO: this should actually be turned to take up the same space as the overhead of the string deduplication. Since, strings will very efficiently be deduplicated in the dataframe compaction.

NOTE: I actually don't need this as the string_cache crate supports inlining.
   
2. dataframe: a row-oriented buffer for byte slices of fixed (runtime-specified) length, which can be "compacted", storing only the subset of bytes which change row to row. The compacted frame can be iterated over direcly using a lending iterator.
   
3. blockbuffer: stores an uncompacted "active" dataframe for writing new logs,  when the active buffer has reached ~1Kib in size, it is compacted and moved to a ring. (TODO: When the ring reaches its max entries, the oldest ring is dropped. ) This reduces the storage cost of things like timestamps and counters, since most of the bytes will be constant, while still supporting the full range of possible values. 

Blockbuffers for a specific archetype are initially allocated with only enough space for one log, this doubles untill the active buffer reaches the configured size, and the buffer begins compacting old data. This is to help in the case where there are many archetypes. See: [[archetype proliferation]].

4. archetypes: Logs are stored according to archetype. Each archetype get's it's own blockbuffer. Every callsite + *set of **provided** fields* gets it's own archetype (note that if `myfield: Option<T>` that `trace!(myfield)` will potentially have two archetypes depending on whether myfield is some or none).

Log data is packed into a byte_slice as (timestamp, end_timestamp (for spans), event/span id, parent id, follows_from id, fields...). If the log has no follows_from id, it is not stored (the log will get a different archetype).

By default Timestamps are stored as u64 nanoseconds. However, 6 of those bytes represent days. A bit shift can be set to disregard the low bits, if they are noise. This is specified with the env var `RUST_LOG_TIMER_RES=<amount to shift>`

*TODO: End timestamps are relative to the start timestamp?*

Event id's are assigned as per-archetype sequential counters, to maximise how many bytes can be compacted. Archetypes ids themselves are also sequential.

Because the archetype is part of the id, following parent/follows_from refs can also be fast.

## Span Id problem.
Ideally the id of an event would simply be it's index in the blockbuffer.
This would eliminate the need to store it at all, resulting in ~2 bytes savings per row. It would also allow id's to be accessed in O(1) instead of seaching the blockbuffer.

However, the id is based on span creation, where-as the span is stored on *completion*. When the span is created we do not yet know it's archetype, as it's empty fields may still be recorded with span.record. Additionally we do not know it's end timestamp. 

Consider the case of a log which is written to the cache. Minutes later it's span ends. When the log is written, there is no way to know where in the buffer the span will go. The ID must be fixed when it is created. Meanwhile, several thousand of these spans have finished while this slow one was running. 

We *could* insert in the compacted old frame. This would not be so bad if we reserved space for it. But we don't know whether all fields will be present (or their type), so we don't know it's archetype. 

We could make fields nullable and unify the archetypes, however:
1. this would add 1 byte (per 8 fields), though it will normally be compacted.
2. it would require some logic to prevent absent fields from registering as non-constant. If a field is either Some(5) or None, the field is not variable, only it's presence.

However if a field's *type* changes, it will still alter the archetype.

Options:
1: Sort frames before/during/after compacting.
2: Lookup function that assumes sorting, and then scans.

Regardless, we will need metadata on min/max id of each dataframe.

**Solution. ID is generated when span is created. Fields provided later are stored when the span ends, in a separate table. This table has a pointer to the first.**

# Archetype Proliferation

# Further optimization
## Bit packing
The compact frames could be compacted at the bit level instead of the byte level. 
This would require alot of shifting and masking, and perfomance would need to be benchmarked.
Real application log data would be needed to see whether bit packing would save space.

## String Split
Tracing logs are meant to be structured. However, many programs and librarys use tracing's support for format macro's instead, for example

`error!("problem with entity {}", entity);` with `entity : usize 2623`

This is passed to the tracing subscriber as "problem with entity 2623". We do not have access to the format string literal or fields.
If there are many of these messages for different entities, it will result in lots of string interning. 

As an optimization for this common case, for each callsite with a message the logger remembers a prefix. When a new log is processed, the prefix is used to split the string into (prefix, remainder), possibly shortening the prefix if needed. If the prefix does not overlap at all, then it is discarded and the current log becomes the new prefix. Both the prefix and the remainder are interned, and stored as seperate fields in the dataframe. Because the prefix is expected to be constant, it will normally be compacted. If no prefix is used, the the message is stored in the remainder, and the prefix field is disabled.

Note that prefix shortening *could* point to the same data as the already stored message. However this complicates reference tracking.

Because prefixes are expected to occur per callsite, we could actually check more than the last message.

## Static string detection
`info!("starting server");`

In this log, the message field will be a static string. 
If we can detect this at runtime (by checking if &'static str is pointing to something in rodata) then we can skip the hashmap lookup and all the work of interning it. 

Unfortionately the bounds of rodata are not accessible from code. It may be recoverable from the applications binary, but whether this could be done reliably enough for an safe api exposing `rodata: &'static [u8]` is questionable.

## Collection Thread
