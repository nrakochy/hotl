# hotl completion for bash — the ':' prefix plugin is zsh-only today (hotl init zsh)
# Subcommands: watch fleet doctor resume acp bg attach approve serve setup gc mcp plugins skills workflows update init export stats help version
_hotl_sessions() {
  hotl __sessions 2>/dev/null | cut -f1
}

_hotl() {
  local cur prev
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  local subcommands="watch doctor resume acp bg attach approve setup gc mcp plugins skills workflows update init export stats help version"
  local globals="-p --print --json --json-schema --goal --plan --ephemeral --search -r --resume --fork-from --keep --keep-turns -n --name --version --help"
  case "$prev" in
    -r|--resume|--fork-from|resume|attach|export)
      COMPREPLY=( $(compgen -W "$(_hotl_sessions)" -- "$cur") ); return ;;
    --json-schema)
      COMPREPLY=( $(compgen -f -- "$cur") ); return ;;
    init) COMPREPLY=( $(compgen -W "zsh bash fish" -- "$cur") ); return ;;
    mcp) COMPREPLY=( $(compgen -W "list show add untrust test" -- "$cur") ); return ;;
    skills|plugins) COMPREPLY=( $(compgen -W "list add update remove" -- "$cur") ); return ;;
    workflows) COMPREPLY=( $(compgen -W "list show check" -- "$cur") ); return ;;
  esac
  local sub="${COMP_WORDS[1]}"
  case "$sub" in
    gc) COMPREPLY=( $(compgen -W "--dry-run --days --keep" -- "$cur") ); return ;;
    export) COMPREPLY=( $(compgen -W "--md --html $(_hotl_sessions)" -- "$cur") ); return ;;
    stats) COMPREPLY=( $(compgen -W "--days --json" -- "$cur") ); return ;;
    update) COMPREPLY=( $(compgen -W "--check -y --yes --version" -- "$cur") ); return ;;
    workflows) COMPREPLY=( $(compgen -W "--mermaid" -- "$cur") ); return ;;
  esac
  if [[ $COMP_CWORD -eq 1 ]]; then
    COMPREPLY=( $(compgen -W "$subcommands $globals" -- "$cur") )
  else
    COMPREPLY=( $(compgen -W "$globals" -- "$cur") )
  fi
}
complete -F _hotl hotl
