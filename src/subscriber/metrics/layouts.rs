//! Raw active-column and layout-history leaves. Consumers choose whether to
//! include spare capacity, group callsites, or compare independent replay runs.
use super::*;

pub fn write_layouts(
    mut output: impl Write,
    run: &str,
    recorder: &RecordingLayer,
    ids: Option<&HashMap<usize, u64>>,
) -> io::Result<()> {
    writeln!(
        output,
        "run,callsite,archetype,scope,block_index,column,component,source_column,logical_width,stored_width,values,retained_bits,target,file,line,name"
    )?;
    for (key, shared) in recorder.callsites.read().iter() {
        let site = shared.read();
        let callsite = ids.map_or(site.id, |mapping| mapping[key]);
        let names = site
            .metadata
            .fields()
            .iter()
            .map(|f| f.name().to_owned())
            .collect::<Vec<_>>();
        for arch in &site.cache {
            let fields = fields(&arch.archetype, &names);
            let source = [
                site.metadata.target().to_owned(),
                site.metadata.file().unwrap_or("").to_owned(),
                site.metadata
                    .line()
                    .map_or(String::new(), |n| n.to_string()),
                site.metadata.name().to_owned(),
            ];
            let mut emit = |scope: &str,
                            block: usize,
                            column: &str,
                            component: &str,
                            from: &str,
                            logical: usize,
                            stored: usize,
                            values: usize,
                            bits: usize| {
                let cells = [
                    run.to_owned(),
                    callsite.to_string(),
                    arch.id.to_string(),
                    scope.into(),
                    block.to_string(),
                    column.into(),
                    component.into(),
                    from.into(),
                    logical.to_string(),
                    stored.to_string(),
                    values.to_string(),
                    bits.to_string(),
                ];
                csv(&mut output, cells.iter().chain(source.iter()))
            };
            for layout in &arch.data.layouts {
                // Definitions carry no per-block cost. A boundary >0 is exactly
                // a promotion after the first block; same-block changes coalesce.
                emit(
                    "layout",
                    layout.block_index,
                    "@layout",
                    "definition",
                    "",
                    0,
                    0,
                    0,
                    0,
                )?;
                for f in &layout.fields {
                    let column = fields
                        .iter()
                        .find(|c| c.range == f.logical.range())
                        .unwrap();
                    let canonical = &layout.fields[usize::from(f.source)];
                    let from = fields
                        .iter()
                        .find(|c| c.range == canonical.logical.range())
                        .unwrap();
                    emit(
                        "layout",
                        layout.block_index,
                        &column.name,
                        "column",
                        &from.name,
                        usize::from(f.logical.width),
                        usize::from(f.stored.width),
                        0,
                        0,
                    )?;
                }
            }
            // Shared allocation leaves are kept apart from row payload. The
            // outer inline object includes the default room for one layout.
            emit(
                "schema",
                0,
                "@layout",
                "inline",
                "",
                0,
                0,
                0,
                size_of::<SmallVec<[physical::PhysicalLayout; 1]>>() * 8,
            )?;
            emit(
                "schema",
                0,
                "@layout",
                "heap",
                "",
                0,
                0,
                0,
                arch.data.layouts_heap_bytes() * 8,
            )?;
            let Some(layout) = arch.data.layouts.last() else {
                continue;
            };
            let frame = &arch.data.active;
            let block = arch.data.offset + arch.data.ring.len();
            for field in &fields {
                let stored = layout
                    .physical_range(field.range.clone())
                    .map_or(0, |r| r.len());
                let from = layout
                    .fields
                    .iter()
                    .find(|f| f.logical.range() == field.range)
                    .map(|f| {
                        let canonical = layout.fields[usize::from(f.source)].logical.range();
                        fields
                            .iter()
                            .find(|c| c.range == canonical)
                            .unwrap()
                            .name
                            .as_str()
                    })
                    .unwrap_or(field.name.as_str());
                emit(
                    "active",
                    block,
                    &field.name,
                    "mask",
                    from,
                    field.range.len(),
                    stored,
                    0,
                    stored * 8,
                )?;
                emit(
                    "active",
                    block,
                    &field.name,
                    "rows",
                    from,
                    field.range.len(),
                    stored,
                    frame.length,
                    frame.length * stored * 8,
                )?;
                emit(
                    "active",
                    block,
                    &field.name,
                    "spare",
                    from,
                    field.range.len(),
                    stored,
                    0,
                    (frame.capacity() - frame.length) * stored * 8,
                )?;
            }
        }
    }
    Ok(())
}
