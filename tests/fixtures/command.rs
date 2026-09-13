// Controlled external process for shell integration tests; no terminal required.
use std::{env, fs, process};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "add") {
        // History recording must not replace a successful cd's status.
        process::exit(7);
    }
    if args.first().is_some_and(|arg| arg == "pick") {
        fs::write(env::var("TEST_QUERY_LOG").unwrap(), env::var("TADORU_QUERY").unwrap_or_default()).unwrap();
    } else if args.first().is_some_and(|arg| arg == "query") {
        fs::write(env::var("TEST_QUERY_LOG").unwrap(), args[2..].join(" ")).unwrap();
    }
    let code = env::var("TEST_EXIT").unwrap().parse::<i32>().unwrap();
    if let Ok(path) = env::var("TEST_PATH") {
        println!("{path}");
    }
    if code != 0 {
        eprintln!("fixture error {code}");
    }
    process::exit(code);
}
