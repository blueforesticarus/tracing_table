#![feature(ptr_metadata)]
pub mod blockbuffer;
pub mod dataframe;
pub mod dyntuple;
// pub mod smartstring;
pub mod stringcache;
pub mod subscriber;

pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

// pub struct LogRecorder {
//     // like components/archetypes, callsites are relatively limited.
//     // also, like components, they are of semi individual concern
//     // we can treat each span tree (possibly terminated with an event) as it's own archetype.
//     // this will let us construct fixed unit buffers based on the metadata.
//     pub lines: HashMap<LogArchetype, DynRingBuffer>,

//     pub data: HashMap<&str, Weak<String>>,
// }

// pub struct StringRefOrPacked;

// enum ValueData {
//     F64(f64),
//     I64(i64),
//     U64(u64),
//     I128(i128),
//     U128(u128),
//     Bool(bool),
//     Str(Arc<String>),
// }

// impl ValueKind {
//     fn size(&self) {
//         match self {
//             ValueKind::F64 => 8,
//             ValueKind::I64 => 8,
//             ValueKind::U64 => 8,
//             ValueKind::I128 => 16,
//             ValueKind::U128 => 16,
//             ValueKind::Bool => 1,
//             ValueKind::Str => 8,
//         }
//     }
// }

// struct FieldTypeSpec(Vec<Field, ValueKind>);
// struct FieldValueSpec(Vec<Field, ValueData>);

// #[derive(Debug, Clone, Hash)]
// pub struct LogArchetype {
//     //parent: Identifier,
//     callsite: Identifier,
//     fields: FieldTypeSpec,

//     // optional hash use by the
//     static_fields: FieldValueSpec,
// }

// pub struct LogRecord {
//     timestamp: SystemTime,
//     fields: Vec<FieldsData>,
// }

// // Fields data will
// #[derive(Debug, Clone, Copy)]
// pub struct DynRingBuffer {
//     buffer: Vec<usize>,
//     unit_size: usize,
//     max_length: usize,
//     head: usize, // the index 0
//     tail: usize, //
// }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
