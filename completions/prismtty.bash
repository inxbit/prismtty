_prismtty() {
  local cur prev words cword
  _init_completion || return

  case "$prev" in
    --profile|-p)
      COMPREPLY=($(compgen -W "generic juniper cisco versa arista fortinet palo-alto linux-unix" -- "$cur"))
      return
      ;;
    --config|-c)
      _filedir yml
      return
      ;;
    --trace-io)
      _filedir
      return
      ;;
  esac

  if [[ ${words[1]} == profiles ]]; then
    COMPREPLY=($(compgen -W "list show validate test" -- "$cur"))
    return
  fi

  COMPREPLY=($(compgen -W "-p --profile --no-auto-detect -c --config --strip-ansi --show-profile --local-echo --trace-io -R --rgb --pcre -b --benchmark -r --reload -h --help -V -v --version profiles" -- "$cur"))
}
complete -F _prismtty prismtty
complete -F _prismtty ptty
complete -F _prismtty ct
