_prismtty_compgen() {
    local candidate
    COMPREPLY=()
    while IFS= read -r candidate; do
        COMPREPLY+=("${candidate}")
    done < <(compgen "$@")
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

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
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

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
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

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
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
