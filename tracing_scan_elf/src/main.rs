//! CODEX GENERATED
//! CLI arguments and orchestration; reusable behavior lives in the library.

use clap::{Parser, ValueEnum};
use std::{
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::Instant,
};
use tracing_scan_elf::{
    Result, extract, lookup_templates, write_csv, write_raw_section, write_section,
};

// ---- CLI options -----------------------------------------------------------

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum OutputFormat {
    Both,
    Csv,
    Section,
}

// Keep CLI defaults and validation together; Clap provides help and usage errors.
#[derive(Parser)]
#[command(
    version,
    about = "Extract tracing and log callsites and templates from a linked ELF"
)]
struct Args {
    /// Executable or shared library built with full DWARF.
    #[arg(value_name = "ELF")]
    input: PathBuf,
    /// Output directory (default: nearest ancestor target/tracing_scan_elf).
    #[arg(short = 'o', long, value_name = "DIR")]
    output_dir: Option<PathBuf>,
    /// Output formats to generate (section emits both .bin and .o).
    #[arg(long, value_enum, default_value = "both")]
    format: OutputFormat,
    /// Relocate relative source paths to this directory.
    #[arg(long, value_name = "DIR")]
    source_root: Option<PathBuf>,
    /// Print elapsed time for reading, DWARF extraction, source lookup, and writing.
    #[arg(long)]
    timings: bool,
    /// Scan worker threads (default: RAYON_NUM_THREADS or available CPUs).
    #[arg(long, value_name = "N")]
    threads: Option<NonZeroUsize>,
}

// ---- Scan and write artifacts ----------------------------------------------

fn default_output_dir(input: &Path) -> Result<PathBuf> {
    // Match embed_section.sh: choose the nearest target, including nested builds.
    input
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .find(|dir| dir.file_name().is_some_and(|name| name == "target"))
        .map(|dir| dir.join("tracing_scan_elf"))
        .ok_or_else(|| "no ancestor target directory found; specify --output-dir DIR".into())
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Configure only the CLI process; library callers can use their Rayon pool.
    if let Some(threads) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads.get())
            .build_global()?;
    }
    if args.timings {
        eprintln!("Scan workers: {}", rayon::current_num_threads());
    }
    // Separate phases so large-input regressions can be located without a profiler.
    let mut phase = Instant::now();
    let mut report = |name: &str| {
        if args.timings {
            eprintln!("{name}: {:.3}s", phase.elapsed().as_secs_f64());
        }
        phase = Instant::now();
    };
    // Resolve relative paths and symlinks consistently with the embedding script.
    let input = fs::canonicalize(&args.input)?;
    let output_dir = match args.output_dir {
        Some(dir) => dir,
        None => default_output_dir(&input)?,
    };
    let file = fs::File::open(input)?;
    // SAFETY: the linked input must remain unchanged for the duration of the scan,
    // just as its DWARF/source must match it. Keep the mapping alive through output.
    // Prefault on platforms supporting it: DWARF strings otherwise cause many
    // small random page faults, which can erase the gain from avoiding a copy.
    let bytes = unsafe { memmap2::MmapOptions::new().populate().map(&file)? };
    let elf = object::File::parse(bytes.as_ref())?;
    report("Read ELF");
    let mut sites = extract(&elf)?;
    report("Extract DWARF");
    lookup_templates(&mut sites, args.source_root.as_deref());
    report("Look up source templates");
    fs::create_dir_all(&output_dir)?;
    if args.format != OutputFormat::Section {
        write_csv(&sites, &output_dir.join("templates.csv"))?;
    }
    if args.format != OutputFormat::Csv {
        // Raw bytes support post-link embedding; the object supports linking.
        write_raw_section(&sites, &output_dir.join("templates.bin"))?;
        write_section(&sites, &elf, &output_dir.join("templates.o"))?;
    }
    report("Write artifacts");
    eprintln!(
        "{} callsites, {} templates; output: {}",
        sites.len(),
        sites.iter().filter(|s| s.template.is_ok()).count(),
        output_dir.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_uses_nearest_target_ancestor() -> Result<()> {
        // Cover profiles, cross compilation, and target directories nested in targets.
        for (input, expected) in [
            (
                "/project/target/debug/app",
                "/project/target/tracing_scan_elf",
            ),
            (
                "/project/target/triple/release/examples/app",
                "/project/target/tracing_scan_elf",
            ),
            (
                "/outer/target/project/target/debug/app",
                "/outer/target/project/target/tracing_scan_elf",
            ),
        ] {
            assert_eq!(
                default_output_dir(Path::new(input))?,
                PathBuf::from(expected)
            );
        }
        assert!(default_output_dir(Path::new("/project/bin/app")).is_err());
        Ok(())
    }
}
