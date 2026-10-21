//! Optional message templates embedded by the scanner's post-link step.

use std::{collections::HashMap, sync::LazyLock};

use tracing_scan_elf::{Callsite, Template, load_embedded_section};

// Nested maps allow borrowed filename lookups without allocating a tuple key.
type TemplateIndex = HashMap<String, HashMap<u32, Option<Template>>>;

static TEMPLATES: LazyLock<TemplateIndex> = LazyLock::new(|| {
    // The scanner reads the executable's section table, including sections added
    // after linking. Missing/unreadable metadata leaves recording usable.
    index_templates(load_embedded_section().unwrap_or_default())
});

/// Find an embedded template by its exact scanner filename and source line.
///
/// Loads the executable's `.tracing_scan_elf` section into memory once, on first
/// use. Returns `None` when loading fails, the location is absent or unresolved,
/// or multiple callsites at that location have different templates. Filenames
/// are not normalized: use the same path recorded in the callsite metadata.
pub fn lookup(file: &str, line: u32) -> Option<&'static Template> {
    TEMPLATES.get(file)?.get(&line)?.as_ref()
}

fn index_templates(sites: Vec<Callsite>) -> TemplateIndex {
    let mut index = TemplateIndex::new();
    for site in sites {
        let template = site.template.ok();
        index
            .entry(site.file)
            .or_default()
            .entry(site.line)
            .and_modify(|previous| {
                // Modules and logging frameworks can share a file/line. Never
                // select an arbitrary template when those rows disagree.
                if *previous != template {
                    *previous = None;
                }
            })
            .or_insert(template);
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    // Use the public artifact reader because Callsite keeps extraction evidence
    // private; these legacy CSV rows also exercise the supported reader path.
    fn sites(rows: &str) -> Vec<Callsite> {
        let csv = format!("file,line,result,format,args,fields,source,error\n{rows}");
        tracing_scan_elf::read_csv(csv.as_bytes()).unwrap()
    }

    #[test]
    fn indexes_exact_file_and_line_and_keeps_template_data() {
        let index = index_templates(sites(concat!(
            // Artifact dictionaries store expressions explicitly.
            r#"src/a.rs,10,Ok,value {answer},"{""answer"":""value + 1""}","{""value"":""?value""}",,"#,
            "\n",
            "src/a.rs,11,Ok,next,{},{},,\n",
            "src/b.rs,10,Ok,other,{},{},,\n",
            "src/a.rs,12,Err,,,,source,unsupported\n",
        )));
        let template = index["src/a.rs"][&10].as_ref().unwrap();
        assert_eq!(template.format, "value {answer}");
        assert_eq!(template.args["answer"], "value + 1");
        assert_eq!(template.fields["value"], "?value");
        assert_eq!(index["src/a.rs"][&11].as_ref().unwrap().format, "next");
        assert_eq!(index["src/b.rs"][&10].as_ref().unwrap().format, "other");
        assert!(index["src/a.rs"][&12].is_none());
        assert!(!index["src/a.rs"].contains_key(&99));
        assert!(!index.contains_key("a.rs"));
    }

    #[test]
    fn duplicate_locations_require_identical_successful_templates() {
        let index = index_templates(sites(concat!(
            "a.rs,1,Ok,same,{},{},,\n",
            "a.rs,1,Ok,same,{},{},,\n",
            "a.rs,2,Ok,first,{},{},,\n",
            "a.rs,2,Ok,second,{},{},,\n",
            "a.rs,2,Ok,first,{},{},,\n",
            "a.rs,3,Err,,,,source,unsupported\n",
            "a.rs,3,Ok,resolved,{},{},,\n",
            "a.rs,4,Ok,resolved,{},{},,\n",
            "a.rs,4,Err,,,,source,unsupported\n",
        )));
        assert_eq!(index["a.rs"][&1].as_ref().unwrap().format, "same");
        for line in [2, 3, 4] {
            assert!(index["a.rs"][&line].is_none());
        }
    }

    #[test]
    fn absent_templates_are_optional() {
        assert!(index_templates(Vec::new()).is_empty());
        // This remains valid whether or not the test executable has a section.
        assert!(lookup("__missing_template_file__", u32::MAX).is_none());
    }
}
