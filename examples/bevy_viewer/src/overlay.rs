//! Basic UI. All recording locks are released before their owned results render.
use crate::{
    model::{self, SortColumn, SortOrder, StatsCache},
    scene::SceneSettings,
};
use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tracing_table::{
    data::dataframe::CompressionStats,
    subscriber::{ArchetypeId, RecordKind, RecordedRow, RecordingLayer},
};

#[derive(Resource)]
pub struct Viewer {
    recorder: Arc<RecordingLayer>,
    cache: StatsCache,
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
    pub fn new(recorder: Arc<RecordingLayer>) -> Self {
        Self {
            recorder,
            cache: StatsCache::default(),
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

fn stats_text(stats: CompressionStats) -> String {
    format!(
        "raw {} · packed {} · overhead {} · unused {} · compression {:.1}%",
        model::size_text(stats.data),
        model::size_text(stats.compressed),
        model::size_text(stats.overhead),
        model::size_text(stats.overalloc),
        model::compression_percent(stats.data, stats.compressed)
    )
}

fn archetype_table(ui: &mut egui::Ui, viewer: &mut Viewer, height: f32) -> bool {
    let mut selected_changed = false;
    let ids = model::sorted_archetypes(&viewer.cache, &viewer.archetype_filter, viewer.sort);
    egui::ScrollArea::both().id_salt("archetype_table").max_height(height)
        .auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("archetypes").striped(true).max_col_width(160.0).show(ui, |ui| {
            for (column, label) in SortColumn::COLUMNS {
                let arrow = if viewer.sort.column == column { if viewer.sort.descending { " v" } else { " ^" } } else { "" };
                let response = ui.button(format!("{label}{arrow}"));
                if response.clicked() { viewer.sort.select(column); }
                match column {
                    SortColumn::Compression => { response.on_hover_text("Percent of raw row bytes removed: 100 × (1 − packed / raw)."); }
                    SortColumn::Storage => { response.on_hover_text("Share of all measured storage: packed + overhead + unused. Independent of table/entity filters."); }
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
                ui.label(format!("{:?}", snapshot.kind)).on_hover_text(format!("{:?}", snapshot.fields));
                ui.label(snapshot.rows.to_string());
                for bytes in [arch.total.data, arch.total.compressed, arch.total.overhead, arch.total.overalloc] {
                    ui.label(model::size_text(bytes)).on_hover_text(format!("{bytes} bytes"));
                }
                ui.label(format!("{:.1}%", model::compression_percent(arch.total.data, arch.total.compressed)));
                ui.label(format!("{:.1}%", model::storage_percent(arch.total, viewer.cache.total)));
                ui.label(snapshot.blocks.to_string());
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
        "Selected {}:{} · {}",
        arch.snapshot.id.callsite,
        arch.snapshot.id.archetype,
        stats_text(arch.total)
    ));
    egui::CollapsingHeader::new("Per-field compression · completed blocks only").default_open(true).show(ui, |ui| {
        ui.small("Masks deduplicate constant byte positions. First-row bytes are counted; active blocks, headers, masks, and interned string allocations are excluded.");
        if !arch.fields.iter().any(|field| field.blocks > 0) {
            ui.label("No completed blocks yet.");
            return;
        }
        egui::ScrollArea::both().id_salt("field_stats").max_height(120.0).show(ui, |ui| {
            egui::Grid::new("field_compression").striped(true).show(ui, |ui| {
                for label in ["Field", "Type", "Raw", "Packed", "Compression %", "Constant bytes/block", "Blocks", "Rows"] { ui.strong(label); }
                ui.end_row();
                for ((name, kind), field) in arch.snapshot.fields.iter().zip(&arch.fields) {
                    ui.label(name);
                    ui.label(kind.as_ref().map_or_else(|| "absent".into(), |kind| format!("{kind:?}")));
                    ui.label(model::size_text(field.raw));
                    ui.label(model::size_text(field.packed));
                    ui.label(format!("{:.1}%", model::compression_percent(field.raw, field.packed)));
                    ui.label(if field.blocks == 0 { "—".into() } else { model::significant_text(field.constant_bytes as f64 / field.blocks as f64) });
                    ui.label(field.blocks.to_string()); ui.label(field.rows.to_string()); ui.end_row();
                }
            });
        });
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
    context.set_zoom_factor(viewer.scale);
    egui::Window::new("Tracing compression").default_size([900.0, 800.0]).min_size([600.0, 600.0]).show(context, |ui| {
        ui.label("All targets at TRACE · synchronous recording · whole-archetype frame stats");
        ui.small("Sizes: bytes, K = 1,000, M = 1,000,000 (3 significant figures). Interned string allocations excluded.");
        ui.label(stats_text(viewer.cache.total));
        ui.small(format!("{} archetypes · {} completed-frame stat calculations", viewer.cache.archetypes.len(), viewer.cache.completed_recomputations));
        let mut reload = false;
        ui.collapsing("Tuning", |ui| {
            ui.add(egui::Slider::new(&mut viewer.refresh_ms, 100..=5000).text("Refresh ms"));
            reload |= ui.add(egui::Slider::new(&mut viewer.row_limit, 1..=5000).text("Matching rows")).changed();
            ui.add(egui::Slider::new(&mut viewer.scale, 0.75..=1.75).text("UI scale"));
            ui.add(egui::Slider::new(&mut settings.logs_per_second, 0.0..=240.0).text("Scene log pulses/sec"));
            ui.add(egui::Slider::new(&mut settings.move_speed, 0.1..=8.0).text("Cube move speed"));
            ui.add(egui::Slider::new(&mut settings.turn_speed, 0.01..=1.0).text("Cube turn speed"));
            ui.add(egui::Slider::new(&mut settings.sphere_scale_factor, 0.0..=0.2).text("Sphere scale factor"));
        });
        ui.horizontal(|ui| {
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
                viewer.cache.refresh(&viewer.recorder); viewer.last_refresh = Instant::now(); reload = true;
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
