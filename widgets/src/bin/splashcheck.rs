//! `splashcheck` — static semantic analysis of a user-written Splash app.
//!
//! The core is the `splashcheck` module; this binary is the command line
//! around it and owns the one expensive thing: the host context the validator
//! evaluates in, built once and reused across every file it checks.

use std::io::Write;
use std::process::ExitCode;

use makepad_widgets::splashcheck::{self, Parsed};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match splashcheck::parse(&args) {
        Err(message) => {
            let _ = writeln!(std::io::stderr(), "{}: {message}", splashcheck::TOOL_NAME);
            eprint!("{}", splashcheck::usage());
            ExitCode::from(2)
        }
        Ok(Parsed::Help) => {
            print!("{}", splashcheck::usage());
            ExitCode::SUCCESS
        }
        Ok(Parsed::Version) => {
            println!("{} {}", splashcheck::TOOL_NAME, env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Parsed::Run(invocation)) => {
            let mut cx = makepad_widgets::Cx::new(Box::new(|_, _| {}));
            cx.with_vm(makepad_widgets::script_mod);
            let mut out = std::io::stdout();
            let mut err = std::io::stderr();
            ExitCode::from(splashcheck::run(&invocation, &mut cx, &mut out, &mut err) as u8)
        }
    }
}
