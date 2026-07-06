# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_prismtty_global_optspecs
    string join \n h/help V/version b/benchmark r/reload R/rgb pcre no-auto-detect no-dynamic-profile no-minimal-reset strip-ansi sanitize show-profile local-echo trace-io= p/profile= c/config=
end

function __fish_prismtty_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_prismtty_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_prismtty_using_subcommand
    set -l cmd (__fish_prismtty_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c prismtty -n "__fish_prismtty_needs_command" -l trace-io -d 'Append hex-encoded PTY input/output diagnostics (records all keystrokes, including passwords)' -r -F
complete -c prismtty -n "__fish_prismtty_needs_command" -s p -l profile -d 'Force a profile; repeat to enable several' -r
complete -c prismtty -n "__fish_prismtty_needs_command" -s c -l config -d 'Load a ChromaTerm-compatible YAML config' -r -F
complete -c prismtty -n "__fish_prismtty_needs_command" -s h -l help -d 'Show help'
complete -c prismtty -n "__fish_prismtty_needs_command" -s V -s v -l version -d 'Show version'
complete -c prismtty -n "__fish_prismtty_needs_command" -s b -l benchmark -d 'Print per-rule timing and match-count data to stderr'
complete -c prismtty -n "__fish_prismtty_needs_command" -s r -l reload -d 'Ask running PrismTTY sessions to reload config'
complete -c prismtty -n "__fish_prismtty_needs_command" -s R -l rgb -d 'Force RGB color output'
complete -c prismtty -n "__fish_prismtty_needs_command" -l pcre -d 'Accepted for ChromaTerm compatibility; PCRE2 is always used'
complete -c prismtty -n "__fish_prismtty_needs_command" -l no-auto-detect -d 'Use only the generic profile unless --profile is set'
complete -c prismtty -n "__fish_prismtty_needs_command" -l no-dynamic-profile -d 'Disable profile switching inside wrapped interactive shells'
complete -c prismtty -n "__fish_prismtty_needs_command" -l no-minimal-reset -d 'Use full SGR resets instead of minimal foreground/background resets in interactive streams'
complete -c prismtty -n "__fish_prismtty_needs_command" -l strip-ansi -d 'Remove existing ANSI before applying PrismTTY styles'
complete -c prismtty -n "__fish_prismtty_needs_command" -l sanitize -d 'Strip window-title, clipboard (OSC 52), and other OSC/DCS string escapes from program output'
complete -c prismtty -n "__fish_prismtty_needs_command" -l show-profile -d 'Print selected profiles to stderr'
complete -c prismtty -n "__fish_prismtty_needs_command" -l local-echo -d 'Locally echo typed printable keys for no-echo device sessions (also echoes secrets typed at hidden prompts)'
complete -c prismtty -n "__fish_prismtty_needs_command" -a "profiles" -d 'Manage profiles'
complete -c prismtty -n "__fish_prismtty_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "list" -d 'List available profiles'
complete -c prismtty -n "__fish_prismtty_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "show" -d 'Show a profile'
complete -c prismtty -n "__fish_prismtty_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "validate" -d 'Validate a profile file'
complete -c prismtty -n "__fish_prismtty_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "test" -d 'Highlight a fixture with a profile'

# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_ptty_global_optspecs
    string join \n h/help V/version b/benchmark r/reload R/rgb pcre no-auto-detect no-dynamic-profile no-minimal-reset strip-ansi sanitize show-profile local-echo trace-io= p/profile= c/config=
end

function __fish_ptty_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_ptty_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_ptty_using_subcommand
    set -l cmd (__fish_ptty_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c ptty -n "__fish_ptty_needs_command" -l trace-io -d 'Append hex-encoded PTY input/output diagnostics (records all keystrokes, including passwords)' -r -F
complete -c ptty -n "__fish_ptty_needs_command" -s p -l profile -d 'Force a profile; repeat to enable several' -r
complete -c ptty -n "__fish_ptty_needs_command" -s c -l config -d 'Load a ChromaTerm-compatible YAML config' -r -F
complete -c ptty -n "__fish_ptty_needs_command" -s h -l help -d 'Show help'
complete -c ptty -n "__fish_ptty_needs_command" -s V -s v -l version -d 'Show version'
complete -c ptty -n "__fish_ptty_needs_command" -s b -l benchmark -d 'Print per-rule timing and match-count data to stderr'
complete -c ptty -n "__fish_ptty_needs_command" -s r -l reload -d 'Ask running PrismTTY sessions to reload config'
complete -c ptty -n "__fish_ptty_needs_command" -s R -l rgb -d 'Force RGB color output'
complete -c ptty -n "__fish_ptty_needs_command" -l pcre -d 'Accepted for ChromaTerm compatibility; PCRE2 is always used'
complete -c ptty -n "__fish_ptty_needs_command" -l no-auto-detect -d 'Use only the generic profile unless --profile is set'
complete -c ptty -n "__fish_ptty_needs_command" -l no-dynamic-profile -d 'Disable profile switching inside wrapped interactive shells'
complete -c ptty -n "__fish_ptty_needs_command" -l no-minimal-reset -d 'Use full SGR resets instead of minimal foreground/background resets in interactive streams'
complete -c ptty -n "__fish_ptty_needs_command" -l strip-ansi -d 'Remove existing ANSI before applying PrismTTY styles'
complete -c ptty -n "__fish_ptty_needs_command" -l sanitize -d 'Strip window-title, clipboard (OSC 52), and other OSC/DCS string escapes from program output'
complete -c ptty -n "__fish_ptty_needs_command" -l show-profile -d 'Print selected profiles to stderr'
complete -c ptty -n "__fish_ptty_needs_command" -l local-echo -d 'Locally echo typed printable keys for no-echo device sessions (also echoes secrets typed at hidden prompts)'
complete -c ptty -n "__fish_ptty_needs_command" -a "profiles" -d 'Manage profiles'
complete -c ptty -n "__fish_ptty_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "list" -d 'List available profiles'
complete -c ptty -n "__fish_ptty_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "show" -d 'Show a profile'
complete -c ptty -n "__fish_ptty_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "validate" -d 'Validate a profile file'
complete -c ptty -n "__fish_ptty_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "test" -d 'Highlight a fixture with a profile'

# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_ct_global_optspecs
    string join \n h/help V/version b/benchmark r/reload R/rgb pcre no-auto-detect no-dynamic-profile no-minimal-reset strip-ansi sanitize show-profile local-echo trace-io= p/profile= c/config=
end

function __fish_ct_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_ct_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_ct_using_subcommand
    set -l cmd (__fish_ct_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c ct -n "__fish_ct_needs_command" -l trace-io -d 'Append hex-encoded PTY input/output diagnostics (records all keystrokes, including passwords)' -r -F
complete -c ct -n "__fish_ct_needs_command" -s p -l profile -d 'Force a profile; repeat to enable several' -r
complete -c ct -n "__fish_ct_needs_command" -s c -l config -d 'Load a ChromaTerm-compatible YAML config' -r -F
complete -c ct -n "__fish_ct_needs_command" -s h -l help -d 'Show help'
complete -c ct -n "__fish_ct_needs_command" -s V -s v -l version -d 'Show version'
complete -c ct -n "__fish_ct_needs_command" -s b -l benchmark -d 'Print per-rule timing and match-count data to stderr'
complete -c ct -n "__fish_ct_needs_command" -s r -l reload -d 'Ask running PrismTTY sessions to reload config'
complete -c ct -n "__fish_ct_needs_command" -s R -l rgb -d 'Force RGB color output'
complete -c ct -n "__fish_ct_needs_command" -l pcre -d 'Accepted for ChromaTerm compatibility; PCRE2 is always used'
complete -c ct -n "__fish_ct_needs_command" -l no-auto-detect -d 'Use only the generic profile unless --profile is set'
complete -c ct -n "__fish_ct_needs_command" -l no-dynamic-profile -d 'Disable profile switching inside wrapped interactive shells'
complete -c ct -n "__fish_ct_needs_command" -l no-minimal-reset -d 'Use full SGR resets instead of minimal foreground/background resets in interactive streams'
complete -c ct -n "__fish_ct_needs_command" -l strip-ansi -d 'Remove existing ANSI before applying PrismTTY styles'
complete -c ct -n "__fish_ct_needs_command" -l sanitize -d 'Strip window-title, clipboard (OSC 52), and other OSC/DCS string escapes from program output'
complete -c ct -n "__fish_ct_needs_command" -l show-profile -d 'Print selected profiles to stderr'
complete -c ct -n "__fish_ct_needs_command" -l local-echo -d 'Locally echo typed printable keys for no-echo device sessions (also echoes secrets typed at hidden prompts)'
complete -c ct -n "__fish_ct_needs_command" -a "profiles" -d 'Manage profiles'
complete -c ct -n "__fish_ct_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "list" -d 'List available profiles'
complete -c ct -n "__fish_ct_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "show" -d 'Show a profile'
complete -c ct -n "__fish_ct_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "validate" -d 'Validate a profile file'
complete -c ct -n "__fish_ct_using_subcommand profiles; and not __fish_seen_subcommand_from list show validate test" -f -a "test" -d 'Highlight a fixture with a profile'
