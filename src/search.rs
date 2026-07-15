use fff_search::{
    ConstraintVec, FFFMode, FFFQuery, FilePicker, FilePickerOptions, FileSearchConfig, FuzzyQuery,
    FuzzySearchOptions, GrepMode, GrepSearchOptions, PaginationArgs, QueryParser, SharedFilePicker,
    SharedFrecency,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_LIMIT: usize = 16 * 1024;
const QUERY_LIMIT: usize = 4096;
const ERROR_TEXT_LIMIT: usize = 2048;

#[derive(Clone)]
pub enum SearchSession {
    Available(SharedFilePicker),
    Unavailable(String),
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SearchKind {
    Files,
    Content,
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Plain,
    Regex,
    Fuzzy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    pub kind: SearchKind,
    pub query: String,
    pub mode: Option<SearchMode>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

impl SearchSession {
    pub fn new(cwd: &Path) -> Self {
        let Some(base_path) = cwd.to_str() else {
            return Self::Unavailable("The startup workspace path is not valid UTF-8".into());
        };
        let picker = SharedFilePicker::default();
        let options = FilePickerOptions {
            base_path: base_path.to_owned(),
            mode: FFFMode::Ai,
            watch: true,
            enable_content_indexing: true,
            enable_mmap_cache: false,
            follow_symlinks: false,
            enable_fs_root_scanning: false,
            enable_home_dir_scanning: false,
            ..Default::default()
        };
        match FilePicker::new_with_shared_state(picker.clone(), SharedFrecency::default(), options)
        {
            Ok(()) => Self::Available(picker),
            Err(error) => Self::Unavailable(error.to_string()),
        }
    }

    #[cfg(test)]
    pub fn unavailable() -> Self {
        Self::Unavailable("search disabled in this test".into())
    }

    pub async fn execute(&self, args: SearchArgs) -> String {
        if args.query.is_empty() {
            return error("invalid_arguments", "query must not be empty", false);
        }
        if args.query.chars().count() > QUERY_LIMIT {
            return error(
                "invalid_arguments",
                format!("query must not exceed {QUERY_LIMIT} characters"),
                false,
            );
        }
        if !(1..=100).contains(&args.limit) {
            return error(
                "invalid_arguments",
                "limit must be from 1 through 100",
                false,
            );
        }
        let Self::Available(picker) = self else {
            let Self::Unavailable(reason) = self else {
                unreachable!()
            };
            return error("search_unavailable", reason, false);
        };
        let picker = picker.clone();
        match tokio::task::spawn_blocking(move || run_search(picker, args)).await {
            Ok(result) => result,
            Err(error_value) => error("search_failed", error_value, false),
        }
    }
}

fn run_search(picker: SharedFilePicker, args: SearchArgs) -> String {
    let ready = match args.kind {
        SearchKind::Files => picker.wait_for_scan(READY_TIMEOUT),
        SearchKind::Content => picker.wait_for_indexing_complete(READY_TIMEOUT),
    };
    if !ready {
        return error(
            "index_not_ready",
            "The workspace index is still building",
            true,
        );
    }
    let guard = match picker.read() {
        Ok(guard) => guard,
        Err(error_value) => return error("search_failed", error_value, false),
    };
    let Some(file_picker) = guard.as_ref() else {
        return error(
            "search_unavailable",
            "The workspace index is unavailable",
            false,
        );
    };
    match args.kind {
        SearchKind::Files => search_files(file_picker, &args.query, args.limit),
        SearchKind::Content => search_content(
            file_picker,
            &args.query,
            args.mode.unwrap_or(SearchMode::Plain),
            args.limit,
        ),
    }
}

fn search_files(file_picker: &FilePicker, query: &str, limit: usize) -> String {
    let parsed = QueryParser::new(FileSearchConfig).parse(query);
    let result = file_picker.fuzzy_search(
        &parsed,
        None,
        FuzzySearchOptions {
            max_threads: 0,
            pagination: PaginationArgs {
                offset: 0,
                limit: limit + 1,
            },
            ..Default::default()
        },
    );
    let truncated = result.total_matched > limit;
    let matches = result
        .items
        .into_iter()
        .take(limit)
        .map(|file| json!({"path": file.relative_path(file_picker)}))
        .collect();
    bounded_result(
        json!({
            "ok": true,
            "kind": "files",
            "returned": 0,
            "total_matched": result.total_matched,
            "matches": []
        }),
        matches,
        truncated,
    )
}

fn search_content(file_picker: &FilePicker, query: &str, mode: SearchMode, limit: usize) -> String {
    let parsed = FFFQuery {
        raw_query: query,
        constraints: ConstraintVec::new(),
        fuzzy_query: FuzzyQuery::Text(query),
        location: None,
    };
    let grep_mode = match mode {
        SearchMode::Plain => GrepMode::PlainText,
        SearchMode::Regex => GrepMode::Regex,
        SearchMode::Fuzzy => GrepMode::Fuzzy,
    };
    let result = file_picker.grep(
        &parsed,
        &GrepSearchOptions {
            max_matches_per_file: limit + 1,
            page_limit: limit + 1,
            mode: grep_mode,
            time_budget_ms: 3_000,
            before_context: 0,
            after_context: 0,
            classify_definitions: false,
            trim_whitespace: false,
            ..Default::default()
        },
    );
    if let Some(regex_error) = result.regex_fallback_error {
        return error("invalid_regex", regex_error, false);
    }
    let mut truncated = result.matches.len() > limit || result.next_file_offset != 0;
    let mut matches = Vec::with_capacity(result.matches.len().min(limit));
    for matched in result.matches.into_iter().take(limit) {
        let file = result.files[matched.file_index];
        let ranges: Vec<_> = matched.match_byte_offsets.into_iter().collect();
        matches.push(json!({
            "path": file.relative_path(file_picker),
            "line": matched.line_number,
            "column_bytes": matched.col,
            "snippet": matched.line_content,
            "match_ranges_bytes": ranges,
        }));
    }
    if matches.len() == limit && result.next_file_offset != 0 {
        truncated = true;
    }
    bounded_result(
        json!({
            "ok": true,
            "kind": "content",
            "returned": 0,
            "files_searched": result.total_files_searched,
            "eligible_files": result.filtered_file_count,
            "matches": []
        }),
        matches,
        truncated,
    )
}

fn bounded_result(mut envelope: Value, matches: Vec<Value>, mut truncated: bool) -> String {
    envelope["truncated"] = json!(truncated);
    for matched in matches {
        envelope["matches"].as_array_mut().unwrap().push(matched);
        envelope["returned"] = json!(envelope["matches"].as_array().unwrap().len());
        envelope["truncated"] = json!(truncated);
        if serde_json::to_vec(&envelope).is_ok_and(|bytes| bytes.len() > OUTPUT_LIMIT) {
            envelope["matches"].as_array_mut().unwrap().pop();
            truncated = true;
            break;
        }
    }
    envelope["returned"] = json!(envelope["matches"].as_array().unwrap().len());
    if truncated {
        envelope["truncated"] = json!(true);
    }
    envelope.to_string()
}

fn error(code: &str, message: impl ToString, retryable: bool) -> String {
    let mut message = message.to_string();
    if message.len() > ERROR_TEXT_LIMIT {
        let mut end = ERROR_TEXT_LIMIT;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    let hint = match code {
        "invalid_arguments" => "Check kind, query, mode, and limit against the tool schema.",
        "invalid_regex" => "Fix the regex syntax or retry with plain mode.",
        "index_not_ready" => "Retry the same search after a short delay.",
        "search_unavailable" => "Use another available tool to inspect the workspace.",
        "search_failed" => "Retry with a simpler query or use another available tool.",
        _ => "Check the error and adjust the search request.",
    };
    json!({
        "ok": false,
        "error_code": code,
        "error": message,
        "hint": hint,
        "retryable": retryable
    })
    .to_string()
}

pub fn invalid_arguments(message: impl ToString) -> String {
    error("invalid_arguments", message, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn session() -> (tempfile::TempDir, SearchSession) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/authentication_handler.rs"),
            "const HANDSHAKE: &str = \"NEBULA_HANDSHAKE\";\nfn parse_http_header() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(dir.path().join("ignored")).unwrap();
        std::fs::write(dir.path().join("ignored/decoy.rs"), "NEBULA_HANDSHAKE\n").unwrap();
        let search = SearchSession::new(dir.path());
        (dir, search)
    }

    fn args(kind: SearchKind, query: &str, mode: Option<SearchMode>) -> SearchArgs {
        SearchArgs {
            kind,
            query: query.into(),
            mode,
            limit: 20,
        }
    }

    #[tokio::test]
    async fn searches_fuzzy_files_and_ignored_plain_content() {
        let (_dir, search) = session().await;
        let files: Value = serde_json::from_str(
            &search
                .execute(args(SearchKind::Files, "authentcation handlr", None))
                .await,
        )
        .unwrap();
        assert_eq!(files["matches"][0]["path"], "src/authentication_handler.rs");

        let content: Value = serde_json::from_str(
            &search
                .execute(args(SearchKind::Content, "NEBULA_HANDSHAKE", None))
                .await,
        )
        .unwrap();
        assert_eq!(content["matches"].as_array().unwrap().len(), 1);
        assert_eq!(
            content["matches"][0]["path"],
            "src/authentication_handler.rs"
        );
    }

    #[tokio::test]
    async fn supports_regex_and_rejects_invalid_regex() {
        let (_dir, search) = session().await;
        let valid: Value = serde_json::from_str(
            &search
                .execute(args(
                    SearchKind::Content,
                    "parse_[a-z]+_header",
                    Some(SearchMode::Regex),
                ))
                .await,
        )
        .unwrap();
        assert_eq!(valid["matches"][0]["line"], 2);

        let invalid: Value = serde_json::from_str(
            &search
                .execute(args(SearchKind::Content, "[", Some(SearchMode::Regex)))
                .await,
        )
        .unwrap();
        assert_eq!(invalid["ok"], false);
        assert_eq!(invalid["error_code"], "invalid_regex");
    }

    #[tokio::test]
    async fn file_search_ignores_content_mode() {
        let (_dir, search) = session().await;
        let files: Value = serde_json::from_str(
            &search
                .execute(args(
                    SearchKind::Files,
                    "authentcation handlr",
                    Some(SearchMode::Plain),
                ))
                .await,
        )
        .unwrap();
        assert_eq!(files["matches"][0]["path"], "src/authentication_handler.rs");
    }

    #[tokio::test]
    async fn complete_empty_results_report_not_truncated() {
        let (_dir, search) = session().await;
        for result in [
            search
                .execute(args(SearchKind::Files, "definitely_missing_path", None))
                .await,
            search
                .execute(args(
                    SearchKind::Content,
                    "definitely_missing_content",
                    None,
                ))
                .await,
        ] {
            let parsed: Value = serde_json::from_str(&result).unwrap();
            assert!(parsed["matches"].as_array().unwrap().is_empty());
            assert_eq!(parsed["truncated"], false);
        }
    }

    #[test]
    fn errors_are_bounded_valid_json() {
        let output = invalid_arguments("\n".repeat(100_000));
        assert!(output.len() <= OUTPUT_LIMIT);
        let parsed: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["error_code"], "invalid_arguments");
        assert!(parsed["hint"].as_str().unwrap().contains("schema"));
        assert_eq!(parsed["retryable"], false);
    }

    #[test]
    fn output_budget_is_hard() {
        let matches = (0..100)
            .map(|index| json!({"path": format!("{index}-{}", "x".repeat(1024))}))
            .collect();
        let output = bounded_result(
            json!({"ok":true,"kind":"files","returned":0,"matches":[]}),
            matches,
            false,
        );
        assert!(output.len() <= OUTPUT_LIMIT);
        let parsed: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["truncated"], true);
    }
}
