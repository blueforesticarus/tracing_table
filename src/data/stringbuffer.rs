// we need several forms of string storage.
// TEMPLATES: high repeat across blocks but not across archetypes, need quick access -> per archetype dedupped
//            finite count in normal case -> store forever
//            pathological cases -> fallback to storage as values (accurate, bounded memory use, suboptimal compression)
// FIELD VALUES and TEMPLATE (message) VALUES: unknown repeat across blocks or archetypes, more likely repeat with-in block
//            need access + retention only when that block is needed / retained
//            -> store in dedupped chunks of flexible size associated (possibly 1to1) with message chunks. Drop a string chunk when all message chunks which refer to it are dropped.
//            messages/values don't change -> we can build the buffer in-order and compress it and store that where-ever
//
//            Q: how to dedup fast
//            Q: how are values stored
//               - store data per row, since it is more flexible and byte columns are dedupped anyway. getting the equivilent to message chunk metadata essentially free
//               - for each value: which string chunk + where in string chunk (indexed?)
//               - which template
//
// treat strings as seperate concern
//
// Defered work:
// 1. templates can make things worse if the value is the same every time. Since we will be storing template + value references and could be storing just string reference.
//    HOWEVER: this is attenuated by the fact that metadata either lives with the string chunk or the message chunk both of which are compressed
//    PLUS: this is rare.

/// use two bits to represent either
/// &static string
/// an entry in a buffer
/// an entry in an indexed buffer
struct StringRef(usize);

struct TemplatedString {
    template: StringRef,
    values: [StringRef],
}

/// Contains the currently being appended too buffer
/// must include index and data
struct Buffer {
    /// contains actual strings
    data: Vec<u8>,
    index: Vec,
}

impl Buffer {
    /// add string to the buffer, deduplicated, and return it's index and offset
    fn add_string() {}

    fn add_template_string() {
        todo!();
    }
}

/// a Buffer compressed with lz4
struct CompressedBuffer {
    data: Box<[u8]>,
}

/// a collection of buffers, old ones are lz4 compressed
struct BufferCollection {
    compressed: Vec,
}
