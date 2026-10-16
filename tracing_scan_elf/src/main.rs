//! CLI arguments and orchestration; reusable behavior lives in the library.

use clap::{Parser, ValueEnum};
use std::{fs, path::PathBuf};
use tracing_scan_elf::{Result, extract, lookup_templates, write_csv, write_section};

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
    about = "Extract tracing callsites and templates from a linked ELF"
)]
struct Args {
    /// Executable or shared library built with full DWARF.
    #[arg(value_name = "ELF")]
    input: PathBuf,
    /// Directory for generated lookup artifacts.
    #[arg(
        short = 'o',
        long,
        default_value = "target/tracing_scan_elf",
        value_name = "DIR"
    )]
    output_dir: PathBuf,
    /// Output formats to generate.
    #[arg(long, value_enum, default_value = "both")]
    format: OutputFormat,
    /// Relocate relative source paths to this directory.
    #[arg(long, value_name = "DIR")]
    source_root: Option<PathBuf>,
}

// ---- Scan and write artifacts ----------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse();
    let bytes = fs::read(args.input)?;
    let elf = object::File::parse(bytes.as_slice())?;
    let mut sites = extract(&elf)?;
    lookup_templates(&mut sites, args.source_root.as_deref());
    fs::create_dir_all(&args.output_dir)?;
    if args.format != OutputFormat::Section {
        write_csv(&sites, &args.output_dir.join("templates.csv"))?;
    }
    if args.format != OutputFormat::Csv {
        write_section(&sites, &elf, &args.output_dir.join("templates.o"))?;
    }
    eprintln!(
        "{} callsites, {} templates; output: {}",
        sites.len(),
        sites.iter().filter(|s| s.template.is_ok()).count(),
        args.output_dir.display()
    );
    Ok(())
}
