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
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    ExitCode::from(code)
}
