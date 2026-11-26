//! The command line.
//!
//! ```text
//! siphon                         open the application
//! siphon <address>...            hand addresses over, as the browser would
//! siphon add <address>... [--dir <folder>] [--name <file name>]
//!                         [--referer <page>] [--later] [--ask]
//! siphon queue start|stop        start or stop the queue of deferred downloads
//! siphon --help | --version
//! ```
//!
//! `add` adds without asking; `--ask` shows the confirmation dialog instead,
//! and `--later` sets the download aside for the queue. When Siphon is
//! already running the request is passed to it; otherwise it starts.

use std::path::PathBuf;

use siphon_core::ipc::Message;
use siphon_core::{Capture, Request, batch};

pub const USAGE: &str = "\
Siphon, a segmented download manager

Usage:
  siphon                          Open the application
  siphon <address>...             Hand addresses over; Siphon asks where to save
  siphon add <address>... [options]
                                  Add downloads without asking
  siphon queue start|stop         Start or stop the queue of deferred downloads
  siphon browsers                 List the browsers found here, and whether each
                                  can reach Siphon and has the extension
  siphon install [<folder>]       Copy this application into Applications
                                  (or into <folder>) and print where it is

Options for add:
  --dir <folder>                  Save into this folder
  --name <file name>              Save under this name (one address only)
  --referer <page>                The page the link came from
  --later                         Set aside for the queue instead of starting
  --ask                           Show the confirmation dialog

  -h, --help                      Show this text
  -V, --version                   Show the version

Addresses may use series: file[01-20].zip, part-{a,b,c}.bin
";

/// What the command line asks for.
#[derive(Debug, PartialEq)]
pub enum Invocation {
    /// Run (or reach) the application and deliver these requests.
    Run {
        messages: Vec<Message>,
        /// Bring the window forward: the user is waiting to see it.
        raise: bool,
    },
    Help,
    Version,
    WriteIcon(PathBuf),
    /// Copy the application into the Applications folder, or this one.
    Install(Option<PathBuf>),
    /// Say where this copy runs from and whether it should be moved.
    Where,
    /// List the browsers on this computer and how each stands.
    Browsers,
    Usage(String),
}

fn absolute(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() { path } else { std::env::current_dir().map(|dir| dir.join(&path)).unwrap_or(path) }
}

pub fn parse(args: &[String]) -> Invocation {
    let mut args = args.iter().map(String::as_str).peekable();
    match args.peek().copied() {
        None => return Invocation::Run { messages: Vec::new(), raise: true },
        Some("-h" | "--help" | "help") => return Invocation::Help,
        Some("-V" | "--version") => return Invocation::Version,
        Some("--write-icon") => {
            args.next();
            return match args.next() {
                Some(dir) => Invocation::WriteIcon(PathBuf::from(dir)),
                None => Invocation::Usage("--write-icon needs a folder".into()),
            };
        }
        Some("--where") => return Invocation::Where,
        Some("browsers") => {
            args.next();
            return if args.next().is_none() { Invocation::Browsers } else { Invocation::Usage("browsers takes no arguments".into()) };
        }
        Some("install") => {
            args.next();
            return match (args.next(), args.next()) {
                (folder, None) => Invocation::Install(folder.map(absolute)),
                _ => Invocation::Usage("install takes one folder at most".into()),
            };
        }
        Some("queue") => {
            args.next();
            return match (args.next(), args.next()) {
                (Some("start"), None) => Invocation::Run { messages: vec![Message::Queue { start: true }], raise: false },
                (Some("stop"), None) => Invocation::Run { messages: vec![Message::Queue { start: false }], raise: false },
                _ => Invocation::Usage("queue takes one word: start or stop".into()),
            };
        }
        Some("add") => {
            args.next();
        }
        // Bare addresses: the same as a link handed over by the browser.
        Some(_) => {
            let links: Vec<String> = args.flat_map(batch::extract_links).collect();
            if links.is_empty() {
                return Invocation::Usage("that is neither a command nor an address".into());
            }
            let messages = links.into_iter().map(|link| Message::Capture { capture: Capture { request: Request::new(link), ..Default::default() } }).collect();
            return Invocation::Run { messages, raise: true };
        }
    }

    let (mut dir, mut name, mut referer, mut later, mut ask) = (None, None, None, false, false);
    let mut links: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().map(str::to_owned).ok_or_else(|| format!("{flag} needs a value"));
        let outcome = match arg {
            "--dir" | "-d" => value("--dir").map(|v| dir = Some(absolute(&v))),
            "--name" | "-n" => value("--name").map(|v| name = Some(v)),
            "--referer" => value("--referer").map(|v| referer = Some(v)),
            "--later" => {
                later = true;
                Ok(())
            }
            "--ask" => {
                ask = true;
                Ok(())
            }
            flag if flag.starts_with('-') => Err(format!("unknown option {flag}")),
            address => match batch::collect(address) {
                (found, None) if !found.is_empty() => {
                    links.extend(found);
                    Ok(())
                }
                (_, Some(problem)) => Err(problem.to_string()),
                _ => Err(format!("\"{address}\" is not an address")),
            },
        };
        if let Err(problem) = outcome {
            return Invocation::Usage(problem);
        }
    }
    if links.is_empty() {
        return Invocation::Usage("add needs at least one address".into());
    }
    if name.is_some() && links.len() > 1 {
        return Invocation::Usage("--name names one file; it cannot be used with several addresses".into());
    }
    let messages = links
        .into_iter()
        .map(|link| {
            let mut request = Request::new(link);
            request.referer = referer.clone();
            Message::Add { request, dir: dir.clone(), name: name.clone(), later, ask }
        })
        .collect();
    Invocation::Run { messages, raise: ask }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> Invocation {
        parse(&words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn no_arguments_opens_the_application() {
        assert_eq!(parse_words(&[]), Invocation::Run { messages: vec![], raise: true });
        assert_eq!(parse_words(&["--help"]), Invocation::Help);
        assert_eq!(parse_words(&["-V"]), Invocation::Version);
    }

    #[test]
    fn bare_addresses_are_handed_over_like_captures() {
        match parse_words(&["https://a.example/x.zip", "see also ftp://b.example/y.iso."]) {
            Invocation::Run { messages, raise: true } => {
                assert_eq!(messages.len(), 2);
                assert!(matches!(&messages[0], Message::Capture { capture } if capture.request.url == "https://a.example/x.zip"));
                assert!(matches!(&messages[1], Message::Capture { capture } if capture.request.url == "ftp://b.example/y.iso"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_words(&["nonsense"]), Invocation::Usage(_)));
        assert_eq!(parse_words(&["browsers"]), Invocation::Browsers);
        assert!(matches!(parse_words(&["browsers", "chrome"]), Invocation::Usage(_)));
        assert_eq!(parse_words(&["install"]), Invocation::Install(None));
        assert_eq!(parse_words(&["install", "/Applications/Tools"]), Invocation::Install(Some(PathBuf::from("/Applications/Tools"))));
        assert!(matches!(parse_words(&["install", "a", "b"]), Invocation::Usage(_)));
    }

    #[test]
    fn add_takes_options_and_expands_series() {
        match parse_words(&["add", "https://a.example/part[1-3].bin", "--dir", "/tmp/out", "--later", "--referer", "https://a.example/"]) {
            Invocation::Run { messages, raise: false } => {
                assert_eq!(messages.len(), 3);
                match &messages[2] {
                    Message::Add { request, dir, name, later, ask } => {
                        assert_eq!(request.url, "https://a.example/part3.bin");
                        assert_eq!(request.referer.as_deref(), Some("https://a.example/"));
                        assert_eq!(dir.as_deref(), Some(std::path::Path::new("/tmp/out")));
                        assert_eq!((name, *later, *ask), (&None, true, false));
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
        // A relative folder is resolved here, where "here" still means something.
        match parse_words(&["add", "https://a.example/x", "-d", "sub", "--name", "file.bin", "--ask"]) {
            Invocation::Run { messages, raise: true } => match &messages[0] {
                Message::Add { dir: Some(dir), name: Some(name), ask: true, .. } => {
                    assert!(dir.is_absolute() && dir.ends_with("sub"));
                    assert_eq!(name, "file.bin");
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mistakes_are_explained() {
        for (words, needle) in [
            (vec!["add"], "at least one address"),
            (vec!["add", "--dir"], "needs a value"),
            (vec!["add", "https://a.example/x", "--bogus"], "unknown option"),
            (vec!["add", "https://a.example/[1-2]", "--name", "one"], "names one file"),
            (vec!["add", "not-an-address"], "is not an address"),
            (vec!["add", "https://a.example/x[1-999999]"], "most allowed"),
            (vec!["queue"], "start or stop"),
            (vec!["queue", "start", "now"], "start or stop"),
        ] {
            match parse_words(&words) {
                Invocation::Usage(problem) => assert!(problem.contains(needle), "{words:?}: {problem}"),
                other => panic!("{words:?}: {other:?}"),
            }
        }
        assert_eq!(parse_words(&["queue", "stop"]), Invocation::Run { messages: vec![Message::Queue { start: false }], raise: false });
    }
}
