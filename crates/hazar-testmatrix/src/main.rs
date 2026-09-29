//! `hazar-testmatrix` — run the capture matrix.
//!
//! ```bash
//! cargo run -p hazar-testmatrix                     # offline fixtures
//! cargo run -p hazar-testmatrix -- --filter hls     # subset
//! cargo run -p hazar-testmatrix -- --browser        # + headless Chrome & extension
//! HAZAR_LIVE=1 cargo run -p hazar-testmatrix -- --live   # + real site profiles
//! ```

use hazar_testmatrix::{run, RunOptions};

fn main() {
    let mut options = RunOptions::default();
    let mut json = false;
    let mut coverage = false;
    let mut capture: Option<String> = None;
    let mut capture_seconds: u64 = 900;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--filter" => options.filter = args.next(),
            "--browser" => options.browser = true,
            "--live" => options.live = true,
            "--keep" => options.keep_files = true,
            "--json" => json = true,
            "--coverage" => coverage = true,
            "--capture" => capture = args.next(),
            "--capture-seconds" => {
                capture_seconds = args.next().and_then(|v| v.parse().ok()).unwrap_or(900)
            }
            "--help" | "-h" => {
                println!(
                    "hazar-testmatrix [--filter NAME] [--browser] [--live] [--coverage] [--keep] [--json]"
                );
                return;
            }
            other => eprintln!("unknown argument: {other}"),
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");

    if let Some(url) = capture {
        match runtime.block_on(hazar_testmatrix::browser::capture(&url, capture_seconds)) {
            Ok(()) => return,
            Err(error) => {
                eprintln!("capture failed: {error}");
                std::process::exit(1);
            }
        }
    }

    let report = runtime.block_on(run(&options));

    if json {
        println!("{}", report.to_json());
    } else {
        report.print();
        if coverage {
            report.print_coverage();
        }
    }

    if !report.is_green() {
        std::process::exit(1);
    }
}
