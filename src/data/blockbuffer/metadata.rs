use super::{
    ArchivedBlock, ArchivedFrame, CompressionStats, SmallBox, SpanFieldsRef, StringBufferId,
};
use crate::data::dataframe::Frame;

impl ArchivedBlock {
    pub fn new(frame: &Frame, metadata: CollocatedMetadata) -> SmallBox<Self> {
        Self::new_with_trailer(frame, metadata, &[])
    }

    /// Pack an already encoded frame with its caller-supplied trailer. Rollover
    /// and edit rebuilds run column fitting before this allocation; structural
    /// timestamp/parent residuals must not be encoded again here.
    pub fn new_with_trailer(
        frame: &Frame,
        metadata: CollocatedMetadata,
        trailer: &[u8],
    ) -> SmallBox<Self> {
        // SAFETY: ArchivedBlock is repr(C), with metadata followed by its DST frame.
        unsafe {
            SmallBox::with_header(
                metadata,
                ArchivedFrame::initializer_with_trailer(frame, trailer),
            )
        }
    }

    pub fn parent_offsets(
        &self,
        layout: &[crate::data::column::Layout],
    ) -> super::ParentOffsets<'_> {
        super::ParentOffsets::new(
            crate::data::column::Metadata::split(self.frame.trailer(), layout).0,
        )
    }

    pub fn column_metadata<'a, 's>(
        &'a self,
        layout: &'s [crate::data::column::Layout],
    ) -> crate::data::column::Metadata<'a, 's> {
        crate::data::column::Metadata::split(self.frame.trailer(), layout).1
    }

    pub fn column_footer_bytes(&self, layout: &[crate::data::column::Layout]) -> usize {
        self.column_metadata(layout).storage_bytes()
    }

    /// Structural columns retain their existing domain APIs; fields become logical values here.
    pub fn read_row(
        &self,
        index: usize,
        output: &mut [u8],
        layout: &[crate::data::column::Layout],
    ) {
        self.frame.read_row(index, output);
        self.column_metadata(layout)
            .decode(&self.frame, output, index);
    }

    pub fn iter<'a>(
        &'a self,
        layout: &'a [crate::data::column::Layout],
    ) -> super::ArchivedBlockIter<'a> {
        super::ArchivedBlockIter::new(self, layout)
    }
}

/// Shared active/archive metadata; both the thin owner and string ID align to two bytes.
#[repr(C)]
#[derive(Default)]
pub struct CollocatedMetadata {
    // Put the pointer first so no small field introduces padding before it.
    /// most spans have no extra fields (recorded after creation),
    /// if they do a record is created for the fields with the spanid of the span
    /// we only store the set of extra fields blocks associated with any row of this block.
    /// reader is responsible for rebuilding association if desired, this limits search space (likely to one block, if not zero)
    span_fields: Option<SmallBox<[SpanFieldsRef]>>,

    /// index of the string buffer in this arch's collection which indices in this block refer to
    pub string_buffer: StringBufferId,
    /// Predicted increase across the existing frame length, in shifted ticks.
    /// Zero disables prediction and retains unsigned timestamp offsets.
    pub timestamp_stride: crate::data::util::float_u16::FloatU16,
    /// True only when archived starts are unsigned elapsed ticks from their
    /// referenced parent. Active rows always retain the ordinary absolute basis.
    pub timestamp_parent_relative: bool,
}

const _: () = assert!(align_of::<CollocatedMetadata>() == 2);
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Option<SmallBox<[SpanFieldsRef]>>>() == 8);
    assert!(size_of::<CollocatedMetadata>() == 16);
    assert!(std::mem::offset_of!(CollocatedMetadata, span_fields) == 0);
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
        assert_eq!(align_of::<CollocatedMetadata>(), 2);
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
            // The u16 prefix occupies four bytes after aligning the slice.
            4 + 257 * size_of::<SpanFieldsRef>()
        );
    }
}
