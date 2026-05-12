#![cfg(feature = "completion-generation")]

use std::fs;
use std::io;
use std::path::Path;

use clap_complete::{Shell, generate};

const PRIMARY_COMMAND: &str = "prismtty";
const COMMAND_NAMES: [&str; 3] = ["prismtty", "ptty", "ct"];
const ZSH_PRIMARY_COMPDEF: &str = "#compdef prismtty\n";
const ZSH_ALIAS_COMPDEF: &str = "#compdef prismtty ptty ct\n";

fn main() -> io::Result<()> {
    let completions_dir = Path::new("completions");
    fs::create_dir_all(completions_dir)?;

    write_completion_file(Shell::Bash, &completions_dir.join("prismtty.bash"))?;
    write_completion_file(Shell::Fish, &completions_dir.join("prismtty.fish"))?;
    write_completion_file(Shell::Zsh, &completions_dir.join("_prismtty"))?;

    Ok(())
}

fn write_completion_file(shell: Shell, path: &Path) -> io::Result<()> {
    let mut output = Vec::new();

    if shell == Shell::Zsh {
        let mut command = prismtty::cli::completion_command();
        generate(shell, &mut command, PRIMARY_COMMAND, &mut output);
        let generated = String::from_utf8(output).expect("clap_complete emits UTF-8");
        let generated = generated.replacen(ZSH_PRIMARY_COMPDEF, ZSH_ALIAS_COMPDEF, 1);
        return fs::write(path, generated);
    }

    for (index, command_name) in COMMAND_NAMES.into_iter().enumerate() {
        if index > 0 {
            output.push(b'\n');
        }
        let mut command = prismtty::cli::completion_command().name(command_name);
        generate(shell, &mut command, command_name, &mut output);
    }

    fs::write(path, output)
}
