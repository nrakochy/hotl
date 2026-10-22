# hotl completion for fish — the ':' prefix plugin is zsh-only today (hotl init zsh)
# Subcommands: watch fleet doctor resume acp bg attach approve serve setup gc mcp plugins skills workflows update init export stats help version
function __hotl_sessions
  hotl __sessions 2>/dev/null | string replace -r '\t' '\t'
end
function __hotl_no_subcommand
  not __fish_seen_subcommand_from watch doctor resume acp bg attach approve setup gc mcp plugins skills workflows update init export stats help version
end

complete -c hotl -f
complete -c hotl -n __hotl_no_subcommand -a watch -d 'tmux agent dashboard'
complete -c hotl -n __hotl_no_subcommand -a doctor -d 'check provider keys, sandbox, config, session store'
complete -c hotl -n __hotl_no_subcommand -a resume -d 'resume a session (same as -r)'
complete -c hotl -n __hotl_no_subcommand -a acp -d 'serve the ACP protocol over stdio'
complete -c hotl -n __hotl_no_subcommand -a bg -d 'background a session'
complete -c hotl -n __hotl_no_subcommand -a attach -d 'connect to a backgrounded session'
complete -c hotl -n __hotl_no_subcommand -a approve -d 'answer what a backgrounded session is waiting on'
complete -c hotl -n __hotl_no_subcommand -a setup -d 'write default config'
complete -c hotl -n __hotl_no_subcommand -a gc -d 'prune old sessions and blobs'
complete -c hotl -n __hotl_no_subcommand -a mcp -d 'list MCP servers and trust state'
complete -c hotl -n __hotl_no_subcommand -a plugins -d 'list Agent Plugins'
complete -c hotl -n __hotl_no_subcommand -a skills -d 'list skills and marketplaces'
complete -c hotl -n __hotl_no_subcommand -a workflows -d 'list saved workflows'
complete -c hotl -n __hotl_no_subcommand -a update -d 'install the latest release'
complete -c hotl -n __hotl_no_subcommand -a init -d 'print shell integration (zsh, bash, fish)'
complete -c hotl -n __hotl_no_subcommand -a export -d 'print a session transcript'
complete -c hotl -n __hotl_no_subcommand -a stats -d 'sessions, turns, tokens and cost'
complete -c hotl -n __hotl_no_subcommand -a help -d 'show help'
complete -c hotl -n __hotl_no_subcommand -a version -d 'print the version'

complete -c hotl -s p -l print -d 'headless one-shot prompt' -r
complete -c hotl -l json -d 'emit JSON events'
complete -c hotl -l json-schema -d 'validated JSON answer' -r -F
complete -c hotl -l goal -d 'run until the condition is met' -r
complete -c hotl -l plan -d 'start in plan mode'
complete -c hotl -l ephemeral -d 'keep no session'
complete -c hotl -l search -d 'filter the resume picker by content' -r
complete -c hotl -s r -l resume -d 'resume a session' -a '(__hotl_sessions)'
complete -c hotl -l fork-from -d 'start a new session with another history' -r -a '(__hotl_sessions)'
complete -c hotl -l keep -d 'fork at an item count' -r
complete -c hotl -l keep-turns -d 'fork at a turn count' -r
complete -c hotl -s n -l name -d 'name the session' -r
complete -c hotl -s V -l version -d 'print the version'
complete -c hotl -s h -l help -d 'show help'

complete -c hotl -n '__fish_seen_subcommand_from gc' -l dry-run -d 'show what would be deleted'
complete -c hotl -n '__fish_seen_subcommand_from gc' -l days -d 'age cutoff' -r
complete -c hotl -n '__fish_seen_subcommand_from gc' -l keep -d 'sessions to keep' -r
complete -c hotl -n '__fish_seen_subcommand_from export resume attach' -a '(__hotl_sessions)'
complete -c hotl -n '__fish_seen_subcommand_from export' -l md -d 'Markdown'
complete -c hotl -n '__fish_seen_subcommand_from export' -l html -d 'HTML page'
complete -c hotl -n '__fish_seen_subcommand_from stats' -l days -d 'only the last N days' -r
complete -c hotl -n '__fish_seen_subcommand_from stats' -l json -d 'JSON output'
complete -c hotl -n '__fish_seen_subcommand_from update' -l check -d 'only look'
complete -c hotl -n '__fish_seen_subcommand_from update' -s y -l yes -d 'do not ask'
complete -c hotl -n '__fish_seen_subcommand_from update' -l version -d 'install a version' -r
complete -c hotl -n '__fish_seen_subcommand_from init' -a 'zsh bash fish'
complete -c hotl -n '__fish_seen_subcommand_from mcp' -a 'list show add untrust test'
complete -c hotl -n '__fish_seen_subcommand_from skills plugins' -a 'list add update remove'
complete -c hotl -n '__fish_seen_subcommand_from workflows' -a 'list show check'
complete -c hotl -n '__fish_seen_subcommand_from workflows' -l mermaid -d 'print a Mermaid graph'
