complete -c prismtty -s p -l profile -xa "generic juniper cisco arubacx versa arista fortinet palo-alto linux-unix" -d "Force a profile"
complete -c prismtty -l no-auto-detect -d "Use only generic unless profiles are forced"
complete -c prismtty -l no-dynamic-profile -d "Disable profile switching inside wrapped interactive shells"
complete -c prismtty -s c -l config -r -d "Load a ChromaTerm-compatible YAML config"
complete -c prismtty -l strip-ansi -d "Strip existing ANSI before highlighting"
complete -c prismtty -l show-profile -d "Print selected profiles to stderr"
complete -c prismtty -l local-echo -d "Locally echo typed printable keys for no-echo device sessions"
complete -c prismtty -l trace-io -r -d "Append hex-encoded PTY input/output diagnostics"
complete -c prismtty -s R -l rgb -d "Force RGB color output"
complete -c prismtty -l pcre -d "Accepted for ChromaTerm compatibility"
complete -c prismtty -s b -l benchmark -d "Print per-rule benchmark timings"
complete -c prismtty -s r -l reload -d "Reload running PrismTTY sessions"
complete -c prismtty -s h -l help -d "Show help"
complete -c prismtty -s V -l version -d "Show version"
complete -c prismtty -f -n "__fish_use_subcommand" -a profiles -d "Manage profiles"
complete -c prismtty -f -n "__fish_seen_subcommand_from profiles" -a "list show validate test"

complete -c ptty -w prismtty
complete -c ct -w prismtty
