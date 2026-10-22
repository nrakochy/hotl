# hotl completion for zsh — printed by `hotl init zsh` after the `:` plugin.
# Subcommands: watch fleet doctor resume acp bg attach approve serve setup gc mcp plugins skills workflows update init export stats help version
_hotl_sessions() {
  local -a rows
  rows=(${(f)"$(hotl __sessions 2>/dev/null)"})
  local -a ids
  local r
  for r in $rows; do
    ids+=("${r%%$'\t'*}:${r#*$'\t'}")
  done
  _describe -t sessions 'session' ids
}

_hotl() {
  local -a subcommands
  subcommands=(
    'watch:tmux agent dashboard'
    'doctor:check provider keys, sandbox, config, session store'
    'resume:resume a session (same as -r)'
    'acp:serve the ACP protocol over stdio'
    'bg:background a session'
    'attach:connect to a backgrounded session'
    'approve:answer what a backgrounded session is waiting on'
    'setup:write default config'
    'gc:prune old sessions and blobs'
    'mcp:list MCP servers and trust state'
    'plugins:list Agent Plugins'
    'skills:list skills and marketplaces'
    'workflows:list saved workflows'
    'update:install the latest release'
    'init:print shell integration (zsh, bash, fish)'
    'export:print a session transcript'
    'stats:sessions, turns, tokens and cost'
    'help:show help'
    'version:print the version'
  )
  local curcontext="$curcontext" state line
  typeset -A opt_args
  _arguments -C \
    '(-p --print)'{-p,--print}'[headless one-shot prompt]:prompt:' \
    '--json[emit JSON events]' \
    '--json-schema[validated JSON answer]:schema file:_files' \
    '--goal[run until the condition is met]:condition:' \
    '--plan[start in plan mode]' \
    '--ephemeral[keep no session]' \
    '--search[filter the resume picker by content]:text:' \
    '(-r --resume)'{-r,--resume}'[resume a session]::session:_hotl_sessions' \
    '--fork-from[start a new session with another history]:session:_hotl_sessions' \
    '--keep[fork at an item count]:items:' \
    '--keep-turns[fork at a turn count]:turns:' \
    '(-n --name)'{-n,--name}'[name the session]:name:' \
    '(-V --version)'{-V,--version}'[print the version]' \
    '(-h --help)'{-h,--help}'[show help]' \
    '1: :->cmd' \
    '*:: :->args'
  case $state in
    cmd)
      _describe -t subcommands 'hotl subcommand' subcommands
      _hotl_sessions
      ;;
    args)
      case $line[1] in
        gc) _arguments '--dry-run[show what would be deleted]' '--days[age cutoff]:days:' '--keep[sessions to keep]:count:' ;;
        export) _arguments '1:session:_hotl_sessions' '--md[Markdown]' '--html[HTML page]' ;;
        stats) _arguments '--days[only the last N days]:days:' '--json[JSON output]' ;;
        update) _arguments '--check[only look]' '(-y --yes)'{-y,--yes}'[do not ask]' '--version[install a version]:version:' ;;
        init) _arguments '1:shell:(zsh bash fish)' ;;
        mcp) _arguments '1:verb:(list show add untrust test)' ;;
        skills|plugins) _arguments '1:verb:(list add update remove)' ;;
        workflows) _arguments '1:verb:(list show check)' '--mermaid[print a Mermaid graph]' ;;
        resume|attach) _arguments '1:session:_hotl_sessions' ;;
      esac
      ;;
  esac
}
(( $+functions[compdef] )) && compdef _hotl hotl
