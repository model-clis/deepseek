use command_group::AsyncCommandGroup;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::BufRead,
    path::{Path, PathBuf},
    process::Stdio,
    time::Instant,
};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    time::{Duration, timeout},
};

const MAX_OUTPUT: usize = 256 * 1024;
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
            "Read a regular UTF-8 file with 1-based line offsets (up to 2000 lines and 128 KiB per call). The JSON result includes truncated and next_offset; continue from next_offset when truncated is true",
            json!({"type":"object","additionalProperties":false,"properties":{"path":{"type":"string","minLength":1},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":2000}},"required":["path"]}),
        ),
        def(
            "write",
            "Create or replace a UTF-8 file",
            json!({"type":"object","additionalProperties":false,"properties":{"path":{"type":"string","minLength":1},"content":{"type":"string"}},"required":["path","content"]}),
        ),
        def(
            "edit",
            "Replace one unique text occurrence. The call fails without changing the file unless old_text matches exactly once",
            json!({"type":"object","additionalProperties":false,"properties":{"path":{"type":"string","minLength":1},"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}},"required":["path","old_text","new_text"]}),
        ),
        def(
            "shell",
            "Run a non-interactive shell command. The JSON result distinguishes tool success (ok) from command_succeeded and includes exit_code, timed_out, stdout, stderr, and truncation flags",
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
    let mut line = Vec::with_capacity(8192);
    let mut content = Vec::new();
    let mut total = 0u64;
    let mut taken = 0u64;
    let mut byte_limited = false;
    loop {
        line.clear();
        let mut eof = false;
        loop {
            let buf = match r.fill_buf() {
                Ok(b) => b,
                Err(e) => return error(e),
            };
            if buf.is_empty() {
                eof = true;
                break;
            }
            let n = buf
                .iter()
                .position(|&b| b == b'\n')
                .map_or(buf.len(), |i| i + 1);
            if line.len() + n > MAX_LINE {
                return error("File contains a line longer than 128 KiB");
            }
            if buf[..n].contains(&0) {
                return error("File contains NUL bytes");
            }
            line.extend_from_slice(&buf[..n]);
            r.consume(n);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        if line.is_empty() && eof {
            break;
        }
        total += 1;
        if std::str::from_utf8(&line).is_err() {
            return error("File is not valid UTF-8");
        }
        if total >= a.offset && taken < a.limit && !byte_limited {
            if content.len() + line.len() <= MAX_LINE {
                content.extend_from_slice(&line);
                taken += 1
            } else {
                byte_limited = true
            }
        }
    }
    let end = a.offset.saturating_sub(1) + taken;
    let truncated = end < total;
    let next = if truncated {
        Some(if taken == 0 { a.offset + 1 } else { end + 1 })
    } else {
        None
    };
    json!({"ok":true,"path":path.to_string_lossy(),"content":String::from_utf8(content).unwrap(),"offset":a.offset,"end_line":end,"total_lines":total,"truncated":truncated,"next_offset":next}).to_string()
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
    match std::fs::write(&path, &a.content) {
        Ok(()) => {
            json!({"ok":true,"path":path.to_string_lossy(),"bytes":a.content.len()}).to_string()
        }
        Err(e) => error(e),
    }
}
fn edit(a: EditArgs, cwd: &Path) -> String {
    if let Err(e) = valid_path(&a.path) {
        return error(e);
    }
    if a.old_text.is_empty() {
        return error("old_text must not be empty");
    }
    let path = absolute(cwd, &a.path);
    let text = match std::fs::read_to_string(&path) {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    let (mut o, mut n) = (a.old_text, a.new_text);
    let mut count = text.matches(&o).count();
    if count == 0 && text.contains("\r\n") {
        o = o.replace('\n', "\r\n");
        n = n.replace('\n', "\r\n");
        count = text.matches(&o).count()
    }
    if count != 1 {
        return error(format!(
            "old_text matched {count} times; exactly one match is required"
        ));
    }
    let before = text.len();
    let result = text.replacen(&o, &n, 1);
    match std::fs::write(&path,&result){Ok(())=>json!({"ok":true,"path":path.to_string_lossy(),"bytes_before":before,"bytes_after":result.len()}).to_string(),Err(e)=>error(e)}
}

struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}
async fn drain(mut r: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Capture> {
    let mut c = Capture {
        head: Vec::with_capacity(MAX_OUTPUT / 2),
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
                if c.tail.len() == MAX_OUTPUT / 2 {
                    c.tail.pop_front();
                }
                c.tail.push_back(x);
            }
        }
    }
    Ok(c)
}
fn captured(c: Capture) -> (Vec<u8>, bool) {
    if c.total <= MAX_OUTPUT {
        let mut v = c.head;
        v.extend(c.tail);
        (v, false)
    } else {
        let mut v = c.head;
        v.extend_from_slice(b"\n...[truncated]...\n");
        v.extend(c.tail);
        (v, true)
    }
}
async fn shell(a: ShellArgs, cwd: &Path, info: &ShellInfo) -> String {
    if a.command.is_empty() {
        return error("command must not be empty");
    }
    if !(1..=3600).contains(&a.timeout_seconds) {
        return error("timeout_seconds must be from 1 through 3600");
    }
    let start = Instant::now();
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
    let (o, ot) = captured(oc);
    let (e, et) = captured(ec);
    json!({"ok":true,"command_succeeded":!timed_out && status.success(),"exit_code":status.code(),"timed_out":timed_out,"duration_ms":start.elapsed().as_millis(),"stdout":String::from_utf8_lossy(&o),"stderr":String::from_utf8_lossy(&e),"stdout_truncated":ot,"stderr_truncated":et}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(name: &str, args: &str, dir: &Path) -> Value {
        serde_json::from_str(&execute(name, args, dir, &ShellInfo::detect().unwrap()).await)
            .unwrap()
    }

    #[tokio::test]
    async fn read_paginates_and_validates_files_and_arguments() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), "one\ntwo\nthree\n").unwrap();
        let page = run("read", r#"{"path":"x","offset":2,"limit":1}"#, d.path()).await;
        assert_eq!(page["content"], "two\n");
        assert_eq!(page["total_lines"], 3);
        assert_eq!(page["next_offset"], 3);
        assert!(page["truncated"].as_bool().unwrap());
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
        assert!(!v["command_succeeded"].as_bool().unwrap());
        assert_eq!(v["stdout"], "done");
        assert_eq!(v["exit_code"], 7);
        let v = run("shell", &json!({"command":large}).to_string(), d.path()).await;
        assert!(v["command_succeeded"].as_bool().unwrap());
        assert!(v["stdout_truncated"].as_bool().unwrap());
        assert!(v["stdout"].as_str().unwrap().ends_with("TAIL"));
        let v = run(
            "shell",
            &json!({"command":slow,"timeout_seconds":1}).to_string(),
            d.path(),
        )
        .await;
        assert!(!v["command_succeeded"].as_bool().unwrap());
        assert!(v["timed_out"].as_bool().unwrap());
    }
}
