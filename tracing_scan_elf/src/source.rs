//! Source template parsing, shared file caches, and conservative wrapper recovery.
//! The framework comes from compiled metadata; source paths may use aliases.

use super::{Callsite, Expressions, LookupError, Result, SPAN_TEMPLATE, Template};
use proc_macro2::{TokenStream, TokenTree as Token};
use quote::ToTokens;
use rayon::prelude::*;
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};
use syn::{Token as T, parse::Parser as _, punctuated::Punctuated, spanned::Spanned, visit::Visit};

// ---- Rust source and template lookup ---------------------------------------

// Both tracing and log share Rust format arguments and implicit named captures.
fn parse_format_args(
    input: syn::parse::ParseStream,
    fields: BTreeMap<String, String>,
) -> syn::Result<Template> {
    let format = input.parse::<syn::LitStr>()?.value();
    if !input.is_empty() {
        input.parse::<T![,]>()?;
    }
    let expressions = Punctuated::<syn::Expr, T![,]>::parse_terminated(input)?;
    let mut args = argument_map(expressions);
    add_captures(&format, &mut args);
    Ok(Template {
        format,
        args,
        fields,
    })
}

fn argument_map(expressions: impl IntoIterator<Item = syn::Expr>) -> BTreeMap<String, String> {
    let mut args = BTreeMap::new();
    let mut position = 0;
    for expression in expressions {
        if let syn::Expr::Assign(assign) = &expression
            && let syn::Expr::Path(path) = &*assign.left
            && let Some(name) = path.path.get_ident()
        {
            args.insert(name.to_string(), assign.right.to_token_stream().to_string());
        } else {
            args.insert(
                position.to_string(),
                expression.to_token_stream().to_string(),
            );
            position += 1;
        }
    }
    args
}

// Inspect placeholders, skipping escaped braces. Named width/precision (`width$`)
// also capture variables; explicit named arguments override implicit captures.
fn add_captures(format: &str, args: &mut BTreeMap<String, String>) {
    let mut rest = format;
    let mut capture = |name: &str| {
        if syn::parse_str::<syn::Ident>(name).is_ok() {
            args.entry(name.to_owned())
                .or_insert_with(|| name.to_owned());
        }
    };
    while let Some(start) = rest.find('{') {
        rest = &rest[start + 1..];
        if let Some(escaped) = rest.strip_prefix('{') {
            rest = escaped;
            continue;
        }
        let Some(end) = rest.find('}') else {
            break;
        };
        let (argument, spec) = rest[..end].split_once(':').unwrap_or((&rest[..end], ""));
        capture(argument.trim());
        for (at, _) in spec.match_indices('$') {
            let name = spec[..at]
                .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap();
            capture(name.strip_prefix('0').unwrap_or(name)); // Optional zero-padding flag.
        }
        rest = &rest[end + 1..];
    }
}

// Parse expressions rather than splitting text: generics, closures, nested macros,
// raw strings, named format arguments, and tracing's ?/% fields retain their meaning.
pub(super) fn parse_tracing_template(tokens: TokenStream, name: &str) -> syn::Result<Template> {
    if name == "span" || name.ends_with("_span") {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            SPAN_TEMPLATE,
        ));
    }
    (|input: syn::parse::ParseStream| {
        let mut fields = BTreeMap::new();
        let mut level = name == "event";
        let mut message = None;
        while !input.is_empty() {
            if input.peek(syn::LitStr) && !input.peek2(T![=]) {
                return parse_format_args(input, fields);
            }
            if input.peek(syn::Ident) && input.peek2(T![:]) && !input.peek3(T![:]) {
                input.parse::<syn::Ident>()?;
                input.parse::<T![:]>()?;
                input.parse::<syn::Expr>()?;
            } else if level {
                input.parse::<syn::Expr>()?;
                level = false;
            } else if input.peek(syn::token::Brace) {
                let content;
                syn::braced!(content in input);
                message = parse_tracing_fields(&content, &mut fields)?.or(message);
            } else {
                message = parse_tracing_field(input, &mut fields)?.or(message);
            }
            if !input.is_empty() {
                input.parse::<T![,]>()?;
            }
        }
        let mut template =
            message.ok_or_else(|| input.error("no literal format string in macro"))?;
        template.fields = fields;
        Ok(template)
    })
    .parse2(tokens)
}

// Spans and braced event fields share comma handling, including trailing commas.
fn parse_tracing_fields(
    input: syn::parse::ParseStream,
    fields: &mut BTreeMap<String, String>,
) -> syn::Result<Option<Template>> {
    let mut message = None;
    while !input.is_empty() {
        message = parse_tracing_field(input, fields)?.or(message);
        if !input.is_empty() {
            input.parse::<T![,]>()?;
        }
    }
    Ok(message)
}

fn parse_tracing_field(
    input: syn::parse::ParseStream,
    fields: &mut BTreeMap<String, String>,
) -> syn::Result<Option<Template>> {
    let mut value = TokenStream::new();
    if input.peek(T![?]) || input.peek(T![%]) {
        value.extend([input.parse::<Token>()?]);
    }
    let mut key = TokenStream::from(input.parse::<Token>()?);
    while input.peek(T![.]) {
        key.extend([input.parse::<Token>()?, input.parse::<Token>()?]);
    }
    let name = syn::parse2::<syn::LitStr>(key.clone())
        .map(|s| s.value())
        .unwrap_or_else(|_| key.to_string().replace(' ', ""));
    let expression = if input.peek(T![=]) {
        input.parse::<T![=]>()?;
        if input.peek(T![?]) || input.peek(T![%]) {
            value.extend([input.parse::<Token>()?]);
        }
        input.parse::<syn::Expr>()?
    } else {
        // Shorthand fields such as ?user or %item.id use their name as the value.
        syn::parse2::<syn::Expr>(key)?
    };
    let template = if name == "message" {
        match &expression {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) => Some(Template {
                format: s.value(),
                args: BTreeMap::new(),
                fields: BTreeMap::new(),
            }),
            syn::Expr::Macro(m) => Some(parse_tracing_template(
                m.mac.tokens.clone(),
                &m.mac.path.to_token_stream().to_string(),
            )?),
            _ => None,
        }
    } else {
        None
    };
    expression.to_tokens(&mut value);
    fields.insert(name, value.to_string());
    Ok(template)
}

// Log keeps its own field grammar: `key[:capture][=value]` before the semicolon.
fn parse_log_field(
    input: syn::parse::ParseStream,
    fields: &mut BTreeMap<String, String>,
) -> syn::Result<()> {
    let key: Token = input.parse()?;
    let name = syn::parse2::<syn::LitStr>(key.clone().into())
        .map(|s| s.value())
        .unwrap_or_else(|_| key.to_string());
    let mut value = TokenStream::new();
    if input.peek(T![:]) {
        value.extend([input.parse::<Token>()?, input.parse::<Token>()?]);
    }
    let capture = value.to_string();
    let mut value = TokenStream::new();
    if input.peek(T![=]) {
        input.parse::<T![=]>()?;
        input.parse::<syn::Expr>()?.to_tokens(&mut value);
    } else {
        value.extend([key]);
    }
    let value = if capture.is_empty() {
        value.to_string()
    } else {
        format!("{capture} {value}")
    };
    fields.insert(name, value);
    Ok(())
}

// Log has optional logger/target directives, a level in log!(), and key/value
// fields before `;`. Keep this grammar separate from tracing's field syntax.
pub(super) fn parse_log_template(tokens: TokenStream, name: &str) -> syn::Result<Template> {
    (|input: syn::parse::ParseStream| {
        while input.peek(syn::Ident) && input.peek2(T![:]) && !input.peek3(T![:]) {
            // A field such as `user:?` belongs to the key/value list below.
            let directive = input.fork().parse::<syn::Ident>()?.to_string();
            if !matches!(directive.as_str(), "logger" | "target") {
                break;
            }
            input.parse::<syn::Ident>()?;
            input.parse::<T![:]>()?;
            input.parse::<syn::Expr>()?;
            input.parse::<T![,]>()?;
        }
        if name == "log" {
            input.parse::<syn::Expr>()?;
            input.parse::<T![,]>()?;
        }
        let mut fields = BTreeMap::new();
        if !input.peek(syn::LitStr) || input.peek2(T![=]) || input.peek2(T![:]) {
            loop {
                parse_log_field(input, &mut fields)?;
                if input.peek(T![;]) {
                    input.parse::<T![;]>()?;
                    break;
                }
                input.parse::<T![,]>()?;
            }
        }
        parse_format_args(input, fields)
    })
    .parse2(tokens)
}

// Upgrade legacy token-string lists when loading old CSVs or section payloads.
pub(super) fn decode_template(
    format: String,
    args: Expressions,
    fields: Expressions,
) -> Result<Template> {
    let args = match args {
        Expressions::Dictionary(map) => map,
        Expressions::Legacy(list) => argument_map(
            list.iter()
                .map(|s| syn::parse_str(s))
                .collect::<syn::Result<Vec<_>>>()?,
        ),
    };
    let fields = match fields {
        Expressions::Dictionary(map) => map,
        Expressions::Legacy(list) => {
            let mut fields = BTreeMap::new();
            for field in list {
                (|input: syn::parse::ParseStream| {
                    if input.peek2(T![:]) && !input.peek3(T![:]) {
                        parse_log_field(input, &mut fields)
                    } else {
                        parse_tracing_field(input, &mut fields).map(|_| ())
                    }
                })
                .parse_str(&field)?;
            }
            fields
        }
    };
    Ok(Template {
        format,
        args,
        fields,
    })
}

/// Parse source with the same token traversal and template grammar as the CLI.
/// Keys are one-based macro invocation lines. Ambiguous/unsupported invocations
/// remain errors; callers must not select an arbitrary macro on the same line.
/// `is_log` identifies the compiled framework; source qualifiers may be aliases.
pub fn parse_source_templates(
    source: &str,
    is_log: bool,
) -> Result<BTreeMap<u32, Result<Template, LookupError>>, String> {
    let tokens = source.parse::<TokenStream>().map_err(|e| e.to_string())?;
    Ok(templates_from_tokens(source, tokens, is_log))
}

// A scan can reuse its token stream for both frameworks and macro text capture.
fn templates_from_tokens(source: &str, tokens: TokenStream, is_log: bool) -> ParsedTemplates {
    let lines: Vec<_> = source.lines().collect();
    let mut found = BTreeMap::new();
    source_templates(tokens, is_log, &mut found);
    found
        .into_iter()
        .map(|(line, results)| {
            // A single invocation uses the framework established by DWARF (or
            // runtime metadata), even when its path names the other framework.
            // Keep qualifier hints only to separate multiple same-line calls.
            let mut matches = results
                .iter()
                .filter(|result| results.len() == 1 || !result.other_framework);
            let template = match (matches.next(), matches.next()) {
                (Some(result), None) => result.template.clone(),
                _ => Err("ambiguous macros on one line".into()),
            };
            let template = template.map_err(|error| LookupError {
                error,
                line: lines
                    .get(line.saturating_sub(1) as usize)
                    .copied()
                    .unwrap_or_default()
                    .into(),
            });
            (line, template)
        })
        .collect()
}

type ParsedTemplates = BTreeMap<u32, Result<Template, LookupError>>;

// Completed parses contain only owned strings/maps and can cross worker threads.
// Keep compiler tokens and ASTs in SourceFile below: those are thread-bound.
#[derive(Default)]
struct SourceData {
    text: String,
    lines: Vec<std::ops::Range<usize>>,
    tracing_templates: OnceCell<Option<ParsedTemplates>>,
    log_templates: OnceCell<Option<ParsedTemplates>>,
    calls: OnceCell<BTreeMap<u32, Vec<String>>>,
}

// Share source text and lazy parses across lookup stages on their owning thread.
// ASTs are much more expensive than token streams; only wrapper evidence needs one.
#[derive(Default)]
struct SourceFile {
    data: SourceData,
    tokens: OnceCell<Result<TokenStream, String>>,
    ast: OnceCell<Option<syn::File>>,
}

impl SourceFile {
    fn new(text: String) -> Self {
        // Keep byte ranges into owned text, without self-references or pointers.
        let mut start = 0;
        let lines = text
            .split_inclusive('\n')
            .map(|line| {
                let content = match line.strip_suffix('\n') {
                    Some(line) => line.strip_suffix('\r').unwrap_or(line),
                    None => line, // Preserve a bare final CR, as str::lines does.
                };
                let range = start..start + content.len();
                start += line.len();
                range
            })
            .collect();
        Self {
            data: SourceData {
                text,
                lines,
                ..SourceData::default()
            },
            ..Self::default()
        }
    }

    fn line(&self, line: u32) -> &str {
        self.data
            .lines
            .get(line.saturating_sub(1) as usize)
            .map_or("", |range| &self.data.text[range.clone()])
    }

    fn tokens(&self) -> Result<&TokenStream, &String> {
        self.tokens
            .get_or_init(|| {
                self.data
                    .text
                    .parse::<TokenStream>()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
    }

    fn templates(&self, is_log: bool) -> Option<&ParsedTemplates> {
        let templates = if is_log {
            &self.data.log_templates
        } else {
            &self.data.tracing_templates
        };
        templates
            .get_or_init(|| {
                self.tokens()
                    .ok()
                    .map(|tokens| templates_from_tokens(&self.data.text, tokens.clone(), is_log))
            })
            .as_ref()
    }

    fn ast(&self) -> Option<&syn::File> {
        // parse_file also handles shebangs/BOMs, unlike parsing tokens directly.
        self.ast
            .get_or_init(|| syn::parse_file(&self.data.text).ok())
            .as_ref()
    }

    fn calls(&self) -> &BTreeMap<u32, Vec<String>> {
        self.data.calls.get_or_init(|| {
            self.tokens().map_or_else(
                |_| BTreeMap::new(),
                |tokens| macro_calls_from_tokens(&self.data.text, tokens.clone()),
            )
        })
    }
}

type SourceFiles = BTreeMap<PathBuf, Result<Rc<SourceFile>, String>>;

fn read_source(path: &Path) -> Result<SourceFile, String> {
    fs::read_to_string(path)
        .map(SourceFile::new)
        .map_err(|error| error.to_string())
}

fn load_source(files: &mut SourceFiles, path: PathBuf) -> Result<Rc<SourceFile>, String> {
    // Cache read errors too; an unavailable dependency should be tried only once.
    files
        .entry(path)
        .or_insert_with_key(|path| read_source(path).map(Rc::new))
        .clone()
}

fn source_path(site: &Callsite, source_root: Option<&Path>) -> PathBuf {
    source_root.map_or_else(|| site.source.clone(), |root| root.join(&site.file))
}

#[derive(Default)]
struct SourceNeeds {
    tracing: bool,
    log: bool,
}

fn prepare_sources(sites: &[Callsite], source_root: Option<&Path>) -> SourceFiles {
    // One task per file shares tokenization between frameworks and text capture.
    // Plan before filtering log false positives so row/link decisions stay on
    // the caller in their original order, using exactly the existing rules.
    let mut jobs = BTreeMap::<PathBuf, SourceNeeds>::new();
    for site in sites {
        let needs = jobs.entry(source_path(site, source_root)).or_default();
        if site.wrapper_row.is_none() && !site.skip_template() {
            if site.is_log {
                needs.log = true;
            } else {
                needs.tracing = true;
            }
        }
    }
    let prepared: Vec<_> = jobs
        .into_par_iter()
        .map(|(path, needs)| {
            let data = read_source(&path).map(|source| {
                if needs.tracing {
                    source.templates(false);
                }
                if needs.log {
                    source.templates(true);
                }
                source.calls();
                source.data // Tokens/ASTs are dropped on this worker, never sent.
            });
            (path, data)
        })
        .collect();
    // Rc and lazy compiler caches are created only after returning to the caller.
    prepared
        .into_iter()
        .map(|(path, data)| {
            let source = data.map(|data| {
                Rc::new(SourceFile {
                    data,
                    ..SourceFile::default()
                })
            });
            (path, source)
        })
        .collect()
}

#[derive(Clone)]
pub(super) struct SourceTemplate {
    pub(super) template: Result<Template, String>,
    // This is a disambiguation hint, never proof of a macro's compiled framework.
    other_framework: bool,
}

pub(super) fn source_templates(
    tokens: TokenStream,
    is_log: bool,
    found: &mut BTreeMap<u32, Vec<SourceTemplate>>,
) {
    let tokens: Vec<_> = tokens.into_iter().collect();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        i += 1;
        // Token groups end at the matching delimiter, across lines and nested
        // parentheses. Parentheses in strings/comments cannot extend the group.
        if let (Token::Ident(name), Some(Token::Punct(bang)), Some(Token::Group(args))) =
            (token, tokens.get(i), tokens.get(i + 1))
            && bang.as_char() == '!'
        {
            let name = name.to_string();
            if !is_log && (name == "span" || name.ends_with("_span")) {
                i += 2; // Span arguments are source text, never format arguments.
                continue;
            }
            // These wrappers contain calls as source, rather than format args.
            // Keep other macros opaque: nested format!/concat! belongs to its call.
            if matches!(name.as_str(), "once" | "cfg_if" | "cfg_select") {
                let mut nested = BTreeMap::new();
                source_templates(args.stream(), is_log, &mut nested);
                // once! may put the call on a later line; DWARF can name either.
                // Multiple calls stay ambiguous rather than selecting the first.
                if name == "once" && !nested.is_empty() {
                    let results = nested.values().flatten().cloned().collect();
                    nested.insert(token.span().start().line as u32, results);
                }
                for (line, results) in nested {
                    found.entry(line).or_default().extend(results);
                }
                i += 2;
                continue;
            }
            // Qualifications help with same-line calls, but imports can rename
            // either framework (for example, `use bevy_log::{self as log}`).
            let qualifier = i.checked_sub(4).and_then(|j| {
                if matches!((&tokens[j + 1], &tokens[j + 2]), (Token::Punct(a), Token::Punct(b)) if a.as_char() == ':' && b.as_char() == ':') {
                    Some(tokens[j].to_string())
                } else { None }
            });
            if !(is_log && name == "log_enabled") {
                let template = if is_log {
                    parse_log_template(args.stream(), &name)
                } else {
                    parse_tracing_template(args.stream(), &name)
                };
                found
                    .entry(token.span().start().line as u32)
                    .or_default()
                    .push(SourceTemplate {
                        template: template.map_err(|e| e.to_string()),
                        other_framework: matches!(qualifier.as_deref(), Some("log") if !is_log)
                            || matches!(qualifier.as_deref(), Some("tracing") if is_log),
                    });
            }
            i += 2;
            continue;
        }
        if let Token::Group(group) = token {
            source_templates(group.stream(), is_log, found);
        }
    }
}

// ---- Wrapper invocation recovery ------------------------------------------

// A wrapper is identified by a DWARF marker inside a macro_rules definition,
// never by a hard-coded name. The parser below only interprets its invocations;
// it does not expand Rust macros or claim to resolve arbitrary macro aliases.
#[derive(Clone)]
pub(super) struct WrapperDefinition {
    name: String,
    macro_name: String,
    pub(super) passthrough: bool,
}

// Verify the conservative `($($args:tt)+) -> log!($($args)+)` subset. A wrapper
// that rewrites values may look format-like at the callsite but mean something else.
fn forwarded_tokens(tokens: TokenStream, matcher: bool) -> Option<(String, char)> {
    let tokens: Vec<_> = tokens.into_iter().collect();
    let [
        Token::Punct(dollar),
        Token::Group(group),
        Token::Punct(repeat),
    ] = tokens.as_slice()
    else {
        return None;
    };
    if dollar.as_char() != '$' || !matches!(repeat.as_char(), '*' | '+') {
        return None;
    }
    let inner: Vec<_> = group.stream().into_iter().collect();
    let name = if matcher {
        let [
            Token::Punct(dollar),
            Token::Ident(name),
            Token::Punct(colon),
            Token::Ident(fragment),
        ] = inner.as_slice()
        else {
            return None;
        };
        if dollar.as_char() != '$' || colon.as_char() != ':' || fragment != "tt" {
            return None;
        }
        name
    } else {
        let [Token::Punct(dollar), Token::Ident(name)] = inner.as_slice() else {
            return None;
        };
        if dollar.as_char() != '$' {
            return None;
        }
        name
    };
    Some((name.to_string(), repeat.as_char()))
}

pub(super) fn wrapper_definition(file: &syn::File, line: u32) -> Option<WrapperDefinition> {
    struct Definitions {
        line: usize,
        found: Vec<WrapperDefinition>,
    }
    impl<'ast> Visit<'ast> for Definitions {
        fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
            if item.mac.path.is_ident("macro_rules")
                && let Some(name) = &item.ident
                && item.mac.tokens.span().start().line <= self.line
                && self.line <= item.mac.tokens.span().end().line
            {
                fn names(tokens: TokenStream, line: usize, found: &mut Vec<(String, TokenStream)>) {
                    let tokens: Vec<_> = tokens.into_iter().collect();
                    for window in tokens.windows(3) {
                        if let [Token::Ident(name), Token::Punct(bang), Token::Group(args)] = window
                            && bang.as_char() == '!'
                            && name.span().start().line == line
                        {
                            found.push((name.to_string(), args.stream()));
                        }
                    }
                    for token in tokens {
                        if let Token::Group(group) = token {
                            names(group.stream(), line, found);
                        }
                    }
                }
                let mut targets = Vec::new();
                names(item.mac.tokens.clone(), self.line, &mut targets);
                if let [(macro_name, arguments)] = targets.as_slice() {
                    let tokens: Vec<_> = item.mac.tokens.clone().into_iter().collect();
                    let binding = tokens.windows(4).find_map(|window| {
                        let [
                            Token::Group(matcher),
                            Token::Punct(eq),
                            Token::Punct(gt),
                            Token::Group(body),
                        ] = window
                        else {
                            return None;
                        };
                        (eq.as_char() == '='
                            && gt.as_char() == '>'
                            && body.span().start().line <= self.line
                            && self.line <= body.span().end().line)
                            .then(|| forwarded_tokens(matcher.stream(), true))
                            .flatten()
                    });
                    self.found.push(WrapperDefinition {
                        name: name.to_string(),
                        macro_name: macro_name.clone(),
                        passthrough: binding.is_some()
                            && binding == forwarded_tokens(arguments.clone(), false),
                    });
                }
            }
            syn::visit::visit_item_macro(self, item);
        }
    }
    let mut visitor = Definitions {
        line: line as usize,
        found: Vec::new(),
    };
    visitor.visit_file(file);
    match visitor.found.as_slice() {
        [definition] => Some(definition.clone()),
        _ => None,
    }
}

// Use the innermost function/closure containing the DWARF declaration. Descendant
// functions and closures are visited independently only when DWARF identifies them.
fn source_scope(file: &syn::File, line: u32) -> Option<TokenStream> {
    struct Scope {
        line: usize,
        selected: Option<(usize, TokenStream)>,
    }
    impl Scope {
        fn consider(&mut self, start: usize, body: impl ToTokens + Spanned) {
            let end = body.span().end().line;
            if start <= self.line
                && self.line <= end
                && self
                    .selected
                    .as_ref()
                    .is_none_or(|(size, _)| end - start < *size)
            {
                self.selected = Some((end - start, body.to_token_stream()));
            }
        }
    }
    impl<'ast> Visit<'ast> for Scope {
        fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
            self.consider(function.sig.span().start().line, &function.block);
            syn::visit::visit_item_fn(self, function);
        }
        fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
            self.consider(function.sig.span().start().line, &function.block);
            syn::visit::visit_impl_item_fn(self, function);
        }
        fn visit_trait_item_fn(&mut self, function: &'ast syn::TraitItemFn) {
            if let Some(body) = &function.default {
                self.consider(function.sig.span().start().line, body);
            }
            syn::visit::visit_trait_item_fn(self, function);
        }
        fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
            self.consider(closure.span().start().line, &closure.body);
            syn::visit::visit_expr_closure(self, closure);
        }
    }
    let mut visitor = Scope {
        line: line as usize,
        selected: None,
    };
    visitor.visit_file(file);
    visitor.selected.map(|(_, tokens)| tokens)
}

pub(super) fn wrapper_invocations(tokens: TokenStream, name: &str) -> Vec<syn::Macro> {
    fn conditional(attributes: &[syn::Attribute]) -> bool {
        attributes
            .iter()
            .any(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"))
    }
    struct Calls<'a> {
        name: &'a str,
        found: Vec<syn::Macro>,
        shadowed: bool,
        guarded: bool,
    }
    impl<'ast> Visit<'ast> for Calls<'_> {
        fn visit_stmt(&mut self, statement: &'ast syn::Stmt) {
            let guarded = self.guarded;
            syn::visit::visit_stmt(self, statement);
            self.guarded = guarded;
        }
        fn visit_expr(&mut self, expression: &'ast syn::Expr) {
            // Conditional attributes guard their whole subtree; restore state
            // at expression boundaries so sibling calls remain independent.
            let guarded = self.guarded;
            syn::visit::visit_expr(self, expression);
            self.guarded = guarded;
        }
        fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
            self.guarded |=
                attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr");
        }
        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            // Qualified names need import/name resolution before they can be
            // linked safely. Equal final identifiers alone are insufficient.
            if !self.guarded
                && mac.path.segments.len() == 1
                && mac
                    .path
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == self.name)
            {
                self.found.push(mac.clone());
            }
        }
        fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
            // Local macro shadowing makes the definition-to-invocation link ambiguous.
            self.shadowed |= item.ident.as_ref().is_some_and(|name| name == self.name);
        }
        fn visit_item_fn(&mut self, _: &'ast syn::ItemFn) {}
        fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
        fn visit_stmt_macro(&mut self, statement: &'ast syn::StmtMacro) {
            if !conditional(&statement.attrs) {
                self.visit_macro(&statement.mac);
            }
        }
        fn visit_expr_macro(&mut self, expression: &'ast syn::ExprMacro) {
            if !conditional(&expression.attrs) {
                self.visit_macro(&expression.mac);
            }
        }
        fn visit_expr_block(&mut self, expression: &'ast syn::ExprBlock) {
            if !conditional(&expression.attrs) {
                syn::visit::visit_expr_block(self, expression);
            }
        }
    }
    let Ok(expression) = syn::parse2::<syn::Expr>(tokens) else {
        return Vec::new();
    };
    let mut visitor = Calls {
        name,
        found: Vec::new(),
        shadowed: false,
        guarded: false,
    };
    visitor.visit_expr(&expression);
    if visitor.shadowed {
        Vec::new()
    } else {
        visitor.found
    }
}

fn recover_wrapper_invocations(
    sites: &mut Vec<Callsite>,
    source_root: Option<&Path>,
    files: &mut SourceFiles,
) {
    let mut load = |file: &str| {
        let path = source_root.map_or_else(|| PathBuf::from(file), |root| root.join(file));
        load_source(files, path).ok()
    };
    let mut recovered = BTreeMap::new();
    let mut tracing_templates = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for (parent, site) in sites.iter().enumerate() {
        if site.wrapper_row.is_some() || site.scopes.is_empty() || site.skip_template() {
            continue;
        }
        let (definition_file, definition_line) = site
            .declaration
            .as_ref()
            .map(|decl| (decl.file.as_str(), decl.line))
            .unwrap_or((&site.file, site.line));
        let Some(file) = load(definition_file) else {
            continue;
        };
        // Every macro_rules definition contains this literal identifier, even
        // with comments/raw identifiers. Avoid parsing an AST when none exists.
        if !file.data.text.contains("macro_rules") {
            continue;
        }
        let Some(definition) = file
            .ast()
            .and_then(|ast| wrapper_definition(ast, definition_line))
        else {
            continue;
        };
        for scope in &site.scopes {
            let Some(file) = load(&scope.file) else {
                continue;
            };
            let Some(tokens) = file.ast().and_then(|ast| source_scope(ast, scope.line)) else {
                continue;
            };
            for invocation in wrapper_invocations(tokens, &definition.name) {
                let line = invocation
                    .path
                    .segments
                    .last()
                    .unwrap()
                    .ident
                    .span()
                    .start()
                    .line as u32;
                if !seen.insert((
                    scope.file.clone(),
                    line,
                    invocation.span().start().column,
                    parent,
                )) {
                    continue;
                }
                let template = if !definition.passthrough {
                    Err(syn::Error::new_spanned(
                        &invocation,
                        "wrapper does not transparently forward its format arguments",
                    ))
                } else if site.is_log {
                    parse_log_template(invocation.tokens.clone(), &definition.macro_name)
                } else {
                    parse_tracing_template(invocation.tokens.clone(), &definition.macro_name)
                };
                let template = template.map_err(|error| LookupError {
                    line: file.line(line).into(),
                    error: error.to_string(),
                });
                if !site.is_log {
                    // Tracing already supplies the actual invocation's identity.
                    // Use declaration evidence only to parse that invocation's
                    // template; don't manufacture a second metadata location.
                    if line == site.line && Path::new(&scope.file) == site.source {
                        tracing_templates.insert(parent, template);
                    }
                    continue;
                }
                let mut child = Callsite::loaded(
                    scope.file.clone(),
                    line,
                    site.is_log,
                    site.module_path.clone(),
                    template,
                );
                child.wrapper_row = Some(parent);
                let key = (
                    child.file.clone(),
                    line,
                    child.is_log,
                    child.module_path.clone(),
                );
                match recovered.entry(key) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(child);
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        let previous = entry.get_mut();
                        if previous.wrapper_row != Some(parent) {
                            previous.wrapper_row = None;
                        }
                        previous.template = Err(LookupError {
                            line: String::new(),
                            error: "ambiguous wrapper invocations at one location".into(),
                        });
                    }
                }
            }
        }
    }
    for (row, template) in tracing_templates {
        sites[row].template = template;
    }
    for (_, child) in recovered {
        if let Some((row, existing)) = sites.iter_mut().enumerate().find(|(_, site)| {
            site.file == child.file
                && site.line == child.line
                && site.is_log == child.is_log
                && site.module_path == child.module_path
        }) {
            if child.wrapper_row == Some(row) {
                existing.template = Err(LookupError {
                    line: String::new(),
                    error: "wrapper and invocation share one source location".into(),
                });
            } else if existing.wrapper_row.is_none() {
                existing.wrapper_row = child.wrapper_row;
            }
        } else {
            sites.push(child);
        }
    }
}

// Capture invocation text without interpreting its arguments as a template.
#[cfg(test)]
pub(super) fn macro_calls(source: &str) -> Result<BTreeMap<u32, Vec<String>>, String> {
    let tokens = source.parse::<TokenStream>().map_err(|e| e.to_string())?;
    Ok(macro_calls_from_tokens(source, tokens))
}

fn macro_calls_from_tokens(source: &str, tokens: TokenStream) -> BTreeMap<u32, Vec<String>> {
    fn walk(
        tokens: TokenStream,
        source: &str,
        found: &mut BTreeMap<u32, Vec<String>>,
        parents: &mut Vec<u32>,
    ) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        let mut i = 0;
        while i < tokens.len() {
            if let (Token::Ident(name), Some(Token::Punct(bang)), Some(Token::Group(args))) =
                (&tokens[i], tokens.get(i + 1), tokens.get(i + 2))
                && bang.as_char() == '!'
            {
                let mut start = i;
                let colon =
                    |index: usize| matches!(&tokens[index], Token::Punct(p) if p.as_char() == ':');
                while start >= 3
                    && colon(start - 1)
                    && colon(start - 2)
                    && matches!(&tokens[start - 3], Token::Ident(_))
                {
                    start -= 3;
                }
                if start >= 2 && colon(start - 1) && colon(start - 2) {
                    start -= 2;
                }
                if start > 0 && matches!(&tokens[start - 1], Token::Punct(p) if p.as_char() == '$')
                {
                    start -= 1; // Preserve $crate:: paths inside macro definitions.
                }
                let first = tokens[start].span();
                let lines = [first.start().line as u32, name.span().start().line as u32];
                if let Some(call) =
                    source.get(first.byte_range().start..args.span().byte_range().end)
                {
                    for (index, line) in lines.into_iter().enumerate() {
                        if !parents.contains(&line) && (index == 0 || line != lines[0]) {
                            found.entry(line).or_default().push(call.to_owned());
                        }
                    }
                }
                // A nested format_args! on the same line belongs to its outer call.
                let depth = parents.len();
                parents.extend(lines);
                walk(args.stream(), source, found, parents);
                parents.truncate(depth);
                i += 3;
                continue;
            }
            if let Token::Group(group) = &tokens[i] {
                walk(group.stream(), source, found, parents);
            }
            i += 1;
        }
    }
    let mut found = BTreeMap::new();
    walk(tokens, source, &mut found, &mut Vec::new());
    found
}

/// Resolve templates, sharing source reads and token streams across frameworks.
/// File reads/template parsing/text capture use the current Rayon pool; row
/// filtering and wrapper recovery remain ordered on the calling thread.
/// Resolve relative canonical filenames beneath `source_root`; absolute names stay intact.
/// Drop log marker candidates that source confirms are not logging macros
/// (ordinary `lvl` locals or `log_enabled!` checks). Unavailable source stays Err.
pub fn lookup_templates(sites: &mut Vec<Callsite>, source_root: Option<&Path>) {
    let mut files = prepare_sources(sites, source_root);
    let mut remap = vec![None; sites.len()];
    let (mut old_row, mut next_row) = (0, 0);
    sites.retain_mut(|site| {
        let row = old_row;
        old_row += 1;
        if site.wrapper_row.is_some() || site.skip_template() {
            remap[row] = Some(next_row);
            next_row += 1;
            return true;
        }
        let path = source_path(site, source_root);
        let source = load_source(&mut files, path);
        let templates = source
            .as_ref()
            .ok()
            .and_then(|file| file.templates(site.is_log));
        // Two macros on one line cannot be distinguished by a file + line key.
        let result = match templates.and_then(|templates| templates.get(&site.line)) {
            Some(result) => result.clone().map_err(|error| error.error),
            None if site.is_log && templates.is_some() => return false,
            // A canonical tracing invocation may use a wrapper that was already
            // resolved from declaration evidence on the previous lookup.
            None if !site.is_log && site.template.is_ok() => {
                site.template.clone().map_err(|e| e.error)
            }
            None => Err(source
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "no macro with a format string at DWARF location".into())),
        };
        site.template = result.map_err(|error| LookupError {
            error,
            line: source
                .as_ref()
                .ok()
                .map(|file| file.line(site.line))
                .unwrap_or_default()
                .into(),
        });
        remap[row] = Some(next_row);
        next_row += 1;
        true
    });
    for site in sites.iter_mut() {
        site.wrapper_row = site
            .wrapper_row
            .and_then(|row| remap.get(row).copied().flatten());
    }
    recover_wrapper_invocations(sites, source_root, &mut files);
    // Source capture is independent of eligibility and runs for recovered rows too.
    for site in sites {
        let path = source_path(site, source_root);
        if let Ok(source) = load_source(&mut files, path) {
            site.source_line = source.line(site.line).to_owned();
            site.macro_call = match source.calls().get(&site.line).map(Vec::as_slice) {
                Some([call]) => call.clone(),
                _ => site.source_line.clone(), // Missing/ambiguous macro: retain the line.
            };
            if let Err(error) = &mut site.template {
                error.line = site.source_line.clone();
            }
        }
        site.scopes.clear();
        site.declaration = None;
    }
}
