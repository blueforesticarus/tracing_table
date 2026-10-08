//! CLI dimensions and target matching for the streaming compression example.

use std::{collections::HashSet, path::PathBuf};

use super::{DedupMode, FieldGroup, Result, invalid};

pub const HELP: &str = "\
Usage: dump_message_compression [OPTIONS] <dump.msgpack>

Dimensions (comma-separated values form a Cartesian product):
  --dedup=plain,block,archetype,global  Default: all four
  --prefix=off,on               Default: off; split the longest common block prefix
                                on requires message fields and block/archetype/global dedup.
                                Prefix + dictionary compression is discouraged.
  --block_size=100,1000          Row indices per block; default: 100
  --fields=message,other         Default: message
  --compression=none,lz4,zstd    Default: all three
  --compression_mode=blocks,whole,dict  Default: all three compressed layouts
                                none always uses compression_mode=none.
                                whole is a reference only, not a viable layout.
  --zstd_level=3,6               Default: 3
  --dict_size=8192,16384         Dictionary byte limit; default: 8192
  --sample_size=256,512          Block-dictionary training target; default: 256
                                Archetype/global dictionaries use messages.
  --target='glfw*,corngame'      Separate target selections; default: *
                                Bare names match that target and its :: modules.
                                Glob operators: * (any text), ? (one character).

Tables (all enabled when none is explicitly selected):
  --timing                      Four columns: compression/decompression total
                                and byte-weighted average per unit; labeled units
  --sizes                       Uncompressed, blocks, whole archetype, and
                                trained dictionary sizes for selected codecs
  --summary                     Rows, string uniqueness, duplicate bytes, and
                                cross-archetype sharing per target/field selection
  --list_targets                Print sorted distinct callsite targets and exit
  --dump_strings=messages.tsv    Write archetype_id<TAB>target<TAB>message and exit
                                Unique archetype ID is callsite_id:archetype_id.
                                Original messages, duplicates included; creates a new file.
                                Escapes backslash, tab, newline, and carriage return.
  --help                        Print this help

Hyphens may replace underscores in option names. Tables are labeled CSV sections.
Storage and duplicate byte columns are bytes per original matched row (six decimals).
Dictionary/sample size settings remain absolute bytes. Empty targets report n/a.
Run with cargo run --release --example dump_message_compression -- ... for timings.
";

#[derive(Clone, Copy, Default)]
pub struct Tables {
    pub timing: bool,
    pub sizes: bool,
    pub summary: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Lz4,
    Zstd,
}

impl Compression {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "lz4" => Ok(Self::Lz4),
            "zstd" => Ok(Self::Zstd),
            _ => Err(invalid("--compression must contain none, lz4, or zstd").into()),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lz4 => "lz4",
            Self::Zstd => "zstd",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionMode {
    None,
    Blocks,
    Whole,
    Dict,
}

impl CompressionMode {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "blocks" => Ok(Self::Blocks),
            "whole" => Ok(Self::Whole),
            "dict" => Ok(Self::Dict),
            _ => {
                Err(invalid("--compression_mode must contain none, blocks, whole, or dict").into())
            }
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Blocks => "blocks",
            Self::Whole => "whole",
            Self::Dict => "dict",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Setting {
    pub group: FieldGroup,
    pub target: String,
    pub mode: DedupMode,
    pub prefix: bool,
    pub block_rows: u64,
    pub zstd_level: i32,
    pub dict_max_bytes: usize,
    pub sample_bytes: usize,
}

pub struct Options {
    pub path: PathBuf,
    pub groups: Vec<FieldGroup>,
    pub targets: Vec<String>,
    pub settings: Vec<Setting>,
    pub compression: Vec<Compression>,
    pub compression_modes: Vec<CompressionMode>,
    pub tables: Tables,
    pub list_targets: bool,
    pub dump_strings: Option<PathBuf>,
}

fn values<T: PartialEq>(value: &str, mut parse: impl FnMut(&str) -> Result<T>) -> Result<Vec<T>> {
    let mut result = Vec::new();
    for item in value.split(',') {
        if item.is_empty() {
            return Err(invalid("dimension values must not be empty").into());
        }
        let item = parse(item)?;
        // Repeated values should not create duplicate experiment rows.
        if !result.contains(&item) {
            result.push(item);
        }
    }
    Ok(result)
}

fn positive(value: &str) -> Result<u64> {
    let number = value.parse::<u64>()?;
    if number == 0 {
        return Err(invalid("block, dictionary, and sample sizes must be positive").into());
    }
    Ok(number)
}

fn prefix_mode(value: &str) -> Result<bool> {
    match value {
        "off" => Ok(false),
        "on" => Ok(true),
        _ => Err(invalid("--prefix must contain off or on").into()),
    }
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut path = None;
        let mut groups = vec![FieldGroup::Message];
        let mut targets = vec!["*".to_owned()];
        let mut modes = DedupMode::ALL.to_vec();
        let mut prefixes = vec![false];
        let mut block_sizes = vec![100];
        let mut zstd_levels = vec![3];
        let mut dict_sizes = vec![8192];
        let mut sample_sizes = vec![256];
        let mut compression = vec![Compression::None, Compression::Lz4, Compression::Zstd];
        let mut compression_modes = vec![
            CompressionMode::Blocks,
            CompressionMode::Whole,
            CompressionMode::Dict,
        ];
        let mut tables = Tables::default();
        let mut list_targets = false;
        let mut dump_strings = None;
        let mut explicit_tables = false;
        let mut seen_options = HashSet::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if !arg.starts_with('-') {
                if path.replace(PathBuf::from(arg)).is_some() {
                    return Err(invalid("expected exactly one dump path").into());
                }
                continue;
            }
            let (name, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(n, v)| (n, Some(v)));
            let name = name
                .strip_prefix("--")
                .ok_or_else(|| invalid(format!("unknown option {arg}")))?
                .replace('-', "_");
            if !seen_options.insert(name.clone()) {
                return Err(invalid(format!("option {name} was provided more than once")).into());
            }
            match name.as_str() {
                "dump_strings" => {
                    let value = inline
                        .map(str::to_owned)
                        .or_else(|| args.next())
                        .ok_or_else(|| invalid("missing output path for dump_strings"))?;
                    if value.is_empty() {
                        return Err(invalid("dump_strings output path must not be empty").into());
                    }
                    dump_strings = Some(PathBuf::from(value));
                }
                "timing" | "sizes" | "summary" | "list_targets" => {
                    if inline.is_some() {
                        return Err(invalid("table and listing flags take no value").into());
                    }
                    match name.as_str() {
                        "timing" => tables.timing = true,
                        "sizes" => tables.sizes = true,
                        "summary" => tables.summary = true,
                        _ => list_targets = true,
                    }
                    explicit_tables |= name != "list_targets";
                }
                "dedup" | "block_size" | "fields" | "zstd_level" | "dict_size" | "sample_size"
                | "target" | "compression" | "compression_mode" | "prefix" => {
                    let value = inline
                        .map(str::to_owned)
                        .or_else(|| args.next())
                        .ok_or_else(|| invalid(format!("missing value for {name}")))?;
                    match name.as_str() {
                        "dedup" => modes = values(&value, DedupMode::parse)?,
                        "prefix" => prefixes = values(&value, prefix_mode)?,
                        "fields" => groups = values(&value, FieldGroup::parse)?,
                        "compression" => compression = values(&value, Compression::parse)?,
                        "compression_mode" => {
                            compression_modes = values(&value, CompressionMode::parse)?
                        }
                        "target" => targets = values(&value, |s| Ok(s.to_owned()))?,
                        "block_size" => block_sizes = values(&value, positive)?,
                        "zstd_level" => zstd_levels = values(&value, |s| Ok(s.parse()?))?,
                        "dict_size" => {
                            dict_sizes = values(&value, |s| Ok(usize::try_from(positive(s)?)?))?
                        }
                        "sample_size" => {
                            sample_sizes = values(&value, |s| Ok(usize::try_from(positive(s)?)?))?
                        }
                        _ => unreachable!(),
                    }
                }
                _ => return Err(invalid(format!("unknown option {arg}; use --help")).into()),
            }
        }
        if !explicit_tables {
            tables = Tables {
                timing: true,
                sizes: true,
                summary: true,
            };
        }
        let path = path.ok_or_else(|| invalid("missing dump path; use --help"))?;
        if list_targets && dump_strings.is_some() {
            return Err(invalid("--dump_strings and --list_targets cannot be combined").into());
        }
        if list_targets || dump_strings.is_some() {
            // Export modes use only input/output paths and target filters.
            // They never expand experiments, apply dedup, or load codecs.
            return Ok(Self {
                path,
                groups,
                targets,
                settings: Vec::new(),
                compression,
                compression_modes,
                tables: Tables::default(),
                list_targets,
                dump_strings,
            });
        }
        if compression_modes == [CompressionMode::None] && !compression.contains(&Compression::None)
        {
            return Err(invalid("compression_mode=none requires compression=none").into());
        }
        let mut settings = Vec::new();
        // Keep dimension order stable, with block sizes adjacent for each mode.
        for group in &groups {
            for target in &targets {
                for &mode in &modes {
                    for &prefix in &prefixes {
                        // Expand only allowed prefix combinations, so off/on can
                        // be compared alongside plain dedup and other fields.
                        if prefix && (*group != FieldGroup::Message || mode == DedupMode::Plain) {
                            continue;
                        }
                        for &block_rows in
                            block_sizes
                                .iter()
                                .take(if mode.uses_message_dictionary() && !prefix {
                                    1
                                } else {
                                    block_sizes.len()
                                })
                        {
                            for &zstd_level in &zstd_levels {
                                for &dict_max_bytes in &dict_sizes {
                                    for &sample_bytes in sample_sizes.iter().take(
                                        if mode.uses_message_dictionary() {
                                            1
                                        } else {
                                            sample_sizes.len()
                                        },
                                    ) {
                                        settings.push(Setting {
                                            group: *group,
                                            target: target.clone(),
                                            mode,
                                            prefix,
                                            block_rows,
                                            zstd_level,
                                            dict_max_bytes,
                                            sample_bytes,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if settings.is_empty() {
            return Err(invalid(
                "--prefix=on requires --fields=message and block, archetype, or global dedup",
            )
            .into());
        }
        Ok(Self {
            path,
            groups,
            targets,
            settings,
            compression,
            compression_modes,
            tables,
            list_targets,
            dump_strings,
        })
    }
}

pub fn target_matches(pattern: &str, target: &str) -> bool {
    // Bare crate/module names include their child modules, without accidentally
    // matching another crate whose name merely starts with the same letters.
    if !pattern.contains(['*', '?']) {
        return target == pattern
            || target
                .strip_prefix(pattern)
                .is_some_and(|s| s.starts_with("::"));
    }
    // Standard wildcard matching with backtracking to the last star. A star
    // spans :: separators because Rust targets are names, not filesystem paths.
    let pattern: Vec<_> = pattern.chars().collect();
    let target: Vec<_> = target.chars().collect();
    let (mut p, mut t) = (0, 0);
    let (mut star, mut retry) = (None, 0);
    while t < target.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == target[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            retry = t;
        } else if let Some(previous) = star {
            retry += 1;
            t = retry;
            p = previous + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_expand_in_stable_order_and_validate_values() {
        let options = Options::parse(
            [
                "--fields=message,other",
                "--target=glfw*,corngame",
                "--dedup=plain,block,archetype",
                "--block-size=100,1000,100",
                "--zstd_level=3,6",
                "--dict_size=4096,8192",
                "--sample_size=256,512",
                "--timing",
                "dump.msgpack",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(options.settings.len(), 144);
        assert!(options.tables.timing);
        assert!(!options.tables.sizes);
        assert!(!options.tables.summary);
        assert_eq!(options.settings[0].block_rows, 100);
        assert_eq!(options.settings[8].block_rows, 1000);
        for bad in [
            "--block_size=0",
            "--dedup=unknown",
            "--fields=message,",
            "--sample_size=0",
            "--bogus=1",
            "--compression=gzip",
            "--compression_mode=invalid",
        ] {
            assert!(Options::parse([bad, "dump.msgpack"].map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn target_globs_and_module_prefixes_match() {
        assert!(target_matches("glfw*", "glfw_core::whatever"));
        assert!(target_matches("corngame", "corngame::whatever"));
        assert!(target_matches("corngame", "corngame"));
        assert!(!target_matches("corngame", "corngame_extra::whatever"));
        assert!(target_matches("*core::*", "glfw_core::whatever"));
        assert!(target_matches("gl?w*", "glfw_core::whatever"));
        assert!(!target_matches("glfw*", "winit::window"));
        assert!(!target_matches("*window", "winit::window::event"));
    }

    #[test]
    fn dump_strings_accepts_a_path_and_bypasses_experiments() {
        let options =
            Options::parse(["--dump-strings", "messages.tsv", "dump.msgpack"].map(str::to_owned))
                .unwrap();
        assert_eq!(options.dump_strings, Some(PathBuf::from("messages.tsv")));
        assert!(options.settings.is_empty());
        assert!(!options.tables.timing && !options.tables.sizes && !options.tables.summary);
        for args in [
            vec!["--dump_strings=", "dump.msgpack"],
            vec!["--dump_strings=out.tsv", "--list_targets", "dump.msgpack"],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn prefix_dimensions_keep_block_sizes_and_skip_disallowed_combinations() {
        let options = Options::parse(
            [
                "--prefix=off,on",
                "--fields=message,other",
                "--block_size=100,1000",
                "dump.msgpack",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        // Off: six settings per field group. On: two block sizes for each
        // non-plain message dedup mode, including archetype/global pools.
        assert_eq!(options.settings.len(), 18);
        for setting in options.settings.iter().filter(|s| s.prefix) {
            assert_eq!(setting.group, FieldGroup::Message);
            assert_ne!(setting.mode, DedupMode::Plain);
        }
        for mode in [DedupMode::Archetype, DedupMode::Global] {
            assert_eq!(
                options
                    .settings
                    .iter()
                    .filter(|s| s.mode == mode && s.prefix)
                    .map(|s| s.block_rows)
                    .collect::<Vec<_>>(),
                [100, 1000]
            );
        }
        for args in [
            vec!["--prefix=on", "--dedup=plain", "dump.msgpack"],
            vec!["--prefix=on", "--fields=other", "dump.msgpack"],
            vec!["--prefix=yes", "dump.msgpack"],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }
}
