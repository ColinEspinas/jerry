use std::process::ExitCode;

fn main() -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("jerry: the current directory is not accessible: {error}");
            return ExitCode::from(jerry_cli::exit::FAILED);
        }
    };
    let code = jerry_cli::run(
        std::env::args_os(),
        &|key| std::env::var_os(key),
        &cwd,
        // Owned, not `.lock()`'d: `hook`'s stdin read runs on its own thread so a deadline can
        // bound it, which needs a `'static` handle it can move rather than a borrowed lock.
        Box::new(std::io::stdin()),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    ExitCode::from(code)
}
