//! CODEX GENERATED, minimally reviewed
//! Embedded templates with a development fallback to the scanner's source parser.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

use parking_lot::Mutex;
use tracing_scan_elf::{
    Callsite, LookupError, Template, load_embedded_section, parse_source_templates,
};

// Keep scanner rows intact; both aliases point at the same process-lifetime data.
type EmbeddedIndex = HashMap<PathBuf, HashMap<u32, Vec<&'static Callsite>>>;
type SourceTemplates = BTreeMap<u32, Result<Template, LookupError>>;

struct Templates {
    root: Option<PathBuf>,
    // Some(empty) still means missing entries should warn about a stale section.
    embedded: Option<EmbeddedIndex>,
    sources: HashMap<(PathBuf, bool), Option<SourceTemplates>>,
}

static TEMPLATES: LazyLock<Mutex<Templates>> = LazyLock::new(|| {
    let root = std::env::var_os("TRACING_TABLE_SOURCE_ROOT")
        .and_then(|root| fs::canonicalize(root).ok())
        .or_else(|| project_root(&std::env::current_exe().ok()?));
    Mutex::new(Templates::new(root, load_embedded_section().ok()))
});

/// Find a template at a one-based line, using the runtime framework/module metadata.
///
/// Call once when registering a callsite and retain the result, including `None`:
/// the subscriber owns result caching and warning deduplication. Source files are
/// parsed once per framework; successful fallback templates are intentionally leaked.
/// Relative paths resolve from the executable's project above `target`, or from
/// `TRACING_TABLE_SOURCE_ROOT` when using a custom/shared target directory.
pub fn lookup(
    file: &str,
    line: u32,
    is_log: bool,
    module_path: Option<&str>,
) -> Option<CompiledTemplate> {
    let template = TEMPLATES.lock().lookup(
        Path::new(file),
        line,
        is_log,
        module_path,
        &mut io::stderr().lock(),
    )?;
    // The callsite owns the compiled matcher and releases it with its recording data.
    CompiledTemplate::compile(&template.format).ok()
}

impl Templates {
    fn new(root: Option<PathBuf>, sites: Option<Vec<Callsite>>) -> Self {
        let embedded = sites.map(|sites| {
            // One allocation retains all embedded rows and their templates.
            let sites: &'static [Callsite] = Box::leak(sites.into_boxed_slice());
            let mut index = EmbeddedIndex::new();
            for site in sites {
                for path in file_paths(root.as_deref(), Path::new(&site.file))
                    .into_iter()
                    .flatten()
                {
                    index
                        .entry(path)
                        .or_default()
                        .entry(site.line)
                        .or_default()
                        .push(site);
                }
            }
            index
        });
        Self {
            root,
            embedded,
            sources: HashMap::new(),
        }
    }

    // Outer None means absent; Some(None) means present but unresolved/ambiguous.
    fn embedded(
        &self,
        paths: &[Option<PathBuf>; 2],
        line: u32,
        is_log: bool,
        module_path: Option<&str>,
    ) -> Option<Option<&'static Template>> {
        let index = self.embedded.as_ref()?;
        let mut rows = paths
            .iter()
            .flatten()
            .filter_map(|path| index.get(path)?.get(&line))
            .flatten()
            .copied()
            .filter(|site| {
                site.is_log == is_log
                    && (site.module_path.is_none()
                        || module_path.is_none()
                        || site.module_path.as_deref() == module_path)
            });
        let template = rows.next()?.template.as_ref().ok();
        // Never select an arbitrary conflicting row at the same source location.
        Some(
            if rows.all(|site| site.template.as_ref().ok() == template) {
                template
            } else {
                None
            },
        )
    }

    fn lookup(
        &mut self,
        file: &Path,
        line: u32,
        is_log: bool,
        module_path: Option<&str>,
        warnings: &mut impl Write,
    ) -> Option<&'static Template> {
        let paths = file_paths(self.root.as_deref(), file);
        let stored = self.embedded(&paths, line, is_log, module_path);
        if stored.is_none() && self.embedded.is_some() {
            // Direct stderr writes cannot recurse through the tracing subscriber.
            let _ = writeln!(
                warnings,
                "warning: tracing_table: {}:{line} is missing from the embedded template section; trying source",
                file.display()
            );
        }
        let template = stored
            .flatten()
            .or_else(|| self.source(paths[1].as_ref()?, line, is_log));
        if template.is_none() {
            let _ = writeln!(
                warnings,
                "warning: tracing_table: template not found in embedded section or source for {}:{line}",
                file.display()
            );
        }
        template
    }

    fn source(&mut self, path: &Path, line: u32, is_log: bool) -> Option<&'static Template> {
        if line == 0 {
            return None;
        }
        let templates = self
            .sources
            .entry((path.to_path_buf(), is_log))
            .or_insert_with(|| {
                let source = fs::read_to_string(path).ok()?;
                parse_source_templates(&source, is_log).ok()
            });
        let template = templates.as_ref()?.get(&line)?.as_ref().ok()?;
        // Only the requested template needs permanent storage, not the whole file.
        Some(Box::leak(Box::new(template.clone())))
    }
}

// Cargo nests binaries beneath target in profiles, triples, deps, and build dirs.
fn project_root(executable: &Path) -> Option<PathBuf> {
    let target = executable
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "target"));
    let start = target
        .and_then(Path::parent)
        .or_else(|| executable.parent())?;
    start
        .ancestors()
        .find(|path| path.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

// Keep the stored spelling and a proven filesystem alias. Preserve .. across
// symlinks and never guess matches using filenames or arbitrary path suffixes.
fn file_paths(root: Option<&Path>, file: &Path) -> [Option<PathBuf>; 2] {
    let stored = file
        .components()
        .filter(|component| *component != Component::CurDir)
        .collect();
    let source = if file.is_absolute() {
        Some(file.to_path_buf())
    } else {
        root.map(|root| root.join(file))
    };
    [
        Some(stored),
        source.and_then(|path| fs::canonicalize(path).ok()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Isolated projects make root/path tests independent of cwd and Cargo's layout.
    struct Project(PathBuf);
    impl Project {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "tracing-template-tests-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("Cargo.toml"), "[package]\nname = 'fixture'\n").unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn write(&self, file: &str, source: &str) -> PathBuf {
            let path = self.0.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, source).unwrap();
            path
        }
        fn templates(&self, sites: Option<Vec<Callsite>>) -> Templates {
            Templates::new(Some(self.0.clone()), sites)
        }
    }
    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn sites(rows: &str) -> Vec<Callsite> {
        let csv =
            format!("file,line,result,format,args,fields,source,error,is_log,module_path\n{rows}");
        tracing_scan_elf::read_csv(csv.as_bytes()).unwrap()
    }
    fn tracing(
        templates: &mut Templates,
        file: impl AsRef<Path>,
        line: u32,
        output: &mut Vec<u8>,
    ) -> Option<&'static Template> {
        templates.lookup(file.as_ref(), line, false, None, output)
    }
    fn warnings(bytes: &[u8]) -> &str {
        std::str::from_utf8(bytes).unwrap()
    }

    #[test]
    fn subscriber_retains_the_template_on_repeated_registration() {
        use crate::subscriber::{LocalRecordingLayer, RecordingLayer};
        use std::sync::Arc;
        use tracing_subscriber::{Registry, layer::SubscriberExt};

        let recorder = Arc::new(RecordingLayer::new());
        ::tracing::subscriber::with_default(
            Registry::default().with(LocalRecordingLayer(recorder.clone())),
            || {
                // Resolve a real callsite through executable-root discovery.
                ::tracing::info!("cached subscriber template");
            },
        );
        let (metadata, template_address) = {
            let sites = recorder.callsites.read();
            let site = sites.values().next().unwrap().read();
            let template = site.template.as_ref().unwrap();
            assert_eq!(template.format, "cached subscriber template");
            (site.metadata, std::ptr::from_ref(template))
        };
        for _ in 0..3 {
            // Re-registration retains the matcher owned by this callsite.
            let site = recorder.register(metadata);
            assert_eq!(
                template_address,
                std::ptr::from_ref(site.template.as_ref().unwrap())
            );
        }
    }

    #[test]
    fn discovers_project_from_all_cargo_binary_layouts() {
        let project = Project::new();
        for executable in [
            "target/debug/app",
            "target/release/examples/app",
            "target/aarch64-unknown-linux-gnu/debug/deps/app-hash",
            "target/debug/build/package/hash/out/app",
        ] {
            assert_eq!(
                project_root(&project.0.join(executable)),
                Some(project.0.clone())
            );
        }
        project.write("member/Cargo.toml", "[package]\nname = 'member'\n");
        assert_eq!(
            project_root(&project.0.join("member/target/debug/app")),
            Some(project.0.join("member"))
        );
        assert_eq!(project_root(Path::new("/nonexistent/location/app")), None);
    }

    #[test]
    fn source_fallback_uses_scanner_grammar_and_reuses_parsed_files() {
        let project = Project::new();
        project.write(
            "member/src/main.rs",
            concat!(
                "fn main() {\n",
                "    tracing::info!(answer = ?answer,\n",
                "        \"value {} {name}\", compute::<u8>(1, 2));\n",
                "    tracing::info!(\"next\");\n",
                "}\n",
            ),
        );
        let mut templates = project.templates(None);
        let mut output = Vec::new();
        let template = tracing(&mut templates, "member/src/main.rs", 2, &mut output).unwrap();
        assert_eq!(template.format, "value {} {name}");
        assert_eq!(template.args["0"], "compute :: < u8 > (1 , 2)");
        assert_eq!(template.args["name"], "name");
        assert_eq!(template.fields["answer"], "? answer");
        // Another callsite in the same file reuses the original parse.
        project.write("member/src/main.rs", "fn main() {}\n");
        assert_eq!(
            tracing(&mut templates, "member/src/main.rs", 4, &mut output)
                .unwrap()
                .format,
            "next"
        );
        assert_eq!(templates.sources.len(), 1);
        assert!(output.is_empty());
    }

    #[test]
    fn source_supports_absolute_log_paths_and_relative_workspace_paths() {
        let project = Project::new();
        let path = project.write(
            "member/src/lib.rs",
            "log::info!(target: \"test\", value:? = value; \"item {}\", value);\n",
        );
        let mut templates = project.templates(None);
        let mut output = Vec::new();
        for file in [path, PathBuf::from("./member/src/../src/lib.rs")] {
            let template = templates.lookup(&file, 1, true, None, &mut output).unwrap();
            assert_eq!(template.format, "item {}");
            assert_eq!(template.fields["value"], ":? value");
        }
        assert!(output.is_empty());
        assert_eq!(templates.sources.len(), 1);
    }

    #[test]
    fn embedded_aliases_match_without_basename_or_suffix_guessing() {
        let project = Project::new();
        let path = project.write("member/src/lib.rs", "tracing::info!(\"source\");\n");
        let mut templates = project.templates(Some(sites(
            "./member/src/lib.rs,1,Ok,embedded,{},{},,,false,\n",
        )));
        let mut output = Vec::new();
        let template = tracing(&mut templates, &path, 1, &mut output).unwrap();
        let alias = tracing(&mut templates, "member/src/../src/lib.rs", 1, &mut output).unwrap();
        assert_eq!(template.format, "embedded");
        assert!(std::ptr::eq(template, alias));
        assert!(output.is_empty());
        assert!(templates.sources.is_empty());
        assert!(tracing(&mut templates, "src/lib.rs", 1, &mut output).is_none());
        assert_eq!(warnings(&output).matches("warning:").count(), 2);
    }

    #[test]
    fn missing_section_entry_warns_even_when_source_succeeds() {
        let project = Project::new();
        project.write("src/main.rs", "tracing::info!(\"source\");\n");
        let mut templates = project.templates(Some(Vec::new()));
        let mut output = Vec::new();
        assert_eq!(
            tracing(&mut templates, "src/main.rs", 1, &mut output)
                .unwrap()
                .format,
            "source"
        );
        assert_eq!(warnings(&output).matches("warning:").count(), 1);
        assert!(warnings(&output).contains("missing from the embedded template section"));
    }

    #[test]
    fn failures_distinguish_absent_and_unresolved_rows() {
        let project = Project::new();
        for (section, expected) in [
            (None, 1),
            (Some(Vec::new()), 2),
            (
                Some(sites("missing.rs,1,Err,,,,source,unsupported,false,\n")),
                1,
            ),
        ] {
            let mut templates = project.templates(section);
            let mut output = Vec::new();
            assert!(tracing(&mut templates, "missing.rs", 1, &mut output).is_none());
            assert_eq!(warnings(&output).matches("warning:").count(), expected);
            assert!(warnings(&output).contains("template not found in embedded section or source"));
        }
    }

    #[test]
    fn module_and_framework_disambiguate_shared_embedded_locations() {
        let project = Project::new();
        let mut templates = project.templates(Some(sites(concat!(
            "virtual.rs,1,Ok,tracing a,{},{},,,false,a\n",
            "virtual.rs,1,Ok,tracing b,{},{},,,false,b\n",
            "virtual.rs,1,Ok,log a,{},{},,,true,a\n",
        ))));
        let mut output = Vec::new();
        for (module, is_log, format) in [
            ("a", false, "tracing a"),
            ("b", false, "tracing b"),
            ("a", true, "log a"),
        ] {
            assert_eq!(
                templates
                    .lookup(
                        Path::new("./virtual.rs"),
                        1,
                        is_log,
                        Some(module),
                        &mut output
                    )
                    .unwrap()
                    .format,
                format
            );
        }
        assert!(output.is_empty());
        assert!(tracing(&mut templates, "virtual.rs", 1, &mut output).is_none());
        assert_eq!(warnings(&output).matches("warning:").count(), 1);
    }

    #[test]
    fn ambiguous_invalid_or_out_of_range_source_does_not_guess() {
        let project = Project::new();
        project.write(
            "ambiguous.rs",
            "tracing::info!(\"a\"); tracing::info!(\"b\");\n",
        );
        project.write("invalid.rs", "fn main( {\n");
        project.write("valid.rs", "tracing::info!(\"a\");\n");
        project.write("dynamic.rs", "tracing::info!(concat!(\"a\"));\n");
        let mut templates = project.templates(None);
        let mut output = Vec::new();
        for (file, line) in [
            ("ambiguous.rs", 1),
            ("invalid.rs", 1),
            ("valid.rs", 0),
            ("valid.rs", 20),
            ("dynamic.rs", 1),
        ] {
            assert!(tracing(&mut templates, file, line, &mut output).is_none());
        }
        assert_eq!(warnings(&output).matches("warning:").count(), 5);
    }

    #[test]
    fn absolute_dependencies_work_without_a_project_root() {
        let project = Project::new();
        let path = project.write(
            "registry/pkg/src/lib.rs",
            "tracing::info!(\"dependency\");\n",
        );
        let mut templates = Templates::new(None, None);
        let mut output = Vec::new();
        assert_eq!(
            tracing(&mut templates, path, 1, &mut output)
                .unwrap()
                .format,
            "dependency"
        );
        assert!(output.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn parent_components_crossing_symlinks_keep_their_real_meaning() {
        let project = Project::new();
        project.write("elsewhere/child/placeholder", "");
        project.write("elsewhere/source.rs", "tracing::info!(\"correct\");\n");
        project.write("source.rs", "tracing::info!(\"wrong\");\n");
        std::os::unix::fs::symlink(project.0.join("elsewhere/child"), project.0.join("link"))
            .unwrap();
        let mut templates = project.templates(Some(sites(concat!(
            "source.rs,1,Ok,wrong embedded,{},{},,,false,\n",
            "elsewhere/source.rs,1,Ok,correct embedded,{},{},,,false,\n",
        ))));
        let mut output = Vec::new();
        assert_eq!(
            tracing(&mut templates, "link/../source.rs", 1, &mut output)
                .unwrap()
                .format,
            "correct embedded"
        );
        assert!(output.is_empty());
    }
}

// ---- Reverse template compilation and matching ------------------------------

/// A message template split into literals and named capture slots.
/// Captures contain rendered text, including any padding/debug formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledTemplate {
    /// Original format, with adjacent placeholder groups rewritten as one `{}`.
    pub format: String,
    /// One name per capture occurrence; repeated arguments retain repeated names.
    /// Merged groups use their zero-based capture slot as a numeric name.
    pub field_names: Vec<String>,
    // Decoded literals surround captures: always field_names.len() + 1 entries.
    literals: Vec<String>,
}

impl CompiledTemplate {
    /// Compile once per callsite. Adjacent placeholders cannot be separated, so
    /// `{a}{b}` becomes one anonymous field. Escaped braces remain literal text.
    pub fn compile(format: &str) -> Result<Self, &'static str> {
        let mut compiled = Self {
            format: String::new(),
            field_names: Vec::new(),
            literals: vec![String::new()],
        };
        let mut chars = format.char_indices().peekable();
        let mut next_argument = 0usize;
        let mut previous_placeholder = 0;
        while let Some((start, character)) = chars.next() {
            match character {
                '{' if chars.peek().is_some_and(|(_, next)| *next == '{') => {
                    chars.next();
                    compiled.literals.last_mut().unwrap().push('{');
                    compiled.format.push_str("{{");
                }
                '}' if chars.peek().is_some_and(|(_, next)| *next == '}') => {
                    chars.next();
                    compiled.literals.last_mut().unwrap().push('}');
                    compiled.format.push_str("}}");
                }
                '{' => {
                    let mut end = None;
                    for (at, character) in chars.by_ref() {
                        match character {
                            '}' => {
                                end = Some(at);
                                break;
                            }
                            '{' => return Err("nested opening brace in placeholder"),
                            _ => {}
                        }
                    }
                    let end = end.ok_or("unclosed placeholder")?;
                    let (argument, spec) = format[start + 1..end]
                        .split_once(':')
                        .unwrap_or((&format[start + 1..end], ""));
                    let argument = argument.trim();
                    // Rust's .* consumes the implicit precision argument before
                    // the value argument. Width/precision $ references do not.
                    if spec.contains(".*") {
                        next_argument = next_argument
                            .checked_add(1)
                            .ok_or("argument index overflow")?;
                    }
                    let name = if argument.is_empty() {
                        let name = next_argument.to_string();
                        next_argument = next_argument
                            .checked_add(1)
                            .ok_or("argument index overflow")?;
                        name
                    } else if argument.bytes().all(|byte| byte.is_ascii_digit()) {
                        argument
                            .parse::<usize>()
                            .map_err(|_| "argument index overflow")?
                            .to_string()
                    } else {
                        // Rust source already validates identifiers; preserve
                        // Unicode names without imposing an ASCII-only grammar.
                        argument.to_owned()
                    };
                    if !compiled.field_names.is_empty()
                        && compiled.literals.last().unwrap().is_empty()
                    {
                        compiled.format.truncate(previous_placeholder);
                        compiled.format.push_str("{}");
                        let slot = compiled.field_names.len() - 1;
                        compiled.field_names[slot] = slot.to_string();
                    } else {
                        previous_placeholder = compiled.format.len();
                        compiled.format.push_str(&format[start..=end]);
                        compiled.field_names.push(name);
                        compiled.literals.push(String::new());
                    }
                }
                '}' => return Err("unescaped closing brace"),
                character => {
                    compiled.literals.last_mut().unwrap().push(character);
                    compiled.format.push(character);
                }
            }
        }
        Ok(compiled)
    }

    /// Match the entire message and return borrowed values in field_names order.
    /// Internal delimiters use their first available occurrence, assigning extra
    /// text to later fields: `hello {} {}` on `hello a b c` gives ["a", "b c"].
    /// The final literal is anchored to the message end. No backtracking is needed.
    pub fn captures<'message>(&self, message: &'message str) -> Option<Vec<&'message str>> {
        if self.field_names.is_empty() {
            return (message == self.literals[0]).then(Vec::new);
        }
        message.strip_prefix(&self.literals[0])?;
        let suffix = self.literals.last().unwrap();
        message.strip_suffix(suffix)?;
        let end = message.len().checked_sub(suffix.len())?;
        let mut start = self.literals[0].len();
        let mut captures = Vec::with_capacity(self.field_names.len());
        for literal in &self.literals[1..self.literals.len() - 1] {
            let next = start + message.get(start..end)?.find(literal)?;
            captures.push(&message[start..next]);
            start = next + literal.len();
        }
        captures.push(message.get(start..end)?);
        Some(captures)
    }
}

#[cfg(test)]
mod matcher_tests {
    use super::*;

    fn check(format: &str, message: &str, names: &[&str], values: &[&str]) {
        let compiled = CompiledTemplate::compile(format).unwrap();
        assert_eq!(compiled.field_names, names);
        let captures = compiled.captures(message).unwrap();
        assert_eq!(captures, values);
        // Reconstruct by concatenating rendered captures, never formatting them again.
        let mut restored = compiled.literals[0].clone();
        for (capture, literal) in captures.iter().zip(&compiled.literals[1..]) {
            restored.push_str(capture);
            restored.push_str(literal);
        }
        assert_eq!(restored, message);
    }

    #[test]
    fn retains_named_positional_and_repeated_argument_names() {
        check(
            "file {file} took {:.2}ms ({file:?})",
            "file cache took 12.30ms (\"cache\")",
            &["file", "0", "file"],
            &["cache", "12.30", "\"cache\""],
        );
        check(
            "{2} / {} / {0} / {}",
            "c / a / a / b",
            &["2", "0", "0", "1"],
            &["c", "a", "a", "b"],
        );
        check(
            "{:.*} / {} / {value:width$.precision$}",
            "1.25 / next /  2.0",
            &["1", "2", "value"],
            &["1.25", "next", " 2.0"],
        );
        check(
            "{value:.*} / {}",
            "1.25 / next",
            &["value", "1"],
            &["1.25", "next"],
        );
    }

    #[test]
    fn merges_adjacent_placeholders_into_one_field() {
        for format in ["{a}{b}", "{}{}{}", "{a:?}{0:04}{b}"] {
            let compiled = CompiledTemplate::compile(format).unwrap();
            assert_eq!(compiled.format, "{}");
            check(format, "all text", &["0"], &["all text"]);
        }
        let compiled = CompiledTemplate::compile("before {a}{b} after {c}{d}!").unwrap();
        assert_eq!(compiled.format, "before {} after {}!");
        check(
            &compiled.format,
            "before ab after cd!",
            &["0", "1"],
            &["ab", "cd"],
        );
        check(
            "{first}-{a}{b}-{last}",
            "one-two-three",
            &["first", "1", "last"],
            &["one", "two", "three"],
        );
        check("{}{} / {}", "ab / c", &["0", "2"], &["ab", "c"]);
    }

    #[test]
    fn ambiguous_splits_are_deterministic_and_suffix_is_anchored() {
        check("hello {} {}", "hello a b c", &["0", "1"], &["a", "b c"]);
        check(
            "{}::{}::{}",
            "a::b::c::d",
            &["0", "1", "2"],
            &["a", "b", "c::d"],
        );
        check("{}END", "aENDbEND", &["0"], &["aENDb"]);
        check("{}aba{}ba", "ababa", &["0", "1"], &["", ""]);
    }

    #[test]
    fn escaped_braces_utf8_multiline_and_empty_values_round_trip() {
        check(
            "{{{name}}} = {}",
            "{é🦀} = \nline two",
            &["name", "0"],
            &["é🦀", "\nline two"],
        );
        check("{{}} {a}{{}}{b}", "{} a{}b", &["a", "b"], &["a", "b"]);
        check(
            "pré {clé} 🦀 {} fin",
            "pré été 🦀 日本 fin",
            &["clé", "0"],
            &["été", "日本"],
        );
        check("{} {}", " ", &["0", "1"], &["", ""]);
        check("{}", "", &["0"], &[""]);
        check("{{literal}}", "{literal}", &[], &[]);
        check("", "", &[], &[]);
    }

    #[test]
    fn mismatches_fail_without_partial_results() {
        for (format, message) in [
            ("hello {}!", "other a!"),
            ("hello {}!", "hello a?"),
            ("{} / {}", "no delimiter"),
            ("abc{}cde", "abcde"),
            ("literal", "literal extra"),
            ("", "nonempty"),
        ] {
            assert!(
                CompiledTemplate::compile(format)
                    .unwrap()
                    .captures(message)
                    .is_none(),
                "{format:?}: {message:?}"
            );
        }
    }

    #[test]
    fn delimiter_occurrences_inside_values_still_allow_lossless_splitting() {
        // Generated messages exercise ambiguity without assuming original argument
        // boundaries can be recovered. Every split must reconstruct the exact text.
        for format in ["prefix {}::{}::{} suffix", "{}🦀{}🦀{}", "{}a{}a{}a"] {
            let compiled = CompiledTemplate::compile(format).unwrap();
            for first in ["", "a", "::", "🦀"] {
                for second in ["", "aaa", "a::b", "日本🦀"] {
                    for third in ["", "a", "::🦀", " suffix"] {
                        let values = [first, second, third];
                        let mut message = compiled.literals[0].clone();
                        for (value, literal) in values.iter().zip(&compiled.literals[1..]) {
                            message.push_str(value);
                            message.push_str(literal);
                        }
                        let captures = compiled.captures(&message).unwrap();
                        let mut restored = compiled.literals[0].clone();
                        for (capture, literal) in captures.iter().zip(&compiled.literals[1..]) {
                            restored.push_str(capture);
                            restored.push_str(literal);
                        }
                        assert_eq!(restored, message);
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_unbalanced_braces_and_overflowing_positions() {
        for format in [
            "{",
            "}",
            "{{}",
            "{name",
            "{outer{inner}}",
            "{99999999999999999999999999999999999}",
        ] {
            assert!(CompiledTemplate::compile(format).is_err(), "{format:?}");
        }
    }

    #[test]
    fn lookup_returns_a_compiled_matcher_with_field_names() {
        // A real callsite confirms the compiled object is stored at registration.
        use crate::subscriber::{LocalRecordingLayer, RecordingLayer};
        use std::sync::Arc;
        use tracing_subscriber::{Registry, layer::SubscriberExt};
        let recorder = Arc::new(RecordingLayer::new());
        tracing::subscriber::with_default(
            Registry::default().with(LocalRecordingLayer(recorder.clone())),
            || {
                tracing::info!("compiled {name} {}", 42, name = "example");
            },
        );
        let sites = recorder.callsites.read();
        let site = sites.values().next().unwrap().read();
        let compiled = site.template.as_ref().unwrap();
        assert_eq!(compiled.field_names, ["name", "0"]);
        assert_eq!(
            compiled.captures("compiled example 42").unwrap(),
            ["example", "42"]
        );
    }
}
