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
             running until `reset` or the end of the session, but lose their output. Returns \
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
        assert_eq!(tools.len(), 7);
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
