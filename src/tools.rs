use atomic_write_file::AtomicWriteFile;
use command_group::AsyncCommandGroup;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    time::{Duration, timeout},
};

const MAX_OUTPUT: usize = 50 * 1024;
const MAX_LINE: usize = 128 * 1024;

#[derive(Clone, Debug)]
pub struct ShellInfo {
    program: PathBuf,
    pub description: String,
}

impl ShellInfo {
    pub fn detect() -> Result<Self, String> {
        #[cfg(windows)]
        let (program, name) = {
            let found = |name: &str| {
                std::process::Command::new("where.exe")
                    .arg(name)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .and_then(|s| s.lines().next().map(PathBuf::from))
            };
            found("pwsh.exe")
                .map(|p| (p, "PowerShell (pwsh)"))
                .or_else(|| found("powershell.exe").map(|p| (p, "Windows PowerShell")))
                .ok_or_else(|| {
                    "Neither pwsh.exe nor powershell.exe was found on PATH".to_string()
                })?
        };
        #[cfg(unix)]
        let (program, name) = {
            let program = PathBuf::from("/bin/sh");
            if !program.is_file() {
                return Err("/bin/sh was not found".into());
            }
            (program, "/bin/sh")
        };
        let mut version_command = std::process::Command::new(&program);
        #[cfg(windows)]
        version_command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$PSVersionTable.PSVersion.ToString()",
        ]);
        #[cfg(unix)]
        version_command.arg("--version");
        let version = version_command
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|o| {
                if o.stdout.is_empty() {
                    o.stderr
                } else {
                    o.stdout
                }
            })
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.lines().next().map(str::to_owned))
            .unwrap_or_else(|| "version unavailable".into());
        Ok(Self {
            program,
            description: format!("{name}; {version}"),
        })
    }
}

pub fn definitions() -> Vec<Value> {
    vec![
        def(
            "read",
            "Read a regular UTF-8 file with 1-based line offsets (up to 2000 lines and 128 KiB per call). UTF-8 BOM is hidden and CRLF/CR are returned as LF. Continue from next_offset when truncated is true",
            json!({"type":"object","additionalProperties":false,"properties":{"path":{"type":"string","minLength":1},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":2000}},"required":["path"]}),
        ),
        def(
            "write",
            "Atomically create or replace a file with the exact supplied UTF-8 content",
            json!({"type":"object","additionalProperties":false,"properties":{"path":{"type":"string","minLength":1},"content":{"type":"string"}},"required":["path","content"]}),
        ),
        def(
            "edit",
            "Atomically replace one unique occurrence in a UTF-8 file. Matching accepts LF, CRLF, or CR and preserves the file's BOM and untouched line endings",
            json!({"type":"object","additionalProperties":false,"properties":{"path":{"type":"string","minLength":1},"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}},"required":["path","old_text","new_text"]}),
        ),
        def(
            "shell",
            "Run a non-interactive shell command. Check status for success, failure, or timeout; stdout and stderr are omitted when empty",
            json!({"type":"object","additionalProperties":false,"properties":{"command":{"type":"string","minLength":1},"timeout_seconds":{"type":"integer","minimum":1,"maximum":3600}},"required":["command"]}),
        ),
    ]
}
fn def(name: &str, description: &str, parameters: Value) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}})
}
fn absolute(cwd: &Path, p: &str) -> PathBuf {
    let p = PathBuf::from(p);
    if p.is_absolute() { p } else { cwd.join(p) }
}
fn error(s: impl ToString) -> String {
    json!({"ok":false,"error":s.to_string()}).to_string()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default = "one")]
    offset: u64,
    #[serde(default = "two_thousand")]
    limit: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    command: String,
    #[serde(default = "six_hundred")]
    timeout_seconds: u64,
}
fn one() -> u64 {
    1
}
fn two_thousand() -> u64 {
    2000
}
fn six_hundred() -> u64 {
    600
}
fn parse<T: for<'a> Deserialize<'a>>(args: &str) -> Result<T, String> {
    serde_json::from_str(args).map_err(|e| format!("Invalid arguments: {e}"))
}

pub async fn execute(name: &str, args: &str, cwd: &Path, shell_info: &ShellInfo) -> String {
    match name {
        "read" => parse(args).map_or_else(error, |a| read(a, cwd)),
        "write" => parse(args).map_or_else(error, |a| write(a, cwd)),
        "edit" => parse(args).map_or_else(error, |a| edit(a, cwd)),
        "shell" => match parse(args) {
            Ok(a) => shell(a, cwd, shell_info).await,
            Err(e) => error(e),
        },
        _ => error(format!("Unknown tool: {name}")),
    }
}

fn valid_path(p: &str) -> Result<(), String> {
    if p.is_empty() {
        Err("path must not be empty".into())
    } else {
        Ok(())
    }
}

fn logical_line(r: &mut impl BufRead) -> Result<Option<(Vec<u8>, bool)>, String> {
    let mut line = Vec::with_capacity(8192);
    loop {
        let (take, ending) = {
            let buf = r.fill_buf().map_err(|e| e.to_string())?;
            if buf.is_empty() {
                return Ok((!line.is_empty()).then_some((line, false)));
            }
            match buf.iter().position(|byte| matches!(byte, b'\r' | b'\n')) {
                Some(index) => (index, Some(buf[index])),
                None => (buf.len(), None),
            }
        };
        if line.len() + take > MAX_LINE {
            return Err("File contains a line longer than 128 KiB".into());
        }
        line.extend_from_slice(&r.fill_buf().map_err(|e| e.to_string())?[..take]);
        r.consume(take);
        if let Some(ending) = ending {
            if line.len() == MAX_LINE {
                return Err("File contains a line longer than 128 KiB".into());
            }
            r.consume(1);
            if ending == b'\r' && r.fill_buf().map_err(|e| e.to_string())?.first() == Some(&b'\n') {
                r.consume(1);
            }
            return Ok(Some((line, true)));
        }
    }
}

fn read(a: ReadArgs, cwd: &Path) -> String {
    if let Err(e) = valid_path(&a.path) {
        return error(e);
    }
    if a.offset < 1 {
        return error("offset must be at least 1");
    }
    if !(1..=2000).contains(&a.limit) {
        return error("limit must be from 1 through 2000");
    }
    let path = absolute(cwd, &a.path);
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return error("Path is not a regular file"),
        Err(e) => return error(e),
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => return error(e),
    };
    let mut r = std::io::BufReader::new(file);
    let mut content = Vec::new();
    let mut line_number = 0u64;
    let mut taken = 0u64;
    let mut next_offset = None;
    loop {
        if taken == a.limit {
            match r.fill_buf() {
                Ok([]) => break,
                Ok(_) => {
                    next_offset = Some(line_number + 1);
                    break;
                }
                Err(e) => return error(e),
            }
        }
        let (mut line, ended) = match logical_line(&mut r) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => return error(e),
        };
        line_number += 1;
        if line_number == 1 && line.starts_with(b"\xef\xbb\xbf") {
            line.drain(..3);
        }
        if line.contains(&0) {
            return error("File contains NUL bytes");
        }
        if std::str::from_utf8(&line).is_err() {
            return error("File is not valid UTF-8");
        }
        if line_number < a.offset {
            continue;
        }
        let added = line.len() + usize::from(ended);
        if content.len() + added > MAX_LINE {
            next_offset = Some(line_number);
            break;
        }
        content.extend_from_slice(&line);
        if ended {
            content.push(b'\n');
        }
        taken += 1;
    }
    if a.offset > 1 && line_number < a.offset {
        return error("offset exceeds file length");
    }
    let mut result = json!({"ok":true,"content":String::from_utf8(content).unwrap()});
    if let Some(next) = next_offset {
        result["truncated"] = json!(true);
        result["next_offset"] = json!(next);
    }
    result.to_string()
}

fn atomic_target(path: &Path) -> Result<PathBuf, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => match path.canonicalize() {
            Ok(target) if target.metadata().is_ok_and(|metadata| metadata.is_file()) => Ok(target),
            Ok(_) => Err("Symlink target is not a regular file".into()),
            Err(e) => Err(e.to_string()),
        },
        Ok(metadata) if metadata.is_file() => Ok(path.to_owned()),
        Ok(_) => Err("Path is not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path.to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let target = atomic_target(path)?;
    let mut file = AtomicWriteFile::open(target).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.commit().map_err(|e| e.to_string())
}

fn write(a: WriteArgs, cwd: &Path) -> String {
    if let Err(e) = valid_path(&a.path) {
        return error(e);
    }
    let path = absolute(cwd, &a.path);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return error(e);
        }
    }
    match atomic_write(&path, a.content.as_bytes()) {
        Ok(()) => json!({"ok":true}).to_string(),
        Err(e) => error(e),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Eol {
    Lf,
    CrLf,
    Cr,
}

impl Eol {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Lf => b"\n",
            Self::CrLf => b"\r\n",
            Self::Cr => b"\r",
        }
    }
}

struct Normalized {
    text: String,
    original_offsets: Vec<usize>,
    eols: Vec<(usize, Eol)>,
}

fn normalize_eols(text: &str) -> Normalized {
    let bytes = text.as_bytes();
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut original_offsets = vec![0];
    let mut eols = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            let (width, style) = if bytes.get(index + 1) == Some(&b'\n') {
                (2, Eol::CrLf)
            } else {
                (1, Eol::Cr)
            };
            eols.push((normalized.len(), style));
            normalized.push(b'\n');
            index += width;
            original_offsets.push(index);
        } else {
            let width = text[index..].chars().next().unwrap().len_utf8();
            normalized.extend_from_slice(&bytes[index..index + width]);
            for offset in 1..=width {
                original_offsets.push(index + offset);
            }
            if bytes[index] == b'\n' {
                eols.push((normalized.len() - 1, Eol::Lf));
            }
            index += width;
        }
    }
    Normalized {
        text: String::from_utf8(normalized).unwrap(),
        original_offsets,
        eols,
    }
}

fn replacement_bytes(
    replacement: &str,
    source: &Normalized,
    start: usize,
    end: usize,
) -> Result<Vec<u8>, String> {
    let replacement = normalize_eols(replacement);
    let old_styles: Vec<_> = source
        .eols
        .iter()
        .filter(|(position, _)| *position >= start && *position < end)
        .map(|(_, style)| *style)
        .collect();
    let new_count = replacement.eols.len();
    let styles = if new_count == old_styles.len() {
        old_styles
    } else if let Some(first) = old_styles.first().copied() {
        if old_styles.iter().any(|style| *style != first) {
            return Err(
                "replacement changes the newline count of a mixed-line-ending match; use a smaller edit"
                    .into(),
            );
        }
        vec![first; new_count]
    } else {
        let nearby = source
            .eols
            .iter()
            .rev()
            .find(|(position, _)| *position < start)
            .or_else(|| source.eols.iter().find(|(position, _)| *position >= end))
            .map_or(Eol::Lf, |(_, style)| *style);
        vec![nearby; new_count]
    };
    let mut rendered = Vec::with_capacity(replacement.text.len() + styles.len());
    let mut style = styles.into_iter();
    for byte in replacement.text.bytes() {
        if byte == b'\n' {
            rendered.extend_from_slice(style.next().unwrap().bytes());
        } else {
            rendered.push(byte);
        }
    }
    Ok(rendered)
}

fn edit(a: EditArgs, cwd: &Path) -> String {
    if let Err(e) = valid_path(&a.path) {
        return error(e);
    }
    if a.old_text.is_empty() {
        return error("old_text must not be empty");
    }
    let path = absolute(cwd, &a.path);
    let target = match atomic_target(&path) {
        Ok(target) => target,
        Err(e) => return error(e),
    };
    let bytes = match std::fs::read(&target) {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    let (bom, payload) = if bytes.starts_with(b"\xef\xbb\xbf") {
        (&bytes[..3], &bytes[3..])
    } else {
        (&bytes[..0], bytes.as_slice())
    };
    if payload.contains(&0) {
        return error("File contains NUL bytes");
    }
    let text = match std::str::from_utf8(payload) {
        Ok(text) => text,
        Err(_) => return error("File is not valid UTF-8"),
    };
    let source = normalize_eols(text);
    let old_text = a.old_text.strip_prefix('\u{feff}').unwrap_or(&a.old_text);
    let old_text = normalize_eols(old_text).text;
    if old_text.is_empty() {
        return error("old_text must not be empty after removing a UTF-8 BOM");
    }
    let matches: Vec<_> = source.text.match_indices(&old_text).take(2).collect();
    if matches.len() != 1 {
        return error(format!(
            "old_text matched {} times; exactly one match is required",
            matches.len()
        ));
    }
    let normalized_start = matches[0].0;
    let normalized_end = normalized_start + old_text.len();
    let start = source.original_offsets[normalized_start];
    let end = source.original_offsets[normalized_end];
    let replacement =
        match replacement_bytes(&a.new_text, &source, normalized_start, normalized_end) {
            Ok(replacement) => replacement,
            Err(e) => return error(e),
        };
    let mut result = Vec::with_capacity(bytes.len() - (end - start) + replacement.len());
    result.extend_from_slice(bom);
    result.extend_from_slice(&payload[..start]);
    result.extend_from_slice(&replacement);
    result.extend_from_slice(&payload[end..]);
    match atomic_write(&target, &result) {
        Ok(()) => json!({"ok":true}).to_string(),
        Err(e) => error(e),
    }
}

struct Capture {
    head: Vec<u8>,
    after_head: Vec<u8>,
    before_tail: VecDeque<u8>,
    tail: VecDeque<u8>,
    total: usize,
}
type TruncatedCapture = (Vec<u8>, Vec<u8>, Vec<u8>);

async fn drain(mut r: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Capture> {
    let mut c = Capture {
        head: Vec::with_capacity(MAX_OUTPUT / 2),
        after_head: Vec::with_capacity(3),
        before_tail: VecDeque::with_capacity(3),
        tail: VecDeque::with_capacity(MAX_OUTPUT / 2),
        total: 0,
    };
    let mut b = [0; 8192];
    loop {
        let n = r.read(&mut b).await?;
        if n == 0 {
            break;
        }
        c.total += n;
        for &x in &b[..n] {
            if c.head.len() < MAX_OUTPUT / 2 {
                c.head.push(x)
            } else {
                if c.after_head.len() < 3 {
                    c.after_head.push(x);
                }
                if c.tail.len() == MAX_OUTPUT / 2 {
                    if c.before_tail.len() == 3 {
                        c.before_tail.pop_front();
                    }
                    c.before_tail.push_back(c.tail.pop_front().unwrap());
                }
                c.tail.push_back(x);
            }
        }
    }
    Ok(c)
}
fn captured(c: Capture) -> (Vec<u8>, Option<TruncatedCapture>) {
    if c.total <= MAX_OUTPUT {
        let mut v = c.head;
        v.extend(c.tail);
        (v, None)
    } else {
        (
            c.head,
            Some((c.after_head, c.before_tail.into(), c.tail.into())),
        )
    }
}

struct Escaped {
    text: String,
    boundaries: Vec<usize>,
    invalid: bool,
}

impl Escaped {
    fn prefix(&self, limit: usize) -> &str {
        let end = self
            .boundaries
            .partition_point(|boundary| *boundary <= limit)
            .checked_sub(1)
            .map_or(0, |index| self.boundaries[index]);
        &self.text[..end]
    }

    fn suffix(&self, limit: usize) -> &str {
        let target = self.text.len().saturating_sub(limit);
        let index = self
            .boundaries
            .partition_point(|boundary| *boundary < target);
        &self.text[self.boundaries[index.min(self.boundaries.len() - 1)]..]
    }
}

fn escape_utf8(bytes: &[u8]) -> Escaped {
    let mut text = String::new();
    let mut boundaries = vec![0];
    let mut remaining = bytes;
    let mut invalid = false;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                for character in valid.chars() {
                    text.push(character);
                    boundaries.push(text.len());
                }
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                for character in std::str::from_utf8(&remaining[..valid]).unwrap().chars() {
                    text.push(character);
                    boundaries.push(text.len());
                }
                let invalid_len = error.error_len().unwrap_or(remaining.len() - valid);
                for byte in &remaining[valid..valid + invalid_len] {
                    text.push_str(&format!("\\x{byte:02x}"));
                    boundaries.push(text.len());
                }
                invalid = true;
                remaining = &remaining[valid + invalid_len..];
            }
        }
    }
    Escaped {
        text,
        boundaries,
        invalid,
    }
}

fn rendered_capture(head: Vec<u8>, tail: Option<TruncatedCapture>) -> (String, bool, bool) {
    const MARKER: &str = "\n...[truncated]...\n";
    if let Some((after_head, before_tail, mut tail)) = tail {
        let mut head = head;
        if let Err(error) = std::str::from_utf8(&head) {
            if error.error_len().is_none() {
                let start = error.valid_up_to();
                for length in 1..=after_head.len() {
                    let mut boundary = head[start..].to_vec();
                    boundary.extend_from_slice(&after_head[..length]);
                    if std::str::from_utf8(&boundary).is_ok_and(|text| text.chars().count() == 1) {
                        head.truncate(start);
                        break;
                    }
                }
            }
        }
        let leading_continuations = tail
            .iter()
            .take(3)
            .take_while(|byte| **byte & 0b1100_0000 == 0b1000_0000)
            .count();
        if leading_continuations > 0 {
            let mut crossing = false;
            for start in 0..before_tail.len() {
                let prefix = &before_tail[start..];
                let mut candidate = prefix.to_vec();
                candidate.extend_from_slice(&tail[..leading_continuations]);
                if let Ok(character) = std::str::from_utf8(&candidate) {
                    if character.chars().count() == 1 && prefix.len() < character.len() {
                        crossing = true;
                        break;
                    }
                }
            }
            if crossing {
                tail.drain(..leading_continuations);
            }
        }
        let head = escape_utf8(&head);
        let tail = escape_utf8(&tail);
        let budget = MAX_OUTPUT - MARKER.len();
        (
            format!(
                "{}{MARKER}{}",
                head.prefix(budget / 2),
                tail.suffix(budget - budget / 2)
            ),
            true,
            head.invalid || tail.invalid,
        )
    } else {
        let escaped = escape_utf8(&head);
        if escaped.text.len() <= MAX_OUTPUT {
            (escaped.text, false, escaped.invalid)
        } else {
            let budget = MAX_OUTPUT - MARKER.len();
            (
                format!(
                    "{}{MARKER}{}",
                    escaped.prefix(budget / 2),
                    escaped.suffix(budget - budget / 2)
                ),
                true,
                escaped.invalid,
            )
        }
    }
}

async fn shell(a: ShellArgs, cwd: &Path, info: &ShellInfo) -> String {
    if a.command.is_empty() {
        return error("command must not be empty");
    }
    if !(1..=3600).contains(&a.timeout_seconds) {
        return error("timeout_seconds must be from 1 through 3600");
    }
    let mut cmd = Command::new(&info.program);
    #[cfg(windows)]
    cmd.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &a.command,
    ]);
    #[cfg(unix)]
    cmd.args(["-c", &a.command]);
    cmd.current_dir(cwd)
        .env_remove("DEEPSEEK_API_KEY")
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.group().kill_on_drop(true).spawn() {
        Ok(c) => c,
        Err(e) => return error(e),
    };
    let out = child.inner().stdout.take().unwrap();
    let err = child.inner().stderr.take().unwrap();
    let mut ro = tokio::spawn(drain(out));
    let mut re = tokio::spawn(drain(err));
    let waited = timeout(Duration::from_secs(a.timeout_seconds), child.wait()).await;
    let timed_out = waited.is_err();
    let status = if timed_out {
        if let Err(e) = child.kill().await {
            return error(format!("Failed to terminate timed-out process group: {e}"));
        }
        match child.wait().await {
            Ok(status) => status,
            Err(e) => return error(format!("Failed to reap timed-out process group: {e}")),
        }
    } else {
        match waited.expect("non-timeout wait result") {
            Ok(status) => status,
            Err(e) => return error(format!("Failed to wait for process group: {e}")),
        }
    };
    let joins = timeout(Duration::from_secs(2), async {
        tokio::join!(&mut ro, &mut re)
    })
    .await;
    let (oc, ec) = match joins {
        Ok((Ok(Ok(o)), Ok(Ok(e)))) => (o, e),
        _ => {
            ro.abort();
            re.abort();
            return error("Output readers did not finish");
        }
    };
    let (stdout, stdout_truncated, stdout_invalid) = {
        let (head, tail) = captured(oc);
        rendered_capture(head, tail)
    };
    let (stderr, stderr_truncated, stderr_invalid) = {
        let (head, tail) = captured(ec);
        rendered_capture(head, tail)
    };
    let state = if timed_out {
        "timed_out"
    } else if status.success() {
        "success"
    } else {
        "failed"
    };
    let mut result = json!({"ok":true,"status":state});
    if !timed_out && !status.success() {
        if let Some(code) = status.code() {
            result["exit_code"] = json!(code);
        }
    }
    if !stdout.is_empty() {
        result["stdout"] = json!(stdout);
    }
    if !stderr.is_empty() {
        result["stderr"] = json!(stderr);
    }
    for (name, value) in [
        ("stdout_truncated", stdout_truncated),
        ("stderr_truncated", stderr_truncated),
        ("stdout_invalid_utf8", stdout_invalid),
        ("stderr_invalid_utf8", stderr_invalid),
    ] {
        if value {
            result[name] = json!(true);
        }
    }
    result.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    async fn run(name: &str, args: &str, dir: &Path) -> Value {
        serde_json::from_str(&execute(name, args, dir, &ShellInfo::detect().unwrap()).await)
            .unwrap()
    }

    #[cfg(windows)]
    async fn run_with_shell(name: &str, args: &str, dir: &Path, shell: &ShellInfo) -> Value {
        serde_json::from_str(&execute(name, args, dir, shell).await).unwrap()
    }

    fn keys(value: &Value) -> BTreeSet<&str> {
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn definitions_are_small_strict_and_describe_text_adaptation() {
        let definitions = definitions();
        assert_eq!(definitions.len(), 4);
        for definition in &definitions {
            assert_eq!(
                definition["function"]["parameters"]["additionalProperties"],
                false
            );
        }
        assert!(
            definitions[0]["function"]["description"]
                .as_str()
                .unwrap()
                .contains("BOM")
        );
        assert!(
            definitions[2]["function"]["description"]
                .as_str()
                .unwrap()
                .contains("CRLF")
        );
    }

    #[tokio::test]
    async fn read_paginates_and_validates_files_and_arguments() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), "one\ntwo\nthree\n").unwrap();
        let page = run("read", r#"{"path":"x","offset":2,"limit":1}"#, d.path()).await;
        assert_eq!(page["content"], "two\n");
        assert_eq!(page["next_offset"], 3);
        assert!(page["truncated"].as_bool().unwrap());
        assert_eq!(
            keys(&page),
            BTreeSet::from(["content", "next_offset", "ok", "truncated"])
        );
        let final_page = run("read", r#"{"path":"x","offset":3}"#, d.path()).await;
        assert_eq!(final_page, json!({"ok":true,"content":"three\n"}));
        assert!(
            !run("read", r#"{"path":"x","offset":5}"#, d.path()).await["ok"]
                .as_bool()
                .unwrap()
        );
        std::fs::write(d.path().join("bad-next"), b"good\n\xff").unwrap();
        assert_eq!(
            run("read", r#"{"path":"bad-next","limit":1}"#, d.path()).await,
            json!({"ok":true,"content":"good\n","truncated":true,"next_offset":2})
        );
        assert!(
            !run("read", r#"{"path":"bad-next","offset":2}"#, d.path()).await["ok"]
                .as_bool()
                .unwrap()
        );
        assert!(
            !run("read", r#"{"path":"x","limit":0}"#, d.path()).await["ok"]
                .as_bool()
                .unwrap()
        );
        assert!(
            !run("read", r#"{"path":"x","extra":1}"#, d.path()).await["ok"]
                .as_bool()
                .unwrap()
        );
        assert!(
            !run("read", r#"{"path":"."}"#, d.path()).await["ok"]
                .as_bool()
                .unwrap()
        );
        std::fs::write(d.path().join("bad"), [0xff]).unwrap();
        assert!(
            !run("read", r#"{"path":"bad"}"#, d.path()).await["ok"]
                .as_bool()
                .unwrap()
        );
        std::fs::write(d.path().join("nul"), b"a\0b").unwrap();
        assert!(
            !run("read", r#"{"path":"nul"}"#, d.path()).await["ok"]
                .as_bool()
                .unwrap()
        );
        std::fs::write(d.path().join("long"), vec![b'a'; MAX_LINE + 1]).unwrap();
        assert!(
            !run("read", r#"{"path":"long"}"#, d.path()).await["ok"]
                .as_bool()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn read_and_write_preserve_utf8_bom_and_line_endings_exactly() {
        let d = tempfile::tempdir().unwrap();
        let content = "\u{feff}中文🙂\r\nsecond\nthird\rlast";
        let written = run(
            "write",
            &json!({"path":"nested/x.txt","content":content}).to_string(),
            d.path(),
        )
        .await;
        assert_eq!(written, json!({"ok":true}));
        assert_eq!(
            std::fs::read(d.path().join("nested/x.txt")).unwrap(),
            content.as_bytes()
        );

        let read_back = run("read", r#"{"path":"nested/x.txt"}"#, d.path()).await;
        assert_eq!(
            read_back,
            json!({"ok":true,"content":"中文🙂\nsecond\nthird\nlast"})
        );
    }

    #[tokio::test]
    async fn read_handles_empty_unterminated_and_byte_limited_pages() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("empty"), []).unwrap();
        assert_eq!(
            run("read", r#"{"path":"empty"}"#, d.path()).await,
            json!({"ok":true,"content":""})
        );
        std::fs::write(d.path().join("unterminated"), "first\r\nlast🙂").unwrap();
        assert_eq!(
            run("read", r#"{"path":"unterminated"}"#, d.path()).await,
            json!({"ok":true,"content":"first\nlast🙂"})
        );

        let first = "a".repeat(MAX_LINE - 10) + "\n";
        std::fs::write(d.path().join("large"), format!("{first}中文🙂\nlast")).unwrap();
        let page = run("read", r#"{"path":"large"}"#, d.path()).await;
        assert_eq!(page["content"], first);
        assert_eq!(page["next_offset"], 2);
        let rest = run("read", r#"{"path":"large","offset":2}"#, d.path()).await;
        assert_eq!(rest, json!({"ok":true,"content":"中文🙂\nlast"}));
    }

    #[tokio::test]
    async fn write_rejects_directories_and_follows_existing_symlinks_when_supported() {
        let d = tempfile::tempdir().unwrap();
        assert!(
            !run("write", r#"{"path":".","content":"no"}"#, d.path()).await["ok"]
                .as_bool()
                .unwrap()
        );

        let target = d.path().join("target.txt");
        let link = d.path().join("link.txt");
        std::fs::write(&target, "old").unwrap();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&target, &link).is_ok();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, &link).is_ok();
        if linked {
            assert_eq!(
                run("write", r#"{"path":"link.txt","content":"new"}"#, d.path()).await,
                json!({"ok":true})
            );
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(std::fs::read_to_string(target).unwrap(), "new");
        }
    }

    #[tokio::test]
    async fn edit_is_unique_and_preserves_crlf() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), "a\r\nb\r\n").unwrap();
        assert!(
            run(
                "edit",
                r#"{"path":"x","old_text":"a\nb","new_text":"x\ny"}"#,
                d.path()
            )
            .await["ok"]
                .as_bool()
                .unwrap()
        );
        assert_eq!(std::fs::read(d.path().join("x")).unwrap(), b"x\r\ny\r\n");
        std::fs::write(d.path().join("x"), "same same").unwrap();
        assert!(
            !run(
                "edit",
                r#"{"path":"x","old_text":"same","new_text":"x"}"#,
                d.path()
            )
            .await["ok"]
                .as_bool()
                .unwrap()
        );
        assert!(
            !run(
                "edit",
                &json!({"path":"x","old_text":"\u{feff}","new_text":"x"}).to_string(),
                d.path()
            )
            .await["ok"]
                .as_bool()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn edit_preserves_utf8_and_does_not_rewrite_mixed_line_endings() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("x");
        std::fs::write(&path, "\u{feff}中文\r\nsecond\nlast🙂\r\n").unwrap();
        let edited = run(
            "edit",
            &json!({"path":"x","old_text":"中文","new_text":"汉字🙂"}).to_string(),
            d.path(),
        )
        .await;
        assert_eq!(edited, json!({"ok":true}));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            "\u{feff}汉字🙂\r\nsecond\nlast🙂\r\n".as_bytes()
        );

        let before = std::fs::read(&path).unwrap();
        let rejected = run(
            "edit",
            &json!({
                "path":"x",
                "old_text":"汉字🙂\nsecond\nlast🙂",
                "new_text":"replacement"
            })
            .to_string(),
            d.path(),
        )
        .await;
        assert!(!rejected["ok"].as_bool().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn edit_preserves_mixed_endings_positionally_and_adapts_newline_counts() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("x");
        std::fs::write(&path, "before\r\na🙂\r\nb\nc\rd\r\nafter").unwrap();
        assert_eq!(
            run(
                "edit",
                &json!({
                    "path":"x",
                    "old_text":"a🙂\nb\nc\nd",
                    "new_text":"一\n二\n三\n四"
                })
                .to_string(),
                d.path(),
            )
            .await,
            json!({"ok":true})
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            "before\r\n一\r\n二\n三\r四\r\nafter".as_bytes()
        );

        std::fs::write(&path, "before\r\none\r\ntwo\r\nafter").unwrap();
        assert_eq!(
            run(
                "edit",
                r#"{"path":"x","old_text":"one\ntwo","new_text":"1\n2\n3"}"#,
                d.path(),
            )
            .await,
            json!({"ok":true})
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"before\r\n1\r\n2\r\n3\r\nafter"
        );
    }

    #[tokio::test]
    async fn edit_rejects_invalid_text_and_leaves_files_unchanged() {
        let d = tempfile::tempdir().unwrap();
        for (name, bytes) in [("invalid", vec![0xff]), ("contains-nul", b"a\0b".to_vec())] {
            let path = d.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            let result = run(
                "edit",
                &json!({"path":name,"old_text":"a","new_text":"x"}).to_string(),
                d.path(),
            )
            .await;
            assert!(!result["ok"].as_bool().unwrap());
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert!(
            !run(
                "edit",
                r#"{"path":".","old_text":"a","new_text":"x"}"#,
                d.path()
            )
            .await["ok"]
                .as_bool()
                .unwrap()
        );
        #[cfg(unix)]
        assert!(
            !run(
                "edit",
                r#"{"path":"/dev/null","old_text":"a","new_text":"x"}"#,
                d.path()
            )
            .await["ok"]
                .as_bool()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn shell_handles_eof_failure_large_tail_and_timeout() {
        let d = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let (fail, large, slow) = (
            "[Console]::Out.Write('done'); exit 7",
            "[Console]::Out.Write(('a' * 300000) + 'TAIL')",
            "Start-Sleep -Seconds 5",
        );
        #[cfg(unix)]
        let (fail, large, slow) = (
            "printf done; exit 7",
            "head -c 300000 /dev/zero | tr '\\0' a; printf TAIL",
            "sleep 5",
        );
        let v = run("shell", &json!({"command":fail}).to_string(), d.path()).await;
        assert_eq!(v["status"], "failed");
        assert_eq!(v["stdout"], "done");
        assert_eq!(v["exit_code"], 7);
        assert_eq!(
            keys(&v),
            BTreeSet::from(["exit_code", "ok", "status", "stdout"])
        );
        let v = run("shell", &json!({"command":large}).to_string(), d.path()).await;
        assert_eq!(v["status"], "success");
        assert!(v["stdout_truncated"].as_bool().unwrap());
        assert!(v["stdout"].as_str().unwrap().ends_with("TAIL"));
        let v = run(
            "shell",
            &json!({"command":slow,"timeout_seconds":1}).to_string(),
            d.path(),
        )
        .await;
        assert_eq!(v, json!({"ok":true,"status":"timed_out"}));
    }

    #[test]
    fn shell_rendering_escapes_invalid_bytes_and_truncates_on_utf8_boundaries() {
        let escaped = escape_utf8(b"ok\xffend");
        assert_eq!(escaped.text, "ok\\xffend");
        assert!(escaped.invalid);
        let text = "中🙂文".repeat(MAX_OUTPUT);
        let bytes = text.as_bytes();
        let capture = Capture {
            head: bytes[..MAX_OUTPUT / 2].to_vec(),
            after_head: bytes[MAX_OUTPUT / 2..MAX_OUTPUT / 2 + 3].to_vec(),
            before_tail: bytes[bytes.len() - MAX_OUTPUT / 2 - 3..bytes.len() - MAX_OUTPUT / 2]
                .iter()
                .copied()
                .collect(),
            tail: bytes[bytes.len() - MAX_OUTPUT / 2..]
                .iter()
                .copied()
                .collect(),
            total: bytes.len(),
        };
        let (head, tail) = captured(capture);
        let (rendered, truncated, invalid) = rendered_capture(head, tail);
        assert!(truncated);
        assert!(!invalid);
        assert!(rendered.contains("...[truncated]..."));
        assert!(!rendered.contains('\u{fffd}'));

        let (rendered, _, invalid) = rendered_capture(
            vec![b'a', 0xf0],
            Some((vec![b'A'], vec![], vec![0x80, b'B'])),
        );
        assert!(invalid);
        assert!(rendered.contains("\\xf0"));
        assert!(rendered.contains("\\x80B"));

        let (rendered, _, invalid) = rendered_capture(
            vec![b'a', 0xc3],
            Some((vec![0xa9, 0xe4, 0xb8], vec![], vec![b'B'])),
        );
        assert!(!invalid);
        assert!(!rendered.contains("\\xc3"));

        let (rendered, truncated, invalid) = rendered_capture(vec![0xff; MAX_OUTPUT], None);
        assert!(truncated);
        assert!(invalid);
        assert!(rendered.len() <= MAX_OUTPUT);
        assert!(rendered.starts_with("\\xff"));
        assert!(rendered.ends_with("\\xff"));
    }

    #[tokio::test]
    async fn shell_sets_utf8_noninteractive_environment_and_captures_unicode() {
        let d = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let command = "[Console]::Out.Write(\"中文🙂|$env:PYTHONUTF8|$env:PYTHONIOENCODING|$env:NO_COLOR|$env:GIT_TERMINAL_PROMPT|$([Environment]::CurrentDirectory)\")";
        #[cfg(unix)]
        let command = "printf '中文🙂|%s|%s|%s|%s|%s' \"$PYTHONUTF8\" \"$PYTHONIOENCODING\" \"$NO_COLOR\" \"$GIT_TERMINAL_PROMPT\" \"$PWD\"";
        let result = run("shell", &json!({"command":command}).to_string(), d.path()).await;
        assert_eq!(result["status"], "success");
        let stdout = result["stdout"].as_str().unwrap();
        assert!(stdout.starts_with("中文🙂|1|utf-8|1|0|"));
        assert!(stdout.contains(d.path().to_string_lossy().as_ref()));

        #[cfg(windows)]
        let invalid =
            "$s=[Console]::OpenStandardOutput();$b=[byte[]](255,65);$s.Write($b,0,$b.Length)";
        #[cfg(unix)]
        let invalid = "printf '\\377A'";
        let result = run("shell", &json!({"command":invalid}).to_string(), d.path()).await;
        assert_eq!(result["stdout"], "\\xffA");
        assert_eq!(result["stdout_invalid_utf8"], true);

        #[cfg(windows)]
        let stderr = "[Console]::Error.Write('problem')";
        #[cfg(unix)]
        let stderr = "printf problem >&2";
        let result = run("shell", &json!({"command":stderr}).to_string(), d.path()).await;
        assert_eq!(
            result,
            json!({"ok":true,"status":"success","stderr":"problem"})
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn both_windows_powershell_versions_emit_unicode_and_python_utf8_when_available() {
        let d = tempfile::tempdir().unwrap();
        for (program, description) in [
            ("pwsh.exe", "PowerShell 7"),
            ("powershell.exe", "Windows PowerShell"),
        ] {
            let output = std::process::Command::new("where.exe")
                .arg(program)
                .output();
            if !output.is_ok_and(|output| output.status.success()) {
                continue;
            }
            let shell = ShellInfo {
                program: PathBuf::from(program),
                description: description.into(),
            };
            let result = run_with_shell(
                "shell",
                &json!({"command":"[Console]::Out.Write('中文🙂')"}).to_string(),
                d.path(),
                &shell,
            )
            .await;
            assert_eq!(
                result,
                json!({"ok":true,"status":"success","stdout":"中文🙂"})
            );
            let result = run_with_shell(
                "shell",
                &json!({"command":"using namespace System.Text; param($value = 'grammar-ok'); [Console]::Out.Write($value)"}).to_string(),
                d.path(),
                &shell,
            )
            .await;
            assert_eq!(
                result,
                json!({"ok":true,"status":"success","stdout":"grammar-ok"})
            );

            if std::process::Command::new("where.exe")
                .arg("python.exe")
                .output()
                .is_ok_and(|output| output.status.success())
            {
                let result = run_with_shell(
                    "shell",
                    &json!({"command":"python -c \"import sys; print(sys.stdout.encoding); print('中文🙂')\""}).to_string(),
                    d.path(),
                    &shell,
                )
                .await;
                assert_eq!(result["status"], "success");
                assert_eq!(result["stdout"], "utf-8\r\n中文🙂\r\n");
            }
        }
    }
}
