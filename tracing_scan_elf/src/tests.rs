//! Parser, artifact, and real-DWARF fixtures kept apart from production code.

use super::artifacts::{COLUMNS, HEADER_BYTES, RECORD_BYTES, encode, word};
use super::dwarf::{Namespaces, symbol_value_scope};
use super::source::{macro_calls, parse_log_template, parse_tracing_template, source_templates};
use super::source::{wrapper_definition, wrapper_invocations};
use super::*;
use object::{Object, ObjectSection};
use std::fs;

// Compact expected dictionaries keep parser assertions readable.
fn dictionary(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

// Exercise quoting and UTF-8 in both formats, including stored lookup errors.
fn sample_sites() -> Vec<Callsite> {
    vec![
        Callsite::loaded(
            "src/é,\"file\".rs".into(),
            7,
            false,
            Some("fixture::nested".into()),
            Ok(Template {
                format: "hello,\r\n\"世界\" {}".into(),
                args: dictionary(&[("0", "(a, b)")]),
                fields: dictionary(&[("user", "? user")]),
            }),
        ),
        Callsite::loaded(
            "src/error.rs".into(),
            9,
            true,
            None,
            Err(LookupError {
                line: "tracing::info!(\n  answer = \"a,b\"\n);".into(),
                error: "no literal format string".into(),
            }),
        ),
    ]
}

#[test]
fn artifacts_round_trip() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("tracing-scan-test-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let mut expected = sample_sites();
    expected[0].source_line = "tracing::info!(".into();
    expected[0].macro_call = "tracing::info!(\n  \"世界 {}\",\n  (a, b)\n)".into();
    // A skipped span still round-trips its classification and entire source.
    let mut span = expected[0].clone();
    span.kind = CallsiteKind::Span;
    span.file = "src/span.rs".into();
    span.source = PathBuf::from(&span.file);
    span.macro_call = "tracing::info_span!(\n  \"span\",\n  message = \"text\"\n)".into();
    span.template = Err(LookupError {
        line: span.source_line.clone(),
        error: SPAN_TEMPLATE.into(),
    });
    expected.push(span);
    // An invocation can link to a wrapper whose own template is unresolved.
    let mut invocation = expected[1].clone();
    invocation.file = "src/invocation.rs".into();
    invocation.source = PathBuf::from(&invocation.file);
    invocation.wrapper_row = Some(1);
    invocation.template = expected[0].template.clone();
    expected.push(invocation);
    let mut sites = expected.clone();
    // The lookup path can differ in memory; artifacts always preserve `file`.
    sites[0].source = PathBuf::from("/original/build/src/file.rs");
    let csv = dir.join("templates.csv");
    write_csv(&sites, &csv)?;
    assert_eq!(load_csv(&csv)?, expected);
    assert_eq!(read_csv(fs::read(&csv)?.as_slice())?, expected);

    let bytes = fs::read(std::env::current_exe()?)?;
    let elf = object::File::parse(bytes.as_slice())?;
    let object_path = dir.join("templates.o");
    write_section(&sites, &elf, &object_path)?;
    assert_eq!(load_elf_section(&object_path)?, expected);
    let bytes = fs::read(&object_path)?;
    let object = object::File::parse(bytes.as_slice())?;
    let raw = object.section_by_name(SECTION_NAME).unwrap().data()?;
    assert_eq!(read_section(raw)?, expected);
    // The standalone payload must match the object's section byte for byte.
    let raw_path = dir.join("templates.bin");
    write_raw_section(&sites, &raw_path)?;
    let raw_bytes = fs::read(&raw_path)?;
    assert_eq!(raw_bytes, raw);
    assert_eq!(read_section(&raw_bytes)?, expected);
    assert!(read_section(&encode(&[])?)?.is_empty());

    // A valid empty CSV consists of its header; an empty file is invalid.
    write_csv(&[], &csv)?;
    assert!(load_csv(&csv)?.is_empty());
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn rejects_invalid_csv() {
    let header = format!("{}\n", COLUMNS[..10].join(","));
    for row in [
        "file.rs,1,Unknown,hello,[],[],,,false,fixture\n",
        "file.rs,-1,Ok,hello,[],[],,,false,fixture\n",
        "file.rs,1,Ok,hello,42,[],,,false,fixture\n",
        "file.rs,1,Ok,hello,,[],,,false,fixture\n",
        "file.rs,1,Ok,hello,[],[],,,2,fixture\n",
        "file.rs,1,Ok\n",
    ] {
        assert!(
            read_csv(format!("{header}{row}").as_bytes()).is_err(),
            "{row}"
        );
    }
    assert!(read_csv(&b""[..]).is_err());
    assert!(read_csv(&b"file,line\nfile.rs,1\n"[..]).is_err());
}

#[test]
fn rejects_invalid_sections() -> Result<()> {
    let valid = encode(&sample_sites())?;
    // Every truncated prefix must fail without panicking or accepting a
    // partial table. These records reference the final byte of the payload.
    for end in 0..valid.len() {
        assert!(read_section(&valid[..end]).is_err(), "prefix length {end}");
    }
    for at in [0, 6] {
        let mut corrupt = valid.clone();
        corrupt[at] ^= 0xff;
        assert!(read_section(&corrupt).is_err());
    }
    // Oversized count and out-of-bounds file/result offsets and lengths.
    for at in [8, 12, 16, 24, 28, 32, 36, 40] {
        let mut corrupt = valid.clone();
        corrupt[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_section(&corrupt).is_err());
    }
    let mut corrupt = valid.clone();
    corrupt[12..16].copy_from_slice(&0u32.to_le_bytes());
    assert!(read_section(&corrupt).is_err()); // Filename overlaps the header.
    let mut corrupt = valid.clone();
    corrupt[HEADER_BYTES + 2 * RECORD_BYTES] = 0xff;
    assert!(read_section(&corrupt).is_err()); // Invalid UTF-8 filename.
    let mut corrupt = valid.clone();
    corrupt[word(&valid[24..28]) as usize] = b'!';
    assert!(read_section(&corrupt).is_err()); // Invalid JSON result.
    Ok(())
}

mod wrapper_fixture {
    // Names deliberately have no special relationship to log/tracing names.
    macro_rules! relay {
        ($($args:tt)+) => { log::info!($($args)+) };
    }
    macro_rules! routed {
        ($target:expr; $($args:tt)+) => { log::warn!(target: $target, $($args)+) };
    }
    macro_rules! trace_relay {
        ($($args:tt)+) => { tracing::info!($($args)+) };
    }
    #[inline(never)]
    pub fn emit() -> [u32; 4] {
        let plain = line!() + 1;
        relay!("forwarded {user}", user = 7);
        let multiline = line!() + 1;
        relay!("forwarded {:?}", (1, 2));
        let unsupported = line!() + 1;
        routed!("fixture"; "routed {}", 42);
        let tracing = line!() + 1;
        trace_relay!(?plain, "tracing {plain}");
        let _ = stringify!("unrelated macro must not become a log row");
        [plain, multiline, unsupported, tracing]
    }
}

#[test]
fn dwarf_wrapper_links_and_invocation_templates() -> Result<()> {
    let [plain, multiline, unsupported, tracing] = wrapper_fixture::emit();
    let bytes = fs::read(std::env::current_exe()?)?;
    let elf = object::File::parse(bytes.as_slice())?;
    let mut sites = extract(&elf)?;
    lookup_templates(&mut sites, None);
    for line in [plain, multiline, unsupported, tracing] {
        let rows: Vec<_> = sites
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.file.ends_with(file!()) && s.line == line && s.is_log == (line != tracing)
            })
            .collect();
        assert_eq!(rows.len(), 1, "invocation at {line}");
        let (index, invocation) = rows[0];
        if line == tracing {
            // Canonical tracing metadata already identifies this invocation.
            assert_eq!(invocation.wrapper_row, None);
        } else {
            let parent = invocation
                .wrapper_row
                .expect("log invocation must reference its wrapper row");
            assert_ne!(index, parent);
            assert_eq!(sites[parent].is_log, invocation.is_log);
            assert_ne!(sites[parent].line, line);
        }
        if line == unsupported {
            assert!(invocation.template.is_err());
        } else {
            assert!(invocation.template.is_ok(), "{:?}", invocation.template);
        }
        if line == plain {
            assert_eq!(
                invocation.template.as_ref().unwrap().args,
                dictionary(&[("user", "7")])
            );
        }
    }
    let mut stored = sites.clone();
    for site in &mut stored {
        site.source = PathBuf::from(&site.file);
    }
    assert_eq!(read_section(&encode(&sites)?)?, stored);
    // A second lookup must not create duplicate rows or change references.
    let expected = sites.clone();
    lookup_templates(&mut sites, None);
    assert_eq!(sites, expected);
    Ok(())
}

#[test]
fn wrapper_links_reject_cycles_and_dangling_rows() -> Result<()> {
    let mut sites = sample_sites();
    sites[0].wrapper_row = Some(10);
    assert!(encode(&sites).is_err());
    sites[0].wrapper_row = Some(0);
    assert!(encode(&sites).is_err());
    sites[0].wrapper_row = Some(1);
    assert!(encode(&sites).is_err()); // Cross-framework link.
    sites[1].is_log = false;
    sites[1].kind = CallsiteKind::Event;
    sites[1].wrapper_row = Some(0);
    assert!(encode(&sites).is_err()); // Two-row cycle.
    sites[1].wrapper_row = None;
    let mut raw = encode(&sites)?;
    raw[HEADER_BYTES + 32..HEADER_BYTES + 36].copy_from_slice(&10u32.to_le_bytes());
    assert!(read_section(&raw).is_err());
    Ok(())
}

#[test]
fn wrapper_recovery_rejects_ambiguous_or_rewritten_calls() -> Result<()> {
    let file = syn::parse_file(
        "macro_rules! changed { ($value:expr) => { log::info!(\"fixed {}\", $value) }; }",
    )?;
    let wrapper = wrapper_definition(&file, 1).unwrap();
    assert!(!wrapper.passthrough); // changed!("looks like a format") must not silently become that format.
    let calls = wrapper_invocations(
        quote::quote!({
            relay!("valid");
            other::relay!("different definition");
            #[cfg(any())]
            relay!("not compiled");
            #[cfg(any())]
            {
                relay!("not compiled either");
            }
            #[cfg(any())]
            if true {
                relay!("guarded conditional");
            }
            #[cfg(any())]
            let _disabled = 1;
            relay!("independent sibling");
            let _ = || relay!("different lexical function");
        }),
        "relay",
    );
    assert_eq!(calls.len(), 2);
    assert!(
        wrapper_invocations(
            quote::quote!({
                macro_rules! relay {
                    () => {};
                }
                relay!("shadowed");
            }),
            "relay"
        )
        .is_empty()
    );
    Ok(())
}

#[test]
fn dwarf_to_templates() -> Result<()> {
    // Real callsites in this binary exercise DWARF and source lookup together.
    let good = line!() + 1;
    tracing::info!(answer = 42, "hello {}", 42);
    let bad = line!() + 1;
    tracing::info!(answer = 42);
    let span_line = line!() + 1;
    let _span = tracing::info_span!("plain span");
    let span_message = line!() + 1;
    let _with_message = tracing::info_span!(
        "span with message",
        message = "this is not an event format string"
    );
    let bytes = fs::read(std::env::current_exe()?)?;
    let elf = object::File::parse(bytes.as_slice())?;
    let mut sites = extract(&elf)?;
    sites.retain(|site| {
        site.source.ends_with(file!()) && [good, bad, span_line, span_message].contains(&site.line)
    });
    assert_eq!(sites.len(), 4);
    assert!(sites.iter().all(|site| !site.is_log));
    for site in &sites {
        // file!() is relative in this build; source lookup remains absolute.
        assert_eq!(site.file, file!());
        assert!(site.source.is_absolute());
        assert!(site.source.ends_with(&site.file));
        assert_eq!(site.module_path.as_deref(), Some(module_path!()));
    }
    lookup_templates(&mut sites, None);
    assert_eq!(sites.len(), 4); // Ineligible callsites remain in both outputs.
    for site in &sites {
        assert!(!site.source_line.is_empty());
        assert!(site.macro_call.starts_with("tracing::"));
        assert!(site.macro_call.ends_with(')'));
        if [span_line, span_message].contains(&site.line) {
            assert_eq!(site.kind, CallsiteKind::Span);
            assert_eq!(site.template.as_ref().unwrap_err().error, SPAN_TEMPLATE);
        } else {
            assert_eq!(site.kind, CallsiteKind::Event);
        }
    }
    let multi = sites.iter().find(|site| site.line == span_message).unwrap();
    assert!(multi.macro_call.contains('\n'));
    assert!(
        multi
            .macro_call
            .contains("message = \"this is not an event format string\"")
    );
    assert_eq!(
        sites
            .iter()
            .find(|s| s.line == bad)
            .unwrap()
            .template
            .as_ref()
            .unwrap_err()
            .error,
        NO_MESSAGE
    );
    assert_eq!(
        sites[0].template.as_ref().unwrap().args,
        dictionary(&[("0", "42")])
    );
    assert!(
        sites[1]
            .template
            .as_ref()
            .unwrap_err()
            .line
            .contains("tracing::info!")
    );
    Ok(())
}

#[test]
fn macro_text_capture_and_line_fallback_are_independent_of_template_parsing() -> Result<()> {
    let source = "fn main() {\n    ::tracing::info_span!(\n        \"é\", /* preserved comment */\n        message = format_args!(\"{}\", 42)\n    );\n    tracing::info!(\"a\"); tracing::info!(\"b\");\n    let ordinary = 42;\n}\n";
    let calls = macro_calls(source)?;
    assert_eq!(
        calls[&2],
        [
            "::tracing::info_span!(\n        \"é\", /* preserved comment */\n        message = format_args!(\"{}\", 42)\n    )"
        ]
    );
    assert_eq!(calls[&6].len(), 2); // Ambiguous calls retain the whole source line.
    assert_eq!(
        macro_calls("$crate::info!(\"text\");")?[&1],
        ["$crate::info!(\"text\")"]
    );
    let dir = std::env::temp_dir().join(format!("tracing-source-text-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let path = dir.join("source.rs");
    fs::write(&path, source)?;
    let mut sites: Vec<_> = [2, 6, 7]
        .into_iter()
        .map(|line| {
            let mut site = Callsite::loaded(
                path.to_string_lossy().into(),
                line,
                false,
                None,
                Err(LookupError {
                    line: String::new(),
                    error: SPAN_TEMPLATE.into(),
                }),
            );
            site.kind = CallsiteKind::Span;
            site
        })
        .collect();
    lookup_templates(&mut sites, None);
    assert_eq!(sites[0].macro_call, calls[&2][0]);
    for site in &sites[1..] {
        assert_eq!(site.macro_call, site.source_line);
    }
    assert_eq!(sites[2].macro_call.trim(), "let ordinary = 42;");
    assert!(
        sites
            .iter()
            .all(|s| s.template.as_ref().unwrap_err().error == SPAN_TEMPLATE)
    );
    let mut expected = sites.clone();
    for site in &mut expected {
        site.source = PathBuf::from(&site.file);
    }
    assert_eq!(read_section(&encode(&sites)?)?, expected);
    let csv = dir.join("source.csv");
    write_csv(&sites, &csv)?;
    assert_eq!(load_csv(&csv)?, expected);
    fs::remove_dir_all(dir)?;
    Ok(())
}

// Preserve the same-line case: the boolean must distinguish these entries.
#[rustfmt::skip]
fn mixed_framework_line() -> u32 {
    let line = line!() + 1;
    tracing::info!("tracing same line"); log::info!("log same line");
    line
}

mod renamed_framework_fixture {
    #[inline(never)]
    pub fn tracing_line() -> u32 {
        // Reproduce bevy_egui's `bevy_log as log` tracing reexport without
        // adding Bevy as a test dependency or hardcoding its crate name.
        use ::tracing as log;
        let window_entity = 7;
        let line = line!() + 1;
        log::warn!(
            "Cannot access an underlying winit window for a window entity {}",
            window_entity
        );
        line
    }

    #[inline(never)]
    pub fn log_line() -> u32 {
        // Also exercise the opposite rename with log's distinct field grammar.
        use ::log as tracing;
        let line = line!() + 1;
        tracing::warn!(window = 7; "renamed log {}", 7);
        line
    }
}

#[test]
fn shared_source_lookup_preserves_lines_and_failures() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("tracing-source-cache-{}", std::process::id()));
    fs::create_dir_all(dir.join("nested"))?;
    // Exercise shared framework reads, Unicode byte offsets, CRLF, a trailing
    // bare CR, parse failure, and missing source through the public lookup.
    let source = "tracing::info!(\"hé {}\", value);\r\nlog::info!(\"log {value}\");\r\ntracing::info_span!(\"work\");\r";
    fs::write(dir.join("nested/source.rs"), source)?;
    fs::write(dir.join("broken.rs"), "// hé\r\ninfo!(\"unfinished\r\n")?;
    let site = |file: &str, line, is_log| {
        Callsite::loaded(
            file.into(),
            line,
            is_log,
            None,
            Err(LookupError {
                line: String::new(),
                error: "source not examined".into(),
            }),
        )
    };
    let mut sites = vec![
        site("nested/source.rs", 1, false),
        site("nested/source.rs", 2, true),
        site("nested/source.rs", 3, false),
        site("broken.rs", 2, false),
        site("missing.rs", 1, true),
    ];
    sites[2].kind = CallsiteKind::Span;
    sites[2].template.as_mut().unwrap_err().error = SPAN_TEMPLATE.into();
    lookup_templates(&mut sites, Some(&dir));
    assert_eq!(sites.len(), 5);
    assert_eq!(sites[0].template.as_ref().unwrap().format, "hé {}");
    assert_eq!(sites[1].template.as_ref().unwrap().format, "log {value}");
    for (site, line) in sites[..3].iter().zip(source.lines()) {
        assert_eq!(site.source_line, line);
    }
    assert!(sites[2].skip_template());
    assert_eq!(sites[3].source_line, "info!(\"unfinished");
    assert_eq!(sites[3].macro_call, sites[3].source_line);
    assert_eq!(
        sites[3].template.as_ref().unwrap_err().line,
        sites[3].source_line
    );
    assert!(sites[4].template.is_err());
    assert!(sites[4].source_line.is_empty());
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn renamed_framework_paths_use_compiled_metadata() -> Result<()> {
    let tracing_line = renamed_framework_fixture::tracing_line();
    let log_line = renamed_framework_fixture::log_line();
    let bytes = fs::read(std::env::current_exe()?)?;
    let elf = object::File::parse(bytes.as_slice())?;
    let mut sites = extract(&elf)?;
    sites.retain(|site| {
        site.source.ends_with(file!()) && [tracing_line, log_line].contains(&site.line)
    });
    lookup_templates(&mut sites, None);
    assert_eq!(sites.len(), 2);
    let tracing = sites.iter().find(|site| !site.is_log).unwrap();
    let template = tracing.template.as_ref().unwrap();
    assert_eq!(tracing.line, tracing_line);
    assert_eq!(
        template.format,
        "Cannot access an underlying winit window for a window entity {}"
    );
    assert_eq!(template.args, dictionary(&[("0", "window_entity")]));
    let log = sites.iter().find(|site| site.is_log).unwrap();
    assert_eq!(log.line, log_line);
    let template = log.template.as_ref().unwrap();
    assert_eq!(template.format, "renamed log {}");
    assert_eq!(template.fields, dictionary(&[("window", "7")]));
    // An alias cannot make two same-framework invocations unambiguous.
    let ambiguous = parse_source_templates(r#"log::warn!("first"); log::warn!("second");"#, false)?;
    assert_eq!(
        ambiguous[&1].as_ref().unwrap_err().error,
        "ambiguous macros on one line"
    );
    Ok(())
}

#[test]
fn parallel_extraction_preserves_results_and_errors() -> Result<()> {
    // Retain both frameworks, aliases, wrapper links, and optional metadata in
    // the inspected binary, including in optimized test builds.
    wrapper_fixture::emit();
    renamed_framework_fixture::tracing_line();
    renamed_framework_fixture::log_line();
    optional_metadata_fixture::metadata();
    let bytes = fs::read(std::env::current_exe()?)?;
    let elf = object::File::parse(bytes.as_slice())?;
    let single = rayon::ThreadPoolBuilder::new().num_threads(1).build()?;
    let parallel = rayon::ThreadPoolBuilder::new().num_threads(4).build()?;
    let scan = || extract(&elf).map_err(|error| error.to_string());
    let mut expected = single.install(scan)?;
    let mut actual = parallel.install(scan)?;
    assert_eq!(actual, expected); // Includes scope sets and declaration evidence.
    single.install(|| lookup_templates(&mut expected, None));
    parallel.install(|| lookup_templates(&mut actual, None));
    assert_eq!(actual, expected);
    assert!(actual.iter().any(|site| site.wrapper_row.is_some()));
    assert_eq!(encode(&actual)?, encode(&expected)?);

    // Corrupt a retained canonical filename in data sections, leaving the ELF
    // and DWARF intact. Worker failures must retain their text and ordering.
    let mut corrupt = bytes.clone();
    let needle = "virtual/元.rs".as_bytes();
    let mut changed = false;
    for section in elf.sections() {
        if !matches!(section.flags(), object::SectionFlags::Elf { sh_flags }
            if sh_flags & u64::from(object::elf::SHF_ALLOC) != 0)
        {
            continue;
        }
        let Some((offset, _)) = section.file_range() else {
            continue;
        };
        for (index, window) in section.data()?.windows(needle.len()).enumerate() {
            if window == needle {
                corrupt[offset as usize + index] = 0xff;
                changed = true;
            }
        }
    }
    assert!(changed);
    let elf = object::File::parse(corrupt.as_slice())?;
    let scan = || extract(&elf).map_err(|error| error.to_string());
    let expected = single.install(scan).unwrap_err();
    assert!(expected.contains("cannot decode tracing __CALLSITE"));
    assert_eq!(parallel.install(scan).unwrap_err(), expected);
    Ok(())
}

#[test]
fn parallel_source_lookup_preserves_remapped_links_and_fresh_reads() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("tracing-parallel-source-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let source = "tracing::info!(\"hé {value}\");\r\nlog::info!(\"log {}\", value);\r\nlet lvl = log::Level::Info;\r\nrelay!(\"wrapped\");\r\ntracing::info_span!(\"work\");\r\n";
    let mut input = Vec::new();
    for index in 0..8 {
        let name = format!("file-{index}.rs");
        fs::write(dir.join(&name), source)?;
        input.push(Callsite::unresolved(name.clone(), 1, false, None));
        // Filtering this false-positive marker must remap links in later files.
        input.push(Callsite::unresolved(name.clone(), 3, true, None));
        let parent = input.len();
        input.push(Callsite::unresolved(name.clone(), 2, true, None));
        let mut child = Callsite::loaded(
            name.clone(),
            4,
            true,
            None,
            Ok(Template {
                format: "wrapped".into(),
                args: BTreeMap::new(),
                fields: BTreeMap::new(),
            }),
        );
        child.wrapper_row = Some(parent);
        input.push(child);
        let mut span = Callsite::unresolved(name, 5, false, None);
        span.kind = CallsiteKind::Span;
        input.push(span);
    }
    fs::write(dir.join("broken.rs"), "info!(\"unfinished")?;
    input.push(Callsite::unresolved("broken.rs".into(), 1, false, None));
    input.push(Callsite::unresolved("missing.rs".into(), 1, true, None));
    let single = rayon::ThreadPoolBuilder::new().num_threads(1).build()?;
    let parallel = rayon::ThreadPoolBuilder::new().num_threads(4).build()?;
    let mut expected = input.clone();
    let mut actual = input.clone();
    single.install(|| lookup_templates(&mut expected, Some(&dir)));
    parallel.install(|| lookup_templates(&mut actual, Some(&dir)));
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 34);
    assert_eq!(
        actual
            .iter()
            .filter(|site| site.wrapper_row.is_some())
            .count(),
        8
    );
    assert_eq!(encode(&actual)?, encode(&expected)?);
    // Worker caches belong to one scan; a later lookup must observe new source.
    fs::write(dir.join("file-0.rs"), source.replace("hé", "changed"))?;
    parallel.install(|| lookup_templates(&mut input, Some(&dir)));
    assert_eq!(
        input[0].template.as_ref().unwrap().format,
        "changed {value}"
    );
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn log_dwarf_to_templates() -> Result<()> {
    use log::warn as aliased;
    // Same variable/type names from another namespace must not match log.
    enum Level {
        Other,
    }
    let unrelated = line!() + 1;
    let lvl = Level::Other;
    std::hint::black_box(lvl);
    let plain = line!() + 1;
    log::info!("log {}", 42);
    let structured = line!() + 1;
    log::info!(logger: log::logger(), target: "fixture", code = 42, user = "Ada"; "structured {}", 7);
    let generic = line!() + 1;
    log::log!(target: "fixture", log::Level::Warn, "generic {value:?}", value = (1, 2));
    let alias = line!() + 1;
    aliased!("aliased warning");
    let unsupported = line!() + 1;
    log::info!(concat!("joined ", "{}"), 42);
    let ordinary = line!() + 1;
    let lvl = log::Level::Info;
    std::hint::black_box(lvl);
    let enabled = line!() + 1;
    std::hint::black_box(log::log_enabled!(log::Level::Info));
    let mixed = mixed_framework_line();
    let lines = [
        plain,
        structured,
        generic,
        alias,
        unsupported,
        ordinary,
        enabled,
        mixed,
        unrelated,
    ];
    let bytes = fs::read(std::env::current_exe()?)?;
    let elf = object::File::parse(bytes.as_slice())?;
    let mut sites = extract(&elf)?;
    sites.retain(|s| s.source.ends_with(file!()) && lines.contains(&s.line));
    assert!(!sites.iter().any(|s| s.line == unrelated));
    lookup_templates(&mut sites, None);
    assert_eq!(sites.len(), 7); // Five log calls plus two distinct same-line calls.
    assert!(!sites.iter().any(|s| [ordinary, enabled].contains(&s.line)));
    let template = |line| {
        sites
            .iter()
            .find(|s| s.line == line)
            .unwrap()
            .template
            .as_ref()
            .unwrap()
    };
    assert_eq!(template(plain).args, dictionary(&[("0", "42")]));
    assert_eq!(
        template(structured).fields,
        dictionary(&[("code", "42"), ("user", "\"Ada\"")])
    );
    assert_eq!(template(generic).format, "generic {value:?}");
    assert_eq!(template(alias).format, "aliased warning");
    assert!(
        sites
            .iter()
            .find(|s| s.line == unsupported)
            .unwrap()
            .template
            .is_err()
    );
    assert_eq!(sites.iter().filter(|s| s.is_log).count(), 6);
    assert!(
        sites
            .iter()
            .all(|s| s.module_path.as_deref() == Some(module_path!()))
    );
    let mut mixed_sites: Vec<_> = sites.iter().filter(|s| s.line == mixed).collect();
    mixed_sites.sort_by_key(|s| s.is_log);
    assert_eq!(
        mixed_sites[0].template.as_ref().unwrap().format,
        "tracing same line"
    );
    assert_eq!(
        mixed_sites[1].template.as_ref().unwrap().format,
        "log same line"
    );
    // Preserve the framework on real DWARF records when encoding the section.
    let mut expected = sites.clone();
    for site in &mut expected {
        site.source = PathBuf::from(&site.file);
    }
    assert_eq!(read_section(&encode(&sites)?)?, expected);
    Ok(())
}

#[test]
fn multiline_wrappers_stop_at_matching_delimiters() -> Result<()> {
    // Based on the once!/cfg_select! failures in the Bevy viewer CSV.
    // Delimiters in literals/comments and nested expressions are not boundaries.
    let source = r##"once!(
        warn!(r#"message ) {}"#, call((1, 2)), /* ) */)
    );
    info!("next statement");
    cfg_select! { _ => { error!("conditional"); } }
    cfg_if! { if #[cfg(unix)] { debug!("unix"); } }
    once!(info!("first"); warn!("second"));
    info!(concat!("not", " a literal"));"##;
    for is_log in [false, true] {
        let mut found = BTreeMap::new();
        source_templates(source.parse()?, is_log, &mut found);
        assert_eq!(found.len(), 7);
        for line in [1, 2] {
            let t = found[&line][0].template.as_ref().unwrap();
            assert_eq!(t.format, "message ) {}");
            assert_eq!(t.args, dictionary(&[("0", "call ((1 , 2))")]));
        }
        for (line, format) in [(4, "next statement"), (5, "conditional"), (6, "unix")] {
            assert_eq!(found[&line][0].template.as_ref().unwrap().format, format);
        }
        assert_eq!(found[&7].len(), 2); // Preserve ambiguity, never pick one call.
        assert!(found[&8][0].template.is_err()); // Do not reinterpret concat! as a log call.
    }
    // Span names must never be interpreted as message templates.
    assert!(parse_tracing_template("\"work\", answer = 42,".parse()?, "info_span").is_err());
    assert!(
        parse_source_templates("tracing::info_span!(\"work\", message = \"text\");", false)?
            .is_empty()
    );
    Ok(())
}

#[test]
fn log_format_arguments_and_fields() -> Result<()> {
    let t = parse_log_template(
        r##"logger: logger, target: "app", log::Level::Info, user:?, count = make::<u8, u16>(); r#"hello {name} {}"#, (1, 2), name = "Ada""##.parse()?,
        "log",
    )?;
    assert_eq!(t.format, "hello {name} {}");
    assert_eq!(t.args.len(), 2);
    assert_eq!(
        t.fields,
        dictionary(&[("user", ":? user"), ("count", "make :: < u8 , u16 > ()")])
    );
    assert!(parse_log_template("concat!(\"x\")".parse()?, "info").is_err());
    Ok(())
}

mod metadata_owner {
    // The impl lives elsewhere: its type's namespace is not the log's module.
    pub struct Remote;
}

mod metadata_emitter {
    impl super::metadata_owner::Remote {
        #[track_caller]
        #[inline(never)]
        pub fn emit() {
            // Runtime capture supplies independent ground truth for the ELF reader.
            tracing::info!(target: "canonical-target", answer = 42, "canonical event");
            let span = tracing::info_span!("canonical_span", answer = 42);
            let _entered = span.enter();
        }
    }
}

mod optional_metadata_fixture {
    use tracing::{
        Metadata,
        callsite::{DefaultCallsite, Identifier},
        field::FieldSet,
        metadata::Kind,
    };

    // Deliberately unrelated to the declaration path/module/line. A namespace
    // or declaration fallback would invent the wrong values for this object.
    static __CALLSITE: DefaultCallsite = DefaultCallsite::new(&META);
    static META: Metadata<'static> = Metadata::new(
        "canonical fixture",
        "fixture-target",
        tracing::Level::INFO,
        Some("virtual/元.rs"),
        Some(65501),
        None,
        FieldSet::new(&[], Identifier(&__CALLSITE)),
        Kind::EVENT,
    );

    pub fn metadata() -> &'static Metadata<'static> {
        // Keep the real static and its debug entry alive through linking.
        use tracing::callsite::Callsite as _;
        std::hint::black_box(&__CALLSITE).metadata()
    }
}

#[test]
fn static_tracing_locations_equal_runtime_metadata() -> Result<()> {
    #[derive(Default)]
    struct Capture(std::sync::Mutex<Vec<&'static tracing::Metadata<'static>>>);
    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            self.0.lock().unwrap().push(span.metadata());
            tracing::span::Id::from_u64(1)
        }
        fn event(&self, event: &tracing::Event<'_>) {
            self.0.lock().unwrap().push(event.metadata());
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let capture = std::sync::Arc::new(Capture::default());
    tracing::subscriber::with_default(capture.clone(), metadata_owner::Remote::emit);
    let runtime = capture.0.lock().unwrap();
    assert_eq!(runtime.len(), 2, "capture both the event and span");
    let custom = optional_metadata_fixture::metadata();
    let bytes = fs::read(std::env::current_exe()?)?;
    let elf = object::File::parse(bytes.as_slice())?;
    let mut sites = extract(&elf)?;
    for metadata in runtime.iter().copied().chain([custom]) {
        let matches: Vec<_> = sites
            .iter()
            .filter(|site| {
                !site.is_log
                    && Some(site.file.as_str()) == metadata.file()
                    && Some(site.line) == metadata.line()
            })
            .collect();
        assert_eq!(matches.len(), 1, "{metadata:?}");
        assert_eq!(matches[0].module_path.as_deref(), metadata.module_path());
    }
    // Neither track_caller nor the type owner's namespace overrides tracing's
    // lexical metadata. Missing optional module data must remain None.
    assert!(
        runtime
            .iter()
            .all(|m| m.module_path() == Some(concat!(module_path!(), "::metadata_emitter")))
    );
    assert_eq!(custom.module_path(), None);
    let custom_site = sites.iter().find(|s| s.file == "virtual/元.rs").unwrap();
    assert!(custom_site.source.ends_with("virtual/元.rs"));
    sites.retain(|s| {
        runtime
            .iter()
            .any(|m| m.file() == Some(s.file.as_str()) && m.line() == Some(s.line))
    });
    lookup_templates(&mut sites, None);
    assert!(
        sites.iter().all(|s| match s.kind {
            CallsiteKind::Span => s.template.as_ref().is_err_and(|e| e.error == SPAN_TEMPLATE),
            _ => s.template.is_ok(),
        }),
        "{sites:?}"
    );
    assert_eq!(
        sites
            .iter()
            .find(|s| s
                .template
                .as_ref()
                .is_ok_and(|t| t.format == "canonical event"))
            .unwrap()
            .template
            .as_ref()
            .unwrap()
            .fields,
        dictionary(&[("answer", "42")])
    );
    Ok(())
}

// Inline modules deliberately do not correspond to filesystem directories.
mod scope_fixture {
    pub struct Worker;
    impl Worker {
        #[inline(never)]
        pub fn emit<T>() -> ([u32; 2], &'static str) {
            std::hint::black_box(std::any::type_name::<T>());
            let tracing_line = line!() + 1;
            tracing::info!("method tracing");
            let log_line = line!() + 1;
            log::info!("method log");
            ([tracing_line, log_line], module_path!())
        }
    }
}

#[test]
fn module_metadata_excludes_function_and_type_scopes() -> Result<()> {
    let (lines, module) = scope_fixture::Worker::emit::<u8>();
    let bytes = fs::read(std::env::current_exe()?)?;
    let elf = object::File::parse(bytes.as_slice())?;
    let sites: Vec<_> = extract(&elf)?
        .into_iter()
        .filter(|s| s.source.ends_with(file!()) && lines.contains(&s.line))
        .collect();
    assert_eq!(sites.len(), 2);
    for site in &sites {
        assert_eq!(site.module_path.as_deref(), Some(module), "{site:?}");
    }
    // Absent scope metadata remains absent; no filename is consulted.
    assert_eq!(Namespaces::default().module_path(&BTreeSet::new()), None);
    // A retained closure namespace without its enclosing function DIE is
    // insufficient to distinguish module names from function names.
    let unresolved = Namespaces(vec![
        (1, "fixture".into()),
        (2, "save_to_disk".into()),
        (3, "{closure#0}".into()),
    ]);
    assert_eq!(unresolved.module_path(&BTreeSet::new()), None);
    // The retained callsite symbol identifies save_to_disk as a value scope,
    // recovering the exact module even when that function DIE was discarded.
    let symbol = "_RNvNCNvNtNtNtCsa0eul5DiDrQ_11bevy_render4view6window10screenshot12save_to_disk010___CALLSITE";
    let scope = symbol_value_scope(symbol).unwrap();
    assert_eq!(scope, "bevy_render::view::window::screenshot::save_to_disk");
    let namespaces = Namespaces(vec![
        (1, "bevy_render".into()),
        (2, "view".into()),
        (3, "window".into()),
        (4, "screenshot".into()),
        (5, "save_to_disk".into()),
        (6, "{closure#0}".into()),
    ]);
    assert_eq!(
        namespaces.module_path(&BTreeSet::from([scope])).as_deref(),
        Some("bevy_render::view::window::screenshot")
    );
    for symbol in [
        "",
        "_RN",
        "_RNvCs_999x",
        "_RNvC1x999999999999999999999999999x",
        "_RNvB0_3foo",
        "_RNvCu4caf_3foo",
        "_ZN3foo3barE",
    ] {
        assert_eq!(symbol_value_scope(symbol), None);
    }
    Ok(())
}

#[test]
fn loads_legacy_tracing_artifacts() -> Result<()> {
    let csv = b"file,line,result,format,args,fields,source,error\nlib.rs,7,Ok,legacy,[],[],,\n";
    let expected = read_csv(csv.as_slice())?;
    assert_eq!(expected.len(), 1);
    assert!(!expected[0].is_log);
    assert_eq!(expected[0].module_path, None);
    // Frozen v1 layout, independent of the current encoder's record width.
    let json = br#"{"Ok":{"format":"legacy","args":[],"fields":[]}}"#;
    let mut section = b"TSCAN\0\x01\0".to_vec();
    for value in [1u32, 32, 6, 7, 38, json.len() as u32] {
        section.extend(value.to_le_bytes());
    }
    section.extend(b"lib.rs");
    section.extend(json);
    assert_eq!(read_section(&section)?, expected);
    // V2 already has the framework boolean, but no module metadata.
    let csv = b"file,line,result,format,args,fields,source,error,is_log\nlib.rs,7,Ok,legacy,[],[],,,true\n";
    let expected = read_csv(csv.as_slice())?;
    let mut section = b"TSCAN\0\x02\0".to_vec();
    for value in [1u32, 36, 6, 7, 42, json.len() as u32, 1] {
        section.extend(value.to_le_bytes());
    }
    section.extend(b"lib.rs");
    section.extend(json);
    assert_eq!(read_section(&section)?, expected);
    assert!(expected[0].is_log);
    assert_eq!(expected[0].module_path, None);
    // Frozen v5 uses the old result-only JSON with the current record width.
    let old_json = br#"{"Ok":{"format":"legacy","args":{},"fields":{}}}"#;
    let mut v5 = b"TSCAN\0\x05\0".to_vec();
    for value in [
        1u32,
        48,
        6,
        7,
        54,
        old_json.len() as u32,
        0,
        54 + old_json.len() as u32,
        0,
        u32::MAX,
    ] {
        v5.extend(value.to_le_bytes());
    }
    v5.extend(b"lib.rs");
    v5.extend(old_json);
    let legacy = read_section(&v5)?;
    assert_eq!(legacy[0].kind, CallsiteKind::Event);
    assert_eq!(legacy[0].template.as_ref().unwrap().format, "legacy");
    assert!(legacy[0].macro_call.is_empty());
    // V3 had module metadata but still stored expression arrays in JSON.
    let json = br#"{"Ok":{"format":"legacy {} {name}","args":["x","name = user"],"fields":["?user","answer = 42"]}}"#;
    let mut section = b"TSCAN\0\x03\0".to_vec();
    for value in [
        1u32,
        44,
        6,
        7,
        50,
        json.len() as u32,
        1,
        50 + json.len() as u32,
        7,
    ] {
        section.extend(value.to_le_bytes());
    }
    section.extend(b"lib.rs");
    section.extend(json);
    section.extend(b"fixture");
    let sites = read_section(&section)?;
    assert_eq!(sites[0].module_path.as_deref(), Some("fixture"));
    let t = sites[0].template.as_ref().unwrap();
    assert_eq!(t.args, dictionary(&[("0", "x"), ("name", "user")]));
    assert_eq!(
        t.fields,
        dictionary(&[("user", "? user"), ("answer", "42")])
    );
    Ok(())
}

#[test]
fn named_captures_and_field_dictionaries() -> Result<()> {
    // Escaped braces are literal; explicit names override captures. Width and
    // precision variables count as arguments even though they are not values.
    let tokens = r#""{{ignored}} {} {user:?} {alias} {value:0width$.precision$}", call((1, 2)), alias = other"#;
    for parser in [parse_tracing_template, parse_log_template] {
        let t = parser(tokens.parse()?, "info")?;
        assert_eq!(
            t.args,
            dictionary(&[
                ("0", "call ((1 , 2))"),
                ("user", "user"),
                ("alias", "other"),
                ("value", "value"),
                ("width", "width"),
                ("precision", "precision")
            ])
        );
        let t = parser("\"error: {e}\"".parse()?, "error")?;
        assert_eq!(t.args, dictionary(&[("e", "e")]));
    }
    let t = parse_tracing_template(
        r#"user, ?debug, %item.id, count = value + 1, "custom key" = %label, "done""#.parse()?,
        "info",
    )?;
    assert_eq!(
        t.fields,
        dictionary(&[
            ("user", "user"),
            ("debug", "? debug"),
            ("item.id", "% item . id"),
            ("count", "value + 1"),
            ("custom key", "% label")
        ])
    );
    assert!(t.args.is_empty());
    // A message field is a literal value, not a Rust format invocation.
    let t = parse_tracing_template("message = \"{literal}\"".parse()?, "info")?;
    assert!(t.args.is_empty());
    assert_eq!(
        serde_json::from_slice::<Template>(&serde_json::to_vec(&t)?)?,
        t
    );
    Ok(())
}

#[test]
fn formats_arguments_and_fields() {
    let t = parse_tracing_template(
        r##"target: "app", ?user, item.id = %id, "got {value:?}\n{}", make::<u8, u16>(), value = (1, 2)"##
            .parse()
            .unwrap(),
        "info",
    )
    .unwrap();
    assert_eq!(t.format, "got {value:?}\n{}");
    assert_eq!(t.args.len(), 2);
    assert_eq!(
        t.fields,
        dictionary(&[("user", "? user"), ("item.id", "% id")])
    );
    let t = parse_tracing_template(
        r##"tracing::Level::INFO, { answer = 42 }, r#"raw {}"#, value"##
            .parse()
            .unwrap(),
        "event",
    )
    .unwrap();
    assert_eq!(t.format, "raw {}");
    assert_eq!(t.fields, dictionary(&[("answer", "42")]));
    let t = parse_tracing_template(
        r#"message = %format_args!("hi {}", user), answer = 42"#
            .parse()
            .unwrap(),
        "info",
    )
    .unwrap();
    assert_eq!(t.args, dictionary(&[("0", "user")]));
    assert_eq!(t.fields.len(), 2);
    assert!(parse_tracing_template("answer = 42".parse().unwrap(), "info").is_err());
}
