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

// clap_complete looks for a subcommand name among all the words before the
// cursor and offers PrismTTY's options at every position. PrismTTY stops
// reading its own arguments at `--` or at the first word that is not an
// option, so the generated scan is narrowed to what the parser would read.
// LONG_VALUE_OPTIONS and SHORT_VALUE_OPTIONS are filled in from the command.
const BASH_WRAPPED_COMMAND_HELPER: &str = r#"# Finds where PrismTTY's own words end. Returns 0 when the word under the
# cursor belongs to the wrapped command: COMPREPLY stays empty there and Bash
# falls back to its default completion. Otherwise returns 1 and sets
# _prismtty_subcommand_at to the index of the `profiles` subcommand word, or to
# COMP_CWORD when there is none.
_prismtty_wrapped_command() {
    local i=1 word rest
    _prismtty_subcommand_at=${COMP_CWORD}
    while (( i < COMP_CWORD )); do
        word="${COMP_WORDS[i]}"
        if [[ -n "${word}" && -z "${word//[:=]/}" ]]; then
            # Bash 4 and later split words at `=` and `:`; this piece and the
            # next one belong to the word before them.
            (( i += 2 ))
            continue
        fi
        if [[ -n "${word}" && -z "${word//[0-9]/}" && "${COMP_WORDS[i+1]}" == [\<\>]* ]]; then
            # A file descriptor in front of a redirection.
            (( i += 1 ))
            continue
        fi
        if [[ -n "${word}" && -z "${word//[<>&|]/}" ]]; then
            # A redirection and its target.
            (( i += 2 ))
            continue
        fi
        case "${word}" in
            --)
                return 0
                ;;
            LONG_VALUE_OPTIONS)
                # The value is the next word, unless Bash split `--option=value`.
                [[ "${COMP_WORDS[i+1]}" == "=" ]] || (( i += 1 ))
                (( i += 1 ))
                ;;
            --*)
                (( i += 1 ))
                ;;
            -?*)
                rest="${word#-}"
                (( i += 1 ))
                while [[ -n "${rest}" ]]; do
                    case "${rest:0:1}" in
                        SHORT_VALUE_OPTIONS)
                            # The value is the rest of this word, or the next word.
                            [[ ${#rest} -eq 1 ]] && (( i += 1 ))
                            break
                            ;;
                    esac
                    rest="${rest:1}"
                done
                ;;
            profiles)
                _prismtty_subcommand_at=${i}
                return 1
                ;;
            *)
                return 0
                ;;
        esac
    done
    return 1
}

"#;
const BASH_SUBCOMMAND_SCAN: &str = r#"    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
"#;
const BASH_NARROWED_SUBCOMMAND_SCAN: &str = r#"    cmd=""
    opts=""

    local _prismtty_subcommand_at
    _prismtty_wrapped_command && return 0

    for i in "${COMP_WORDS[0]}" "${COMP_WORDS[@]:_prismtty_subcommand_at:COMP_CWORD-_prismtty_subcommand_at}"
"#;

// clap_complete puts the subcommand after the wrapped command's first word, so
// zsh offered the `profiles` subcommands one word late. Both share the first
// word here, and the words of a wrapped command go to its own completion.
const ZSH_ROOT_CALL: &str = r#"    local context curcontext="$curcontext" state line
    _arguments "${_arguments_options[@]}" : \
"#;
const ZSH_ROOT_CALL_STOPPING_AT_COMMAND: &str = r#"    local context curcontext="$curcontext" state line
    local -a prismtty_words
    prismtty_words=("${words[@]}")
    local -i prismtty_current=$CURRENT
    local prismtty_subcommand
    _arguments "${_arguments_options[@]}" -A "-*" : \
"#;
const ZSH_ROOT_POSITIONALS: &str = r#"'::command:_default' \
":: :_prismtty_commands" \
"*::: :->prismtty" \
&& ret=0
    case $state in
    (prismtty)
        words=($line[2] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:prismtty-command-$line[2]:"
        case $line[2] in
            (profiles)
"#;
const ZSH_ROOT_FIRST_WORD: &str = r#"": :_prismtty_first_word" \
"*::: :->prismtty" \
&& ret=0
    case $state in
    (prismtty)
        prismtty_subcommand=${(Q)line[1]}
        [[ $prismtty_words[prismtty_current-CURRENT-1] == -- ]] && prismtty_subcommand=
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:prismtty-command-$line[1]:"
        case $prismtty_subcommand in
            (profiles)
"#;
const ZSH_ROOT_END: &str = r#"    ;;
esac
;;
        esac
    ;;
esac
}
"#;
const ZSH_ROOT_END_WITH_WRAPPED_COMMAND: &str = r#"    ;;
esac
;;
            (*)
_normal && ret=0
;;
        esac
    ;;
esac
}
"#;
const ZSH_COMMANDS_FUNCTION: &str = r#"(( $+functions[_prismtty_commands] )) ||
_prismtty_commands() {"#;
const ZSH_FIRST_WORD_FUNCTION: &str = r#"(( $+functions[_prismtty_first_word] )) ||
_prismtty_first_word() {
    local ret=1
    if [[ $words[CURRENT-1] != -- ]]; then
        _prismtty_commands "$@" && ret=0
    fi
    _command_names -e "$@" && ret=0
    _default "$@" && ret=0
    return ret
}
"#;

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
        return fs::write(path, stop_zsh_completion_at_wrapped_command(&generated));
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
        let generated = stop_bash_completion_at_wrapped_command(&generated);
        return fs::write(path, harden_bash_filename_completion(generated));
    }

    fs::write(path, output)
}

fn replace_generated(generated: &str, from: &str, to: &str, occurrences: usize) -> String {
    assert_eq!(
        generated.matches(from).count(),
        occurrences,
        "clap_complete output changed; update the transform that expects {from:?}"
    );
    generated.replace(from, to)
}

fn stop_bash_completion_at_wrapped_command(generated: &str) -> String {
    let mut long_value_options = Vec::new();
    let mut short_value_options = Vec::new();
    let command = prismtty::cli::completion_command();
    for arg in command.get_arguments() {
        if arg.is_positional() || !arg.get_action().takes_values() {
            continue;
        }
        if let Some(long) = arg.get_long() {
            long_value_options.push(format!("--{long}"));
        }
        if let Some(short) = arg.get_short() {
            short_value_options.push(short.to_string());
        }
    }
    let helper = BASH_WRAPPED_COMMAND_HELPER
        .replace("LONG_VALUE_OPTIONS", &long_value_options.join("|"))
        .replace("SHORT_VALUE_OPTIONS", &short_value_options.join("|"));
    let generated = replace_generated(
        generated,
        BASH_SUBCOMMAND_SCAN,
        BASH_NARROWED_SUBCOMMAND_SCAN,
        COMMAND_NAMES.len(),
    );
    format!("{helper}{generated}")
}

fn stop_zsh_completion_at_wrapped_command(generated: &str) -> String {
    let generated = replace_generated(
        generated,
        ZSH_ROOT_CALL,
        ZSH_ROOT_CALL_STOPPING_AT_COMMAND,
        1,
    );
    let generated = replace_generated(&generated, ZSH_ROOT_POSITIONALS, ZSH_ROOT_FIRST_WORD, 1);
    let generated = replace_generated(
        &generated,
        ZSH_ROOT_END,
        ZSH_ROOT_END_WITH_WRAPPED_COMMAND,
        1,
    );
    let first_word_function = format!("{ZSH_FIRST_WORD_FUNCTION}{ZSH_COMMANDS_FUNCTION}");
    replace_generated(&generated, ZSH_COMMANDS_FUNCTION, &first_word_function, 1)
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
