use super::{
    ArchivedBlock, ArchivedFrame, CompressionStats, SmallBox, SpanFieldsRef, StringBufferId,
};
use crate::data::dataframe::Frame;

impl ArchivedBlock {
    pub fn new(frame: &Frame, metadata: CollocatedMetadata) -> SmallBox<Self> {
        // SAFETY: ArchivedBlock is repr(C), with metadata followed by its DST frame.
        unsafe { SmallBox::with_header(metadata, ArchivedFrame::initializer(frame)) }
    }
}

/// Shared active/archive metadata; the thin owner keeps alignment at four bytes.
#[repr(C)]
#[derive(Default)]
pub struct CollocatedMetadata {
    pub string_buffer: StringBufferId,
    span_fields: Option<SmallBox<[SpanFieldsRef]>>,
}

const _: () = assert!(align_of::<CollocatedMetadata>() == 4);
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Option<SmallBox<[SpanFieldsRef]>>>() == 8);
    assert!(size_of::<CollocatedMetadata>() == 12);
};

impl CollocatedMetadata {
    pub fn span_fields(&self) -> Option<&[SpanFieldsRef]> {
        self.span_fields.as_deref()
    }

    pub(super) fn add_span_fields_ref(&mut self, reference: SpanFieldsRef) {
        let fields = self.span_fields().unwrap_or_default();
        if fields.contains(&reference) {
            return;
        }
        // Lists are sparse: rebuild an exact-size slice instead of tracking capacity.
        let fields = fields.iter().copied().chain([reference]).collect();
        self.span_fields = Some(SmallBox::from_box(fields));
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        CompressionStats {
            overhead: size_of::<Self>() + self.heap_bytes(),
            ..Default::default()
        }
    }

    /// Exclude inline metadata already charged to its active or archived owner.
    pub fn heap_bytes(&self) -> usize {
        self.span_fields
            .as_ref()
            .map_or(0, SmallBox::allocation_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thin_span_list_preserves_entries_and_deduplicates_across_growth() {
        let mut metadata = CollocatedMetadata::default();
        assert_eq!(align_of::<CollocatedMetadata>(), 4);
        assert!(metadata.span_fields().is_none());
        assert_eq!(metadata.heap_bytes(), 0);
        // Revisit every entry after each replacement; duplicates must not allocate.
        for block in 1..=257 {
            let reference = SpanFieldsRef {
                archetype: 3,
                block,
            };
            metadata.add_span_fields_ref(reference);
            let bytes = metadata.heap_bytes();
            metadata.add_span_fields_ref(reference);
            assert_eq!(metadata.heap_bytes(), bytes);
            let refs = metadata.span_fields().unwrap();
            assert_eq!(refs.len(), block as usize);
            assert!(
                refs.iter()
                    .enumerate()
                    .all(|(i, r)| r.archetype == 3 && r.block == i as u32 + 1)
            );
        }
        assert_eq!(
            metadata.heap_bytes(),
            size_of::<u32>() + 257 * size_of::<SpanFieldsRef>()
        );
    }
}
