_prismtty_compgen() {
    local candidate
    COMPREPLY=()
    while IFS= read -r candidate; do
        COMPREPLY+=("${candidate}")
    done < <(compgen "$@")
}

# Finds where PrismTTY's own words end. Returns 0 when the word under the
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
            --trace-io|--profile|--config)
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
                        p|c)
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

_prismtty() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    local _prismtty_subcommand_at
    _prismtty_wrapped_command && return 0

    for i in "${COMP_WORDS[0]}" "${COMP_WORDS[@]:_prismtty_subcommand_at:COMP_CWORD-_prismtty_subcommand_at}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="prismtty"
                ;;
            prismtty,profiles)
                cmd="prismtty__subcmd__profiles"
                ;;
            prismtty__subcmd__profiles,list)
                cmd="prismtty__subcmd__profiles__subcmd__list"
                ;;
            prismtty__subcmd__profiles,show)
                cmd="prismtty__subcmd__profiles__subcmd__show"
                ;;
            prismtty__subcmd__profiles,test)
                cmd="prismtty__subcmd__profiles__subcmd__test"
                ;;
            prismtty__subcmd__profiles,validate)
                cmd="prismtty__subcmd__profiles__subcmd__validate"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        prismtty)
            opts="-h -v -V -b -r -R -p -c --help --version --benchmark --reload --rgb --pcre --no-auto-detect --no-dynamic-profile --no-minimal-reset --strip-ansi --sanitize --show-profile --local-echo --trace-io --profile --config profiles"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                --trace-io)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                --profile)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                -p)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                --config)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                -c)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        prismtty__subcmd__profiles)
            opts="list show validate test"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        prismtty__subcmd__profiles__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        prismtty__subcmd__profiles__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        prismtty__subcmd__profiles__subcmd__test)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        prismtty__subcmd__profiles__subcmd__validate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _prismtty -o nosort -o bashdefault -o default prismtty
else
    complete -F _prismtty -o bashdefault -o default prismtty
fi

_ptty() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    local _prismtty_subcommand_at
    _prismtty_wrapped_command && return 0

    for i in "${COMP_WORDS[0]}" "${COMP_WORDS[@]:_prismtty_subcommand_at:COMP_CWORD-_prismtty_subcommand_at}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="ptty"
                ;;
            ptty,profiles)
                cmd="ptty__subcmd__profiles"
                ;;
            ptty__subcmd__profiles,list)
                cmd="ptty__subcmd__profiles__subcmd__list"
                ;;
            ptty__subcmd__profiles,show)
                cmd="ptty__subcmd__profiles__subcmd__show"
                ;;
            ptty__subcmd__profiles,test)
                cmd="ptty__subcmd__profiles__subcmd__test"
                ;;
            ptty__subcmd__profiles,validate)
                cmd="ptty__subcmd__profiles__subcmd__validate"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        ptty)
            opts="-h -v -V -b -r -R -p -c --help --version --benchmark --reload --rgb --pcre --no-auto-detect --no-dynamic-profile --no-minimal-reset --strip-ansi --sanitize --show-profile --local-echo --trace-io --profile --config profiles"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                --trace-io)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                --profile)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                -p)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                --config)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                -c)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ptty__subcmd__profiles)
            opts="list show validate test"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ptty__subcmd__profiles__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ptty__subcmd__profiles__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ptty__subcmd__profiles__subcmd__test)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ptty__subcmd__profiles__subcmd__validate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _ptty -o nosort -o bashdefault -o default ptty
else
    complete -F _ptty -o bashdefault -o default ptty
fi

_ct() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    local _prismtty_subcommand_at
    _prismtty_wrapped_command && return 0

    for i in "${COMP_WORDS[0]}" "${COMP_WORDS[@]:_prismtty_subcommand_at:COMP_CWORD-_prismtty_subcommand_at}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="ct"
                ;;
            ct,profiles)
                cmd="ct__subcmd__profiles"
                ;;
            ct__subcmd__profiles,list)
                cmd="ct__subcmd__profiles__subcmd__list"
                ;;
            ct__subcmd__profiles,show)
                cmd="ct__subcmd__profiles__subcmd__show"
                ;;
            ct__subcmd__profiles,test)
                cmd="ct__subcmd__profiles__subcmd__test"
                ;;
            ct__subcmd__profiles,validate)
                cmd="ct__subcmd__profiles__subcmd__validate"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        ct)
            opts="-h -v -V -b -r -R -p -c --help --version --benchmark --reload --rgb --pcre --no-auto-detect --no-dynamic-profile --no-minimal-reset --strip-ansi --sanitize --show-profile --local-echo --trace-io --profile --config profiles"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                --trace-io)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                --profile)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                -p)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                --config)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                -c)
                    _prismtty_compgen -f -- "${cur}"
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ct__subcmd__profiles)
            opts="list show validate test"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ct__subcmd__profiles__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ct__subcmd__profiles__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ct__subcmd__profiles__subcmd__test)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
        ct__subcmd__profiles__subcmd__validate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                _prismtty_compgen -W "${opts}" -- "${cur}"
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            _prismtty_compgen -W "${opts}" -- "${cur}"
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _ct -o nosort -o bashdefault -o default ct
else
    complete -F _ct -o bashdefault -o default ct
fi
