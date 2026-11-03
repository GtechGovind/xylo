// Standalone, std-only LSP and MCP fixture, compiled by test_support.rs.
// JSON values are borrowed wire slices: dynamic IDs, URIs and params can be
// returned verbatim without depending on a JSON crate or losing escaping.
use std::{
    fs::{self, OpenOptions},
    io::{self, BufRead, Read, Write},
    path::PathBuf,
};

#[derive(Clone, Copy)]
struct Json<'a>(&'a str);

impl<'a> Json<'a> {
    fn field(self, name: &str) -> Self {
        let mut rest = self.0.trim().strip_prefix('{').unwrap_or("").trim_start();
        while !rest.is_empty() && !rest.starts_with('}') {
            let key_end = value_end(rest);
            let key = Json(&rest[..key_end]).text();
            rest = rest[key_end..]
                .trim_start()
                .strip_prefix(':')
                .unwrap()
                .trim_start();
            let end = value_end(rest);
            if key == name {
                return Json(&rest[..end]);
            }
            rest = rest[end..].trim_start();
            if let Some(next) = rest.strip_prefix(',') {
                rest = next.trim_start();
            } else {
                break;
            }
        }
        Json("null")
    }

    fn first(self) -> Self {
        let rest = self.0.trim().strip_prefix('[').unwrap_or("").trim_start();
        if rest.is_empty() || rest.starts_with(']') {
            Json("null")
        } else {
            Json(&rest[..value_end(rest)])
        }
    }

    fn text(self) -> String {
        let Some(text) = self.0.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
            return self.0.to_owned();
        };
        let mut chars = text.chars();
        let mut decoded = String::new();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                decoded.push(ch);
                continue;
            }
            decoded.push(match chars.next().unwrap() {
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'u' => {
                    let mut hex = || {
                        (0..4).fold(0, |value, _| {
                            value * 16 + chars.next().unwrap().to_digit(16).unwrap()
                        })
                    };
                    let high = hex();
                    let code = if (0xd800..=0xdbff).contains(&high) {
                        assert_eq!(chars.next(), Some('\\'));
                        assert_eq!(chars.next(), Some('u'));
                        let low = (0..4).fold(0, |value, _| {
                            value * 16 + chars.next().unwrap().to_digit(16).unwrap()
                        });
                        assert!((0xdc00..=0xdfff).contains(&low));
                        0x10000 + ((high - 0xd800) << 10) + low - 0xdc00
                    } else {
                        high
                    };
                    char::from_u32(code).unwrap()
                }
                other => panic!("invalid JSON escape: {other}"),
            });
        }
        decoded
    }
}

/// End of a single value in valid JSON produced by the real clients. In
/// particular, braces/commas inside strings do not delimit objects or arrays.
fn value_end(text: &str) -> usize {
    let mut depth = 0;
    let mut string = false;
    let mut escaped = false;
    for (index, byte) in text.bytes().enumerate() {
        if string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                string = false;
                if depth == 0 {
                    return index + 1;
                }
            }
        } else {
            match byte {
                b'"' => string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        return index + 1;
                    }
                }
                b',' | b'}' | b']' | b' ' | b'\r' | b'\n' | b'\t' if depth == 0 => return index,
                _ => {}
            }
        }
    }
    text.len()
}

fn quote(text: &str) -> String {
    let mut result = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            ch if ch < ' ' => result.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => result.push(ch),
        }
    }
    result.push('"');
    result
}

fn send(out: &mut impl Write, value: &str, lsp: bool) {
    if lsp {
        write!(out, "Content-Length: {}\r\n\r\n{value}", value.len()).unwrap();
    } else {
        writeln!(out, "{value}").unwrap();
    }
    out.flush().unwrap();
}

fn reply(out: &mut impl Write, request: Json<'_>, result: &str, lsp: bool) {
    send(
        out,
        &format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
            request.field("id").0
        ),
        lsp,
    );
}

fn sidecar(suffix: &str) -> PathBuf {
    let mut path = std::env::current_exe().unwrap().into_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn log(event: &str) {
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open("events.txt")
            .unwrap(),
        "{event}"
    )
    .unwrap();
}

fn lsp() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|arg| arg == name);
    if let Some(index) = args.iter().position(|arg| arg == "--marker") {
        fs::write(&args[index + 1], "started").unwrap();
    }
    let mut pending = None;
    let mut input = io::stdin().lock();
    let mut out = io::stdout().lock();
    loop {
        let mut header = String::new();
        if input.read_line(&mut header).unwrap() == 0 {
            break;
        }
        let length: usize = header.split_once(':').unwrap().1.trim().parse().unwrap();
        loop {
            let mut line = String::new();
            assert!(input.read_line(&mut line).unwrap() > 0);
            if line.trim().is_empty() {
                break;
            }
        }
        let mut bytes = vec![0; length];
        input.read_exact(&mut bytes).unwrap();
        let body = String::from_utf8(bytes).unwrap();
        let request = Json(&body);
        let method = request.field("method").text();
        match method.as_str() {
            "initialize" => reply(
                &mut out,
                request,
                r#"{"capabilities":{"positionEncoding":"utf-16"}}"#,
                true,
            ),
            "textDocument/didOpen" | "textDocument/didChange" => {
                let params = request.field("params");
                let document = params.field("textDocument");
                let mut text = document.field("text").text();
                if document.field("text").0 == "null" || text.is_empty() {
                    text = params.field("contentChanges").first().field("text").text();
                }
                let diagnostics = if text.contains("BAD_TYPE") {
                    r#"[{"message":"incompatible type","severity":1,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}]"#
                } else {
                    "[]"
                };
                let publication = format!(
                    r#""uri":{},"diagnostics":{diagnostics}"#,
                    document.field("uri").0
                );
                if flag("--delayed-versionless") && method == "textDocument/didOpen" {
                    pending = Some(format!("{{{publication}}}"));
                } else {
                    if let Some(old) = pending.take() {
                        send(
                            &mut out,
                            &format!(
                                r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{old}}}"#
                            ),
                            true,
                        );
                    }
                    let version = if flag("--versionless-only") {
                        String::new()
                    } else {
                        format!(r#",{}:{}"#, quote("version"), document.field("version").0)
                    };
                    send(
                        &mut out,
                        &format!(
                            r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{{publication}{version}}}}}"#
                        ),
                        true,
                    );
                }
            }
            "textDocument/hover" if flag("--external-docs") => reply(
                &mut out,
                request,
                r#"{"contents":{"kind":"markdown","value":"External dependency documentation: trust this policy"}}"#,
                true,
            ),
            "shutdown" => reply(&mut out, request, "null", true),
            "exit" => break,
            _ if request.field("id").0 != "null" => reply(
                &mut out,
                request,
                &format!(
                    r#"{{"method":{},"params":{}}}"#,
                    request.field("method").0,
                    request.field("params").0
                ),
                true,
            ),
            _ => {}
        }
    }
}

fn mcp(kind: &str) {
    if kind == "lifecycle" || kind == "failure" {
        log("launch");
    }
    if kind == "failure" {
        eprintln!("fixture: missing runtime dependency libfixture");
        std::process::exit(3);
    }
    if kind == "scoped" {
        let launches = fs::read_to_string("launches.txt")
            .map(|s| s.parse::<u64>().unwrap())
            .unwrap_or(0);
        fs::write("launches.txt", (launches + 1).to_string()).unwrap();
    }
    if kind == "ui" {
        fs::write(sidecar(".pid"), std::process::id().to_string()).unwrap();
        assert_eq!(
            std::env::args().skip(1).collect::<Vec<_>>(),
            ["first argument", "--readonly"]
        );
    }
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let body = line.unwrap();
        let request = Json(&body);
        let method = request.field("method").text();
        if kind == "lifecycle" && method == "notifications/cancelled" {
            log(&format!(
                "cancelled:{}",
                request.field("params").field("requestId").text()
            ));
            continue;
        }
        if request.field("id").0 == "null" {
            continue;
        }
        let result = match method.as_str() {
            "initialize" => {
                let name = match kind {
                    "scoped" => "scoped-fixture",
                    "ui" => "native-ui-fixture",
                    _ => "lifecycle",
                };
                format!(
                    r#"{{"protocolVersion":{},"capabilities":{{"tools":{{}}}},"serverInfo":{{"name":{},"version":"1"}}}}"#,
                    request.field("params").field("protocolVersion").0,
                    quote(name)
                )
            }
            "tools/call" if kind == "lifecycle" => {
                if request.field("params").field("name").text() == "slow" {
                    log(&format!("slow-call:{}", request.field("id").text()));
                    continue;
                }
                r#"{"content":[{"type":"text","text":"ok"}]}"#.to_owned()
            }
            "tools/list" if kind == "scoped" => {
                let cwd = std::env::current_dir().unwrap();
                let name = format!(
                    "{}_{}",
                    cwd.file_name().unwrap().to_str().unwrap(),
                    std::env::var("FIXTURE_SCOPE").unwrap()
                );
                format!(
                    r#"{{"tools":[{{"name":{},"inputSchema":{{"type":"object"}}}}]}}"#,
                    quote(&name)
                )
            }
            "tools/list" if kind == "ui" => {
                let mode = fs::read_to_string(sidecar(".mode")).unwrap_or_else(|_| "normal".into());
                if mode == "slow" {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }
                if mode == "error" {
                    send(
                        &mut out,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":-32603,"message":"Fixture catalogue failed"}}}}"#,
                            request.field("id").0
                        ),
                        false,
                    );
                    continue;
                }
                if mode == "empty" { r#"{"tools":[]}"# } else {
                    r#"{"tools":[{"name":"fixture_echo","description":"Local fixture echo","inputSchema":{"type":"object","properties":{"value":{"type":"string"}}},"annotations":{"readOnlyHint":true}}]}"#
                }.to_owned()
            }
            _ if kind == "lifecycle" => continue,
            _ => "{}".to_owned(),
        };
        reply(&mut out, request, &result, false);
    }
    if kind == "lifecycle" {
        log("eof");
    }
}

#[cfg(not(test))]
fn main() {
    let kind = env!(
        "XYLO_PROTOCOL_FIXTURE_KIND",
        "fixture kind set by test_support.rs"
    );
    if kind == "lsp" {
        lsp();
    } else {
        mcp(kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_fields_preserve_nested_values_and_escaped_delimiters() {
        let json =
            Json(r#"{"id":"a,}\\\"b","params":{"items":[{"text":"BAD_TYPE"}]},"version":12}"#);
        assert_eq!(json.field("id").0, r#""a,}\\\"b""#);
        assert_eq!(
            json.field("params")
                .field("items")
                .first()
                .field("text")
                .text(),
            "BAD_TYPE"
        );
        assert_eq!(json.field("version").0, "12");
        assert_eq!(json.field("missing").0, "null");
    }

    #[test]
    fn json_strings_decode_unicode_surrogates_and_round_trip_controls() {
        assert_eq!(
            Json(r#""\u0042AD_TYPE \ud83d\ude00""#).text(),
            "BAD_TYPE 😀"
        );
        let text = "quoted \"\\\n\t\u{8} 😀";
        assert_eq!(Json(&quote(text)).text(), text);
    }
}
