//! Basic UI. All recording locks are released before their owned results render.
use crate::inspection::{ArchetypeId, InspectRecording, RecordKind, RecordedRow, RecordingControl};
use crate::{
    model::{self, SortColumn, SortOrder, StatsCache},
    scene::SceneSettings,
    strings::{StringSnapshot, StringSortColumn},
};
use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tracing_table::{
    data::{
        dataframe::CompressionStats,
        stringcache::{STRING_INDEX, STRING_STATS},
    },
    subscriber::{RecordingLayer, SpanId, USE_INDEXED_ATOM},
};

#[derive(Resource)]
pub struct Viewer {
    recorder: Arc<RecordingLayer>,
    control: Arc<RecordingControl>,
    per_row: bool,
    indexed: bool,
    cache: StatsCache,
    // Snapshot global string stats at the existing refresh rate, outside UI rendering.
    strings: StringSnapshot,
    strings_tab: bool,
    string_sort: StringSortColumn,
    string_descending: bool,
    // Column width is adjustable in Tuning; rows stay single-line for virtualization.
    string_width: f32,
    last_refresh: Instant,
    refresh_ms: u64,
    row_limit: usize,
    scale: f32,
    archetype_filter: String,
    entity_filter: String,
    entity_choices: Vec<String>,
    selected: Option<ArchetypeId>,
    rows: Vec<RecordedRow>,
    sort: SortOrder,
    // Ratio rather than pixels keeps both panes usable when resizing the window.
    split_fraction: f32,
}
impl Viewer {
    pub fn new(recorder: Arc<RecordingLayer>, control: Arc<RecordingControl>) -> Self {
        Self {
            recorder,
            control,
            per_row: false,
            indexed: USE_INDEXED_ATOM.load(std::sync::atomic::Ordering::Relaxed),
            cache: StatsCache::default(),
            strings: StringSnapshot::default(),
            strings_tab: false,
            string_sort: StringSortColumn::Index,
            string_descending: false,
            string_width: 640.0,
            last_refresh: Instant::now() - Duration::from_secs(1),
            refresh_ms: 500,
            row_limit: 1000,
            scale: 1.0,
            archetype_filter: String::new(),
            entity_filter: String::new(),
            entity_choices: Vec::new(),
            selected: None,
            rows: Vec::new(),
            sort: SortOrder::default(),
            split_fraction: 0.38,
        }
    }
    fn refresh_strings(&mut self) {
        // Match the conversion lock order and release both locks before rendering.
        self.strings = StringSnapshot::capture(&mut STRING_STATS.write(), &STRING_INDEX.read());
        self.strings.sort(self.string_sort, self.string_descending);
    }
    fn reload_rows(&mut self) {
        let Some(id) = self.selected else {
            self.rows.clear();
            self.entity_choices.clear();
            return;
        };
        let recent = model::matching_rows(&self.recorder, id, "", self.row_limit);
        self.entity_choices = model::entity_choices(&recent);
        self.rows = if self.entity_filter.trim().is_empty() {
            recent
        } else {
            model::matching_rows(&self.recorder, id, &self.entity_filter, self.row_limit)
        };
    }
}

// Apply normalization only to byte values; counts and ratios remain unchanged.
fn bytes_text(bytes: usize, rows: usize, per_row: bool) -> String {
    if per_row {
        model::significant_text(bytes as f64 / rows.max(1) as f64)
    } else {
        model::size_text(bytes)
    }
}
fn stats_text(stats: CompressionStats, rows: usize, per_row: bool) -> String {
    let size = |bytes| bytes_text(bytes, rows, per_row);
    format!(
        "data {} · total {} · compressed {} · overhead {} · overalloc {} · ratio {:.1}% · raw ratio {:.1}%{}",
        size(stats.data),
        size(model::storage(stats)),
        size(stats.compressed),
        size(stats.overhead),
        size(stats.overalloc),
        100.0 * stats.ratio(),
        100.0 * stats.raw_ratio(),
        if per_row { " · bytes/row" } else { "" }
    )
}

fn archetype_table(ui: &mut egui::Ui, viewer: &mut Viewer, height: f32) -> bool {
    let mut selected_changed = false;
    viewer.sort.per_row = viewer.per_row;
    let ids = model::sorted_archetypes(&viewer.cache, &viewer.archetype_filter, viewer.sort);
    egui::ScrollArea::both().id_salt("archetype_table").max_height(height)
        .auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("archetypes").striped(true).max_col_width(160.0).show(ui, |ui| {
            for (column, label) in SortColumn::COLUMNS {
                let arrow = if viewer.sort.column == column { if viewer.sort.descending { " v" } else { " ^" } } else { "" };
                let response = ui.button(format!("{label}{arrow}"));
                if response.clicked() { viewer.sort.select(column); }
                match column {
                    SortColumn::Compression => { response.on_hover_text("Savings including overhead and overalloc: 100 × (1 − (compressed + overhead + overalloc) / data)."); }
                    SortColumn::RawCompression => { response.on_hover_text("Row-byte savings excluding overhead and overalloc: 100 × (1 − compressed / data)."); }
                    SortColumn::Total => { response.on_hover_text("Compressed + overhead + overalloc."); }
                    SortColumn::BlockDuration => { response.on_hover_text("Average max(ts) − min(ts) across completed blocks. Active blocks and span-update rows are excluded; timestamps are event times or span start times. Click to sort."); }
                    SortColumn::Blocks => { response.on_hover_text("Completed compressed blocks plus the active block."); }
                    _ => { response.on_hover_text("Click to sort; click again to reverse."); }
                }
            }
            ui.end_row();
            for id in ids {
                let arch = &viewer.cache.archetypes[&id];
                let snapshot = &arch.snapshot;
                if ui.selectable_label(viewer.selected == Some(id), format!("{}:{}", id.callsite, id.archetype)).clicked() {
                    viewer.selected = Some(id); selected_changed = true;
                }
                let target = format!("{} / {}", snapshot.metadata.target(), snapshot.metadata.name());
                ui.add(egui::Label::new(&target).truncate()).on_hover_text(&target);
                let file = snapshot.metadata.file().unwrap_or("?");
                ui.label(format!("{}:{}", file.rsplit('/').next().unwrap_or(file), snapshot.metadata.line().unwrap_or(0))).on_hover_text(file);
                // Callsite metadata supplies the severity for every row in this archetype.
                ui.label(snapshot.metadata.level().as_str());
                ui.label(format!("{:?}", snapshot.kind)).on_hover_text(format!("{:?}", snapshot.fields));
                ui.label(snapshot.rows.to_string());
                ui.label(snapshot.blocks.to_string());
                ui.label(model::duration_text(arch.average_block_duration_ns()))
                    .on_hover_text(format!("{} completed timestamped blocks; total duration {}", arch.timed_blocks, model::duration_text(Some(arch.block_duration_sum_ns as f64))));
                for bytes in [arch.total.data, model::storage(arch.total), arch.total.compressed, arch.total.overhead, arch.total.overalloc] {
                    ui.label(bytes_text(bytes, snapshot.rows, viewer.per_row)).on_hover_text(format!("{bytes} bytes total"));
                }
                ui.label(format!("{:.1}%", 100.0 * arch.total.ratio()));
                ui.label(format!("{:.1}%", 100.0 * arch.total.raw_ratio()));

                ui.end_row();
            }
        });
    });
    selected_changed
}

fn field_table(ui: &mut egui::Ui, viewer: &Viewer) {
    let Some(arch) = viewer
        .selected
        .and_then(|id| viewer.cache.archetypes.get(&id))
    else {
        return;
    };
    ui.label(format!(
        "Selected {}:{} · {} rows · {} blocks · {}",
        arch.snapshot.id.callsite,
        arch.snapshot.id.archetype,
        arch.snapshot.rows,
        arch.snapshot.blocks,
        stats_text(arch.total, arch.snapshot.rows, viewer.per_row)
    ));
    ui.label(format!("Average block duration: {} · {} completed timestamped blocks",
        model::duration_text(arch.average_block_duration_ns()), arch.timed_blocks))
        .on_hover_text("Average timestamp envelope per completed block; event times and span start times only. Active blocks excluded.");
    egui::CollapsingHeader::new("Per-row components · completed blocks only").default_open(true).show(ui, |ui| {
        ui.small("Masks deduplicate constant byte positions. First-row bytes are counted; row headers are included; active blocks, frame metadata, masks, and interned string allocations are excluded.");
        if !arch.components.iter().any(|field| field.blocks > 0) {
            ui.label("No completed blocks yet.");
            return;
        }
        egui::ScrollArea::both().id_salt("field_stats").max_height(120.0).show(ui, |ui| {
            egui::Grid::new("field_compression").striped(true).show(ui, |ui| {
                for label in ["Field", "Type", "Sizeof", "Effective size", "Data", "Packed", "Ratio %"] { ui.strong(label); }
                ui.end_row();
                for (component, field) in arch.snapshot.components.iter().zip(&arch.components) {
                    ui.label(&component.name);
                    ui.label(&component.kind);
                    ui.label(component.range.len().to_string());
                    ui.label(bytes_text(field.packed, field.rows, true)).on_hover_text("Packed bytes / completed rows");
                    ui.label(model::size_text(field.raw));
                    ui.label(model::size_text(field.packed));
                    ui.label(format!("{:.1}%", model::compression_percent(field.raw, field.packed)));
                    ui.end_row();
                }
            });
        });
    });
}

fn string_tab(ui: &mut egui::Ui, viewer: &mut Viewer) {
    ui.heading("Strings");
    if ui.button("Refresh strings").clicked() {
        viewer.refresh_strings();
    }
    let snapshot = &viewer.strings;
    ui.label(format!(
        "{} indexed strings · {} tracked strings · {} counted constructions",
        snapshot.entries.len(),
        snapshot.tracked,
        snapshot.constructions
    ));
    ui.label(stats_text(snapshot.total(), 1, false));
    ui.label(format!(
        "Indexed handle size {} bytes · max ID {} · tagged IDs require {} bytes",
        size_of::<tracing_table::data::stringcache::AtomIndexed>(),
        snapshot
            .max_index
            .map_or_else(|| "—".into(), |id| id.to_string()),
        snapshot.index_bytes
    ));
    ui.small("Short suffixes are inline text, not IDs. Mixing inline strings with indexed IDs can make all eight handle bytes vary within one block.");
    ui.small("Global cumulative indexed-string constructions; counts are not live references. Inline strings (≤7 bytes) and non-indexed Atom constructions are not tracked. Clearing logs retains this global index.");
    ui.label(format!(
        "Unique text {} · handles {} · entry metadata {} · fixed Atom table {}",
        model::size_text(snapshot.unique_bytes),
        model::size_text(snapshot.handle_bytes),
        model::size_text(snapshot.atoms.overhead),
        model::size_text(snapshot.atoms.overalloc)
    ));
    ui.label(format!("Tracked text length (bytes): min {} · average {} · max {} · average constructions/string {}",
        snapshot.min_bytes, model::significant_text(snapshot.unique_bytes as f64 / snapshot.tracked.max(1) as f64),
        snapshot.max_bytes, model::significant_text(snapshot.constructions as f64 / snapshot.tracked.max(1) as f64)));
    egui::Grid::new("string_breakdown")
        .striped(true)
        .show(ui, |ui| {
            for label in [
                "Storage",
                "Data",
                "Compressed",
                "Overhead",
                "Overalloc",
                "Total",
                "Ratio %",
                "Raw ratio %",
            ] {
                ui.strong(label);
            }
            ui.end_row();
            for (label, stats) in [
                ("Atom strings + handles", snapshot.atoms),
                ("String → index map", snapshot.forward),
                ("Index → string map (occupied estimate)", snapshot.reverse),
                ("Total known/modeled storage", snapshot.total()),
            ] {
                ui.label(label);
                for (column, bytes) in [
                    stats.data,
                    stats.compressed,
                    stats.overhead,
                    stats.overalloc,
                    model::storage(stats),
                ]
                .into_iter()
                .enumerate()
                {
                    // Missing reverse capacity is unknown, rather than zero cost.
                    if label.starts_with("Index →") && column == 3 {
                        ui.label("unknown");
                    } else {
                        ui.label(model::size_text(bytes))
                            .on_hover_text(format!("{bytes} bytes"));
                    }
                }
                if stats.data == 0 {
                    ui.label("—");
                    ui.label("—");
                } else {
                    ui.label(format!("{:.1}%", 100.0 * stats.ratio()));
                    ui.label(format!("{:.1}%", 100.0 * stats.raw_ratio()));
                }
                ui.end_row();
            }
        });
    ui.small(format!("Estimated forward-map capacity: {} ({} spare). Reverse-map and counts-map capacities are not exposed by the recorder.",
        snapshot.estimated_forward_capacity, snapshot.estimated_forward_capacity - snapshot.entries.len()));
    ui.small("Estimates: compressed = unique text + 8 bytes per counted handle; Atom entry metadata = 40 bytes/string; fixed Atom table = 4096 × 40 bytes. The forward-map estimate includes spare capacity; only occupied reverse-map buckets can be estimated. Reverse-map spare capacity, counts-map allocation, viewer snapshots, and retention pools are excluded.");
    ui.separator();
    // Matching fixed widths keep headers aligned with only the visible rows.
    // show_rows requires single-line, fixed-height rows; escape embedded newlines.
    let index_width = 80.0;
    let count_width = 100.0;
    let mut sort_changed = false;
    ui.horizontal(|ui| {
        for (column, label, width) in [
            (StringSortColumn::Index, "Index", index_width),
            (StringSortColumn::Count, "Count", count_width),
        ] {
            let arrow = if viewer.string_sort == column {
                if viewer.string_descending { " v" } else { " ^" }
            } else {
                ""
            };
            if ui
                .add_sized([width, 0.0], egui::Button::new(format!("{label}{arrow}")))
                .clicked()
            {
                if viewer.string_sort == column {
                    viewer.string_descending = !viewer.string_descending;
                } else {
                    viewer.string_sort = column;
                    viewer.string_descending = column == StringSortColumn::Count;
                }
                sort_changed = true;
            }
        }
        ui.strong("String");
    });
    if sort_changed {
        viewer
            .strings
            .sort(viewer.string_sort, viewer.string_descending);
    }
    if viewer.strings.entries.is_empty() {
        ui.label(
            "No indexed strings yet. Enable Indexed atoms on the Logs tab to populate this index.",
        );
    }
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace) + 4.0;
    let mut scroll = egui::ScrollArea::both()
        .id_salt("indexed_strings")
        .auto_shrink([false, false]);
    if sort_changed {
        scroll = scroll.vertical_scroll_offset(0.0);
    }
    scroll.show_rows(ui, row_height, viewer.strings.entries.len(), |ui, range| {
        for position in range {
            let entry = &viewer.strings.entries[position];
            ui.horizontal(|ui| {
                ui.add_sized(
                    [index_width, row_height],
                    egui::Label::new(egui::RichText::new(entry.index.to_string()).monospace()),
                );
                ui.add_sized(
                    [count_width, row_height],
                    egui::Label::new(egui::RichText::new(entry.count.to_string()).monospace()),
                );
                let text = entry
                    .text
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t");
                ui.add_sized(
                    [viewer.string_width, row_height],
                    egui::Label::new(egui::RichText::new(text).monospace()).truncate(),
                )
                .on_hover_text(&*entry.text);
            });
        }
    });
}

pub fn draw(
    mut contexts: EguiContexts,
    mut viewer: ResMut<Viewer>,
    mut settings: ResMut<SceneSettings>,
) -> Result {
    let viewer = &mut *viewer;
    if viewer.last_refresh.elapsed() >= Duration::from_millis(viewer.refresh_ms) {
        viewer.cache.refresh(&viewer.recorder);
        viewer.refresh_strings();
        viewer.last_refresh = Instant::now();
        if viewer.selected.is_none() {
            // Prefer actual scene events rather than startup-only engine spans.
            viewer.selected = viewer
                .cache
                .archetypes
                .values()
                .filter(|arch| {
                    arch.snapshot.kind == RecordKind::Event
                        && arch.snapshot.metadata.target().ends_with("::scene")
                })
                .max_by_key(|arch| arch.snapshot.rows)
                .map(|arch| arch.snapshot.id);
        }
        viewer.reload_rows();
    }
    let context = contexts.ctx_mut()?;
    context.options_mut(|options| options.zoom_with_keyboard = false);
    // Explicit shortcuts keep the tuning slider and keyboard zoom in sync.
    context.input_mut(|input| {
        if input.consume_key(egui::Modifiers::CTRL, egui::Key::Plus)
            || input.consume_key(egui::Modifiers::CTRL, egui::Key::Equals)
        {
            viewer.scale = (viewer.scale + 0.1).clamp(0.5, 3.0);
        }
        if input.consume_key(egui::Modifiers::CTRL, egui::Key::Minus) {
            viewer.scale = (viewer.scale - 0.1).clamp(0.5, 3.0);
        }
    });
    context.set_zoom_factor(viewer.scale);
    egui::Window::new("Tracing compression").default_size([900.0, 800.0]).min_size([600.0, 600.0]).show(context, |ui| {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut viewer.strings_tab, false, "Logs");
            ui.selectable_value(&mut viewer.strings_tab, true, "Strings");
        });
        ui.separator();
        let mut reload = false;
        ui.collapsing("Tuning", |ui| {
            ui.add(egui::Slider::new(&mut viewer.refresh_ms, 100..=5000).text("Refresh ms"));
            reload |= ui.add(egui::Slider::new(&mut viewer.row_limit, 1..=5000).text("Matching rows")).changed();
            ui.add(egui::Slider::new(&mut viewer.scale, 0.5..=3.0).text("UI scale"));
            ui.add(egui::Slider::new(&mut viewer.string_width, 240.0..=1600.0).text("String column width"));
            ui.add(egui::Slider::new(&mut settings.logs_per_second, 0.0..=240.0).text("Scene log pulses/sec"));
            ui.add(egui::Slider::new(&mut settings.move_speed, 0.1..=8.0).text("Cube move speed"));
            ui.add(egui::Slider::new(&mut settings.turn_speed, 0.01..=1.0).text("Cube turn speed"));
            ui.add(egui::Slider::new(&mut settings.sphere_scale_factor, 0.0..=0.2).text("Sphere scale factor"));
        });
        if viewer.strings_tab {
            string_tab(ui, viewer);
            return;
        }
        ui.label("All targets at TRACE · synchronous recording · whole-archetype frame stats");
        ui.small("Sizes: bytes, K = 1,000, M = 1,000,000 (3 significant figures). Interned string allocations excluded.");
        let rows = viewer.cache.archetypes.values().map(|arch| arch.snapshot.rows).sum();
        let blocks: usize = viewer.cache.archetypes.values().map(|arch| arch.snapshot.blocks).sum();
        ui.label(format!("{rows} rows · {blocks} blocks · {}", stats_text(viewer.cache.total, rows, viewer.per_row)));
        ui.small(format!("{} archetypes · {} completed-frame stat calculations", viewer.cache.archetypes.len(), viewer.cache.completed_recomputations));
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut viewer.per_row, "Bytes / row");
            if ui.checkbox(&mut viewer.indexed, "Indexed atoms").on_hover_text("Change storage mode and clear all recorded logs").changed() {
                viewer.control.set_indexed(viewer.indexed);
                viewer.cache = StatsCache::default();
                viewer.selected = None;
                viewer.rows.clear();
                viewer.entity_choices.clear();
                viewer.last_refresh = Instant::now() - Duration::from_millis(viewer.refresh_ms);
            }
            ui.label("Archetypes:");
            ui.add(egui::TextEdit::singleline(&mut viewer.archetype_filter).desired_width(170.0));
            ui.label("Entity:");
            reload |= ui.add(egui::TextEdit::singleline(&mut viewer.entity_filter).desired_width(180.0).hint_text("numeric ID or str:debug value")).changed();
            ui.menu_button("Select entity…", |ui| {
                if ui.button("All entities").clicked() { viewer.entity_filter.clear(); reload = true; ui.close(); }
                ui.small(format!("Entities from newest {} unfiltered records", viewer.row_limit));
                egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                    for entity in &viewer.entity_choices {
                        if ui.selectable_label(viewer.entity_filter == *entity, entity).clicked() {
                            viewer.entity_filter = entity.clone(); reload = true; ui.close();
                        }
                    }
                });
            });
            if ui.button("Clear entity").clicked() { viewer.entity_filter.clear(); reload = true; }
            if ui.button("Refresh").clicked() {
                viewer.cache.refresh(&viewer.recorder); viewer.refresh_strings(); viewer.last_refresh = Instant::now(); reload = true;
            }
        });
        // Reserve a real pane height, then use a small drag target between panes.
        // Clamping leaves room for inspection even with the tuning section open.
        let available = ui.available_height().max(240.0);
        let top_height = (available * viewer.split_fraction).clamp(80.0, (available - 160.0).max(80.0));
        reload |= archetype_table(ui, viewer, top_height);
        let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 8.0), egui::Sense::drag());
        if response.dragged() { viewer.split_fraction = ((top_height + response.drag_delta().y) / available).clamp(0.1, 0.8); }
        let stroke = if response.hovered() || response.dragged() { ui.visuals().widgets.active.bg_stroke } else { ui.visuals().widgets.noninteractive.bg_stroke };
        ui.painter().hline(rect.x_range(), rect.center().y, stroke);
        response.on_hover_cursor(egui::CursorIcon::ResizeVertical).on_hover_text("Drag to resize archetypes and logs");
        if reload { viewer.reload_rows(); }
        field_table(ui, viewer);
        ui.small(format!("Newest {} matching records · click an entity button to filter its logs", viewer.rows.len()));
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace) + 8.0;
        let mut picked_entity = None;
        egui::ScrollArea::both().id_salt("logs").auto_shrink([false, false]).show_rows(ui, row_height, viewer.rows.len(), |ui, range| {
            for index in range {
                let row = &viewer.rows[index];
                let timestamp = row.timestamp_ns.map_or_else(|| "span update".into(), |ns| format!("{:.6}s", ns as f64 / 1e9));
                let fields = row.fields.iter().filter(|(name, _)| name != "message" && name != "entity")
                    .map(|(name, value)| format!("{name}={}", model::field_text(value))).collect::<Vec<_>>().join(" ");
                ui.horizontal(|ui| {
                    if let Some((_, entity)) = row.fields.iter().find(|(name, _)| name == "entity") {
                        let entity = model::field_text(entity);
                        if ui.small_button(format!("entity={entity}")).on_hover_text("View logs for this entity").clicked() { picked_entity = Some(entity); }
                    }
                    if let (Some(reference), Some(selected)) = (row.extra_fields, viewer.selected) {
                        ui.small_button(format!("extra {}:{}", reference.archetype, reference.block))
                            .on_hover_ui(|ui| {
                                let span = SpanId { callsite: selected.callsite as u32, archetype: selected.archetype, index: row.index as u64 };
                                if let Some(fields) = viewer.recorder.span_fields(span, reference) {
                                    for (name, value) in fields { ui.monospace(format!("{name}={}", model::field_text(&value))); }
                                } else { ui.label("Extra fields unavailable"); }
                            });
                    }
                    ui.monospace(format!("#{} {timestamp} {} {fields}", row.index, model::message(row)))
                        .on_hover_text(format!("end={:?} span={:?} parent={:?} follows={:?}", row.end_timestamp_ns, row.span, row.parent, row.follows_from));
                });
            }
        });
        // Apply row-button selection after rendering, when no row is borrowed.
        if let Some(entity) = picked_entity { viewer.entity_filter = entity; viewer.reload_rows(); }
    });
    Ok(())
}
