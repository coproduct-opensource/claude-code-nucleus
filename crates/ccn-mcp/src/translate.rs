//! The two alphabets are the same; the two vocabularies are not.
//!
//! `docs/design.md` is right that the built-ins and the proxy's routes line up
//! almost exactly — and that is a claim about *routes*. The **bodies** do not
//! line up at all. `Read` sends `file_path`; `/v1/read` deserialises `path` and
//! answers `422 Unprocessable Entity` to anything else. Every `read`, `write`
//! and `glob` a model made through this bridge failed that way, and `run` could
//! not succeed under any spelling, because `/v1/run` takes `args: Vec<String>`
//! and there is no shell inside the pod to turn a command string into one.
//!
//! So this module exists, and its whole content is a translation table plus the
//! one piece of real work that table cannot express: turning a command *string*
//! into an argument *vector*.
//!
//! ## Why translate here rather than teach the model the proxy's names
//!
//! The gate denies `Read` and names `mcp__nucleus__read` as the replacement. The
//! model has a built-in's argument names in hand at that moment. Making it also
//! learn a second vocabulary to act on the redirect adds a step to the one path
//! the bridge needs to be frictionless, and the mapping is mechanical. The
//! schemas in `main.rs` therefore keep the familiar names and this module makes
//! them true.
//!
//! The exception is `run`, where the difference is not vocabulary but
//! **semantics**, and hiding it would be worse than exposing it — see
//! [`tokenize`].

use serde_json::{json, Map, Value};

/// Translate one `tools/call` argument object into the body its route expects.
///
/// Fails rather than guesses. A body this function is not sure about is a `422`
/// the model cannot read, or — worse for `run` — a command that executes with a
/// meaning the caller did not intend.
pub fn to_proxy_body(tool: &str, args: &Value) -> Result<Value, String> {
    let empty = Map::new();
    let a = args.as_object().unwrap_or(&empty);

    match tool {
        // `/v1/read` takes `path` and nothing else. `offset`/`limit` have no
        // analogue: the route returns the whole file. They are absent from the
        // advertised schema for that reason, and ignored here rather than
        // rejected, so a model reaching for them by habit still gets its read.
        "read" => Ok(json!({ "path": require_str(a, "file_path", "read")? })),

        "write" => Ok(json!({
            "path": require_str(a, "file_path", "write")?,
            "contents": require_str(a, "content", "write")?,
        })),

        // `path` is the built-in's name for it; the route calls the same thing
        // `directory`.
        "glob" => {
            let mut body = json!({ "pattern": require_str(a, "pattern", "glob")? });
            copy_str(a, "path", &mut body, "directory");
            copy_u64(a, "max_results", &mut body, "max_results");
            Ok(body)
        }

        // The one route whose names already agree. `glob` is `#[serde(rename)]`d
        // on the far side to exactly this spelling.
        "grep" => {
            let mut body = json!({ "pattern": require_str(a, "pattern", "grep")? });
            copy_str(a, "path", &mut body, "path");
            copy_str(a, "glob", &mut body, "glob");
            Ok(body)
        }

        // `/v1/web_fetch` returns the response; it does not summarise one, so
        // the built-in's `prompt` has no image here and is dropped rather than
        // forwarded into a field the route does not read.
        "web_fetch" => Ok(json!({ "url": require_str(a, "url", "web_fetch")? })),

        "web_search" => {
            let mut body = json!({ "query": require_str(a, "query", "web_search")? });
            copy_u64(a, "max_results", &mut body, "max_results");
            Ok(body)
        }

        "run" => {
            let command = require_str(a, "command", "run")?;
            let mut body = json!({ "args": tokenize(&command)? });
            copy_str(a, "directory", &mut body, "directory");
            copy_str(a, "stdin", &mut body, "stdin");
            // Milliseconds are the built-in's unit; seconds are the route's.
            // Round up, because rounding a 500 ms budget down to zero is a
            // timeout the caller did not ask for.
            if let Some(ms) = a.get("timeout_ms").and_then(Value::as_u64) {
                body["timeout_seconds"] = json!(ms.div_ceil(1000).max(1));
            }
            Ok(body)
        }

        other => Err(format!("`{other}` has no request translation")),
    }
}

fn require_str(a: &Map<String, Value>, key: &str, tool: &str) -> Result<String, String> {
    a.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("`{tool}` requires a string `{key}`"))
}

fn copy_str(a: &Map<String, Value>, from: &str, body: &mut Value, to: &str) {
    if let Some(v) = a.get(from).and_then(Value::as_str) {
        body[to] = json!(v);
    }
}

fn copy_u64(a: &Map<String, Value>, from: &str, body: &mut Value, to: &str) {
    if let Some(v) = a.get(from).and_then(Value::as_u64) {
        body[to] = json!(v);
    }
}

/// Split a command string into the argument vector `/v1/run` executes.
///
/// **There is no shell inside the pod.** `/v1/run` spawns `args[0]` with
/// `args[1..]` directly — the array form is what makes shell injection
/// unexpressible rather than filtered — and nucleus's default command policy
/// blocks `sh -c`, `bash -c` and their siblings outright, so the obvious
/// workaround of wrapping the string in a shell is both unavailable and
/// contrary to the point.
///
/// That leaves one honest option and one dishonest one. The dishonest one is to
/// split on whitespace and let `ls | wc -l` run `ls` with the literal arguments
/// `|` and `wc` — a command that "succeeds" having done something the caller did
/// not ask for. This function takes the other one: quoting and escaping are
/// honoured, and anything whose meaning requires a shell is **refused with the
/// reason**, so the model can reach for the tool that does express it.
pub fn tokenize(command: &str) -> Result<Vec<String>, String> {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut have_word = false;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if have_word {
                    words.push(std::mem::take(&mut word));
                    have_word = false;
                }
            }

            // A backslash escapes the next character, which is how a caller
            // spells a literal metacharacter without quoting the whole word.
            '\\' => match chars.next() {
                Some(next) => {
                    word.push(next);
                    have_word = true;
                }
                None => return Err(unsupported("a trailing backslash", "nothing to escape")),
            },

            // Single quotes are literal all the way through — no expansion, no
            // escapes — so their contents need no inspection.
            '\'' => {
                have_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err(unbalanced('\'')),
                    }
                }
            }

            // Double quotes suppress splitting and globbing but *not* expansion,
            // so `$` and a backtick stay refusable inside them.
            '"' => {
                have_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(next) => word.push(next),
                            None => return Err(unbalanced('"')),
                        },
                        Some('$') => return Err(expansion('$')),
                        Some('`') => return Err(expansion('`')),
                        Some(c) => word.push(c),
                        None => return Err(unbalanced('"')),
                    }
                }
            }

            // Everything below is unquoted and means something to a shell that
            // it cannot mean here.
            '|' | '&' | ';' => {
                let op: String = std::iter::once(c)
                    .chain(chars.peek().filter(|n| **n == c).copied())
                    .collect();
                return Err(unsupported(
                    &format!("`{op}`"),
                    "run each command in its own call and combine the results yourself",
                ));
            }
            '<' | '>' => {
                return Err(unsupported(
                    &format!("redirection (`{c}`)"),
                    "use `write` to create a file, or `read` to consume one",
                ))
            }
            '(' | ')' => {
                return Err(unsupported(
                    "a subshell",
                    "run the inner command in its own call",
                ))
            }
            '$' | '`' => return Err(expansion(c)),
            '*' | '?' | '[' => {
                return Err(unsupported(
                    &format!("an unquoted glob (`{c}`)"),
                    "call `glob` to expand the pattern, then pass the paths it returns",
                ))
            }
            '~' if !have_word => {
                return Err(unsupported(
                    "a leading `~`",
                    "write the path out — the pod's filesystem is not your home directory",
                ))
            }

            c => {
                word.push(c);
                have_word = true;
            }
        }
    }

    if have_word {
        words.push(word);
    }
    if words.is_empty() {
        return Err("`run` was given an empty command".into());
    }
    Ok(words)
}

fn unsupported(what: &str, instead: &str) -> String {
    format!(
        "there is no shell inside the pod, so {what} cannot be honoured — \
         `run` executes one program with arguments. Instead: {instead}."
    )
}

fn expansion(c: char) -> String {
    unsupported(
        &format!("shell expansion (`{c}`)"),
        "substitute the value yourself, or quote the character with a backslash to pass it through \
         literally",
    )
}

fn unbalanced(q: char) -> String {
    format!("the command has an unbalanced {q} quote")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this module was written for: every one of these sent a field
    /// name the route does not deserialise, and every one of them was a 422.
    #[test]
    fn the_builtins_names_become_the_routes_names() {
        assert_eq!(
            to_proxy_body("read", &json!({ "file_path": "src/a.rs" })).unwrap(),
            json!({ "path": "src/a.rs" })
        );
        assert_eq!(
            to_proxy_body("write", &json!({ "file_path": "a.txt", "content": "hi" })).unwrap(),
            json!({ "path": "a.txt", "contents": "hi" })
        );
        assert_eq!(
            to_proxy_body("glob", &json!({ "pattern": "*.rs", "path": "src" })).unwrap(),
            json!({ "pattern": "*.rs", "directory": "src" })
        );
    }

    /// `grep`, `web_search` and `url` already agreed. Pinned so a later tidy-up
    /// does not "fix" them into disagreement.
    #[test]
    fn the_routes_that_already_agreed_are_left_alone() {
        assert_eq!(
            to_proxy_body("grep", &json!({ "pattern": "fn main", "glob": "*.rs" })).unwrap(),
            json!({ "pattern": "fn main", "glob": "*.rs" })
        );
        assert_eq!(
            to_proxy_body("web_search", &json!({ "query": "firecracker" })).unwrap(),
            json!({ "query": "firecracker" })
        );
    }

    /// The route answers with the response body; it does not summarise it. A
    /// `prompt` forwarded into a field nothing reads would be a silent lie
    /// about what the call did.
    #[test]
    fn web_fetch_drops_the_prompt_the_route_cannot_honour() {
        assert_eq!(
            to_proxy_body(
                "web_fetch",
                &json!({ "url": "https://x", "prompt": "summarise" })
            )
            .unwrap(),
            json!({ "url": "https://x" })
        );
    }

    #[test]
    fn a_command_string_becomes_an_argument_vector() {
        assert_eq!(
            to_proxy_body("run", &json!({ "command": "cargo test --workspace" })).unwrap(),
            json!({ "args": ["cargo", "test", "--workspace"] })
        );
    }

    #[test]
    fn milliseconds_become_seconds_and_never_round_down_to_zero() {
        let body = to_proxy_body("run", &json!({ "command": "id", "timeout_ms": 1 })).unwrap();
        assert_eq!(body["timeout_seconds"], json!(1));
        let body = to_proxy_body("run", &json!({ "command": "id", "timeout_ms": 2500 })).unwrap();
        assert_eq!(body["timeout_seconds"], json!(3));
    }

    /// A missing required field becomes a message naming the field, rather than
    /// a 422 from the far side that names nothing the model can act on.
    #[test]
    fn a_missing_field_is_named_here_rather_than_422ing_over_there() {
        let e = to_proxy_body("write", &json!({ "file_path": "a.txt" })).unwrap_err();
        assert!(e.contains("content"), "{e}");
    }

    #[test]
    fn quoting_and_escaping_survive_tokenisation() {
        assert_eq!(
            tokenize(r#"git commit -m "two words""#).unwrap(),
            ["git", "commit", "-m", "two words"]
        );
        assert_eq!(
            tokenize(r#"grep 'fn .*(' src/lib.rs"#).unwrap(),
            ["grep", "fn .*(", "src/lib.rs"]
        );
        assert_eq!(tokenize(r"echo a\ b").unwrap(), ["echo", "a b"]);
        // A backslash-escaped metacharacter is a literal, not a refusal.
        assert_eq!(tokenize(r"echo \*").unwrap(), ["echo", "*"]);
    }

    /// The falsifier for the whole module. If any of these ever tokenises, the
    /// bridge has started executing a command whose meaning it changed: `ls |
    /// wc -l` would run `ls` against the literal arguments `|` and `wc`, report
    /// success, and have done something else entirely.
    #[test]
    fn shell_syntax_is_refused_with_a_reason_rather_than_passed_through_literally() {
        for command in [
            "ls | wc -l",
            "make && make test",
            "cargo build; cargo test",
            "echo hi > out.txt",
            "cat < in.txt",
            "echo $HOME",
            "echo `id`",
            "echo $(id)",
            "ls *.rs",
            "rm -rf / &",
        ] {
            let e = tokenize(command).unwrap_err();
            assert!(
                e.contains("no shell inside the pod"),
                "{command} was not refused with an explanation: {e}"
            );
        }
    }

    /// The refusal has to tell the model what to do instead, or it is just a
    /// dead end with better prose.
    #[test]
    fn a_refusal_names_the_tool_that_does_express_it() {
        assert!(tokenize("ls *.rs").unwrap_err().contains("`glob`"));
        assert!(tokenize("echo hi > f").unwrap_err().contains("`write`"));
    }

    #[test]
    fn an_unbalanced_quote_is_an_error_not_a_silent_truncation() {
        assert!(tokenize(r#"echo "unterminated"#).is_err());
        assert!(tokenize("echo 'unterminated").is_err());
        assert!(tokenize("echo").is_ok());
        assert!(tokenize("   ").is_err());
    }
}

/// The proxy's request shapes, transcribed from its `Deserialize` structs.
///
/// This bridge's correctness depends on field names in another repository, so
/// they are written down rather than remembered. See the file's own comment for
/// how it is kept honest.
#[cfg(test)]
const CONTRACT: &str = include_str!("../../../contracts/tool-proxy-requests.json");

#[cfg(test)]
mod contract {
    use super::*;
    use ccn_core::mediated_tools;

    /// A call of each tool with every argument the schema advertises, so the
    /// check below sees the widest body the translation can produce.
    fn widest_call(tool: &str) -> Option<Value> {
        Some(match tool {
            "read" => json!({ "file_path": "a.txt", "offset": 1, "limit": 2 }),
            "write" => json!({ "file_path": "a.txt", "content": "x" }),
            "run" => json!({
                "command": "id", "directory": "src", "stdin": "x", "timeout_ms": 1000
            }),
            "glob" => json!({ "pattern": "*.rs", "path": "src", "max_results": 10 }),
            "grep" => json!({ "pattern": "fn", "path": "src", "glob": "*.rs" }),
            "web_fetch" => json!({ "url": "https://x", "prompt": "p" }),
            "web_search" => json!({ "query": "q", "max_results": 5 }),
            // Not a route this bridge translates to; see `ccn_core`.
            _ => return None,
        })
    }

    /// The falsifier for the whole translation, and the one that would have
    /// caught the original defect without a pod: a field the route does not
    /// deserialise is a 422, and a required field left out is the same 422.
    ///
    /// Checked against `contracts/tool-proxy-requests.json` rather than against
    /// a live pod, so it runs in CI with no network and no microVM.
    #[test]
    fn the_translation_emits_only_fields_the_route_deserialises() {
        let contract: Value = serde_json::from_str(CONTRACT).expect("contract is not valid JSON");
        let routes = &contract["routes"];

        for (tool, route) in mediated_tools() {
            let Some(args) = widest_call(tool) else {
                continue;
            };
            let spec = &routes[route.0];
            assert!(
                spec.is_object(),
                "`{tool}` posts to {route}, which the contract does not describe"
            );

            let strings = |key: &str| -> Vec<String> {
                spec[key]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let required = strings("required");
            let mut known = required.clone();
            known.extend(strings("optional"));

            let body = to_proxy_body(tool, &args)
                .unwrap_or_else(|e| panic!("`{tool}` would not translate its own schema: {e}"));
            let body = body.as_object().expect("a body is an object");

            for field in body.keys() {
                assert!(
                    known.contains(field),
                    "`{tool}` sends `{field}` to {route}, which does not deserialise it — a 422"
                );
            }
            for field in &required {
                assert!(
                    body.contains_key(field),
                    "{route} requires `{field}` and `{tool}` does not send it — a 422"
                );
            }
        }
    }

    /// Every mediated route this bridge posts to must appear in the contract, or
    /// the check above silently skips it and proves nothing about it.
    #[test]
    fn the_contract_covers_every_route_the_bridge_posts_to() {
        let contract: Value = serde_json::from_str(CONTRACT).unwrap();
        for (tool, route) in mediated_tools() {
            if widest_call(tool).is_none() {
                continue;
            }
            assert!(
                contract["routes"][route.0].is_object(),
                "{route} is posted to by `{tool}` but is not in the contract"
            );
        }
    }
}
