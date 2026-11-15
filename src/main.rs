//! `sea-movie-creator` のアプリケーションエントリポイント。

use std::path::PathBuf;
use std::process::ExitCode;

use sea_movie_creator::core::schema::{edit_command_schema_json, project_document_schema_json};
use sea_movie_creator::ui::window::launch_app;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut open_project: Option<PathBuf> = None;
    let mut auto_close_ms: Option<u64> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
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
