use std::io::{self, BufRead, Write};

use zinc::engine::Engine;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 {
        // File execution mode
        let filename = &args[1];
        let source = match std::fs::read_to_string(filename) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error reading {filename}: {e}");
                std::process::exit(1);
            }
        };
        // Profiling knobs (Phase 0 perf harness):
        //   ZINC_TIME=1         → print wall time, dispatched-instruction count, and ops/sec
        //   ZINC_OPCODE_HIST=1  → print the opcode-execution histogram
        let want_time = std::env::var("ZINC_TIME").is_ok_and(|v| v == "1");
        let want_hist = std::env::var("ZINC_OPCODE_HIST").is_ok_and(|v| v == "1");
        let mut engine = Engine::new();
        let start = std::time::Instant::now();
        let outcome = engine.eval(&source);
        let elapsed = start.elapsed();
        match outcome {
            Ok(result) => {
                if !result.is_undefined() {
                    println!("{}", engine.display_value(&result));
                }
            }
            Err(e) => {
                eprintln!("{e}");
                if want_hist { zinc::vm::vm::dump_opcode_histogram(); }
                std::process::exit(1);
            }
        }
        if want_time {
            let total: u64 = (0..256)
                .map(|b| zinc::vm::vm::OPCODE_HIST[b].load(std::sync::atomic::Ordering::Relaxed))
                .sum();
            let secs = elapsed.as_secs_f64();
            eprintln!(
                "=== timing: {:.3}s wall ===",
                secs
            );
            if total > 0 {
                eprintln!(
                    "    {total} ops dispatched, {:.1}M ops/sec",
                    (total as f64 / secs) / 1e6
                );
            }
        }
        if want_hist { zinc::vm::vm::dump_opcode_histogram(); }
    } else {
        // REPL mode
        println!("Zinc JavaScript Engine v0.1.0");
        println!("Type JavaScript expressions to evaluate. Ctrl+D to exit.\n");

        let mut engine = Engine::new();
        let stdin = io::stdin();
        let mut stdout = io::stdout();

        loop {
            print!("> ");
            stdout.flush().unwrap();

            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => {
                    println!();
                    break; // EOF
                }
                Ok(_) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match engine.eval(line) {
                        Ok(result) => {
                            println!("{}", engine.display_value(&result));
                        }
                        Err(e) => {
                            eprintln!("{e}");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error reading input: {e}");
                    break;
                }
            }
        }
    }
}
