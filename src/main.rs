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
        let mut engine = Engine::new();
        match engine.eval(&source) {
            Ok(result) => {
                if !result.is_undefined() {
                    println!("{}", engine.display_value(&result));
                }
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
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
