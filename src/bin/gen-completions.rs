#![cfg(feature = "completion-generation")]

use std::fs;
use std::io;
use std::path::Path;

use clap_complete::{Shell, generate};

const PRIMARY_COMMAND: &str = "prismtty";
const COMMAND_NAMES: [&str; 3] = ["prismtty", "ptty", "ct"];
const ZSH_PRIMARY_COMPDEF: &str = "#compdef prismtty\n";
const ZSH_ALIAS_COMPDEF: &str = "#compdef prismtty ptty ct\n";
const BASH_SAFE_COMPGEN_HELPER: &str = r#"_prismtty_compgen() {
    local candidate
    COMPREPLY=()
    while IFS= read -r candidate; do
        COMPREPLY+=("${candidate}")
    done < <(compgen "$@")
}

"#;
const BASH_WORD_COMPLETION: &str = r#"COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )"#;
const BASH_FILE_COMPLETION: &str = r#"COMPREPLY=($(compgen -f "${cur}"))"#;

fn main() -> io::Result<()> {
    // Anchor to the crate root so the tool writes to the repo's completions
    // directory regardless of the current working directory.
    let completions_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("completions");
    fs::create_dir_all(&completions_dir)?;

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
        assert!(
            generated.contains(ZSH_PRIMARY_COMPDEF),
            "clap_complete zsh output no longer contains {ZSH_PRIMARY_COMPDEF:?}; \
             update ZSH_PRIMARY_COMPDEF in gen-completions.rs"
        );
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

    if shell == Shell::Bash {
        let generated = String::from_utf8(output).expect("clap_complete emits UTF-8");
        return fs::write(path, harden_bash_filename_completion(generated));
    }

    fs::write(path, output)
}

fn harden_bash_filename_completion(generated: String) -> String {
    let generated = generated
        .replace(
            BASH_WORD_COMPLETION,
            r#"_prismtty_compgen -W "${opts}" -- "${cur}""#,
        )
        .replace(BASH_FILE_COMPLETION, r#"_prismtty_compgen -f -- "${cur}""#);
    assert!(
        !generated.contains("COMPREPLY=( $(compgen") && !generated.contains("COMPREPLY=($(compgen"),
        "clap_complete Bash output changed; update the safe compgen transform"
    );
    format!("{BASH_SAFE_COMPGEN_HELPER}{generated}")
}
