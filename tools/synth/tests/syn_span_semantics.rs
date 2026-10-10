//! Pins the `syn`/`proc-macro2` span behavior that structural outlines rely on.
//! Spans are converted to plain data on the parsing thread, never kept.

use proc_macro2::Span;
use syn::spanned::Spanned;

const SOURCE: &str = "\
use std::fmt;

/// Documented struct.
/// Second doc line.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Alpha {
    pub a: u32,
}

impl Alpha {
    /// Doc on method.
    #[inline]
    pub fn get(&self) -> u32 {
        self.a
    }
}

fn beta() {}
";

#[derive(Debug, PartialEq, Eq)]
struct ItemSpan {
    kind: &'static str,
    start_line: usize,
    end_line: usize,
    start_byte: usize,
}

fn plain(kind: &'static str, span: Span) -> ItemSpan {
    ItemSpan {
        kind,
        start_line: span.start().line,
        end_line: span.end().line,
        start_byte: span.byte_range().start,
    }
}

/// Parses on a short-lived scoped thread so any thread-local source map dies
/// with it, returning only plain data.
fn parse_on_scoped_thread(src: &str) -> Vec<ItemSpan> {
    std::thread::scope(|s| {
        s.spawn(|| {
            let file = syn::parse_file(src).expect("fixture parses");
            file.items
                .iter()
                .map(|item| {
                    let kind = match item {
                        syn::Item::Use(_) => "use",
                        syn::Item::Struct(_) => "struct",
                        syn::Item::Impl(_) => "impl",
                        syn::Item::Fn(_) => "fn",
                        _ => "other",
                    };
                    plain(kind, item.span())
                })
                .collect()
        })
        .join()
        .expect("parser thread did not panic")
    })
}

#[test]
fn lines_are_one_based_and_attributes_and_docs_are_inside_item_spans() {
    let spans = parse_on_scoped_thread(SOURCE);
    let line = |kind: &str| spans.iter().find(|s| s.kind == kind).unwrap();

    let use_item = line("use");
    assert_eq!((use_item.start_line, use_item.end_line), (1, 1));

    // Doc comments (lines 3-4) and attributes (5-6) belong to the struct span.
    let st = line("struct");
    assert_eq!((st.start_line, st.end_line), (3, 9));

    let im = line("impl");
    assert_eq!((im.start_line, im.end_line), (11, 17));

    let f = line("fn");
    assert_eq!((f.start_line, f.end_line), (19, 19));
}

#[test]
fn inner_item_spans_include_their_docs_and_attributes() {
    let (start, end) = std::thread::scope(|s| {
        s.spawn(|| {
            let file = syn::parse_file(SOURCE).unwrap();
            let syn::Item::Impl(imp) = &file.items[2] else {
                panic!("expected impl")
            };
            let sp = imp.items[0].span();
            (sp.start().line, sp.end().line)
        })
        .join()
        .unwrap()
    });
    assert_eq!((start, end), (12, 16));
}

#[test]
fn repeated_scoped_parses_do_not_grow_the_callers_source_map() {
    // proc-macro2's fallback keeps a per-thread source map whose offsets only
    // grow while a thread lives. A fresh thread must always start at offset 0.
    let baseline = parse_on_scoped_thread(SOURCE);
    assert_eq!(baseline[0].start_byte, 0);
    for _ in 0..50 {
        assert_eq!(parse_on_scoped_thread(SOURCE), baseline);
    }
    // The calling thread's map is untouched: a parse here also starts at 0.
    let file = syn::parse_file(SOURCE).unwrap();
    assert_eq!(file.items[0].span().byte_range().start, 0);
}
