"""Print dumped messages with a shared prefix for each archetype or block."""

import argparse
import csv
import itertools


def unescape(value):
    # The Rust exporter escapes these four characters to keep one row per line.
    replacements = {"\\": "\\", "t": "\t", "n": "\n", "r": "\r"}
    result = []
    index = 0
    while index < len(value):
        if value[index] == "\\" and index + 1 < len(value):
            result.append(replacements.get(value[index + 1], value[index + 1]))
            index += 2
        else:
            result.append(value[index])
            index += 1
    return "".join(result)


def escape(value):
    # Preserve the TSV's one-line-per-message representation in the output.
    return value.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n").replace("\r", "\\r")


def print_group(rows):
    messages = [unescape(row[2]) for row in rows]
    prefix = messages[0]
    for message in messages[1:]:
        while not message.startswith(prefix):
            prefix = prefix[:-1]
    for (archetype, target, _), message in zip(rows, messages):
        print(f"{archetype}\t{target}\t{escape(prefix)}\t{escape(message[len(prefix):])}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tsv", help="TSV produced by --dump_strings")
    parser.add_argument("--block_size", "--block-size", type=int, metavar="ROWS")
    args = parser.parse_args()
    if args.block_size is not None and args.block_size <= 0:
        parser.error("--block_size must be positive")

    # Rows are emitted together for each archetype by the dump command.
    with open(args.tsv, newline="", encoding="utf-8") as source:
        # The exporter escapes control characters but does not quote fields.
        rows = csv.reader(source, delimiter="\t", quoting=csv.QUOTE_NONE)
        first_group = True
        for _, archetype_rows in itertools.groupby(rows, key=lambda row: row[0]):
            archetype_rows = list(archetype_rows)
            # Single-message archetypes have no useful shared pattern to inspect.
            if len(archetype_rows) == 1:
                continue
            size = args.block_size or len(archetype_rows)
            for start in range(0, len(archetype_rows), size):
                if not first_group:
                    print()
                print_group(archetype_rows[start : start + size])
                first_group = False


if __name__ == "__main__":
    main()
