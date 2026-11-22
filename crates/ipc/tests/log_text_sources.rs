use proc_macro2::{TokenStream, TokenTree};
use regex::Regex;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;
use syn::visit::{self, Visit};

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("test")
            || (attr.path().is_ident("cfg")
                && attr
                    .parse_args::<syn::Ident>()
                    .is_ok_and(|arg| arg == "test"))
    })
}

const TITLE_TYPES: &[&str] = &[
    "WindowInfo",
    "LayoutApplyTimeoutCandidate",
    "LayoutApplyTimeoutReport",
    "TabLabel",
    "OverviewCard",
    "WindowInspection",
    "ElevationBlockedRecord",
    "ElevationBlockedWindow",
    "FixtureIdentity",
    "OverviewModel",
    "WorkspaceStateRecord",
    "WindowRule",
    "CompiledWindowRule",
    "StateSnapshot",
];

#[derive(Default)]
struct Source {
    calls: Vec<syn::Macro>,
    title_values: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Source {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !test_only(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if !test_only(&item.attrs) {
            visit::visit_item_fn(self, item);
        }
    }

    fn visit_pat_type(&mut self, pat: &'ast syn::PatType) {
        struct CarriesTitle(bool);
        impl<'ast> Visit<'ast> for CarriesTitle {
            fn visit_type_path(&mut self, typ: &'ast syn::TypePath) {
                self.0 |= typ
                    .path
                    .segments
                    .iter()
                    .any(|seg| TITLE_TYPES.contains(&seg.ident.to_string().as_str()));
                visit::visit_type_path(self, typ);
            }
        }
        let mut carries_title = CarriesTitle(false);
        carries_title.visit_type(&pat.ty);
        if let syn::Pat::Ident(ident) = pat.pat.as_ref() {
            if carries_title.0 {
                self.title_values.insert(ident.ident.to_string());
            }
        }
        visit::visit_pat_type(self, pat);
    }

    fn visit_macro(&mut self, call: &'ast syn::Macro) {
        let name = call.path.segments.last().unwrap().ident.to_string();
        if matches!(
            name.as_str(),
            "event"
                | "span"
                | "trace"
                | "debug"
                | "info"
                | "warn"
                | "error"
                | "trace_span"
                | "debug_span"
                | "info_span"
                | "warn_span"
                | "error_span"
        ) {
            self.calls.push(call.clone());
        } else {
            // Rust macros such as tokio::select! can contain tracing invocations.
            self.visit_macro_tokens(call.tokens.clone());
        }
    }
}

impl Source {
    fn visit_macro_tokens(&mut self, stream: TokenStream) {
        let tokens: Vec<_> = stream.into_iter().collect();
        let mut index = 0;
        while index < tokens.len() {
            if let [TokenTree::Ident(_), bang, TokenTree::Group(_), ..] = &tokens[index..] {
                if punct(bang, '!') {
                    let invocation: TokenStream =
                        tokens[index..index + 3].iter().cloned().collect();
                    if let Ok(call) = syn::parse2::<syn::Macro>(invocation) {
                        self.visit_macro(&call);
                        index += 3;
                        continue;
                    }
                }
            }
            if let TokenTree::Group(group) = &tokens[index] {
                self.visit_macro_tokens(group.stream());
            }
            index += 1;
        }
    }
}

fn ident(token: &TokenTree) -> Option<String> {
    match token {
        TokenTree::Ident(ident) => Some(ident.to_string()),
        _ => None,
    }
}

fn punct(token: &TokenTree, expected: char) -> bool {
    matches!(token, TokenTree::Punct(p) if p.as_char() == expected)
}

fn wrapped(tokens: &[TokenTree]) -> Option<usize> {
    if let [crate_name, a, b, inner @ ..] = tokens {
        if ident(crate_name).as_deref() == Some("leopardwm_ipc") && punct(a, ':') && punct(b, ':') {
            return wrapped(inner).map(|width| width + 3);
        }
    }
    let [module, a, b, kind, TokenTree::Group(group), ..] = tokens else {
        return None;
    };
    (ident(module).as_deref() == Some("log_text")
        && punct(a, ':')
        && punct(b, ':')
        && matches!(
            ident(kind).as_deref(),
            Some("title" | "path" | "text" | "name")
        )
        && group.delimiter() == proc_macro2::Delimiter::Parenthesis)
        .then_some(5)
}

static FOREIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"(?:^|_)(?:titles?|paths?|exe|executable|class|pattern|regex|payload|url)(?:_|$)|^(?:display|file_name|device_name|monitor_device_name|to_string_lossy|preferred_pipe_name|get_window_info|lookup_window_info|received_command)$"
).unwrap()
});
static ERROR_CONTEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"(?i)config|auto-start|save settings|save workspace|debounced save|reload failed|parse saved state|IPC: invalid JSON|pipe server|client handler|Settings window error|ShellExecuteW failed to open|Watchdog fatal error|Failed to render (?:blocking )?toast"
).unwrap()
});
static CAPTURE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([[:alpha:]_][[:alnum:]_]*)(:[^}]*)?\}").unwrap());

fn inspect(
    tokens: &[TokenTree],
    debug: bool,
    error_context: bool,
    title_values: &BTreeSet<String>,
    suspect: &mut BTreeSet<String>,
) {
    let mut index = 0;
    while index < tokens.len() {
        if let Some(width) = wrapped(&tokens[index..]) {
            index += width;
            continue;
        }
        let flag = |name: &str, debug: bool| {
            FOREIGN.is_match(name)
                || (error_context && matches!(name, "e" | "err" | "error"))
                || (debug
                    && (TITLE_TYPES.contains(&name)
                        || title_values.contains(name)
                        || matches!(
                            name,
                            "win_info"
                                | "window_info"
                                | "info"
                                | "candidate"
                                | "candidates"
                                | "report"
                        )))
        };
        match &tokens[index] {
            TokenTree::Ident(name) => {
                let name = name.to_string();
                let member_access = tokens.get(index + 1).is_some_and(|t| punct(t, '.'));
                if flag(&name, debug && !member_access) {
                    suspect.insert(name);
                }
            }
            TokenTree::Literal(literal) => {
                if let Ok(text) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                    let text = text.value().replace("{{", "");
                    for capture in CAPTURE.captures_iter(&text) {
                        let debug = capture
                            .get(2)
                            .is_some_and(|spec| spec.as_str().ends_with('?'));
                        if flag(&capture[1], debug) {
                            suspect.insert(capture[1].to_owned());
                        }
                    }
                }
            }
            TokenTree::Group(group) => inspect(
                &group.stream().into_iter().collect::<Vec<_>>(),
                debug,
                error_context,
                title_values,
                suspect,
            ),
            TokenTree::Punct(_) => {}
        }
        index += 1;
    }
}

fn suspects(call: &syn::Macro, title_values: &BTreeSet<String>) -> BTreeSet<String> {
    let tokens: Vec<_> = call.tokens.clone().into_iter().collect();
    let rendered = call.tokens.to_string();
    let error_context = tokens.iter().any(|token| match token {
        TokenTree::Literal(literal) => ERROR_CONTEXT.is_match(&literal.to_string()),
        _ => false,
    });
    let mut found = BTreeSet::new();
    for arg in tokens.split(|token| punct(token, ',')) {
        if let [_, eq, sigil, rest @ ..] = arg {
            if punct(eq, '=') && punct(sigil, '%') && wrapped(rest) == Some(rest.len()) {
                continue;
            }
        }
        inspect(
            arg,
            rendered.contains(":?")
                || rendered.contains(":#?")
                || arg.iter().any(|t| punct(t, '?')),
            error_context,
            title_values,
            &mut found,
        );
    }
    found
}

// Match the full call so an added unwrapped argument cannot inherit an exemption.
const ALLOW: &[(&str, &str, &str)] = &[
    (
        "daemon/src/event_handler.rs",
        r#""Failed to apply layout after Edit Config pull: {}" , e"#,
        "Layout apply returns placement diagnostics, not config input or paths.",
    ),
    (
        "daemon/src/main.rs",
        r#""Rejecting tab title override ({} bytes > {} max)" , title . len () , leopardwm_platform_win32 :: dialog :: TAB_TITLE_MAX_BYTES"#,
        "The call emits title.len(), a byte count, not the title.",
    ),
];

fn check(root: &Path, path: &Path, failures: &mut Vec<String>) {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            check(root, &entry.unwrap().path(), failures);
        }
    } else if path.extension().is_some_and(|ext| ext == "rs") {
        let name = path.file_stem().unwrap().to_str().unwrap();
        if name == "tests" || name == "test_support" || name.ends_with("_tests") {
            return;
        }
        let file = syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut source = Source::default();
        source.visit_file(&file);
        for call in source.calls {
            let actual = suspects(&call, &source.title_values);
            if actual.is_empty() {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let rendered = call.tokens.to_string();
            if !ALLOW
                .iter()
                .any(|(file, args, _reason)| relative == *file && rendered == *args)
            {
                failures.push(format!("{}: {actual:?} in {}", path.display(), call.tokens));
            }
        }
    }
}

#[test]
fn foreign_tracing_values_are_escaped_at_the_source() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut failures = Vec::new();
    for name in ["daemon", "platform_win32", "watchdog"] {
        check(crates, &crates.join(name).join("src"), &mut failures);
    }
    assert!(
        failures.is_empty(),
        "unwrapped log text:\n{}",
        failures.join("\n")
    );
}

#[test]
fn guard_detects_unwrapped_positional_inline_and_structured_values() {
    let file = syn::parse_file(
        r#"
        fn real_window(window: &WindowInfo) {
            scope! { tracing::warn!(
                title = %window_title,
                dump = ?window,
                "{rule_pattern}: {} {}",
                log_text::title(safe_title),
                exe_path.display(),
            ); }
            tracing::info!(path = %leopardwm_ipc::log_text::path(exe_path.display()), "good call");
        }
    "#,
    )
    .unwrap();
    let mut source = Source::default();
    source.visit_file(&file);
    assert_eq!(source.calls.len(), 2);
    assert_eq!(
        suspects(&source.calls[0], &source.title_values),
        [
            "display",
            "exe_path",
            "rule_pattern",
            "title",
            "window",
            "window_title"
        ]
        .map(str::to_owned)
        .into()
    );
    assert!(suspects(&source.calls[1], &source.title_values).is_empty());
}
