//! `sea-movie-creator` のアプリケーションエントリポイント。

use std::path::PathBuf;
use std::process::ExitCode;

use sea_movie_creator::ai::mcp_server::{McpServerState, run_mcp_stdio};
use sea_movie_creator::core::schema::{edit_command_schema_json, project_document_schema_json};
use sea_movie_creator::ui::window::launch_app;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut open_project: Option<PathBuf> = None;
    let mut auto_close_ms: Option<u64> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "mcp" => return run_mcp_subcommand(args),
            "--print-document-schema" => return print_schema(project_document_schema_json()),
            "--print-command-schema" => return print_schema(edit_command_schema_json()),
            "--open" => {
                let Some(val) = args.next() else {
                    eprintln!("--open requires a file path argument");
                    return ExitCode::FAILURE;
                };
                open_project = Some(PathBuf::from(val));
            }
            "--auto-close-ms" => {
                let Some(val) = args.next() else {
                    eprintln!("--auto-close-ms requires a millisecond integer argument");
                    return ExitCode::FAILURE;
                };
                match val.parse::<u64>() {
                    Ok(ms) => auto_close_ms = Some(ms),
                    Err(err) => {
                        eprintln!("invalid --auto-close-ms `{val}`: {err}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let _code = launch_app(open_project, auto_close_ms);
    ExitCode::SUCCESS
}

fn run_mcp_subcommand(mut args: std::iter::Skip<std::env::Args>) -> ExitCode {
    let mut project_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" => {
                let Some(val) = args.next() else {
                    eprintln!("mcp --project requires a file path argument");
                    return ExitCode::FAILURE;
                };
                project_path = Some(PathBuf::from(val));
            }
            other => {
                eprintln!("unknown mcp argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut state = match McpServerState::load_or_new(project_path.as_deref()) {
        Ok(state) => state,
        Err(err) => {
            eprintln!("failed to initialize MCP state: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    if let Err(err) = run_mcp_stdio(&mut state, &mut stdin, &mut stdout) {
        eprintln!("MCP server error: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn print_schema(result: Result<String, serde_json::Error>) -> ExitCode {
    match result {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("failed to generate schema: {err}");
            ExitCode::FAILURE
        }
    }
}
