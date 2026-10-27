"""Render storage pies and histograms from one measured CSV (requires pycairo).

Run: python3 util/plot_archived_storage.py --prefix doc/reference_archived_storage
The collector is benches/archived_storage.rs. See doc/storage_chart.md for the
CSV contract and statistical denominators. No replay is needed to tune a chart.
"""
import argparse
import csv
import html
import io
import xml.etree.ElementTree as ET
import math
import statistics
import textwrap
from collections import defaultdict
from functools import lru_cache
from pathlib import Path

import cairo


# Visual tuning: palette and shared radial geometry.
ROOT = Path(__file__).resolve().parents[1]
PREFIX = ROOT / "doc/reference_archived_storage"
RADIUS = 180
# Paired-pie tuning: satellite size and center relative to the main center.
SMALL_PIE_SCALE = 0.33
SMALL_PIE_DX = 245
SMALL_PIE_DY = 195
SMALL_PIE_LABEL_SIZE = 17
SMALL_PIE_LABEL_GAP = 22
PAIRED_KEY_X = 555
# Dataset selectors occupy a new gutter; the existing chart shifts as one unit.
DATASET_GUTTER = 220
DATASET_PIE_X = 130
# Left selectors can use more of the section height than the weighting pie.
DATASET_PIE_SCALE = 0.5
DATASET_PIE_Y = (250, 500)
START_ANGLE = -math.pi / 2
FONT_SIZE = 19
SLICE_LABEL_MIN_PERCENT = 5.0
RATE_SMALL_DECIMALS = 2
INDEX_MODE_DARKEN = {'constant': 0.0, 'raw': 0.0, 'offset': 0.12,
                     'cycle': 0.22, 'dictionary': 0.32, 'dictionary_cycle': 0.42,
                     'residual': 0.22}
INDEX_MODE_LABELS = {'constant': 'constant', 'raw': 'raw', 'offset': 'offset residual',
                     'cycle': 'cycle residual', 'dictionary': 'dictionary',
                     'dictionary_cycle': 'dictionary + cycle', 'residual': 'residual'}
OVERHEAD_LIGHTEN = 0.62
RADIAL_LABEL_RADIUS = 0.80
RADIAL_BORDER_WIDTH = 1.5
COLORS = [
    (0.20, 0.42, 0.70), (0.12, 0.62, 0.55), (0.94, 0.57, 0.22),
    (0.58, 0.42, 0.72), (0.83, 0.35, 0.40), (0.34, 0.66, 0.83),
    (0.73, 0.57, 0.31), (0.53, 0.64, 0.27), (0.68, 0.49, 0.63),
    (0.38, 0.46, 0.51), (0.72, 0.73, 0.76),
]
GROUPS = ["metadata", "string_field_index", "format_args_index", "non_string_fields", "compressed_payload", "compressed_index", "end_buffer"]
OVERHEAD_LABELS = {
    "block_cache_structs": "BlockCache inline structs",
    "block_scratch": "Block edit-scratch capacity",
    "block_entries": "Occupied block descriptors (Data)",
    "block_spare": "Spare block-descriptor capacity",
    "block_span_refs": "Block-owned span-reference lists",
    "frame_headers": "Archived heap headers (including shared metadata)",
    "frame_padding": "Archived heap alignment padding",
    "string_collection_structs": "BufferCollection inline structs",
    "string_descriptors": "Occupied compressed-buffer descriptors",
    "string_spare": "Spare compressed-buffer descriptor capacity",
    "fsst_tables": "Shared FSST table allocations",
    "archetype_structs": "PerArchData/schema headers excluding the two containers",
    "archetype_fields": "Archetype field-type and format-reference capacity",
    "format_recipes": "Callsite format recipes",
    # The collector charges the shared weak parent lookup once, as metadata.
    "parent_timestamp_registry": "Parent timestamp weak index",
}
BLOCK_OVERVIEW_LABELS = {
    "block_entries": "Block buffers: used descriptors",
    "frame_headers": "Block buffers: heap headers",
    "frame_padding": "Block buffers: alignment padding",
    "block_spare": "Block buffers: spare descriptors",
    "block_cache_structs": "Block buffers: cache structs",
    "block_scratch": "Block buffers: edit scratch",
    "block_span_refs": "Block buffers: span-reference lists",
}
# Detailed payload pies repeat only their owner's metadata; the overall archive
# pie continues to keep all metadata in its separate slice.
PAYLOAD_BLOCK_OVERHEAD_KEYS = tuple(key for key in BLOCK_OVERVIEW_LABELS if key != 'block_spare')
PAYLOAD_STRING_OVERHEAD_KEYS = ('string_collection_structs', 'string_descriptors',
                                'string_spare', 'fsst_tables')
BLOCK_OVERHEAD_LABEL = 'Per-block overhead'
OVERVIEW_COLORS = {
    "Span payload": COLORS[0],
    "Span rows + strings": COLORS[0],
    # Tune event and bridged-log hues independently in the archive overview.
    "Event payload": COLORS[1],
    "Log payload": (0.02, 0.32, 0.30),
    "Event/log rows + strings": COLORS[1],
    "Block buffers: used descriptors": (0.82, 0.39, 0.10),
    "Block buffers: heap headers": (0.95, 0.59, 0.20),
    # Give alignment its own tunable hue beside the heap-header slice.
    "Block buffers: alignment padding": (0.98, 0.80, 0.45),
    "Block buffers: spare descriptors": (0.70, 0.52, 0.28),
    "Block buffers: cache structs": (0.95, 0.75, 0.40),
    "Block buffers: edit scratch": (0.55, 0.32, 0.16),
    "Block buffers: span-reference lists": (0.62, 0.43, 0.32),
    # Trailer payload uses a distinct hue from container metadata.
    "End buffer: parent table": (0.40, 0.49, 0.25),
    "String-buffer collection overhead": COLORS[3],
    "Archetype overhead": COLORS[4],
    "Metadata overhead": COLORS[2],
}
METADATA_PIE_LABELS = {
    "format_recipes": "Callsite format recipes",
    "parent_timestamp_registry": "Parent timestamp weak index",
    **BLOCK_OVERVIEW_LABELS,
    "string_collection_structs": "String buffers: inline structs",
    "string_descriptors": "String buffers: used descriptors",
    "string_spare": "String buffers: spare descriptors",
    "fsst_tables": "FSST tables (trained archetypes)",
    "archetype_structs": "Archetypes: schema headers",
    "archetype_fields": "Archetypes: field and format capacity",
}
OVERVIEW_COLORS.update({
    # Tune the shared-codec slice independently of string descriptors.
    "FSST tables (trained archetypes)": (0.25, 0.60, 0.65),
    "String buffers: inline structs": (0.48, 0.33, 0.65),
    "String buffers: used descriptors": (0.64, 0.49, 0.77),
    "String buffers: spare descriptors": (0.76, 0.65, 0.85),
    "Archetypes: schema headers": (0.77, 0.27, 0.36),
    "Archetypes: field and format capacity": (0.91, 0.53, 0.59),
})
LIFETIME_LABELS = {
    "archived_messages": "Messages: archived / compacted",
    "active_messages": "Messages: active / uncompressed",
    "archived_strings": "Strings: archived / LZ4",
    "writable_active_raw": "Strings: writable / uncompressed",
}
LIFETIME_OVERHEAD_KEYS = {
    "archived_messages": "archived_message_overhead",
    "active_messages": "active_message_overhead",
    "archived_strings": "archived_string_overhead",
    "writable_active_raw": "writable_string_overhead",
}
OVERVIEW_COLORS.update(zip(LIFETIME_LABELS.values(), [
    (0.20, 0.42, 0.70), (0.43, 0.67, 0.87),
    (0.12, 0.62, 0.55), (0.35, 0.73, 0.48),
]))
LABELS = {
    "metadata": "Block metadata",
    "string_field_index": "String-field indices",
    "format_args_index": "Format-argument indices",
    "non_string_fields": "Non-string fields",
    "compressed_payload": "Strings: compressed payload",
    "compressed_index": "Strings: compressed index",
    "end_buffer": "End buffer: parent table",
    "timestamp": "Timestamp offset (unsigned)",
    "timestamp_index": "Timestamp index (u16)",
    "end_timestamp": "End offset (duration + 1)",
    "spanfieldsref": "SpanFieldsRef",
    "follows_from": "Follows-from SpanId",
    "parent": "Parent SpanId",
    "parent_archetype_callsite": "Parent SpanId: archetype + callsite",
    "parent_index": "Parent SpanId: index",
    "update_span": "Span-update SpanId",
    "row_padding": "Row padding",
}
METADATA_COMPONENTS = {
    "timestamp": ("timestamp",),
    "timestamp_index": ("timestamp_index",),
    "end_timestamp": ("end_timestamp",),
    "parent_archetype_callsite": ("parent.archetype", "parent.callsite", "parent.table_index"),
    "parent_index": ("parent.index",),
    # All span references can use a residual plus shared identity-table key.
    "follows_from": ("follows_from.index", "follows_from.callsite", "follows_from.archetype", "follows_from.table_index"),
    "update_span": ("update_span.index", "update_span.callsite", "update_span.archetype", "update_span.table_index"),
}
COMPONENT_ORDER = ("timestamp", "timestamp_index", "end_timestamp", "parent_archetype_callsite",
                   "parent_index", "follows_from", "spanfieldsref", "update_span", *GROUPS[1:])
COMPONENT_COLORS = {LABELS[name]: COLORS[index % len(COLORS)]
                    for index, name in enumerate(name for name in COMPONENT_ORDER
                                                 if name not in {"parent_archetype_callsite", "timestamp_index"})}
COMPONENT_COLORS[LABELS["parent_archetype_callsite"]] = (0.77, 0.49, 0.13)
COMPONENT_COLORS[LABELS["timestamp_index"]] = (0.26, 0.65, 0.39)
COMPONENT_COLORS[LABELS["row_padding"]] = (0.83, 0.35, 0.40)
COMPONENT_COLORS[LABELS["end_buffer"]] = OVERVIEW_COLORS[LABELS["end_buffer"]]
# Keep the fully overhead wedge distinct from field and string hues.
COMPONENT_COLORS[BLOCK_OVERHEAD_LABEL] = (0.55, 0.32, 0.16)
SPAN_ONLY_COMPONENTS = {"end_timestamp", "follows_from", "spanfieldsref", "update_span"}
METRICS = ["first_bytes", "mask_bytes", "varying_bytes", "padding_bytes", "compressed_bytes", "total_bytes"]
LEAF_METRICS = METRICS + ["column_values"]
INDEX_GROUPS = ("string_field_index", "format_args_index")


# Histogram geometry and adaptive axis tuning.
PAGE_WIDTH = 1675
MARGIN = 48
HIST_HEADER_HEIGHT = 155
CARD_HEIGHT = 420
CARD_GAP = 24
SECTION_GAP = 16
# One column of histograms, each followed horizontally by its extrema table.
HISTOGRAM_WIDTH = 725
EXTREMA_LEFT = 835
PLOT_TOP = 65
PLOT_AXIS_Y = 165
PLOT_HALF_HEIGHT = 150
BIN_COUNT = 32
BUCKET_WIDTH = None  # None selects a width from both populations' useful range.
AUTO_RANGE_PERCENTILE = 95.0
MIN_BUCKET_WIDTH = 1.0
X_OUTLIER_RATIO = 3.0
# Each population chooses its break using the same weighted contribution rule.
Y_BREAK_RATIO = 2.0
Y_BREAK_GAP = 6
Y_BREAK_LOWER_FRACTION = 0.72
Y_BREAK_EDGE_PADDING = 0.10
WAVE_AMPLITUDE = 2.0
CARD_BACKGROUND = (0.97, 0.975, 0.98)
MIN_PERCENT = 5.0
SOURCE_FONT = 17
SOURCE_PATH_FONT = 14
SOURCE_PATH_MIN_FONT = 11
# Compact statistic rows, with one callsite exemplar per statistic.
STAT_TOP = 28
STAT_ENTRY_STEP = 108
SOURCE_CHARS = 90
INK = (0.14, 0.18, 0.23)
MUTED = (0.39, 0.43, 0.49)


# Pie/key geometry; adjust independently of histogram geometry.
HEADER_HEIGHT = 300
ROW_MIN_HEIGHT = 700
ROW_GAP = 28
PIE_CENTER_X = 225
PIE_CENTER_Y = 345
LEGEND_X = 435
LEGEND_TOP = 155
LEGEND_STEP = 56
# Two-line keys need less space than the payload keys' three-line breakdowns.
COMPACT_LEGEND_STEP = 42
PAYLOAD_COMPACT_LEGEND_STEP = 36
LEGEND_NAME_SIZE = 16
LEGEND_RATE_SIZE = 14
LEGEND_BREAKDOWN_SIZE = 13
# Pack columns to their measured text widths; these knobs tune breathing room.
KEY_RIGHT_MARGIN = 16
PIE_COLUMN_GAP = 20
SINGLE_PIE_MIN_WIDTH = 1250
NONFIELD_TOP = 150
NONFIELD_FONT_SIZE = 17
MARGIN = 48


def total(data, owner=None, group=None, component=None, metric="total_bytes"):
    return sum(values[metric] for (kind, section, name), values in data.items()
               if (owner is None or kind in owner) and (group is None or section == group)
               and (component is None or name == component
                    or (component in INDEX_GROUPS and name.startswith(component + "."))))


def measurement_component(row):
    """Preserve optional encoding-mode detail when historical evidence supplies it."""
    component = row['component']
    mode = row.get('storage_mode')
    return component + '.' + mode if row['group'] in INDEX_GROUPS and mode else component


def index_slice_label(group, mode):
    """Related hues identify storage modes without confusing radial constant tints."""
    base = LABELS[group]
    name = base.removesuffix(' (raw / residual)') + f' ({INDEX_MODE_LABELS.get(mode, mode)})'
    color = COMPONENT_COLORS[base]
    COMPONENT_COLORS[name] = tuple(c * (1 - INDEX_MODE_DARKEN.get(mode, 0)) for c in color)
    return name


def kib(value):
    return f"{value / 1024:,.3f}"


def format_rate(value):
    """Integers above ten, tenths above one, then enough places for tiny rates.

    Strip trailing zeroes and re-evaluate rounded magnitudes at boundaries so
    9.99 becomes 10 and 0.999 becomes 1, never 10.0 or a false zero.
    """
    if value == 0:
        return '0'
    magnitude = abs(value)
    decimals = (0 if magnitude >= 10 else 1 if magnitude >= 1 else
                max(RATE_SMALL_DECIMALS, -math.floor(math.log10(magnitude))))
    rounded = round(value, decimals)
    if abs(rounded) >= 10:
        decimals = 0
    result = f'{rounded:.{decimals}f}'
    return result.rstrip('0').rstrip('.') if decimals else result


def per_row(value, rows):
    # The caller chooses the explicitly labeled total-row or per-kind denominator.
    return format_rate(value / rows)


def overview_slices(values, metadata):
    # Keep event and bridged-log ownership separate in the second pie.
    # Attribute each end table to the same owner as its rows and strings.
    # Detailed span/event pies expose table and padding costs separately.
    slices = [("Span payload", total(values, owner=("span",))),
              ("Event payload", total(values, owner=("event",))),
              ("Log payload", total(values, owner=("log",)))]
    # Consolidate metadata here; its disjoint measured leaves get a separate pie.
    slices.append(("Metadata overhead", sum(metadata[key] for key in OVERHEAD_LABELS)))
    assert math.isclose(sum(value for _, value in slices),
                        total(values) + sum(metadata[key] for key in OVERHEAD_LABELS), abs_tol=1e-5)
    return slices


def metadata_slices(metadata):
    assert set(METADATA_PIE_LABELS) == set(OVERHEAD_LABELS)
    return [(name, metadata[key]) for key, name in METADATA_PIE_LABELS.items()]


def lifetime_slices(values):
    # Archived strings combine frozen and pruned writable-buffer snapshots.
    # Writable strings retain every active reference, including shared entries.
    return [(name, values[key] + values.get(LIFETIME_OVERHEAD_KEYS[key], 0)) for key, name in LIFETIME_LABELS.items()]


def text(ctx, x, y, value, size=FONT_SIZE, bold=False, color=(0.14, 0.18, 0.23), center=False):
    ctx.select_font_face("sans-serif", cairo.FONT_SLANT_NORMAL,
                         cairo.FONT_WEIGHT_BOLD if bold else cairo.FONT_WEIGHT_NORMAL)
    ctx.set_font_size(size)
    ctx.set_source_rgb(*color)
    if center:
        x -= ctx.text_extents(value).x_advance / 2
    ctx.move_to(x, y)
    ctx.show_text(value)


def slice_color(name, index):
    # Fixed colors keep overview leaves and payload components stable across modes.
    return OVERVIEW_COLORS.get(name, COMPONENT_COLORS.get(name, COLORS[index % len(COLORS)]))


def component_slices(values, owner, metric="total_bytes", split_indices=False):
    # Filter headers by owner so the index appears only in the event/log pie/key.
    metadata_names = [name for name in METADATA_COMPONENTS
                      if ("span" in owner or name not in SPAN_ONLY_COMPONENTS)
                      and ("span" not in owner or name != "timestamp_index")]
    slices = [(LABELS[name], sum(total(values, owner=owner, group="metadata", component=part, metric=metric)
                               for part in METADATA_COMPONENTS[name]))
              for name in metadata_names]
    for name in GROUPS[1:]:
        # Historical evidence cannot recover modes; keep its original category.
        split = split_indices and name in INDEX_GROUPS and not any(
            group == name and component == name
            for _, group, component in values)
        if split:
            slices += [(index_slice_label(name, mode),
                        total(values, owner=owner, group=name, component=name + '.' + mode, metric=metric))
                       for mode in INDEX_MODE_LABELS
                       if any(kind in owner and group == name and component == name + '.' + mode
                              for kind, group, component in values)]
        else:
            slices.append((LABELS[name], total(values, owner=owner, group=name, metric=metric)))
    assert math.isclose(sum(value for _, value in slices), total(values, owner=owner, metric=metric), abs_tol=1e-5)
    return slices


def component_breakdown(values, owner, split_indices=False):
    """Move padding out of field totals without changing the pie denominator."""
    metrics = {metric: dict(component_slices(values, owner, metric, split_indices)) for metric in METRICS}
    result = []
    for name, value in component_slices(values, owner, split_indices=split_indices):
        value -= metrics["padding_bytes"][name]
        overhead = sum(metrics[metric][name] for metric in ("first_bytes", "mask_bytes"))
        varying = metrics["varying_bytes"][name]
        compressed = metrics["compressed_bytes"][name]
        assert math.isclose(overhead + varying + compressed, value, abs_tol=1e-5)
        assert compressed == 0 or overhead + varying == 0
        result.append((name, value, overhead, varying, compressed))
    padding = total(values, owner=owner, metric="padding_bytes")
    # Solid slice: padding is neither a field's first/mask nor meaningful bits.
    result.append((LABELS["row_padding"], padding, 0, 0, padding))
    assert math.isclose(sum(row[1] for row in result), total(values, owner=owner), abs_tol=1e-5)
    return result


def payload_breakdown(values, owner, sites=None, denominator=None):
    """Add owned block/string metadata using the same weighting as row payload."""
    selected = eligible_sites(sites, owner) if sites is not None else []
    def cost(keys):
        # Weight each callsite's bytes per row before averaging, including zero
        # costs. Never attribute another record kind's tables or containers.
        return sum(sum(site['values'].get('metadata.' + key, 0) for key in keys) /
                   (site['values'][denominator] if denominator else 1)
                   for site in selected) / (len(selected) if denominator and selected else 1)
    block, strings = cost(PAYLOAD_BLOCK_OVERHEAD_KEYS), cost(PAYLOAD_STRING_OVERHEAD_KEYS)
    result = []
    for name, value, overhead, varying, compressed in component_breakdown(values, owner):
        if name == LABELS['compressed_payload']:
            value += strings
            overhead += strings
        result.append((name, value, overhead, varying, compressed))
    # This entire component is overhead: show a solid wedge, without an inner
    # overhead region or a redundant overhead/payload legend breakdown.
    result.append((BLOCK_OVERHEAD_LABEL, block, 0, 0, block))
    assert math.isclose(sum(row[1] for row in result),
                        total(values, owner=owner) + block + strings, abs_tol=1e-5)
    return result


def overhead_color(color):
    # A shared tint preserves the component hue while identifying inner overhead.
    return tuple(channel + (1 - channel) * OVERHEAD_LIGHTEN for channel in color)


def wedge(ctx, x, y, radius, start, end, color):
    """Procedural asset primitive shared by the two radial cost regions."""
    ctx.move_to(x, y)
    ctx.arc(x, y, radius, start, end)
    ctx.close_path()
    ctx.set_source_rgb(*color)
    ctx.fill()


def radial_component_pie(ctx, x, y, slices, rows):
    """Angles encode component totals; inner/outer areas encode their byte costs."""
    size = sum(value for _, value, *_ in slices)
    angle = START_ANGLE
    for index, (name, value, overhead, varying, compressed) in enumerate(slices):
        if value == 0:
            continue
        end = angle + math.tau * value / size
        color = slice_color(name, index)
        wedge(ctx, x, y, RADIUS, angle, end, color)
        # Squared radii, rather than ring thickness, keep the displayed areas honest.
        inner_radius = RADIUS * math.sqrt(overhead / value)
        if overhead:
            wedge(ctx, x, y, inner_radius, angle, end, overhead_color(color))
            if varying or compressed:
                ctx.arc(x, y, inner_radius, angle, end)
                ctx.set_source_rgb(1, 1, 1)
                ctx.set_line_width(RADIAL_BORDER_WIDTH)
                ctx.stroke()
        # Draw component outlines last so fully overhead slices retain their borders.
        ctx.move_to(x, y)
        ctx.arc(x, y, RADIUS, angle, end)
        ctx.close_path()
        ctx.set_source_rgb(1, 1, 1)
        ctx.set_line_width(RADIAL_BORDER_WIDTH)
        ctx.stroke()
        percent = 100 * value / size
        if percent >= SLICE_LABEL_MIN_PERCENT:
            mid = (angle + end) / 2
            label_radius = RADIAL_LABEL_RADIUS * RADIUS
            text(ctx, x + label_radius * math.cos(mid), y + label_radius * math.sin(mid) + 6,
                 per_row(8 * value, rows), size=19, bold=True, center=True,
                 color=(0.14, 0.18, 0.23) if label_radius < inner_radius + 15 else (1, 1, 1))
            text(ctx, x + label_radius * math.cos(mid), y + label_radius * math.sin(mid) + 22,
                 "bits/row", size=11, center=True,
                 color=(0.14, 0.18, 0.23) if label_radius < inner_radius + 15 else (1, 1, 1))
        angle = end
    assert math.isclose(angle, START_ANGLE + math.tau)


def pie(ctx, x, y, slices, rows):
    """Isolate procedural chart drawing from measurements and report generation."""
    size = sum(value for _, value in slices)
    angle = START_ANGLE
    for index, (name, value) in enumerate(slices):
        if value == 0:
            continue
        end = angle + math.tau * value / size
        ctx.move_to(x, y)
        ctx.arc(x, y, RADIUS, angle, end)
        ctx.close_path()
        ctx.set_source_rgb(*slice_color(name, index))
        ctx.fill_preserve()
        ctx.set_source_rgb(1, 1, 1)
        ctx.set_line_width(2)
        ctx.stroke()
        percent = 100 * value / size
        if percent >= SLICE_LABEL_MIN_PERCENT:
            mid = (angle + end) / 2
            text(ctx, x + 0.68 * RADIUS * math.cos(mid), y + 0.68 * RADIUS * math.sin(mid) + 7,
                 per_row(8 * value, rows), size=21, bold=True, color=(1, 1, 1), center=True)
            text(ctx, x + 0.68 * RADIUS * math.cos(mid), y + 0.68 * RADIUS * math.sin(mid) + 24,
                 "bits/row", size=12, color=(1, 1, 1), center=True)
        angle = end
    assert math.isclose(angle, START_ANGLE + math.tau)


def measurements():
    """Keep owner/component and first/mask/padding attribution independently usable."""
    data = {mode: defaultdict(lambda: defaultdict(float)) for mode in ("false", "true")}
    with PREFIX.with_suffix(".csv").open() as source:
        for row in csv.DictReader(source):
            component = measurement_component(row)
            group = component if row["group"] == "strings" else row["group"]
            leaf = data[row["templates"]][(row["record_kind"], group, component)]
            for metric in LEAF_METRICS:
                value = float(row.get(metric, 0)) if metric != "varying_bytes" else int(row["varying_bits"]) / 8
                leaf[metric] += value
            assert math.isclose(leaf["total_bytes"], sum(leaf[m] for m in METRICS[:-1]), abs_tol=1e-6)
    # Move physical end-buffer bytes into a separate owner-specific payload slice.
    # Its full cost is included in the detailed B/span and B/event denominators.
    for line in PREFIX.with_suffix(".audit.txt").read_text().splitlines():
        if line.startswith("metadata_overhead_by_kind "):
            fields = dict(token.split("=", 1) for token in line.split()[1:])
            leaf = data[fields['templates']][(fields['kind'], 'end_buffer', 'end_buffer')]
            leaf['total_bytes'] = leaf['compressed_bytes'] = (
                int(fields.get('parent_tables', 0)) + int(fields.get('string_starts', 0)))
    return data


def archived_row_counts(data):
    # Read the analyzer's existing audit rather than replaying or counting CSV
    # leaves: one archived row contributes to many component/owner entries.
    counts = {}
    for line in PREFIX.with_suffix(".audit.txt").read_text().splitlines():
        if line.startswith("templates="):
            fields = dict(token.split("=", 1) for token in line.split())
            mode = fields["templates"]
            counts[mode] = int(fields["archived_rows"])
            assert counts[mode] > 0
            assert math.isclose(total(data[mode]) - total(data[mode], group="end_buffer"), int(fields["total_bytes"]), rel_tol=0, abs_tol=1e-5)
    assert counts.keys() == data.keys(), "each template mode needs an audited row count"
    return counts


def metadata_overheads():
    # Sizes come from actual struct layouts and collection capacities. Each
    # nested struct belongs to exactly one group, rather than being counted twice.
    result = {}
    for line in PREFIX.with_suffix(".audit.txt").read_text().splitlines():
        if line.startswith("metadata_overhead "):
            fields = dict(token.split("=", 1) for token in line.split()[1:])
            mode = fields.pop("templates")
            result[mode] = {key: int(value) for key, value in fields.items()}
            # Historical reports predate parent dictionaries and remain renderable.
            result[mode].pop("parent_tables", 0)
            result[mode].pop("string_starts", 0)
            assert set(result[mode]) == set(OVERHEAD_LABELS) | {"archetypes"}
    assert result.keys() == {"true", "false"}
    return result


def macro_excerpt(lines, line):
    """Stop at the macro's closing delimiter, including comma-ended span fields."""
    snippet, stack = [], []
    quoted = escaped = started = False
    for index in range(line - 1, min(line + 15, len(lines))):
        current = lines[index].strip()
        snippet.append(current)
        if index == line - 1:
            marker = current.find('!')
            if marker < 0:
                break
            current = current[marker + 1:]
        # Source snippets need delimiter balance, not Rust semantic parsing.
        for char in current:
            if quoted:
                if escaped:
                    escaped = False
                elif char == '\\':
                    escaped = True
                elif char == '"':
                    quoted = False
            elif char == '"':
                quoted = True
            elif char in '({[':
                started = True
                stack.append(char)
            elif char in ')}]' and stack:
                stack.pop()
                if started and not stack:
                    return '\n'.join(snippet)
    return '\n'.join(snippet)


@lru_cache(maxsize=None)
def resolve_source_path(filename, target=''):
    """Resolve relocated capture paths for both excerpts and SVG hyperlinks."""
    path = Path(filename)
    # Both captures have src/main.rs and src/workload.rs; use the tracing target
    # to avoid attributing an Actix callsite to a same-named Bevy source file.
    packages = ('actix_capture', 'bevy_capture') if target.startswith('tracing_table_actix_capture') else ('bevy_capture', 'actix_capture')
    candidates = [path, ROOT / path, *(ROOT / 'examples' / package / path for package in packages)]
    # Captures may use a different CARGO_HOME; retain the exact package version.
    if '/registry/src/' in filename:
        suffix = filename.split('/registry/src/', 1)[1].split('/', 1)[1]
        candidates.extend((Path.home() / '.cargo/registry/src').glob('*/' + suffix))
    for candidate in candidates:
        if candidate.is_file():
            return candidate.resolve()
    return None


@lru_cache(maxsize=None)
def source_text(filename, line, target=''):
    """Read the macro at its original metadata line, when local source exists."""
    path = resolve_source_path(filename, target)
    if path is not None:
        lines = path.read_text().splitlines()
        if 0 < line <= len(lines):
            return macro_excerpt(lines, line)
    return ''


def source_description(site):
    return source_text(site['file'], int(site['line']), site.get('target', '')) or (
        f"Captured format: {site['format']}\nFields: {site['fields']} [source unavailable]")


def ranked_statistics(samples):
    """Min/median exclude exact zeros; max retains the complete population."""
    ordered = sorted(samples, key=lambda row: (row['value'], row['callsite'],
                                              row.get('archetype', -1), row.get('block_index', -1)))
    # Filter after computing callsite block-averages: zero-cost blocks still
    # participate in those averages, histogram counts and zero percentages.
    positive = [row for row in ordered if row['value'] > 0]
    if not positive:
        return [('Min', None, []), ('Median', None, []),
                ('Max', ordered[-1]['value'], [ordered[-1]])]
    n = len(positive)
    # Keep the numeric interpolated median, but show only its lower central
    # exemplar when the median lies between two observed values.
    median_examples = [positive[(n - 1) // 2]]
    return [('Min', positive[0]['value'], [positive[0]]),
            ('Median', statistics.median(row['value'] for row in positive), median_examples),
            ('Max', ordered[-1]['value'], [ordered[-1]])]


def histogram(samples, edges):
    """Identical, linear bits/row buckets, followed by one overflow bucket."""
    counts = [0] * (len(edges) - 1)
    width = edges[1] - edges[0]
    for sample in samples:
        index = min(len(counts) - 1, int((sample['value'] - edges[0]) / width))
        counts[index] += 1
    assert sum(counts) == len(samples)
    return counts


def displayed_histogram(samples, edges):
    """Separate dominant exact zeros without discarding small positive values."""
    counts = histogram(samples, edges)
    zeros = sum(sample['value'] == 0 for sample in samples)
    positive_counts = counts.copy()
    positive_counts[0] -= zeros
    # Treat a tie for largest as dominant. Compare exact zeros against the
    # positive-only bins, rather than confusing the entire [0, 1) bin with zero.
    separate = zeros > 0 and zeros >= max(positive_counts)
    return dict(counts=positive_counts if separate else counts, zero_count=zeros,
                zero_percent=100 * zeros / len(samples), separate_zero=separate)


def callsite_averages(individual):
    """Each block has equal weight, including blocks of different row counts."""
    by_site = defaultdict(list)
    for block in individual:
        by_site[block['callsite']].append(block['value'])
    return [dict(callsite=site, value=statistics.fmean(rates), block_count=len(rates))
            for site, rates in by_site.items()]


def bucket_edges(populations, bins, width=None, range_percentile=AUTO_RANGE_PERCENTILE):
    """Fit both positive populations, preserving an overflow bucket for outliers."""
    if width is None:
        useful_high = 0.0
        observed_high = 0.0
        for samples in populations:
            positive = sorted(sample['value'] for sample in samples if sample['value'] > 0)
            if not positive:
                continue
            observed_high = max(observed_high, positive[-1])
            # Estimate each population separately so numerous blocks cannot
            # drown out the callsite-average distribution when choosing a range.
            position = (len(positive) - 1) * range_percentile / 100
            lower = int(position)
            upper = min(lower + 1, len(positive) - 1)
            quantile = positive[lower] + (positive[upper] - positive[lower]) * (position - lower)
            useful_high = max(useful_high, quantile)
        # Fit ordinary data through its maximum. Preserve an overflow bucket
        # only when extreme outliers would flatten the meaningful x range.
        if observed_high <= X_OUTLIER_RATIO * useful_high:
            useful_high = observed_high
        # The regular range is half-open: exceed the chosen endpoint so a large
        # cluster exactly on that value does not fall into the overflow bucket.
        width = max(MIN_BUCKET_WIDTH, math.floor(useful_high / bins) + 1)
        bins = max(1, math.floor(useful_high / width) + 1)
    return [width * i for i in range(bins + 1)] + [math.inf]


def bucket_value_totals(samples, edges):
    """Exact sum of bits/row in each bucket, including overflow; no midpoints."""
    totals = [0.0] * (len(edges) - 1)
    width = edges[1] - edges[0]
    for sample in samples:
        index = min(len(totals) - 1, int((sample['value'] - edges[0]) / width))
        totals[index] += sample['value']
    return totals


def y_axis_scale(populations, contributions):
    """Find an empty y interval satisfying the weighted rule for supplied series."""
    values = sorted({value for series in populations for value in series if value > 0})
    peak = values[-1] if values else 0
    step = 10 ** math.floor(math.log10(peak)) if peak > 0 else 1
    ceiling = math.ceil(peak / step) * step if peak > 0 else 1
    candidates = []
    for lower, upper in zip(values, values[1:]):
        if upper <= Y_BREAK_RATIO * lower:
            continue
        counts = []
        for heights, costs in zip(populations, contributions):
            top_cost = sum(cost for height, cost in zip(heights, costs) if height >= upper)
            rest_cost = sum(cost for height, cost in zip(heights, costs) if height <= lower)
            if any(heights) and top_cost >= rest_cost:
                break
            counts.append(sum(height >= upper for height in heights))
        else:
            candidates.append((upper - lower, lower, upper, counts))
    result = dict(max=ceiling, break_low=None, break_high=None, top_bucket_counts=[])
    if candidates:
        _, lower, upper, counts = max(candidates)
        # Round into the empty gap, never across an observed bucket height.
        low_step = 10 ** (math.floor(math.log10(lower)) - 1)
        high_step = 10 ** (math.floor(math.log10(upper)) - 1)
        low = math.ceil(lower / low_step) * low_step
        # Leave a visible cap above the wave even for the shortest broken bar.
        high = math.floor((upper - Y_BREAK_EDGE_PADDING * (upper - lower)) / high_step) * high_step
        if high > low:
            fine_step = 10 ** (math.floor(math.log10(peak)) - 1)
            ceiling = math.ceil(peak / fine_step) * fine_step
            if ceiling <= high:
                ceiling += fine_step
            result.update(max=ceiling, break_low=low, break_high=high, top_bucket_counts=counts)
    return result


def first_visible_bucket(*populations):
    """Crop empty leading x buckets without changing shared bucket boundaries."""
    count = len(populations[0])
    first = next((i for i in range(count) if any(series[i] for series in populations)), 0)
    # Keep one regular bin alongside overflow, even for an explicit tight range.
    return min(first, max(0, count - 2))


def y_height(value, scale, amplitude=PLOT_HALF_HEIGHT):
    """Map two linear y segments separated by a marked omitted interval."""
    if scale['break_low'] is None:
        return amplitude * value / scale['max']
    lower_height = (amplitude - Y_BREAK_GAP) * Y_BREAK_LOWER_FRACTION
    upper_start = lower_height + Y_BREAK_GAP
    if value <= scale['break_low']:
        return lower_height * value / scale['break_low']
    if value >= scale['break_high']:
        return upper_start + (amplitude - upper_start) * (value - scale['break_high']) / (scale['max'] - scale['break_high'])
    raise ValueError('an observed bucket height lies inside the y-axis break')


def wavy_bar_break(ctx, x, y, width):
    """Procedural asset: cut a wavy band across a bar or the y-axis line."""
    ctx.save()
    samples = 20
    for edge in (-1, 1):
        points = [(x + width * i / samples,
                   y + edge * Y_BREAK_GAP / 2 + WAVE_AMPLITUDE * math.sin(math.tau * i / samples))
                  for i in range(samples + 1)]
        if edge == -1:
            ctx.move_to(*points[0])
            for point in points[1:]:
                ctx.line_to(*point)
        else:
            for point in reversed(points):
                ctx.line_to(*point)
    ctx.close_path()
    ctx.set_source_rgb(*CARD_BACKGROUND)
    ctx.fill()
    # Two thin wave outlines make the discontinuity visible in pale bars too.
    ctx.set_source_rgb(*MUTED)
    ctx.set_line_width(0.8)
    for edge in (-1, 1):
        for i in range(samples + 1):
            point = (x + width * i / samples,
                     y + edge * Y_BREAK_GAP / 2 + WAVE_AMPLITUDE * math.sin(math.tau * i / samples))
            (ctx.move_to if i == 0 else ctx.line_to)(*point)
        ctx.stroke()
    ctx.restore()


def make_panels(values, blocks, mode, bins, bucket_width, threshold,
                range_percentile=AUTO_RANGE_PERCENTILE, callsite_shares=None):
    columns = []
    for owner, title in [(('span',), 'Span'), (('event', 'log'), 'Event / log')]:
        selected = [block for block in blocks if block['templates'] == mode and block['kind'] in owner]
        panels = []
        # Small/custom captures may have no archived spans or no archived events.
        if not selected:
            columns.append((title, panels))
            continue
        for index, (name, size, *_) in enumerate(component_breakdown(values, owner)):
            # Independently reconcile every slice, including those below cutoff.
            measured = sum(block['costs'][name] for block in selected)
            assert math.isclose(measured, size, abs_tol=2e-5), (mode, title, name, measured, size)
            percent = 100 * size / total(values, owner=owner)
            # Consolidated charts include components important in either 
            # Standalone historical callers may omit equal-callsite shares.
            callsite_percent = ((callsite_shares or {}).get('span' if owner == ('span',) else 'event', {})
                                .get(name, 0))
            if max(percent, callsite_percent) <= threshold:
                continue
            individual = [dict(block, value=8 * block['costs'][name] / block['rows']) for block in selected]
            # Arithmetic mean of block rates, not pooled bytes / pooled rows.
            averages = callsite_averages(individual)
            # Each component may need a wider range; its two populations always
            # use identical edges. Outliers remain in the last bucket and extrema.
            edges = bucket_edges((individual, averages), bins, bucket_width, range_percentile)
            panels.append(dict(name=name, percent=percent, color=slice_color(name, index),
                               blocks=individual, callsites=averages, edges=edges,
                               block_hist=histogram(individual, edges), site_hist=histogram(averages, edges),
                               block_display=displayed_histogram(individual, edges),
                               site_display=displayed_histogram(averages, edges),
                               block_stats=ranked_statistics(individual), site_stats=ranked_statistics(averages)))
            panel = panels[-1]
            panel['block_values'] = bucket_value_totals(individual, edges)
            panel['site_values'] = bucket_value_totals(averages, edges)
            heights = [[100 * count / len(samples) for count in panel[key + '_display']['counts']]
                       for key, samples in [('block', individual), ('site', averages)]]
            for key, population in zip(('block', 'site'), heights):
                panel[key + '_y_scale'] = y_axis_scale([population], [panel[key + '_values']])
            panel['plot_start'] = first_visible_bucket(panel['block_display']['counts'],
                                                        panel['site_display']['counts'])
            if callsite_shares is not None:
                panels[-1]['callsite_percent'] = callsite_percent
        columns.append((title, panels))
    return columns


def hist_label(ctx, x, y, value, size=20, color=INK, bold=False):
    text(ctx, x, y, value, size=size, color=color, bold=bold)


def wrapped(ctx, x, y, value, chars, max_lines, size=SOURCE_FONT, color=INK):
    """Limit presentation only; the evidence CSV retains the full source text."""
    lines = [part for line in value.splitlines() for part in (textwrap.wrap(line, chars) or [''])]
    if len(lines) > max_lines:
        lines = lines[:max_lines]
        lines[-1] = lines[-1][:max(1, chars - 3)] + '...'
    for index, line in enumerate(lines):
        hist_label(ctx, x, y + index * (size + 4), line, size, color)


def single_line_source(value, chars=SOURCE_CHARS):
    """Collapse multiline macros for the figure; retain full source in CSV."""
    line = ' '.join(value.split())
    return line if len(line) <= chars else line[:max(0, chars - 3)] + '...'


def mirrored_histogram(ctx, x, y, width, panel):
    """Isolate the procedural chart asset from measurement and ranking logic."""
    left, right = x + 66, x + width - 30
    # Population labels live with the extrema, leaving most of this row for bars.
    axis, amplitude = y + PLOT_AXIS_Y, PLOT_HALF_HEIGHT
    start = panel['plot_start']
    visible_count = len(panel['block_hist']) - start
    bar_width = (right - left) / visible_count
    for key, samples, direction, color in [('block', panel['blocks'], -1, panel['color']),
                                           ('site', panel['callsites'], 1, overhead_color(panel['color']))]:
        counts = [100 * n / len(samples) for n in panel[key + '_display']['counts'][start:]]
        scale = panel[key + '_y_scale']
        ticks = ([scale['max'] / 2, scale['max']] if scale['break_low'] is None else
                 sorted({scale['break_low'] / 2, scale['break_low'], scale['break_high'], scale['max']}))
        for tick in ticks:
            yy = axis + direction * y_height(tick, scale, amplitude)
            ctx.set_source_rgb(0.88, 0.90, 0.92)
            ctx.set_line_width(1)
            ctx.move_to(left, yy)
            ctx.line_to(right, yy)
            ctx.stroke()
            # A narrow visual break gets one range hist_label instead of two
            # overlapping boundary labels; grid lines still mark both ends.
            if tick not in (scale['break_low'], scale['break_high']):
                hist_label(ctx, x + 2, yy + 5, f'{tick:.3g}%', 12, MUTED)
        if scale['break_low'] is not None:
            middle = (amplitude - Y_BREAK_GAP) * Y_BREAK_LOWER_FRACTION + Y_BREAK_GAP / 2
            hist_label(ctx, x + 2, axis + direction * middle + 4,
                  f"{scale['break_low']:.3g}–{scale['break_high']:.3g}%", 10, MUTED)
        for index, percent in enumerate(counts):
            height = y_height(percent, scale, amplitude)
            ctx.set_source_rgb(*color)
            ctx.rectangle(left + index * bar_width + 1, axis if direction > 0 else axis - height,
                          max(1, bar_width - 2), height)
            ctx.fill()
            if scale['break_high'] is not None and percent >= scale['break_high']:
                middle = (amplitude - Y_BREAK_GAP) * Y_BREAK_LOWER_FRACTION + Y_BREAK_GAP / 2
                wavy_bar_break(ctx, left + index * bar_width + 1, axis + direction * middle,
                               max(1, bar_width - 2))
        if scale['break_high'] is not None:
            middle = (amplitude - Y_BREAK_GAP) * Y_BREAK_LOWER_FRACTION + Y_BREAK_GAP / 2
            wavy_bar_break(ctx, left - 9, axis + direction * middle, 18)
    ctx.set_source_rgb(*INK)
    ctx.set_line_width(1.5)
    ctx.move_to(left, axis)
    ctx.line_to(right, axis)
    ctx.stroke()
    high = panel['edges'][-2]
    low = panel['edges'][start]
    regular_fraction = (visible_count - 1) / visible_count
    # The final boundary is labeled only as overflow, avoiding adjacent "32 32+".
    for i in range(4):
        xx = left + (right - left) * regular_fraction * i / 4
        hist_label(ctx, xx, axis + amplitude + 23, format_rate(low + (high - low) * i / 4), 14)
    overflow_label = format_rate(high) + '+'
    ctx.set_font_size(14)
    hist_label(ctx, right - ctx.text_extents(overflow_label).x_advance, axis + amplitude + 23, overflow_label, 14)


def append_svg_source_links(path, links, x_offset=0, y_offset=0):
    """Cairo outlines text and drops SVG link tags; add exact transparent hitboxes."""
    anchors = []
    for link in links:
        uri = html.escape(link['uri'], quote=True)
        title = html.escape(link['title'])
        # Preserve explicit path/line metadata as well as the URI fragment for
        # source-aware SVG viewers and callers that handle these anchors. Both
        # offsets follow the histogram surface when composed below the pies.
        source_path = html.escape(link.get('source_path', ''), quote=True)
        source_line = html.escape(str(link.get('source_line', '')), quote=True)
        anchors.append(f'<a href="{uri}" xlink:href="{uri}" target="_blank" '
                       f'data-source-path="{source_path}" data-source-line="{source_line}"><title>{title}</title>'
                       f'<rect x="{link["x"] + x_offset:.3f}" y="{link["y"] + y_offset:.3f}" '
                       f'width="{link["width"]:.3f}" height="{link["height"]:.3f}" '
                       'fill="white" fill-opacity="0" pointer-events="all"/></a>')
    svg = path.read_text()
    path.write_text(svg.replace('</svg>', '\n'.join(anchors) + '\n</svg>'))


def source_location(path, line):
    """Show the project or crate path, not just an ambiguous source basename."""
    path = Path(path)
    if path.is_relative_to(ROOT):
        display = path.relative_to(ROOT).as_posix()
    elif '/registry/src/' in path.as_posix():
        # The registry hash is machine-specific; keep package/version and all
        # source directories. The SVG target retains the complete absolute path.
        display = path.as_posix().split('/registry/src/', 1)[1].split('/', 1)[1]
    else:
        display = path.as_posix()
    return f'{display}:{line}'


def extrema_rows(block_stat, site_stat):
    """Share B/C callsite details; merge rates that are equal at display precision."""
    def rate(value):
        return 'N/A' if value is None else format_rate(value)
    same_site = [w['callsite'] for w in block_stat[2]] == [w['callsite'] for w in site_stat[2]]
    if same_site:
        b, c = rate(block_stat[1]), rate(site_stat[1])
        text = f'B=C: {b}' if b == c else f'B: {b} · C: {c}'
        return [(text, block_stat[2])]
    return [('B: ' + rate(block_stat[1]), block_stat[2]), ('C: ' + rate(site_stat[1]), site_stat[2])]


def draw_extrema_column(ctx, x, y, width, panel, sites, source_links=None):
    """Stack three extrema in one column, merging identical B/C exemplars."""
    hist_label(ctx, x, y, 'Extrema · bits/row · B: blocks · C: callsite means', 16, bold=True)
    summary = f"nB={len(panel['blocks']):,} · nC={len(panel['callsites']):,}"
    for key, short in [('block', 'B'), ('site', 'C')]:
        if panel[key + '_display']['separate_zero']:
            summary += f" · zero {short}={panel[key + '_display']['zero_percent']:.2f}%"
    hist_label(ctx, x, y + 22, summary, 14, MUTED)
    for index, (block_stat, site_stat) in enumerate(zip(panel['block_stats'], panel['site_stats'])):
        name = block_stat[0]
        yy = y + 52 + index * STAT_ENTRY_STEP
        metric_name = f'{name} (>0)' if name in ('Min', 'Median') else name
        hist_label(ctx, x, yy, metric_name, 18, bold=True)
        for position, (rates, witnesses) in enumerate(extrema_rows(block_stat, site_stat)):
            baseline = yy + 21 + position * 38
            if not witnesses:
                hist_label(ctx, x, baseline, rates + ' (no positive samples)', 15)
                continue
            witness = witnesses[0]
            site = sites[witness['callsite']]
            identity = witness['callsite']
            display_id = f'{identity[0]} #{identity[1]}' if isinstance(identity, tuple) else f'#{identity}'
            prefix = f"{rates} · {display_id} · "
            hist_label(ctx, x, baseline, prefix, 15)
            link_x = x + ctx.text_extents(prefix).x_advance
            path = resolve_source_path(site['file'], site.get('target', ''))
            location = source_location(path or site['file'], site['line'])
            link_font = SOURCE_PATH_FONT
            ctx.set_font_size(link_font)
            available = x + width - 8 - link_x
            while link_font > SOURCE_PATH_MIN_FONT and ctx.text_extents(location).x_advance > available:
                link_font -= 1
                ctx.set_font_size(link_font)
            # Rare very long paths keep their beginning, basename and line;
            # hover text and the anchor always retain the unabridged path.
            if ctx.text_extents(location).x_advance > available:
                suffix = '/' + Path(site['file']).name + ':' + site['line']
                head = location[:-len(suffix)] if location.endswith(suffix) else ''
                while head and ctx.text_extents(head + '/…' + suffix).x_advance > available:
                    head = head[:-1]
                location = head + '/…' + suffix
            hist_label(ctx, link_x, baseline, location, link_font, (0.20, 0.42, 0.70))
            link_width = ctx.text_extents(location).x_advance
            ctx.move_to(link_x, baseline + 2)
            ctx.line_to(link_x + link_width, baseline + 2)
            ctx.set_line_width(0.6)
            ctx.stroke()
            if source_links is not None and path is not None:
                source_links.append(dict(x=link_x, y=baseline - 15, width=link_width,
                                         height=20, uri=path.as_uri() + '#L' + site['line'],
                                         title=str(path) + ':' + site['line'],
                                         source_path=str(path), source_line=site['line']))
            source = single_line_source(source_description(site))
            ctx.set_font_size(SOURCE_FONT)
            # Character limits alone cannot bound a proportional font's width.
            while len(source) > 3 and ctx.text_extents(source).x_advance > width - 16:
                source = source[:-4].rstrip() + '...'
            hist_label(ctx, x, baseline + 18, source, SOURCE_FONT)


def render_height(columns):
    """Share one-column geometry between consolidated and independent exports."""
    return HIST_HEADER_HEIGHT + sum(28 + len(panels) * (CARD_HEIGHT + CARD_GAP) + SECTION_GAP
                               for _, panels in columns) + 30


def hist_render(surface, columns, sites, mode, threshold, source_links=None):
    ctx = cairo.Context(surface)
    ctx.set_source_rgb(1, 1, 1)
    ctx.paint()
    hist_label(ctx, MARGIN, 52, 'Storage cost distributions', 32, bold=True)
    paired = any('callsite_percent' in panel for _, panels in columns for panel in panels)
    cutoff = 'either paired pie' if paired else 'their owning pie'
    hist_label(ctx, MARGIN, 86, f'{replay_description(mode)} · >{threshold:g}% of {cutoff} · x: bits/row · ↑ blocks / ↓ callsite block-means', 18)
    hist_label(ctx, MARGIN, 112, 'Min/median >0 · dominant zeros listed separately · independent B/C y scales; waves mark gaps', 17, MUTED)
    width = PAGE_WIDTH - 2 * MARGIN
    x, top = MARGIN, HIST_HEADER_HEIGHT
    for title, panels in columns:
        hist_label(ctx, x, top, title, 30, bold=True)
        top += 28
        for panel in panels:
            ctx.set_source_rgb(*CARD_BACKGROUND)
            ctx.rectangle(x, top, width, CARD_HEIGHT)
            ctx.fill()
            ctx.set_source_rgb(*panel['color'])
            ctx.rectangle(x, top, 9, CARD_HEIGHT)
            ctx.fill()
            hist_label(ctx, x + 25, top + 37, panel['name'], 25, bold=True)
            share = (f"Rows: {panel['percent']:.2f}% · equal-callsite: {panel['callsite_percent']:.2f}%"
                     if 'callsite_percent' in panel else f"{panel['percent']:.2f}% of {title.lower()} pie")
            hist_label(ctx, x + 25, top + 67, share + f" · {panel['edges'][1]:g} bits/bin", 17, panel['color'])
            mirrored_histogram(ctx, x + 10, top + PLOT_TOP, HISTOGRAM_WIDTH, panel)
            stats_x = x + EXTREMA_LEFT
            stats_width = width - EXTREMA_LEFT - 25
            draw_extrema_column(ctx, stats_x, top + STAT_TOP, stats_width, panel, sites, source_links)
            top += CARD_HEIGHT + CARD_GAP
        top += SECTION_GAP


def write_evidence(prefix, modes, sites):
    """Persist full, untruncated witnesses and exact histogram counts for audit."""
    with prefix.with_suffix('.callsite_extremes.csv').open('w', newline='') as output:
        writer = csv.writer(output)
        writer.writerow(['templates', 'kind', 'component', 'population', 'statistic', 'statistic_bits_per_row',
                         'run', 'callsite', 'archetype', 'block_index', 'rows', 'block_count', 'witness_bits_per_row',
                         'file', 'line', 'target', 'captured_format', 'source'])
        for mode, columns in modes.items():
            for title, panels in columns:
                for panel in panels:
                    for population in ('block', 'site'):
                        for stat, value, witnesses in panel[population + '_stats']:
                            # No positive samples means no valid min/median
                            # witness; retain an explicit empty statistic row.
                            if not witnesses:
                                writer.writerow([mode, title, panel['name'], population, stat, ''] + [''] * 12)
                            for witness in witnesses:
                                site = sites[witness['callsite']]
                                writer.writerow([mode, title, panel['name'], population, stat, value,
                                                 site.get('run', ''), site['callsite'], witness.get('archetype', ''), witness.get('block_index', ''),
                                                 witness.get('rows', ''), witness.get('block_count', 1), witness['value'],
                                                 site['file'], site['line'], site['target'], site['format'], source_description(site)])
    with prefix.with_suffix('.callsite_histograms.csv').open('w', newline='') as output:
        writer = csv.writer(output)
        writer.writerow(['templates', 'kind', 'component', 'bucket_low_bits_per_row', 'bucket_high_bits_per_row',
                         'blocks', 'callsites', 'block_percent', 'callsite_percent',
                         'displayed_blocks', 'displayed_callsites',
                         'block_zero_count', 'block_zero_percent', 'block_zero_separate',
                         'callsite_zero_count', 'callsite_zero_percent', 'callsite_zero_separate',
                         'block_bucket_bits_per_row_sum', 'callsite_bucket_bits_per_row_sum',
                         'block_y_break_low_percent', 'block_y_break_high_percent', 'block_y_max_percent',
                         'callsite_y_break_low_percent', 'callsite_y_break_high_percent', 'callsite_y_max_percent',
                         'plot_min_bits_per_row'])
        for mode, columns in modes.items():
            for title, panels in columns:
                for panel in panels:
                    for i, (blocks, callsites) in enumerate(zip(panel['block_hist'], panel['site_hist'])):
                        writer.writerow([mode, title, panel['name'], panel['edges'][i], panel['edges'][i + 1], blocks,
                                         callsites, 100 * blocks / len(panel['blocks']), 100 * callsites / len(panel['callsites']),
                                         panel['block_display']['counts'][i], panel['site_display']['counts'][i],
                                         panel['block_display']['zero_count'], panel['block_display']['zero_percent'],
                                         panel['block_display']['separate_zero'],
                                         panel['site_display']['zero_count'], panel['site_display']['zero_percent'],
                                         panel['site_display']['separate_zero'],
                                         panel['block_values'][i], panel['site_values'][i],
                                         panel['block_y_scale']['break_low'], panel['block_y_scale']['break_high'],
                                         panel['block_y_scale']['max'],
                                         panel['site_y_scale']['break_low'], panel['site_y_scale']['break_high'],
                                         panel['site_y_scale']['max'], panel['edges'][panel['plot_start']]])


def pie_layout(groups, style):
    """Fit either key in the shared slot when paired pies can swap."""
    ctx = cairo.Context(cairo.RecordingSurface(cairo.CONTENT_COLOR_ALPHA, None))
    columns, offset = [], 0
    styles = ('rows', 'callsites') if style == 'both' else (style,)
    legend_x = PAIRED_KEY_X if style == 'both' else LEGEND_X
    for current in styles:
        widest = 0
        for group in groups:
            for variant in group.get('datasets', {'combined': group}).values():
                for row in variant[current]:
                    for line, size in zip(key_lines(variant, current, row),
                                          (LEGEND_NAME_SIZE, LEGEND_RATE_SIZE, LEGEND_BREAKDOWN_SIZE)):
                        ctx.select_font_face('sans-serif')
                        ctx.set_font_size(size)
                        widest = max(widest, ctx.text_extents(line).x_advance)
        width = math.ceil(legend_x + 22 + widest + KEY_RIGHT_MARGIN)
        columns.append(dict(style=current, pie_x=PIE_CENTER_X if style == 'both' else offset + PIE_CENTER_X,
                            key_x=legend_x if style == 'both' else offset + legend_x))
        offset = max(offset, width + PIE_COLUMN_GAP) if style == 'both' else offset + width + PIE_COLUMN_GAP
    width = offset - PIE_COLUMN_GAP
    if any(group.get('datasets') for group in groups):
        width += DATASET_GUTTER
    return dict(width=max(width, SINGLE_PIE_MIN_WIDTH),
                columns=columns)


def leaf_map():
    return defaultdict(lambda: defaultdict(float))


def eligible_sites(sites, owner=None):
    """Eligibility is archival presence, never whether a component is nonzero."""
    return [site for site in sites.values() if site['values']['archived_blocks'] > 0
            and (owner is None or site['kind'] in owner)]


def average_leaves(sites, denominator):
    """Pool bytes/rows within each callsite, then give every callsite one vote."""
    result = leaf_map()
    assert sites, 'no callsites with archived blocks for this pie'
    for site in sites:
        rows = site['values'][denominator]
        assert rows > 0, f'eligible callsite has no {denominator}'
        for key, leaf in site['leaves'].items():
            for metric in LEAF_METRICS:
                result[key][metric] += leaf[metric] / rows / len(sites)
    return result


def average_counters(sites, keys, denominator):
    """Missing categories count as zero for each member of the common cohort."""
    assert sites
    return {key: sum(site['values'].get(key, 0) / denominator(site) for site in sites) / len(sites)
            for key in keys}


def solid_slices(slices):
    # A common five-value format lets solid pies share the radial renderer.
    return [(name, value, 0, 0, value) for name, value in slices]


def metadata_populations(sites):
    """Weight natural populations exactly like bytes without changing pie shares."""
    result = {}
    for style in ('rows', 'callsites'):
        selected = list(sites.values()) if style == 'rows' else eligible_sites(sites)
        populations = {}
        for key, name in METADATA_PIE_LABELS.items():
            counter, unit = (
                (None, 'callsite') if key == 'format_recipes' else
                ('fsst_archetypes', 'arch') if key == 'fsst_tables' else
                ('string_buffers', 'buffer') if key.startswith('string_') else
                ('archived_archetypes', 'arch') if key.startswith('archetype_') else
                ('archived_blocks', 'block'))
            # Historical CSVs lack some counts; show an unavailable rate rather
            # than guessing object populations from implementation-specific sizes.
            if counter and any(counter not in site['values'] for site in selected):
                populations[name] = (None, unit)
                continue
            count = sum((site['values'][counter] if counter else 1) /
                        (site['values']['archived_rows'] if style == 'callsites' else 1)
                        for site in selected)
            if style == 'callsites' and selected:
                count /= len(selected)
            populations[name] = (count, unit)
        result[style] = populations
    return result


def build_groups(values, rows, kinds, metadata, lifetime, sites=None):
    """Build five measures, each with a row-weighted and equal-callsite view."""
    # Combine raw and residual index leaves in both pies and column denominators.
    owners = overview_slices(values, metadata)
    definitions = [
        ('lifetime', 'Active vs archived', solid_slices(lifetime_slices(lifetime)),
         rows + lifetime['active_rows'], 'retained rows; row-weighted includes active-only callsites'),
        ('overview', 'Archive-only payload + metadata', solid_slices(owners), rows,
         'archived rows; end buffers remain in their owning payload'),
        ('span', 'Span payload', payload_breakdown(values, ('span',), sites), kinds['span_starts'],
         'archived span starts; includes owned block/string overhead; excludes spare block descriptors'),
        ('event', 'Event / log payload', payload_breakdown(values, ('event', 'log'), sites),
         kinds['events'] + kinds['logs'],
         'archived event/log rows; includes owned block/string overhead; excludes spare block descriptors'),
        ('metadata', 'Metadata overhead', solid_slices(metadata_slices(metadata)), rows,
         'archived rows; container metadata only'),
    ]
    averages, cohorts = {}, {}
    # Denominators count stored values (rows times columns actually present),
    # including metadata, pooled across encoding modes for each component.
    columns = {key: dict(rows=column_populations(values, owner))
               for key, owner in [('span', ('span',)), ('event', ('event', 'log'))]}
    # Nonfield cost includes metadata's attributed padding:
    # timestamps, span/parent links and end tables; exclude fields and strings.
    nonfield = {key: dict(rows=sum(total(values, owner=owner, group=group)
                                  for group in ('metadata', 'end_buffer')), callsites=None)
                for key, owner in [('span', ('span',)), ('event', ('event', 'log'))]}
    # Block containers count as nonfield cost; string containers stay with strings.
    for key, _, slices, _, _ in definitions:
        if key in nonfield:
            nonfield[key]['rows'] += next(row[1] for row in slices if row[0] == BLOCK_OVERHEAD_LABEL)
    if sites is not None:
        eligible = eligible_sites(sites)
        archive = average_leaves(eligible, 'archived_rows')
        overhead = average_counters(eligible, ['metadata.' + key for key in OVERHEAD_LABELS],
                                    lambda site: site['values']['archived_rows'])
        overhead = {key: overhead['metadata.' + key] for key in OVERHEAD_LABELS}
        averages['overview'] = solid_slices(overview_slices(archive, overhead))
        averages['metadata'] = solid_slices(metadata_slices(overhead))
        lifetime_keys = ['lifetime.' + key for key in LIFETIME_LABELS]
        snapshot = average_counters(eligible, lifetime_keys,
                                    lambda site: site['values']['archived_rows'] + site['values']['active_rows'])
        averages['lifetime'] = solid_slices([(name, snapshot['lifetime.' + key])
                                            for key, name in LIFETIME_LABELS.items()])
        cohorts.update({key: len(eligible) for key in ('lifetime', 'overview', 'metadata')})
        for key, owner, denominator in [('span', ('span',), 'span_starts'),
                                        ('event', ('event', 'log'), 'archived_rows')]:
            selected = eligible_sites(sites, owner)
            averaged = average_leaves(selected, denominator)
            averages[key] = payload_breakdown(averaged, owner, sites, denominator)
            # Both numerator and denominator use the common callsite cohort.
            # Their ratio weights column occurrences consistently with this 
            columns[key]['callsites'] = column_populations(averaged, owner)
            nonfield[key]['callsites'] = sum(total(averaged, owner=owner, group=group)
                                             for group in ('metadata', 'end_buffer'))
            nonfield[key]['callsites'] += next(row[1] for row in averages[key]
                                               if row[0] == BLOCK_OVERHEAD_LABEL)
            cohorts[key] = len(selected)
    populations = metadata_populations(sites) if sites is not None else {}
    return [dict(key=key, title=title, rows=slices, denominator=denominator, note=note,
                 callsites=averages.get(key), callsite_count=cohorts.get(key), nonfield=nonfield.get(key),
                 metadata_populations=populations if key == 'metadata' else {},
                 columns=columns.get(key))
            for key, title, slices, denominator, note in definitions]


def build_dataset_groups(sites):
    """Re-pool measured callsites per capture before applying either weighting."""
    result = {}
    runs = sorted({identity[0] for identity in sites if isinstance(identity, tuple)},
                  key=lambda run: (run != 'bevy', run))
    if len(runs) < 2:
        return result
    for run in runs:
        selected = {identity: site for identity, site in sites.items() if identity[0] == run}
        values = leaf_map()
        for site in selected.values():
            for key, leaf in site['leaves'].items():
                for metric, value in leaf.items():
                    values[key][metric] += value
        def counters(key):
            return sum(site['values'].get(key, 0) for site in selected.values())
        kinds = dict(span_starts=counters('span_starts'),
                     events=sum(site['values']['archived_rows'] for site in selected.values() if site['kind'] == 'event'),
                     logs=sum(site['values']['archived_rows'] for site in selected.values() if site['kind'] == 'log'))
        metadata = {key: counters('metadata.' + key) for key in OVERHEAD_LABELS}
        lifetime = {key: counters('lifetime.' + key) for key in LIFETIME_LABELS}
        lifetime['active_rows'] = counters('active_rows')
        result[run] = build_groups(values, counters('archived_rows'), kinds, metadata, lifetime, selected)
    return result


def dataset_choices(group, dataset):
    """Offer the two datasets not currently shown in the main pie."""
    variants = group.get('datasets', {})
    # Keeping both alternatives also permits direct Bevy-to-Actix swaps.
    return [key for key in variants if key != dataset]


def draw_dataset_content(ctx, group, current, top, key_x, paired, dataset='combined'):
    """Shift the established main chart intact and add dataset selectors on the left."""
    variants = group.get('datasets')
    selected = variants[dataset] if variants else group
    ctx.save()
    if variants:
        ctx.translate(DATASET_GUTTER, 0)
    draw_pie_content(ctx, selected, current, top, key_x, paired,
                     dataset.capitalize() if dataset != 'combined' else '')
    ctx.restore()
    if variants:
        choices = dataset_choices(group, dataset)
        for index, choice in enumerate(choices):
            y = DATASET_PIE_Y[index] if len(choices) > 1 else PIE_CENTER_Y
            draw_mini_pie(ctx, variants[choice], current, DATASET_PIE_X, top + y,
                          choice.capitalize(), scale=DATASET_PIE_SCALE)


def field_labels():
    """Main-line field rates remain separate from metadata's split-cost rates."""
    return {LABELS['non_string_fields'], *(LABELS[key] for key in INDEX_GROUPS)}


def column_populations(values, owner):
    """Count every measured column; keep empty fields without inventing counts."""
    labels = field_labels()
    return {name: count for name, count in component_slices(
        values, owner, metric='column_values') if count or name in labels}


def key_lines(group, style, row):
    """Share exact text between measurement and drawing to prevent clipping."""
    name, cost, constant, varying, unsplit = row
    denominator = group['denominator'] if style == 'rows' else 1
    size = sum(item[1] for item in group[style])
    info = f'{100 * cost / size:.2f}%'
    if group['key'] == 'metadata':
        count, unit = group['metadata_populations'][style][name]
        rate = format_rate(cost / count) if count else '—'
        info += f' · {rate} bytes/{unit}'
    else:
        info += f' · {per_row(8 * cost, denominator)} bits/row'
    columns = (group.get('columns') or {}).get(style, {})
    if name in columns and name in field_labels():
        rate = per_row(8 * cost, columns[name]) if columns[name] else '—'
        info += f' · {rate} bits/col'
    if style == 'rows':
        info += f' · {kib(cost)} KiB'
    lines = [name, info]
    # Solid storage (trailers, padding and compressed strings) has no meaningful
    # constant/varying split. Empty components likewise need no breakdown line.
    if group['key'] in ('span', 'event') and (constant or varying):
        # Fields/args retain per-column rates; metadata uses the pie's row
        # denominator so optional members still show their actual cost per row.
        count = columns.get(name, 0)
        field = name in field_labels()
        rate_denominator = count if field else denominator
        constant_rate = per_row(8 * constant, rate_denominator) if rate_denominator else '—'
        varying_rate = per_row(8 * varying, rate_denominator) if rate_denominator else '—'
        frequency = per_row(count, denominator) if count else '—'
        unit = 'bits/col' if field else 'bits per row'
        if name == LABELS['compressed_payload']:
            # Container metadata is overhead, rather than a field's first/mask.
            payload_rate = per_row(8 * (varying + unsplit), rate_denominator)
            detail = f'Buffer overhead {constant_rate} · payload {payload_rate} {unit}'
        else:
            detail = f'Constant (first + mask) {constant_rate} · varying {varying_rate} {unit}'
        # Omit the redundant one-column-per-row frequency outside fields/args.
        if field or (count and not math.isclose(count / denominator, 1, abs_tol=1e-9)):
            detail += f' · {frequency} col/row'
        lines.append(detail)
    return lines


def legend_step(group, style, row):
    """Reserve space only for lines this entry actually displays."""
    if group['key'] not in ('span', 'event'):
        return COMPACT_LEGEND_STEP
    return LEGEND_STEP if len(key_lines(group, style, row)) == 3 else PAYLOAD_COMPACT_LEGEND_STEP


def group_height(group):
    # Paired keys can have different zero-cost entries; fit the taller partner.
    if group.get('datasets'):
        return max(group_height(variant) for variant in group['datasets'].values())
    key_height = max(sum(legend_step(group, style, row) for row in (group.get(style) or []))
                     for style in ('rows', 'callsites'))
    return max(ROW_MIN_HEIGHT, LEGEND_TOP + key_height + 45)


def draw_pie_key(ctx, group, style, x, top):
    """Each pie owns its complete key, with rates and a separate bit breakdown."""
    slices = group[style]
    text(ctx, x, top + 120, 'Component / share', size=18, bold=True)
    y = top + LEGEND_TOP
    for index, row in enumerate(slices):
        name = row[0]
        ctx.set_source_rgb(*slice_color(name, index))
        ctx.rectangle(x, y - 13, 14, 14)
        ctx.fill()
        for line_index, (line, font_size) in enumerate(zip(key_lines(group, style, row),
                (LEGEND_NAME_SIZE, LEGEND_RATE_SIZE, LEGEND_BREAKDOWN_SIZE))):
            text(ctx, x + 22, y + 19 * line_index, line, size=font_size,
                     color=(0.14, 0.18, 0.23) if line_index == 0 else MUTED)
        y += legend_step(group, style, row)


def draw_pie_content(ctx, group, current, top, key_x, paired=False, dataset_label=''):
    """Keep procedural chart geometry together for raster and vector states."""
    x = PIE_CENTER_X
    slices = group[current]
    denominator = group['denominator'] if current == 'rows' else 1
    heading = 'Row weighted' if current == 'rows' else 'Equal callsite'
    if dataset_label:
        heading = f'{dataset_label} - {heading.lower()}'
    text(ctx, x, top + 120, heading, size=24, bold=True, center=True)
    if group['nonfield'] is not None:
        nonfield = group['nonfield'][current]
        total = sum(row[1] for row in slices)
        text(ctx, x, top + NONFIELD_TOP,
                 f'Nonfield data: {100 * nonfield / total:.2f}% · {per_row(8 * nonfield, denominator)} bits/row',
                 size=NONFIELD_FONT_SIZE, center=True)
    radial_component_pie(ctx, x, top + PIE_CENTER_Y, slices, denominator)
    rate = 8 * sum(row[1] for row in slices) / denominator
    text(ctx, x, top + 570, f'{format_rate(rate)} bits/row', size=23, bold=True, center=True)
    population = (f"{group['denominator']:,} rows · {kib(sum(row[1] for row in slices))} KiB"
                  if current == 'rows' else f"{group['callsite_count']:,} equally weighted callsites")
    text(ctx, x, top + 603, population, size=18, center=True)
    draw_pie_key(ctx, group, current, key_x, top)
    if paired:
        draw_small_pie(ctx, group, current, top)


def draw_small_pie(ctx, group, current, top):
    """Add the alternative view without moving the main pie or its labels."""
    other = 'callsites' if current == 'rows' else 'rows'
    draw_mini_pie(ctx, group, other, PIE_CENTER_X + SMALL_PIE_DX,
                  top + PIE_CENTER_Y + SMALL_PIE_DY,
                  'Equal callsite' if other == 'callsites' else 'Row weighted')


def draw_mini_pie(ctx, group, current, x, y, label, scale=SMALL_PIE_SCALE):
    """Share radius and label geometry between weighting and dataset selectors."""
    denominator = group['denominator'] if current == 'rows' else 1
    radius = RADIUS * scale
    text(ctx, x, y - radius - SMALL_PIE_LABEL_GAP, label,
         size=SMALL_PIE_LABEL_SIZE, bold=True, center=True)
    ctx.save()
    ctx.translate(x, y)
    ctx.scale(scale, scale)
    radial_component_pie(ctx, 0, 0, group[current], denominator)
    ctx.restore()
    rate = 8 * sum(row[1] for row in group[current]) / denominator
    text(ctx, x, y + radius + SMALL_PIE_LABEL_GAP,
         f'{format_rate(rate)} bits/row',
         size=SMALL_PIE_LABEL_SIZE, bold=True, center=True)


def pie_controls(group, dataset, weighting):
    """Describe native controls and their destination state in one place."""
    shift = DATASET_GUTTER if group.get('datasets') else 0
    other = 'callsites' if weighting == 'rows' else 'rows'
    controls = [('weight', (dataset, other), PIE_CENTER_X + SMALL_PIE_DX + shift,
                 PIE_CENTER_Y + SMALL_PIE_DY, 'Swap weighting for all pies and keys')]
    choices = dataset_choices(group, dataset)
    for index, choice in enumerate(choices):
        y = DATASET_PIE_Y[index] if len(choices) > 1 else PIE_CENTER_Y
        controls.append(('select' + choice, (choice, weighting), DATASET_PIE_X, y,
                         f'Show {choice} in all main pies and keys'))
    return controls


def pie_control_id(index, dataset, weighting, action):
    # Simple alphanumeric IDs avoid Chromium's SMIL timing offset parsing.
    return f'pie{index}{dataset}{weighting}{action}'


def append_interactive_pies(path, groups, layout):
    """Synchronize every pie with native SMIL click events; no script needed."""
    ns = 'http://www.w3.org/2000/svg'
    ET.register_namespace('', ns)
    # Source-link hitboxes appended later use this established prefix.
    ET.register_namespace('xlink', 'http://www.w3.org/1999/xlink')
    root = ET.parse(path).getroot()
    datasets = tuple(groups[0].get('datasets', {'combined': None}))
    states = [(dataset, weighting) for dataset in datasets for weighting in ('rows', 'callsites')]
    # Destination event lists are shared by every section. Dataset and weighting
    # form one global state, so either kind of click updates all main pies/keys.
    events = {state: [] for state in states}
    for index, group in enumerate(groups):
        for dataset, weighting in states:
            for action, destination, *_ in pie_controls(group, dataset, weighting):
                events[destination].append(pie_control_id(index, dataset, weighting, action) + '.click')
    top = HEADER_HEIGHT
    for index, group in enumerate(groups):
        for dataset, current in states:
            state = dataset, current
            prefix = f'pie{index}{dataset}{current}'
            buffer = io.BytesIO()
            surface = cairo.SVGSurface(buffer, layout['width'], top + group_height(group))
            draw_dataset_content(cairo.Context(surface), group, current, top, PAIRED_KEY_X, True, dataset)
            surface.finish()
            fragment = ET.fromstring(buffer.getvalue())
            # Cairo reuses glyph IDs in each surface; namespace every reference.
            mapping = {node.attrib['id']: f'{prefix}-{node.attrib["id"]}'
                       for node in fragment.iter() if 'id' in node.attrib}
            for node in fragment.iter():
                for key, value in list(node.attrib.items()):
                    if key == 'id':
                        node.set(key, mapping[value])
                    else:
                        for original, replacement in mapping.items():
                            value = value.replace(f'#{original})', f'#{replacement})')
                            if value == f'#{original}':
                                value = f'#{replacement}'
                        node.set(key, value)
            layer = ET.SubElement(root, f'{{{ns}}}g',
                display='inline' if state == ('combined', 'rows') else 'none',
                **{'data-section': group['key'], 'data-dataset': dataset, 'data-weighting': current})
            for child in fragment:
                layer.append(child)
            for action, destination, x, y, label in pie_controls(group, dataset, current):
                trigger = ET.SubElement(layer, f'{{{ns}}}circle',
                    id=pie_control_id(index, dataset, current, action),
                    cx=str(x), cy=str(top + y),
                    # Match each hitbox to its independently tunable pie radius.
                    r=str(RADIUS * (DATASET_PIE_SCALE if action.startswith('select') else SMALL_PIE_SCALE)),
                    fill='transparent', style='cursor:pointer', role='button',
                    **{'pointer-events': 'all', 'aria-label': label, 'data-action': action})
                ET.SubElement(trigger, f'{{{ns}}}title').text = label
            # One show and one hide animation per layer. Both restart on every
            # matching event, allowing arbitrarily repeated cross-section swaps.
            for visible in (True, False):
                begin = ';'.join(event for destination, triggers in events.items()
                                 if (destination == state) == visible for event in triggers)
                ET.SubElement(layer, f'{{{ns}}}set', attributeName='display',
                              to='inline' if visible else 'none', begin=begin, fill='freeze')
        top += group_height(group) + ROW_GAP
    ET.ElementTree(root).write(path, encoding='unicode', xml_declaration=True)


def replay_description(mode):
    """Use measured policy for both chart headers, independent of environment."""
    if not {'format_args', 'extract_templates', 'min_block_rows'} <= SETTINGS.keys():
        raise ValueError('Recollect storage CSV with the current collector: replay mode and minimum block rows are required')
    policy = ('Format patch' if SETTINGS.get('format_args') else
              f"template extraction {'on' if mode == 'true' else 'off'}")
    return f"{policy} · min rows {SETTINGS['min_block_rows']}"


def draw_pie_groups(ctx, groups, mode, style, layout, interactive=False):
    """Draw shared headings and the default main/satellite arrangement."""
    text(ctx, MARGIN, 55, 'Where recorded storage goes', size=35, bold=True)
    runs = ' + '.join(name.capitalize() for name in SETTINGS.get('runs', ('bevy',)))
    text(ctx, MARGIN, 91, f"{runs} reference · {replay_description(mode)} · {timestamp_settings()[2]} ns ticks", size=22)
    # The lifetime snapshot partitions into archive-only and active-only totals.
    # Absolute sizing summaries share that partition for every pie style.
    by_key = {group['key']: group for group in groups}
    archive = by_key['overview']
    snapshot = by_key['lifetime']
    archive_size = sum(row[1] for row in archive['rows'])
    archive_rows = archive['denominator']
    active_size = sum(row[1] for row in snapshot['rows']) - archive_size
    active_rows = snapshot['denominator'] - archive_rows
    text(ctx, MARGIN, 126,
             f'Archive-only overview: {kib(archive_size)} KiB · {archive_rows:,} archived rows · {per_row(8 * archive_size, archive_rows)} bits/row',
             size=23, bold=True)
    text(ctx, MARGIN, 161,
             f'Active-only overview: {kib(active_size)} KiB · {active_rows:,} active rows · {per_row(8 * active_size, active_rows)} bits/row',
             size=23, bold=True)
    text(ctx, MARGIN, 198, 'Row-weighted: summed bytes / summed rows. Equal-callsite: mean of each eligible callsite’s bytes / rows.', size=20)
    text(ctx, MARGIN, 228, 'Equal-callsite cohort: at least one archived block; all archetypes combined. Zero component costs still count.', size=20)
    text(ctx, MARGIN, 258, 'Pie angles use component / total rate. Light = overhead (first + mask or buffers); dark = payload. Solid = unsplit cost.', size=19)
    text(ctx, MARGIN, 282, 'Fields/args use bits/col; other breakdowns use bits per row. Fixed 1 col/row is omitted outside fields/args.', size=17)
    top = HEADER_HEIGHT
    for group in groups:
        ctx.set_source_rgb(0.97, 0.975, 0.98)
        ctx.rectangle(MARGIN - 12, top, layout['width'] - MARGIN + 12, group_height(group))
        ctx.fill()
        text(ctx, MARGIN, top + 40, group['title'], size=27, bold=True)
        text(ctx, MARGIN, top + 71, group['note'], size=18)
        if not interactive:
            current = 'rows' if style == 'both' else style
            draw_dataset_content(ctx, group, current, top,
                             PAIRED_KEY_X if style == 'both' else LEGEND_X, style == 'both')
        top += group_height(group) + ROW_GAP


def export_pie_rates(prefix, modes):
    """Expose both denominators and rates so equal-callsite weighting is auditable."""
    with prefix.with_suffix('.pie_rates.csv').open('w', newline='') as output:
        writer = csv.writer(output)
        writer.writerow(['templates', 'measure', 'style', 'component', 'bits_per_row', 'percent',
                         'archived_callsite_count', 'row_denominator', 'bits_per_column',
                         'column_value_denominator', 'constant_bits_per_row', 'varying_bits_per_row',
                         'unsplit_bits_per_row', 'constant_bits_per_column', 'varying_bits_per_column',
                         'columns_per_row'])
        for mode, groups in modes.items():
            for group in groups:
                for style in ('rows', 'callsites'):
                    if group[style] is None:
                        continue
                    denominator = group['denominator'] if style == 'rows' else 1
                    size = sum(row[1] for row in group[style])
                    for name, cost, constant, varying, unsplit in group[style]:
                        columns = (group.get('columns') or {}).get(style, {}).get(name, 0)
                        writer.writerow([mode, group['key'], style, name, 8 * cost / denominator,
                                         100 * cost / size, group['callsite_count'] or '',
                                         group['denominator'] if style == 'rows' else '',
                                         8 * cost / columns if columns else '', columns or '',
                                         8 * constant / denominator, 8 * varying / denominator,
                                         8 * unsplit / denominator,
                                         8 * constant / columns if columns else '',
                                         8 * varying / columns if columns else '',
                                         columns / denominator if columns and (constant or varying) else ''])


def read_csv(path):
    with path.open(newline='') as source:
        return list(csv.DictReader(source))


def timestamp_settings():
    return 0, SETTINGS['timestamp_bit_shift'], 1 << SETTINGS['timestamp_bit_shift']


BASE_LABELS = LABELS.copy()
BASE_COMPONENT_COLORS = COMPONENT_COLORS.copy()
SETTINGS = {}


def configure_labels(audit=None):
    """Select measured labels once, before building block costs and pie keys."""
    LABELS.clear()
    LABELS.update(BASE_LABELS)
    COMPONENT_COLORS.clear()
    COMPONENT_COLORS.update(BASE_COMPONENT_COLORS)
    current = audit is None
    changes = {}
    if current or 'timestamp_predictor ' in audit:
        changes['timestamp'] = 'Timestamp residual / offset'
    if current or 'parent_dictionary ' in audit:
        changes.update(parent_archetype_callsite='Parent identity table index', parent_index='Parent index residual (ZigZag)')
    if current or 'string_index_prediction ' in audit:
        dictionaries = bool(SETTINGS.get('string_dictionaries', 0)) if current else 'string_dictionaries ' in audit
        changes['end_buffer'] = 'End tables: parents + string indices' if dictionaries else 'End buffer: parents + string starts'
        if not dictionaries:
            changes.update({key: LABELS[key] + ' (raw / residual)' for key in INDEX_GROUPS})
    for key, label in changes.items():
        COMPONENT_COLORS[label] = COMPONENT_COLORS[LABELS[key]]
        LABELS[key] = label


def load_evidence(prefix):
    """Read current single-file evidence or migrate a historical CSV bundle.

    Empty components contribute zero, but their callsite/block remains in its
    cohort. Keeping that membership is essential to both averaging policies.
    """
    global SETTINGS
    rows = read_csv(prefix.with_suffix('.csv'))
    if not rows:
        raise ValueError('empty evidence CSV')
    unified = 'record' in rows[0]
    if unified:
        settings = {}
        run_settings = defaultdict(dict)
        for row in rows:
            if row['record'] != 'setting':
                continue
            key, value = row['key'], int(row['value'])
            if key in settings and settings[key] != value:
                raise ValueError(f'incompatible storage settings across runs: {key}')
            settings[key] = value
            run_settings[row.get('run', '')][key] = value
        if settings.get('schema') not in (1, 2):
            raise ValueError('unsupported storage CSV schema')
        if settings['schema'] == 2:
            # Reject partial/mixed inputs rather than silently merging unnamed IDs.
            if any(not row.get('run') for row in rows):
                raise ValueError('schema 2 requires a run on every record')
            if any(run_settings[row['run']] != settings for row in rows):
                raise ValueError('missing or incompatible settings for run')
        SETTINGS = settings
        SETTINGS['runs'] = tuple(sorted(run_settings)) if settings['schema'] == 2 else ('bevy',)
        configure_labels()
        counters = [row for row in rows if row['record'] == 'counter']
        sources = [row for row in rows if row['record'] == 'source']
        leaves = [row for row in rows if row['record'] == 'leaf']
    else:
        # Transitional reader only: new captures need no sidecars or audit text.
        audit = prefix.with_suffix('.audit.txt').read_text()
        shifts = dict(line.split('=', 1) for line in audit.splitlines()
                      if line.startswith('timestamp_bit_shift='))
        if not shifts:
            fields = next(dict(token.split('=', 1) for token in line.split()[1:])
                          for line in audit.splitlines() if line.startswith('timestamp_shift '))
            shifts['timestamp_bit_shift'] = fields['measured']
        SETTINGS = dict(timestamp_bit_shift=int(shifts['timestamp_bit_shift']))
        configure_labels(audit)
        counters = read_csv(prefix.with_suffix('.callsite_storage.csv'))
        sources = read_csv(prefix.with_suffix('.callsites.csv'))
        leaves = read_csv(prefix.with_suffix('.block_components.csv'))
        for row in counters:
            row.update(key=row['metric'])
    sites = {mode: {} for mode in ('true', 'false')}
    def site_id(row):
        # Old single-capture files retain integer keys for historical callers.
        callsite = int(row['callsite'])
        return (row['run'], callsite) if row.get('run') else callsite

    for row in counters:
        site = sites[row['templates']].setdefault(site_id(row),
            dict(kind=row['record_kind'], values={}, leaves=leaf_map()))
        key = row['key']
        if key not in ('metadata.parent_tables', 'metadata.string_starts'):
            site['values'][key] = int(row['value'])
    data = {mode: leaf_map() for mode in sites}
    blocks = {}
    metadata_components = {leaf: key for key, members in METADATA_COMPONENTS.items() for leaf in members}
    for row in leaves:
        mode, callsite = row['templates'], site_id(row)
        component = measurement_component(row)
        group = component if row['group'] == 'strings' else row['group']
        key = row['record_kind'], group, component
        metrics = {metric: float(row.get(metric) or 0) for metric in LEAF_METRICS}
        metrics['varying_bytes'] = int(row.get('varying_bits') or 0) / 8
        metrics['total_bytes'] = sum(metrics[metric] for metric in METRICS[:-1])
        for metric, value in metrics.items():
            data[mode][key][metric] += value
            sites[mode][callsite]['leaves'][key][metric] += value
        block_key = mode, callsite, int(row['archetype']), int(row['block_index'])
        block = blocks.setdefault(block_key, dict(templates=mode, callsite=callsite,
            archetype=block_key[2], block_index=block_key[3], rows=int(row['rows']),
            kind=row['record_kind'], costs=defaultdict(float)))
        assert block['rows'] == int(row['rows']) and block['kind'] == row['record_kind']
        name = LABELS[metadata_components[row['component']] if group == 'metadata' else
                      row['component'] if row['group'] == 'strings' else row['group']]
        block['costs'][name] += metrics['total_bytes'] - metrics['padding_bytes']
        block['costs'][LABELS['row_padding']] += metrics['padding_bytes']
    counts, kinds, metadata, lifetimes = {}, {}, {}, {}
    for mode, members in sites.items():
        counts[mode] = sum(site['values']['archived_rows'] for site in members.values())
        kinds[mode] = {'span_starts': sum(site['values']['span_starts'] for site in members.values()),
                      'events': sum(site['values']['archived_rows'] for site in members.values() if site['kind'] == 'event'),
                      'logs': sum(site['values']['archived_rows'] for site in members.values() if site['kind'] == 'log')}
        metadata[mode] = {key: sum(site['values'].get('metadata.' + key, 0) for site in members.values()) for key in OVERHEAD_LABELS}
        lifetimes[mode] = {key: sum(site['values']['lifetime.' + key] for site in members.values()) for key in LIFETIME_LABELS}
        lifetimes[mode]['active_rows'] = sum(site['values']['active_rows'] for site in members.values())
        selected = [block for block in blocks.values() if block['templates'] == mode]
        assert sum(block['rows'] for block in selected) == counts[mode]
        assert len(selected) == sum(site['values']['archived_blocks'] for site in members.values())
        archive = sum(lifetimes[mode][key] for key in ('archived_messages', 'archived_strings'))
        assert math.isclose(total(data[mode]) + sum(metadata[mode].values()), archive, abs_tol=2e-5)
    return data, counts, kinds, metadata, lifetimes, sites, list(blocks.values()), {site_id(row): row for row in sources}


def generate(args, data, counts, kinds, metadata, lifetime, sites, blocks, sources):
    """Compose independently renderable pie and histogram areas in SVG and PNG."""
    need_pies = args.view != 'histograms'
    need_histograms = args.view != 'pies'
    need_callsites = need_histograms or args.pie_style != 'rows'
    modes, histogram_modes = {}, {}
    # New captures select one mode; historical bundles may contain both.
    for mode in ('true', 'false'):
        if not counts[mode]:
            continue
        groups = build_groups(data[mode], counts[mode], kinds[mode], metadata[mode], lifetime[mode],
                              sites[mode] if need_callsites else None)
        if need_pies and args.pie_style == 'both':
            datasets = build_dataset_groups(sites[mode])
            if datasets:
                for index, group in enumerate(groups):
                    group['datasets'] = dict(combined=group.copy(),
                                             **{run: variants[index] for run, variants in datasets.items()})
        layout = pie_layout(groups, args.pie_style) if need_pies else None
        modes[mode] = groups
        shares = {group['key']: {name: 100 * cost / sum(row[1] for row in group['callsites'])
                                for name, cost, *_ in group['callsites']}
                  for group in groups if group['key'] in ('span', 'event') and group['callsites'] is not None}
        columns = (make_panels(data[mode], blocks, mode, args.bins, args.bucket_width,
                                   args.min_percent, args.range_percentile, shares) if need_histograms else None)
        if columns is not None:
            histogram_modes[mode] = columns
        pie_height = HEADER_HEIGHT + sum(group_height(group) + ROW_GAP for group in groups) if need_pies else 0
        hist_height = render_height(columns) if need_histograms else 0
        pies_width = layout['width'] if need_pies else 0
        # Stack the two reusable areas; center histograms beneath the pie keys.
        width = max(pies_width, PAGE_WIDTH if need_histograms else 0)
        height = pie_height + hist_height
        hist_x = (width - PAGE_WIDTH) / 2 if need_histograms else 0
        hist_y = pie_height
        hist_asset = None
        source_links = []
        if need_histograms:
            # Bounded recording surfaces retain vector geometry when composed
            # into SVG; standalone histograms use the identical rendering code.
            hist_asset = cairo.RecordingSurface(cairo.CONTENT_COLOR_ALPHA, (0, 0, PAGE_WIDTH, hist_height))
            hist_render(hist_asset, columns, sources, mode, args.min_percent, source_links)

        def draw(surface, interactive=False):
            ctx = cairo.Context(surface)
            ctx.set_source_rgb(1, 1, 1)
            ctx.paint()
            if need_pies:
                draw_pie_groups(ctx, groups, mode, args.pie_style, layout, interactive)
            if hist_asset is not None:
                ctx.save()
                ctx.rectangle(hist_x, hist_y, PAGE_WIDTH, hist_height)
                ctx.clip()
                ctx.set_source_surface(hist_asset, hist_x, hist_y)
                ctx.paint()
                ctx.restore()

        suffix = '' if args.view == 'consolidated' else (
            '_histograms' if args.view == 'histograms' else '_' + args.pie_style + '_pies')
        if mode == 'false' and counts['true']:
            suffix += '_templates_off'
        path = PREFIX.parent / (PREFIX.name + suffix)
        svg = cairo.SVGSurface(str(path.with_suffix('.svg')), width, height)
        interactive = need_pies and args.pie_style == 'both'
        draw(svg, interactive)
        svg.finish()
        if interactive:
            append_interactive_pies(path.with_suffix('.svg'), groups, layout)
        if need_histograms:
            append_svg_source_links(path.with_suffix('.svg'), source_links, hist_x, hist_y)
        png = cairo.ImageSurface(cairo.FORMAT_ARGB32, width, height)
        draw(png)
        png.write_to_png(str(path.with_suffix('.png')))
        # Report both artifacts so successful vector output is visible to callers.
        print(f'{args.view} templates={mode}: {path.with_suffix(".svg")} and {path.with_suffix(".png")} ({width} × {height})')
    if need_pies:
        evidence_prefix = (PREFIX if args.view == 'consolidated' else
                           PREFIX.parent / (PREFIX.name + '_' + args.pie_style + '_pies'))
        export_pie_rates(evidence_prefix, modes)
    if need_histograms:
        write_evidence(PREFIX, histogram_modes, sources)


def main():
    global PREFIX
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--prefix', type=Path, default=PREFIX, help='CSV/output prefix without extension')
    parser.add_argument('--view', choices=('consolidated', 'pies', 'histograms'), default='consolidated')
    parser.add_argument('--pie-style', choices=('both', 'rows', 'callsites'), default='both')
    parser.add_argument('--bins', type=int, default=BIN_COUNT)
    parser.add_argument('--bucket-width', type=float, default=BUCKET_WIDTH)
    parser.add_argument('--range-percentile', type=float, default=AUTO_RANGE_PERCENTILE)
    parser.add_argument('--min-percent', type=float, default=MIN_PERCENT)
    args = parser.parse_args()
    if args.bins < 2:
        parser.error('--bins must be at least 2')
    if args.bucket_width is not None and (not math.isfinite(args.bucket_width) or args.bucket_width <= 0):
        parser.error('--bucket-width must be positive and finite')
    if not math.isfinite(args.range_percentile) or not 0 < args.range_percentile <= 100:
        parser.error('--range-percentile must be greater than 0 and at most 100')
    if not math.isfinite(args.min_percent) or not 0 <= args.min_percent <= 100:
        parser.error('--min-percent must be between 0 and 100')
    PREFIX = args.prefix.resolve()
    generate(args, *load_evidence(PREFIX))


if __name__ == '__main__':
    main()
