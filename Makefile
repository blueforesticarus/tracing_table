project_graph_interactive:
	cargo modules dependencies \
	--no-externs --no-fns --no-sysroot \
	--no-traits --no-types --no-uses \
	| xdot -

project_graph: doc/modules.svg

doc/modules.dot:
	# Generate the project's module dependency graph.
	# --no-externs: exclude external crates
	# --no-fns:     exclude functions
	# --no-sysroot: exclude std/core/alloc
	# --no-traits:  exclude traits
	# --no-types:   exclude structs/enums/type aliases
	# --no-uses:    exclude individual use/import edges
	cargo modules dependencies \
		--no-externs \
		--no-sysroot \
		--layout=dot \
		> $@

doc/modules.svg: doc/modules.dot
	dot -Tsvg $< -o $@

# Captures are explicit so rerendering charts never starts another heavy run.
.PHONY: capture-actix capture-bevy storage-csv storage-charts
capture-actix:
	cargo run --release --locked --manifest-path examples/actix_capture/Cargo.toml

# Use the existing development renderer build; the captured inputs are replayed untimed.
capture-bevy:
	cargo run --locked --manifest-path examples/bevy_capture/Cargo.toml

# Collect both reference inputs by default; publish only a successful CSV.
# One v3 capture supports every mode: FORMAT_ARGS=off TEMPLATES=on|off.
FORMAT_ARGS ?= on
TEMPLATES ?= on
MIN_BLOCK_ROWS ?= 0
storage-csv:
	cargo bench --offline --bench archived_storage -- --format-args $(FORMAT_ARGS) --templates $(TEMPLATES) --min-block-rows $(MIN_BLOCK_ROWS) > doc/reference_archived_storage.csv.tmp
	mv doc/reference_archived_storage.csv.tmp doc/reference_archived_storage.csv

storage-charts: storage-csv
	python3 util/plot_archived_storage.py
