#!/usr/bin/env bash
# Embed scanner metadata in place; repeat runs replace the previous section.
set -euo pipefail

if (( $# < 1 || $# > 2 )); then
    printf 'Usage: %s EXECUTABLE [TEMPLATES.bin]\n' "$0" >&2
    exit 2
fi

# Absolute paths also keep filenames beginning with '-' out of objcopy options.
executable=$(realpath -- "$1")
if (( $# == 2 )); then
    templates=$2
else
    # Match the scanner: walk from the executable to the nearest target ancestor.
    target_dir=$(dirname -- "$executable")
    while [[ ${target_dir##*/} != target ]]; do
        if [[ "$target_dir" == / ]]; then
            printf 'No ancestor target directory found; pass TEMPLATES.bin explicitly.\n' >&2
            exit 1
        fi
        target_dir=$(dirname -- "$target_dir")
    done
    templates=$target_dir/tracing_scan_elf/templates.bin
fi
templates=$(realpath -- "$templates")
if [[ ! -f "$executable" || ! -r "$templates" || ! -f "$templates" ]]; then
    printf 'Executable and readable templates.bin must be regular files.\n' >&2
    exit 1
fi

# Omit alloc/load: the runtime reader reads the section from the executable file.
# Omitting the output argument lets objcopy replace its input via a temporary file.
"${OBJCOPY:-objcopy}" \
    --remove-section=.tracing_scan_elf \
    --add-section ".tracing_scan_elf=$templates" \
    --set-section-flags .tracing_scan_elf=readonly,contents \
    "$executable"
