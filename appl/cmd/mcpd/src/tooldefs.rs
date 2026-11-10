//! The tools mcpd offers: names, descriptions written for a model, and JSON
//! Schemas (2020-12) for arguments and results. [`validate`] checks
//! arguments against the subset of JSON Schema these definitions use.

use crate::json::{Value, obj};

pub const DEFAULT_TIMEOUT_MS: i64 = 30_000;
pub const MAX_TIMEOUT_MS: i64 = 600_000;
/// Default cap on each of stdout and stderr, in bytes.
pub const DEFAULT_OUTPUT: i64 = 64 * 1024;
pub const MAX_OUTPUT: i64 = 1024 * 1024;
/// Default and largest `read_file` length, in bytes.
pub const DEFAULT_READ: i64 = 64 * 1024;
pub const MAX_READ: i64 = 1024 * 1024;
/// Most entries `list_dir` returns.
pub const MAX_ENTRIES: usize = 5000;
/// Output of a job kept for each of stdout and stderr: the last this many
/// bytes.
pub const JOB_BUFFER: usize = 1024 * 1024;
/// Jobs that may run at once, and exited jobs kept for job_read.
pub const MAX_JOBS: usize = 32;
/// Default timeout of `mount`.
pub const MOUNT_TIMEOUT_MS: i64 = 30_000;
/// Largest file `resources/read` returns.
pub const MAX_RESOURCE: usize = 1024 * 1024;
/// Most files `resources/list` lists.
pub const MAX_RESOURCES: usize = 1000;

/// Checks what a schema here cannot say: `job_start` takes exactly one of
/// `command` and `argv`, and `argv` names a program.
pub fn check(name: &str, args: &Value) -> Result<(), String> {
    if name == "job_start" {
        let err = |s: &str| Err(String::from(s));
        match (args.get("command"), args.get("argv")) {
            (Some(_), Some(_)) => return err("give arguments.command or arguments.argv, not both"),
            (None, None) => return err("arguments.command or arguments.argv is required"),
            (None, Some(Value::Array(a))) if a.is_empty() => {
                return err("arguments.argv must not be empty");
            }
            _ => {}
        }
    }
    Ok(())
}

fn string(desc: &str) -> Value {
    obj([("type", "string".into()), ("description", desc.into())])
}

fn boolean(desc: &str) -> Value {
    obj([("type", "boolean".into()), ("description", desc.into())])
}

fn integer(desc: &str, min: i64, max: Option<i64>) -> Value {
    let mut v = vec![
        (String::from("type"), Value::from("integer")),
        (String::from("description"), Value::from(desc)),
        (String::from("minimum"), Value::Int(min)),
    ];
    if let Some(max) = max {
        v.push((String::from("maximum"), Value::Int(max)));
    }
    Value::Object(v)
}

fn choice(desc: &str, values: &[&str]) -> Value {
    obj([
        ("type", "string".into()),
        ("description", desc.into()),
        (
            "enum",
            Value::Array(values.iter().map(|v| Value::from(*v)).collect()),
        ),
    ])
}

fn object(props: Vec<(&str, Value)>, required: &[&str]) -> Value {
    obj([
        ("type", "object".into()),
        (
            "properties",
            Value::Object(
                props
                    .into_iter()
                    .map(|(k, v)| (String::from(k), v))
                    .collect(),
            ),
        ),
        (
            "required",
            Value::Array(required.iter().map(|r| Value::from(*r)).collect()),
        ),
        ("additionalProperties", false.into()),
    ])
}

fn tool(
    name: &str,
    title: &str,
    desc: &str,
    input: Value,
    output: Value,
    read_only: bool,
    destructive: bool,
) -> Value {
    obj([
        ("name", name.into()),
        ("title", title.into()),
        ("description", desc.into()),
        ("inputSchema", input),
        ("outputSchema", output),
        (
            "annotations",
            obj([
                ("readOnlyHint", read_only.into()),
                ("destructiveHint", destructive.into()),
                ("openWorldHint", false.into()),
            ]),
        ),
    ])
}

const PATH: &str = "File name in the session's namespace. Absolute (/tmp/x), or relative to /tmp.";

fn entry_props() -> Vec<(&'static str, Value)> {
    vec![
        ("name", string("Last element of the file name.")),
        ("type", choice("Kind of file.", &["file", "dir"])),
        (
            "size",
            integer("Length in bytes (0 for directories).", 0, None),
        ),
        (
            "mode",
            string("Permission bits in octal, e.g. 644. Owner, group, other."),
        ),
        (
            "append_only",
            boolean("True if writes always go to the end (Inferno DMAPPEND)."),
        ),
        (
            "mtime",
            integer("Last modification, seconds since 1970.", 0, None),
        ),
    ]
}

/// Every tool, in the order `tools/list` gives them.
pub fn tools() -> Vec<Value> {
    let entry_required = ["name", "type", "size", "mode", "append_only", "mtime"];
    let mut stat_props = entry_props();
    stat_props.extend([
        ("path", string("The full name, cleaned.")),
        (
            "atime",
            integer("Last access, seconds since 1970.", 0, None),
        ),
        ("uid", string("Owner.")),
        ("gid", string("Group.")),
        ("muid", string("User who last modified the file.")),
    ]);
    vec![
        tool(
            "run",
            "Run a shell command",
            "Run one command line with Inferno sh inside the session sandbox and wait for it to \
             finish. Inferno sh is NOT bash: quote with single quotes only ('it''s' doubles the \
             quote), group with { }, lists are (a b c), the exit status is a string in $status \
             (empty means success), and if/for/while exist only after `load std` \
             (`load std; if {test -f x} {echo yes} {echo no}`). There is no $(...), no \
             double-quote quoting, no && inside [ ], no 2>&1 (write >[2=1]). Substitute a \
             command with `{cmd}. The server instructions hold a longer primer. Changes to the \
             namespace (bind, mount) and to environment variables persist for later calls; the \
             working directory does not. Programs still running when the command line ends keep \
             running until `reset` or the end of the session, but lose their output; start such \
             programs with job_start instead. Returns \
             stdout and stderr as text (bytes that are not UTF-8 become U+FFFD; use read_file \
             for binary data), the exit status string, and flags telling whether the command \
             timed out and was killed or its output was cut at the cap.",
            object(
                vec![
                    (
                        "command",
                        string("The sh command line. It may hold several commands and newlines."),
                    ),
                    (
                        "stdin",
                        string(
                            "Text given to the command on standard input. Without it, standard input is empty.",
                        ),
                    ),
                    (
                        "cwd",
                        string("Directory to run in, for this call only. Default /tmp."),
                    ),
                    (
                        "timeout_ms",
                        integer(
                            "Kill the command and everything it started after this many milliseconds. Default 30000.",
                            1,
                            Some(MAX_TIMEOUT_MS),
                        ),
                    ),
                    (
                        "max_output_bytes",
                        integer(
                            "Keep at most this many bytes of stdout and of stderr; the rest is dropped and the truncated flag set. Default 65536.",
                            0,
                            Some(MAX_OUTPUT),
                        ),
                    ),
                ],
                &["command"],
            ),
            object(
                vec![
                    ("stdout", string("Standard output, up to the cap.")),
                    (
                        "stderr",
                        string("Standard error (diagnostics), up to the cap."),
                    ),
                    (
                        "status",
                        string(
                            "Inferno exit status: empty for success, otherwise text such as fail:errors or killed.",
                        ),
                    ),
                    (
                        "ok",
                        boolean("True if the status is empty and the command did not time out."),
                    ),
                    (
                        "timed_out",
                        boolean("True if the timeout expired and the command was killed."),
                    ),
                    (
                        "stdout_truncated",
                        boolean("True if stdout was longer than the cap."),
                    ),
                    (
                        "stderr_truncated",
                        boolean("True if stderr was longer than the cap."),
                    ),
                ],
                &[
                    "stdout",
                    "stderr",
                    "status",
                    "ok",
                    "timed_out",
                    "stdout_truncated",
                    "stderr_truncated",
                ],
            ),
            false,
            true,
        ),
        tool(
            "read_file",
            "Read a file",
            "Read bytes from a file, starting at `offset`. The content comes back as text when \
             the bytes are valid UTF-8 and as base64 otherwise; `encoding` says which. A read \
             that would end inside a multi-byte character stops before it. To read a large file, \
             call again with offset set to the previous offset plus `bytes`, until `eof` is true.",
            object(
                vec![
                    ("path", string(PATH)),
                    (
                        "offset",
                        integer("Byte offset to start at. Default 0.", 0, None),
                    ),
                    (
                        "length",
                        integer("Most bytes to read. Default 65536.", 0, Some(MAX_READ)),
                    ),
                    (
                        "encoding",
                        choice(
                            "auto (default): text if valid UTF-8, else base64. base64: always base64.",
                            &["auto", "base64"],
                        ),
                    ),
                ],
                &["path"],
            ),
            object(
                vec![
                    ("content", string("The bytes read, as text or base64.")),
                    (
                        "encoding",
                        choice("How content is encoded.", &["utf8", "base64"]),
                    ),
                    ("offset", integer("Offset the read started at.", 0, None)),
                    (
                        "bytes",
                        integer(
                            "Number of bytes read (before any base64 encoding).",
                            0,
                            None,
                        ),
                    ),
                    (
                        "size",
                        integer("Length of the file in bytes, as stat reports it.", 0, None),
                    ),
                    (
                        "eof",
                        boolean("True if the read reached the end of the file."),
                    ),
                ],
                &["content", "encoding", "offset", "bytes", "size", "eof"],
            ),
            true,
            false,
        ),
        tool(
            "write_file",
            "Write a file",
            "Write `content` to a file. mode truncate (default) replaces the file or creates \
             it; create makes a new file and fails with `file already exists` if there is one; \
             append adds to the end, creating the file if needed. The directory must exist \
             (make it with run: mkdir -p dir). Use encoding base64 for bytes that are not \
             UTF-8 text.",
            object(
                vec![
                    ("path", string(PATH)),
                    (
                        "content",
                        string("What to write: text, or base64 when encoding is base64."),
                    ),
                    (
                        "encoding",
                        choice("How content is encoded. Default utf8.", &["utf8", "base64"]),
                    ),
                    (
                        "mode",
                        choice(
                            "truncate (default), create or append.",
                            &["truncate", "create", "append"],
                        ),
                    ),
                ],
                &["path", "content"],
            ),
            object(
                vec![
                    ("bytes", integer("Number of bytes written.", 0, None)),
                    ("size", integer("Length of the file afterwards.", 0, None)),
                ],
                &["bytes", "size"],
            ),
            false,
            true,
        ),
        tool(
            "list_dir",
            "List a directory",
            "List the entries of a directory, sorted by name, with type, size, permission bits \
             and modification time. A union directory lists every member, so a name can appear \
             twice.",
            object(vec![("path", string(PATH))], &["path"]),
            object(
                vec![
                    (
                        "entries",
                        obj([
                            ("type", "array".into()),
                            ("items", object(entry_props(), &entry_required)),
                        ]),
                    ),
                    (
                        "truncated",
                        boolean("True if the directory has more entries than were returned."),
                    ),
                ],
                &["entries", "truncated"],
            ),
            true,
            false,
        ),
        tool(
            "stat",
            "Describe a file",
            "Return the Inferno directory entry of a file or directory: type, size, permission \
             bits, times, owner and group. Fails with `file does not exist` when there is no \
             such file, which makes it the way to test for one.",
            object(vec![("path", string(PATH))], &["path"]),
            object(
                stat_props,
                &[
                    "name",
                    "type",
                    "size",
                    "mode",
                    "append_only",
                    "mtime",
                    "path",
                    "atime",
                    "uid",
                    "gid",
                    "muid",
                ],
            ),
            true,
            false,
        ),
        tool(
            "remove",
            "Remove a file or directory",
            "Remove a file or an empty directory. With recursive true, remove a directory and \
             everything in it. Files outside the writable parts of the sandbox (such as /bin \
             and /lib) fail with `permission denied`.",
            object(
                vec![
                    ("path", string(PATH)),
                    (
                        "recursive",
                        boolean("Also remove the contents of a directory. Default false."),
                    ),
                ],
                &["path"],
            ),
            object(
                vec![(
                    "removed",
                    integer("Number of files and directories removed.", 0, None),
                )],
                &["removed"],
            ),
            false,
            true,
        ),
        tool(
            "job_start",
            "Start a background job",
            "Start a long-running or interactive program and return at once with its job id. \
             Give `command`, an sh command line (the same Inferno sh as run), or `argv`, a \
             program and its arguments run without sh. The job runs in the session's namespace \
             with pipes for its standard input, output and error. Read its output with \
             job_read, feed its input with job_write, stop it with job_kill. Jobs end with \
             reset and with the session.",
            object(
                vec![
                    ("command", string("An sh command line. Give this or argv.")),
                    (
                        "argv",
                        obj([
                            ("type", "array".into()),
                            (
                                "description",
                                "A program and its arguments, such as [\"cat\", \"-\"]. \
                                 The program is found as sh finds it (cat is /bin/cat.wasm). \
                                 Give this or command."
                                    .into(),
                            ),
                            ("items", obj([("type", "string".into())])),
                        ]),
                    ),
                    (
                        "stdin",
                        string(
                            "Text written to the job's standard input first. More can follow with job_write.",
                        ),
                    ),
                    ("cwd", string("Directory to start in. Default /tmp.")),
                ],
                &[],
            ),
            object(
                vec![
                    (
                        "id",
                        integer("The job id, for the other job tools.", 1, None),
                    ),
                    (
                        "pid",
                        integer("The job's prog id, as ps and /prog show it.", 1, None),
                    ),
                ],
                &["id", "pid"],
            ),
            false,
            true,
        ),
        tool(
            "job_read",
            "Read a job's output",
            "Return a job's output from an offset, and whether it has exited. Offsets count \
             bytes from the start of each stream; by default a read starts where the last one \
             ended, so calling job_read repeatedly returns new output only. With wait_ms, wait \
             up to that long for output or for the job to exit; other calls are answered \
             meanwhile. The server keeps the last 1 MiB of each stream: older output is \
             dropped and counted in `lost`, and the job is never stopped by output nobody \
             reads.",
            object(
                vec![
                    ("id", integer("The job id from job_start.", 1, None)),
                    (
                        "offset",
                        integer(
                            "Where to start in standard output. Default: where the last job_read ended.",
                            0,
                            None,
                        ),
                    ),
                    (
                        "stderr_offset",
                        integer(
                            "Where to start in standard error. Default: where the last job_read ended.",
                            0,
                            None,
                        ),
                    ),
                    (
                        "wait_ms",
                        integer(
                            "If there is no new output, wait up to this many milliseconds for some, or for the exit. Default 0.",
                            0,
                            Some(MAX_TIMEOUT_MS),
                        ),
                    ),
                    (
                        "max_bytes",
                        integer(
                            "Most bytes of each stream to return. Default 65536.",
                            0,
                            Some(MAX_OUTPUT),
                        ),
                    ),
                ],
                &["id"],
            ),
            object(
                vec![
                    (
                        "stdout",
                        string(
                            "Standard output from `offset`. Bytes that are not UTF-8 become U+FFFD.",
                        ),
                    ),
                    ("stderr", string("Standard error from `stderr_offset`.")),
                    (
                        "offset",
                        integer("Offset of the first byte of stdout returned.", 0, None),
                    ),
                    (
                        "next_offset",
                        integer("Offset after the last byte of stdout returned.", 0, None),
                    ),
                    (
                        "stderr_offset",
                        integer("Offset of the first byte of stderr returned.", 0, None),
                    ),
                    (
                        "next_stderr_offset",
                        integer("Offset after the last byte of stderr returned.", 0, None),
                    ),
                    (
                        "lost",
                        integer(
                            "Bytes of stdout before `offset` that were asked for but already dropped.",
                            0,
                            None,
                        ),
                    ),
                    ("stderr_lost", integer("The same for stderr.", 0, None)),
                    (
                        "running",
                        boolean("True until the job has exited and its output is all collected."),
                    ),
                    (
                        "status",
                        string(
                            "Inferno exit status once it has exited: empty for success, otherwise text such as fail:errors or killed.",
                        ),
                    ),
                    (
                        "ok",
                        boolean("True if the job has exited with an empty status."),
                    ),
                ],
                &[
                    "stdout",
                    "stderr",
                    "offset",
                    "next_offset",
                    "stderr_offset",
                    "next_stderr_offset",
                    "lost",
                    "stderr_lost",
                    "running",
                    "status",
                    "ok",
                ],
            ),
            true,
            false,
        ),
        tool(
            "job_write",
            "Write to a job's input",
            "Queue `data` for a job's standard input and return at once; the bytes reach the \
             job as it reads them, in order. With eof true, close the input after the data, so \
             the job reads end of file.",
            object(
                vec![
                    ("id", integer("The job id from job_start.", 1, None)),
                    (
                        "data",
                        string(
                            "What to write: text, or base64 when encoding is base64. Default empty.",
                        ),
                    ),
                    (
                        "encoding",
                        choice("How data is encoded. Default utf8.", &["utf8", "base64"]),
                    ),
                    (
                        "eof",
                        boolean("Close the job's input after this data. Default false."),
                    ),
                ],
                &["id"],
            ),
            object(
                vec![
                    ("queued", integer("Number of bytes queued.", 0, None)),
                    (
                        "eof",
                        boolean(
                            "True if the input is closed or will be once the queue is written.",
                        ),
                    ),
                ],
                &["queued", "eof"],
            ),
            false,
            false,
        ),
        tool(
            "job_kill",
            "Stop a job",
            "Kill a job and every program it started. job_read then reports the exit, with \
             status killed. A job that has already exited is left as it is.",
            object(
                vec![("id", integer("The job id from job_start.", 1, None))],
                &["id"],
            ),
            object(
                vec![(
                    "killed",
                    boolean("True if the job was running and has been sent the kill."),
                )],
                &["killed"],
            ),
            false,
            true,
        ),
        tool(
            "job_list",
            "List jobs",
            "List the session's jobs, running and exited, with their ids, commands and exit \
             status. Exited jobs are kept until there are more than 32 of them.",
            object(Vec::new(), &[]),
            object(
                vec![(
                    "jobs",
                    obj([
                        ("type", "array".into()),
                        (
                            "items",
                            object(
                                vec![
                                    ("id", integer("Job id.", 1, None)),
                                    ("pid", integer("Prog id.", 1, None)),
                                    (
                                        "command",
                                        string("The command line, or the argv joined with spaces."),
                                    ),
                                    ("running", boolean("True until the job has exited.")),
                                    ("status", string("Exit status once exited.")),
                                    ("stdout_bytes", integer("Bytes of stdout so far.", 0, None)),
                                    ("stderr_bytes", integer("Bytes of stderr so far.", 0, None)),
                                ],
                                &[
                                    "id",
                                    "pid",
                                    "command",
                                    "running",
                                    "status",
                                    "stdout_bytes",
                                    "stderr_bytes",
                                ],
                            ),
                        ),
                    ]),
                )],
                &["jobs"],
            ),
            true,
            false,
        ),
        tool(
            "ns",
            "Show the namespace",
            "Return the session's namespace as Inferno's ns command prints it: one bind or \
             mount command per line, in the order that rebuilds it.",
            object(Vec::new(), &[]),
            object(
                vec![("ns", string("The namespace, as ns prints it."))],
                &["ns"],
            ),
            true,
            false,
        ),
        tool(
            "bind",
            "Bind a file or directory",
            "Make `to` refer to `from` in the session's namespace, as Inferno's bind does. \
             order replace (default) hides what was at `to`; before and after make a union \
             directory with `from` searched first or last. create lets new files be made in \
             `from` through the union. Devices (#U, #C, #I ...) cannot be named: the session \
             has given up the right to attach them, and binding one fails with \
             `mount/attach disallowed`. Lasts until reset.",
            object(
                vec![
                    ("from", string("The existing file or directory.")),
                    ("to", string("Where it appears.")),
                    (
                        "order",
                        choice(
                            "replace (default), before or after.",
                            &["replace", "before", "after"],
                        ),
                    ),
                    (
                        "create",
                        boolean("Allow creation in from through to. Default false."),
                    ),
                ],
                &["from", "to"],
            ),
            object(vec![("ok", boolean("Always true."))], &["ok"]),
            false,
            false,
        ),
        tool(
            "mount",
            "Mount a file server",
            "Mount a 9P (Styx) file server on `to` in the session's namespace, as Inferno's \
             mount command does. `source` is a dial string (tcp!host!port, which needs /net \
             granted to the session) or a {command} block run by sh with a pipe as its \
             standard input and output, such as {memfs -s}. The mount authenticates when the \
             server asks, unless authenticate is false. Lasts until reset.",
            object(
                vec![
                    ("source", string("A dial string or a {command} block.")),
                    ("to", string("The mount point.")),
                    ("spec", string("The attach name (aname). Default empty.")),
                    (
                        "order",
                        choice(
                            "replace (default), before or after.",
                            &["replace", "before", "after"],
                        ),
                    ),
                    (
                        "create",
                        boolean("Allow creation through the mount point. Default false."),
                    ),
                    (
                        "authenticate",
                        boolean("Authenticate if the server asks. Default true."),
                    ),
                    (
                        "timeout_ms",
                        integer(
                            "Give up after this many milliseconds. Default 30000.",
                            1,
                            Some(MAX_TIMEOUT_MS),
                        ),
                    ),
                ],
                &["source", "to"],
            ),
            object(vec![("ok", boolean("Always true."))], &["ok"]),
            false,
            false,
        ),
        tool(
            "unmount",
            "Undo a bind or mount",
            "Remove `from` from the union at `to`, or with no `from`, everything bound or \
             mounted at `to`, as Inferno's unmount does.",
            object(
                vec![
                    (
                        "from",
                        string("The member to remove. Default: all of them."),
                    ),
                    ("to", string("The mount point.")),
                ],
                &["to"],
            ),
            object(vec![("ok", boolean("Always true."))], &["ok"]),
            false,
            true,
        ),
        tool(
            "reset",
            "Start a fresh session",
            "Kill every program the session started and rebuild its namespace: /tmp is empty \
             again, and binds, mounts and environment variables made by earlier calls are gone. \
             Use it to recover from a broken namespace or to start a task from a clean state.",
            object(Vec::new(), &[]),
            object(
                vec![("reset", boolean("Always true: the session is new."))],
                &["reset"],
            ),
            false,
            true,
        ),
    ]
}

/// Checks `v` against `schema`. Understands `type` (object, string,
/// integer, boolean, array), `properties`, `required`,
/// `additionalProperties: false`, `enum`, `minimum`, `maximum` and
/// `items`, which is all the tool schemas use. `at` names the value in the
/// error text.
pub fn validate(v: &Value, schema: &Value, at: &str) -> Result<(), String> {
    let want = schema.get("type").and_then(Value::as_str).unwrap_or("");
    let ok = match want {
        "object" => v.is_object(),
        "string" => v.as_str().is_some(),
        "integer" => v.as_i64().is_some(),
        "boolean" => v.as_bool().is_some(),
        "array" => matches!(v, Value::Array(_)),
        _ => true,
    };
    if !ok {
        return Err(format!(
            "{at} must be of type {want}, not {}",
            v.type_name()
        ));
    }
    if let Some(Value::Array(allowed)) = schema.get("enum")
        && !allowed.contains(v)
    {
        let list: Vec<String> = allowed.iter().map(Value::encode).collect();
        return Err(format!("{at} must be one of {}", list.join(", ")));
    }
    if let Some(n) = v.as_i64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_i64)
            && n < min
        {
            return Err(format!("{at} must be at least {min}"));
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_i64)
            && n > max
        {
            return Err(format!("{at} must be at most {max}"));
        }
    }
    if let (Value::Array(items), Some(item)) = (v, schema.get("items")) {
        for (i, x) in items.iter().enumerate() {
            validate(x, item, &format!("{at}[{i}]"))?;
        }
    }
    if let Value::Object(members) = v {
        let props = schema.get("properties");
        if let Some(Value::Array(required)) = schema.get("required") {
            for r in required.iter().filter_map(Value::as_str) {
                if v.get(r).is_none() {
                    return Err(format!("{at}.{r} is required"));
                }
            }
        }
        let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
        for (k, x) in members {
            match props.and_then(|p| p.get(k)) {
                Some(s) => validate(x, s, &format!("{at}.{k}"))?,
                None if closed => return Err(format!("{at}.{k} is not a known property")),
                None => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn definitions_are_well_formed() {
        let tools = tools();
        assert_eq!(tools.len(), 16);
        for t in &tools {
            let name = t.get("name").unwrap().as_str().unwrap();
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{name}"
            );
            assert!(t.get("description").unwrap().as_str().unwrap().len() > 40);
            for key in ["inputSchema", "outputSchema"] {
                let s = t.get(key).unwrap();
                assert_eq!(s.get("type").unwrap().as_str(), Some("object"), "{name}");
                // Every required name is a declared property.
                let Some(Value::Array(req)) = s.get("required") else {
                    panic!("{name}")
                };
                for r in req {
                    assert!(
                        s.get("properties")
                            .unwrap()
                            .get(r.as_str().unwrap())
                            .is_some(),
                        "{name}.{key}: {}",
                        r.encode()
                    );
                }
            }
        }
    }

    #[test]
    fn validation() {
        let schema = tools()[0].get("inputSchema").unwrap().clone();
        let ok = |s: &str| validate(&parse(s).unwrap(), &schema, "arguments");
        assert_eq!(ok(r#"{"command":"ls"}"#), Ok(()));
        assert_eq!(
            ok(r#"{"command":"ls","stdin":"","cwd":"/","timeout_ms":600000,"max_output_bytes":0}"#),
            Ok(())
        );
        assert_eq!(ok(r#"{"command":"ls","timeout_ms":5.0}"#), Ok(()));
        assert_eq!(ok("{}").unwrap_err(), "arguments.command is required");
        assert_eq!(
            ok(r#"{"command":1}"#).unwrap_err(),
            "arguments.command must be of type string, not integer"
        );
        assert_eq!(
            ok(r#"{"command":"x","timeout_ms":600001}"#).unwrap_err(),
            "arguments.timeout_ms must be at most 600000"
        );
        assert_eq!(
            ok(r#"{"command":"x","timeout_ms":1.5}"#).unwrap_err(),
            "arguments.timeout_ms must be of type integer, not number"
        );
        assert_eq!(
            ok(r#"{"command":"x","shell":"bash"}"#).unwrap_err(),
            "arguments.shell is not a known property"
        );
        assert_eq!(
            ok("[]").unwrap_err(),
            "arguments must be of type object, not array"
        );
        let write = tools()[2].get("inputSchema").unwrap().clone();
        assert_eq!(
            validate(
                &parse(r#"{"path":"a","content":"","mode":"w"}"#).unwrap(),
                &write,
                "arguments"
            )
            .unwrap_err(),
            r#"arguments.mode must be one of "truncate", "create", "append""#
        );
    }
}
